use super::loopback::ChildGuard;
use arany::{StateRoot, StoreError};
use std::{
    io::Read,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
#[ignore = "supervised process helper"]
fn account_lock_holder() {
    let state_path = std::env::var_os("ARANY_TEST_STATE_PATH").expect("state path");
    let workspace = std::env::var_os("ARANY_TEST_WORKSPACE_PATH").expect("workspace path");
    let ready_path = std::env::var_os("ARANY_TEST_READY_PATH").expect("ready path");
    let state = StateRoot::admit(Path::new(&state_path)).expect("state admission");
    state
        .with_account_replacement_lock(Path::new(&workspace), || {
            std::fs::write(ready_path, b"held").expect("complete ready marker");
            std::io::stdin()
                .read_exact(&mut [0])
                .expect("parent release signal");
        })
        .expect("hold account lock");
}

#[test]
fn account_replacement_lock_excludes_another_process_and_recovers_after_death() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).expect("state admission");
    let ready_path = temp.path().join("holder-ready");

    let mut command = Command::new(std::env::current_exe().expect("test binary"));
    command
        .env_clear()
        .env("ARANY_TEST_STATE_PATH", &state_path)
        .env("ARANY_TEST_WORKSPACE_PATH", &workspace)
        .env("ARANY_TEST_READY_PATH", &ready_path)
        .args(["--ignored", "--exact", "account_lock::account_lock_holder"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut holder = ChildGuard::new(command.spawn().expect("account lock holder"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::fs::read(&ready_path).ok().as_deref() != Some(b"held") {
        assert!(
            holder.child().try_wait().expect("holder status").is_none(),
            "holder exited before acquiring account lock"
        );
        assert!(
            Instant::now() < deadline,
            "account holder readiness deadline"
        );
        thread::yield_now();
    }

    assert!(matches!(
        state.with_account_replacement_lock(&workspace, || ()),
        Err(StoreError::AccountBusy)
    ));
    holder.child().kill().expect("terminate test-owned holder");
    let status = holder.take().wait().expect("reap account lock holder");
    assert!(
        !status.success(),
        "holder was terminated while holding lock"
    );
    state
        .with_account_replacement_lock(&workspace, || ())
        .expect("account lock recovered after holder death");
}

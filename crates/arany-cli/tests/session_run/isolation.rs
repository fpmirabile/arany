use std::process::Command;

#[cfg(target_os = "linux")]
pub fn isolated_script(root: &std::path::Path) -> Command {
    script(root, true)
}

#[cfg(target_os = "linux")]
pub fn account_isolated_script(root: &std::path::Path) -> Command {
    script(root, false)
}

#[cfg(target_os = "linux")]
fn script(root: &std::path::Path, isolate_network: bool) -> Command {
    let product = std::path::Path::new(env!("CARGO_BIN_EXE_arany"));
    let mut command = account_command(root, isolate_network, product);
    command.arg("/usr/bin/script");
    command
}

#[cfg(target_os = "linux")]
pub fn account_isolated_command(
    root: &std::path::Path,
    executable: &str,
    product: &std::path::Path,
) -> Command {
    let mut command = account_command(root, false, product);
    command.arg(executable);
    command
}

#[cfg(target_os = "linux")]
pub fn enter_account_isolated_test() -> bool {
    const CHILD: &str = "ARANY_ACCOUNT_ISOLATED_TEST";

    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    use crate::process::BoundedOutput;
    let test = std::thread::current()
        .name()
        .expect("named test thread")
        .to_owned();
    let executable = std::env::current_exe().expect("test executable");
    let temp = tempfile::tempdir().expect("isolated account test root");
    let temp_files = temp.path().join("tmp");
    std::fs::create_dir(&temp_files).expect("isolated test temporary directory");
    let mut command = account_isolated_command(temp.path(), "/arany", &executable);
    command
        .env_clear()
        .env(CHILD, "1")
        .env("TMPDIR", temp_files)
        .args(["--exact", &test, "--include-ignored", "--nocapture"]);
    let output = command
        .bounded_output_for(std::time::Duration::from_secs(600), 32 * 1024 * 1024)
        .expect("isolated test process");
    assert!(
        output.status.success(),
        "isolated test failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    crate::process::assert_single_test_completed(&output.stdout, &test);
    false
}

#[cfg(target_os = "linux")]
pub fn enter_native_account_isolated_test() -> bool {
    const CHILD: &str = "ARANY_ACCOUNT_ISOLATED_TEST";
    use crate::process::BoundedOutput;
    use std::{io::Write, os::unix::fs::DirBuilderExt, path::Path, time::Duration};

    if std::env::var_os(CHILD).is_some() {
        let test = std::thread::current()
            .name()
            .expect("named native test thread")
            .to_owned();
        let case = native_account_test_case(&test);
        std::panic::set_hook(Box::new(move |panic| {
            let (file, line, column) = panic.location().map_or(("unknown", 0, 0), |location| {
                let file = Path::new(location.file())
                    .file_name()
                    .and_then(|name| name.to_str())
                    .filter(|name| {
                        !name.is_empty()
                            && name.len() <= 64
                            && name.bytes().all(|byte| {
                                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
                            })
                    })
                    .unwrap_or("unknown");
                (file, location.line(), location.column())
            });
            let _ = writeln!(
                std::io::stderr().lock(),
                "ARANY_NATIVE_PANIC; case={case}; file={file}; line={line}; column={column}"
            );
        }));
        return true;
    }

    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let test = std::thread::current()
        .name()
        .expect("named test thread")
        .to_owned();
    let case = native_account_test_case(&test);
    let executable = std::env::current_exe().expect("test executable");
    let temp = tempfile::tempdir().expect("native account test root");
    for name in ["account-home", "home", "state"] {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(temp.path().join(name))
            .expect("native fixture directory");
    }
    let guard = Path::new(env!("CARGO_BIN_EXE_arany"));
    let mut command = native_account_isolated_test_command(temp.path(), &executable, guard);
    command
        .args(["--exact", &test, "--include-ignored", "--nocapture"])
        .env_clear()
        .current_dir(temp.path());
    let temp_path = temp.path().to_path_buf();
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let output = command
            .bounded_output_for(Duration::from_secs(90), 32 * 1024 * 1024)
            .expect("native account test process");
        if !output.status.success() {
            let relayed =
                crate::process::relay_native_fixture_annotation(&output.stdout, &output.stderr)
                    || crate::process::relay_native_panic_location(&output.stderr, case);
            if !relayed {
                crate::process::annotate_native_fixture(&format!(
                    "fixture={case}; stage=isolated_child_exit_without_fixture_annotation; exit={:?}",
                    output.status.code()
                ));
            }
        }
        assert!(
            output.status.success(),
            "native account test failed; exit={:?}",
            output.status.code()
        );
        crate::process::assert_single_test_completed(&output.stdout, &test);
        assert!(output.stderr.is_empty(), "native test stderr is empty");
    }));
    if let Err(error) = checked {
        let _ = temp.keep();
        std::panic::resume_unwind(error);
    }
    drop(temp);
    assert!(
        !temp_path.exists(),
        "successful native fixture is cleaned up"
    );
    false
}

fn native_account_test_case(test: &str) -> &'static str {
    match test {
        "tools::native_exec::shipped_exec_continues_after_guarded_command_and_replays_closed_events" => {
            "shipped_exec"
        }
        "active_terminal::tool_group::read_group_appends_once_and_denied_read_stays_separate" => {
            "tool_group"
        }
        "tools::exact_goal_token_cap_fails_before_a_tool_request_is_journaled" => "goal_token_cap",
        "tools::unfinished_implementation_goal_holds_workspace_lease_across_runs" => {
            "workspace_lease"
        }
        _ => "native_account_test",
    }
}

#[cfg(target_os = "linux")]
pub fn native_account_isolated_test_command(
    root: &std::path::Path,
    test_executable: &std::path::Path,
    guard_executable: &std::path::Path,
) -> Command {
    native_account_isolated_test_command_with_timeout(root, test_executable, guard_executable, 80)
}

#[cfg(target_os = "linux")]
pub fn native_account_isolated_test_command_with_timeout(
    root: &std::path::Path,
    test_executable: &std::path::Path,
    guard_executable: &std::path::Path,
    timeout_seconds: u32,
) -> Command {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    assert!((1..=1200).contains(&timeout_seconds));
    assert!(
        test_executable.is_absolute() && test_executable.is_file(),
        "isolated test executable must be an absolute file"
    );
    assert!(
        guard_executable.is_absolute() && guard_executable.is_file(),
        "isolated Guard executable must be an absolute file"
    );
    let user = nix::unistd::User::from_uid(nix::unistd::geteuid())
        .unwrap()
        .unwrap();
    assert_ne!(
        user.uid.as_raw(),
        0,
        "native fixture requires an ordinary user"
    );
    assert!(root.is_absolute() && root.join("account-home").is_dir());
    let mut passwd = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join("passwd"))
        .expect("exclusive native fixture passwd file");
    writeln!(
        passwd,
        "arany-fixture:x:{}:{}:Arany native fixture:{}:/usr/bin/sh",
        user.uid.as_raw(),
        user.gid.as_raw(),
        root.join("account-home").display()
    )
    .expect("private native fixture passwd entry");
    passwd.sync_all().expect("native fixture passwd sync");
    let mut command = Command::new("/usr/bin/sudo");
    command
        .args([
            "--non-interactive", "--", "/usr/bin/unshare", "--mount", "--net",
            "--propagation", "private", "--", "/usr/bin/sh", "-eu", "-c",
            "root=$1; uid=$2; gid=$3; duration=$4; shift 4; /usr/bin/mount --bind \"$root/passwd\" /etc/passwd; /usr/bin/mount --options remount,bind,ro /etc/passwd; /usr/sbin/ip link set lo up; exec /usr/bin/timeout --kill-after=2s \"$duration\" /usr/bin/setpriv --reuid \"$uid\" --regid \"$gid\" --clear-groups --no-new-privs /usr/bin/env -i HOME=\"$root/home\" XDG_STATE_HOME=\"$root/environment-state\" XDG_DATA_HOME=\"$root/environment-data\" XDG_CONFIG_HOME=\"$root/home/.config\" GH_CONFIG_DIR=\"$root/home/.config/gh\" GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 PATH=/usr/bin:/bin ARANY_TEST_CACHE_CANARY_ROOT=\"$root\" ARANY_ACCOUNT_ISOLATED_TEST=1 \"$@\"",
            "arany-native-account-fixture",
        ])
        .arg(root)
        .arg(user.uid.as_raw().to_string())
        .arg(user.gid.as_raw().to_string())
        .arg(timeout_seconds.to_string())
        .arg(test_executable);
    command
}

#[cfg(target_os = "linux")]
fn account_command(
    root: &std::path::Path,
    isolate_network: bool,
    product: &std::path::Path,
) -> Command {
    use std::os::unix::fs::PermissionsExt;
    let home = root.join("account-home");
    std::fs::create_dir_all(&home).expect("isolated account home");
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let user = nix::unistd::User::from_uid(nix::unistd::geteuid())
        .unwrap()
        .unwrap();
    let mut command = Command::new("/usr/bin/bwrap");
    command
        .args([
            "--unshare-user",
            "--die-with-parent",
            "--tmpfs",
            "/",
            "--ro-bind",
            "/usr",
            "/usr",
            "--symlink",
            "usr/bin",
            "/bin",
            "--symlink",
            "usr/lib",
            "/lib",
            "--ro-bind-try",
            "/lib64",
            "/lib64",
        ])
        .arg("--ro-bind")
        .arg(product)
        .arg("/arany")
        .args(["--ro-bind", "/etc/passwd", "/etc/passwd"])
        .arg("--bind")
        .arg(root)
        .arg(root)
        .arg("--bind")
        .arg(&home)
        .arg(user.dir)
        .arg("--ro-bind")
        .arg(product)
        .arg(product)
        .args(["--proc", "/proc", "--dev-bind", "/dev", "/dev"]);
    if isolate_network {
        command.arg("--unshare-net");
    }
    if std::path::Path::new("/etc/ssl").is_dir() {
        command.args(["--ro-bind", "/etc/ssl", "/etc/ssl"]);
    }
    // /etc/ssl bundle entries may be symlinks into this read-only trust store (Arch-style hosts).
    command.args([
        "--ro-bind-try",
        "/etc/ca-certificates",
        "/etc/ca-certificates",
    ]);
    command.args([
        "--", "/usr/bin/sh", "-c",
        "HOME=${HOME:-\"$1\"}; XDG_STATE_HOME=${XDG_STATE_HOME:-\"$1/fixture-state\"}; XDG_DATA_HOME=${XDG_DATA_HOME:-\"$1/fixture-data\"}; XDG_CONFIG_HOME=\"$HOME/.config\"; GH_CONFIG_DIR=\"$HOME/.config/gh\"; GIT_CONFIG_GLOBAL=/dev/null; GIT_CONFIG_NOSYSTEM=1; unset SSH_AUTH_SOCK; export HOME XDG_STATE_HOME XDG_DATA_HOME XDG_CONFIG_HOME GH_CONFIG_DIR GIT_CONFIG_GLOBAL GIT_CONFIG_NOSYSTEM; shift; exec \"$@\"",
        "arany-account-fixture",
    ]).arg(home);
    command
}

#[cfg(target_os = "linux")]
pub fn remember_workspace_trust(
    account: &std::path::Path,
    workspace: &std::path::Path,
    admitted_path: &std::path::Path,
) {
    use std::{
        io::Write,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    let identity = std::fs::metadata(workspace).expect("fixture directory identity");
    let root = arany::StateRoot::admit(account).expect("private fixture account root");
    let bytes = serde_json::to_vec(&serde_json::json!({
        "version": 2, "workspaces": [{ "path": admitted_path,
            "device": identity.dev(), "inode": identity.ino(), "mode": "auto_edits",
            "runtime_grants": [] }]
    }))
    .unwrap();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.path().join("workspace-permissions.json"))
        .expect("exclusive fixture trust record");
    file.write_all(&bytes).expect("remember fixture consent");
}

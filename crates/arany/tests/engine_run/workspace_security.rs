#[cfg(target_os = "linux")]
use super::process::BoundedOutput;
use super::*;
use std::os::unix::{
    fs::{MetadataExt, symlink},
    net::UnixListener,
};

#[test]
fn explicit_includes_reject_unsafe_components_and_objects_before_disclosure() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        let notes = workspace.join("notes");
        std::fs::create_dir_all(&notes).expect("Workspace include directory");
        std::fs::write(notes.join("facts.txt"), "safe facts").expect("ordinary include");
        let outside = temp.path().join("outside.txt");
        std::fs::write(&outside, "OUTSIDE_WORKSPACE_CANARY").expect("outside content");
        std::fs::hard_link(&outside, notes.join("hardlink.txt"))
            .expect("outside file selected by a hard link");
        assert_eq!(
            std::fs::metadata(notes.join("hardlink.txt"))
                .expect("hard-link metadata")
                .nlink(),
            2,
            "fixture must have an outside alias"
        );
        let outside_dir = temp.path().join("outside-dir");
        std::fs::create_dir(&outside_dir).expect("outside directory");
        std::fs::write(outside_dir.join("secret.txt"), "OUTSIDE_WORKSPACE_CANARY")
            .expect("outside nested content");
        symlink(&outside_dir, workspace.join("linked")).expect("symlinked include parent");
        symlink(&outside, notes.join("linked.txt")).expect("symlinked final include");
        let _socket = UnixListener::bind(notes.join("socket")).expect("special include object");

        let provider = ScriptedProvider::new(vec![]);
        let requests = Arc::clone(&provider.requests);
        let state_path = temp.path().join("state");
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private State"),
            EngineConfig::default(),
        )
        .expect("Engine");

        for (path, reason) in [
            ("", "Workspace input path is invalid"),
            (".", "Workspace input path is invalid"),
            ("..", "path is outside workspace"),
            ("/outside.txt", "path is outside workspace"),
            ("notes//facts.txt", "Workspace input path is invalid"),
            ("notes/./facts.txt", "Workspace input path is invalid"),
            ("notes/../facts.txt", "path is outside workspace"),
            ("notes/facts.txt/", "not a directory"),
            ("linked/secret.txt", "path is a symbolic link"),
            ("notes/linked.txt", "path is a symbolic link"),
            (".env", "protected path"),
            ("notes/.env.local", "protected path"),
            (".git/config", "protected path"),
            ("notes/line\nfeed", "Workspace input path is invalid"),
            ("notes\\facts.txt", "Workspace input path is invalid"),
        ] {
            let mut candidate = request(&workspace, "Question");
            candidate.include_paths = vec![path.into()];
            let outcome = run_to_end(&engine, &provider, candidate).await;
            assert_eq!(
                outcome.expect_err("include rejected").to_string(),
                format!("{path:?}: {reason}"),
            );
        }

        let mut special = request(&workspace, "Question");
        special.include_paths = vec!["notes/socket".into()];
        let outcome = run_to_end(&engine, &provider, special).await;
        assert!(
            matches!(
                &outcome,
                Err(EngineError::InputPath { source, .. }) if matches!(
                    source.as_ref(), EngineError::WorkspaceUnavailable | EngineError::InputNotRegular
                )
            ),
            "socket include: expected rejected special object, observed {:?}",
            outcome.err()
        );

        let mut linked_include = request(&workspace, "Question");
        linked_include.include_paths = vec!["notes/hardlink.txt".into()];
        let outcome = run_to_end(&engine, &provider, linked_include).await;
        assert!(
            matches!(&outcome, Err(EngineError::InputPath { source, .. })
                if matches!(source.as_ref(), EngineError::WorkspaceUnavailable)),
            "hard-linked include: expected WorkspaceUnavailable, observed {:?}",
            outcome.err()
        );

        std::fs::hard_link(&outside, workspace.join("AGENTS.md"))
            .expect("outside file selected as root guidance");
        let outcome = run_to_end(&engine, &provider, request(&workspace, "Question")).await;
        assert!(
            matches!(&outcome, Err(EngineError::WorkspaceUnavailable)),
            "hard-linked root guidance: expected WorkspaceUnavailable, observed {:?}",
            outcome.err()
        );
        engine.close().await.expect("Engine shutdown");
        assert!(requests.lock().expect("Provider observations").is_empty());

        let connection = rusqlite::Connection::open_with_flags(
            state_path.join("events.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .expect("read-only journal");
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .expect("canonical Event count");
        assert_eq!(count, 0, "rejected includes cannot admit a Run");
    });
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "supervised Linux FIFO include helper"]
fn fifo_include_child() {
    let workspace =
        std::path::PathBuf::from(std::env::var_os("ARANY_TEST_WORKSPACE").expect("test Workspace"));
    let state = std::path::PathBuf::from(std::env::var_os("ARANY_TEST_STATE").expect("test State"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("helper runtime");
    runtime.block_on(async {
        let provider = ScriptedProvider::new(vec![]);
        let requests = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state).expect("private State"),
            EngineConfig::default(),
        )
        .expect("helper Engine");
        let mut candidate = request(&workspace, "Question");
        candidate.include_paths = vec!["notes/fifo".into()];
        let outcome = run_to_end(&engine, &provider, candidate).await;
        assert!(
            matches!(&outcome, Err(EngineError::InputPath { source, .. })
                if matches!(source.as_ref(), EngineError::InputNotRegular)),
            "FIFO include: expected InputNotRegular, observed {:?}",
            outcome.err()
        );
        assert!(requests.lock().expect("Provider observations").is_empty());
        engine.close().await.expect("helper Engine shutdown");
    });
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "native Linux nonblocking FIFO include release gate"]
fn fifo_include_rejects_without_waiting_for_a_writer() {
    use rustix::fs::{CWD, Mode, mkfifoat};
    use std::{
        os::unix::fs::FileTypeExt,
        process::{Command, Stdio},
        time::Duration,
    };

    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    let notes = workspace.join("notes");
    std::fs::create_dir_all(&notes).expect("Workspace include directory");
    let fifo = notes.join("fifo");
    mkfifoat(CWD, &fifo, Mode::RUSR | Mode::WUSR).expect("test-owned FIFO");
    assert!(
        std::fs::symlink_metadata(&fifo)
            .expect("FIFO metadata")
            .file_type()
            .is_fifo(),
        "fixture must be a FIFO"
    );
    let state = temp.path().join("state");
    let mut command = Command::new(std::env::current_exe().expect("absolute test helper"));
    command
        .env_clear()
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_STATE", &state)
        .current_dir(temp.path())
        .args([
            "--ignored",
            "--exact",
            "workspace_security::fifo_include_child",
            "--nocapture",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(debug_assertions)]
    command.env(
        "ARANY_TEST_ACCOUNT_ROOT",
        arany::test_support::isolate_account_root(),
    );
    let output = command
        .bounded_output_for(Duration::from_secs(5), 4096)
        .expect("bounded FIFO helper");
    assert!(output.stdout.len() <= 4096 && output.stderr.len() <= 4096);
    assert!(
        output.status.success(),
        "FIFO helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let connection = rusqlite::Connection::open_with_flags(
        state.join("events.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .expect("read-only journal");
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("canonical Event count");
    assert_eq!(count, 0, "FIFO cannot admit a Run");
}

#[tokio::test]
async fn private_spill_paths_do_not_grant_workspace_include_authority() {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    let reference = "a".repeat(64);
    let spill = state_path.join("spill").join(RunId::new().to_string());
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&spill)
        .unwrap();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(spill.join(format!("{reference}.txt")))
        .unwrap();
    file.write_all(b"PRIVATE_SPILL_INCLUDE_CANARY").unwrap();
    let engine = engine_support::open(state, EngineConfig::default()).unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    for (path, reason) in [
        (
            format!("spill:{reference}"),
            "explicit include does not exist",
        ),
        (
            spill
                .join(format!("{reference}.txt"))
                .to_string_lossy()
                .into_owned(),
            "path is outside workspace",
        ),
    ] {
        let mut input = request(&workspace, "Read retained output");
        input.include_paths = vec![path.clone().into()];
        let error = run_to_end(&engine, &provider, input)
            .await
            .expect_err("spill include rejected")
            .to_string();
        assert!(!error.contains("PRIVATE_SPILL_INCLUDE_CANARY"));
        assert_eq!(error, format!("{path:?}: {reason}"));
    }
    assert!(provider.requests.lock().unwrap().is_empty());
    assert!(workspace.read_dir().unwrap().next().is_none());
    engine.close().await.unwrap();
}

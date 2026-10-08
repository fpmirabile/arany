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
        let mut engine = Engine::open(
            StateRoot::admit(&state_path).expect("private State"),
            provider,
        )
        .expect("Engine");

        for path in [
            "",
            ".",
            "..",
            "/outside.txt",
            "notes//facts.txt",
            "notes/./facts.txt",
            "notes/../facts.txt",
            "notes/facts.txt/",
        ] {
            let mut candidate = request(&workspace, "Question");
            candidate.include_paths = vec![path.into()];
            let outcome = engine.run(candidate).await;
            assert!(
                matches!(&outcome, Err(EngineError::InvalidWorkspacePath)),
                "invalid include spelling {path:?}: expected InvalidWorkspacePath, observed {:?}",
                outcome.err()
            );
        }

        for path in ["linked/secret.txt", "notes/linked.txt"] {
            let mut candidate = request(&workspace, "Question");
            candidate.include_paths = vec![path.into()];
            let outcome = engine.run(candidate).await;
            assert!(
                matches!(&outcome, Err(EngineError::WorkspaceUnavailable)),
                "symlinked include {path}: expected WorkspaceUnavailable, observed {:?}",
                outcome.err()
            );
        }

        let mut directory = request(&workspace, "Question");
        directory.include_paths = vec!["notes".into()];
        let outcome = engine.run(directory).await;
        assert!(
            matches!(&outcome, Err(EngineError::InputNotRegular)),
            "directory include: expected InputNotRegular, observed {:?}",
            outcome.err()
        );

        let mut special = request(&workspace, "Question");
        special.include_paths = vec!["notes/socket".into()];
        let outcome = engine.run(special).await;
        assert!(
            matches!(
                &outcome,
                Err(EngineError::WorkspaceUnavailable | EngineError::InputNotRegular)
            ),
            "socket include: expected rejected special object, observed {:?}",
            outcome.err()
        );

        let mut linked_include = request(&workspace, "Question");
        linked_include.include_paths = vec!["notes/hardlink.txt".into()];
        let outcome = engine.run(linked_include).await;
        assert!(
            matches!(&outcome, Err(EngineError::WorkspaceUnavailable)),
            "hard-linked include: expected WorkspaceUnavailable, observed {:?}",
            outcome.err()
        );

        std::fs::hard_link(&outside, workspace.join("AGENTS.md"))
            .expect("outside file selected as root guidance");
        let outcome = engine.run(request(&workspace, "Question")).await;
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
        let mut engine = Engine::open(StateRoot::admit(&state).expect("private State"), provider)
            .expect("helper Engine");
        let mut candidate = request(&workspace, "Question");
        candidate.include_paths = vec!["notes/fifo".into()];
        let outcome = engine.run(candidate).await;
        assert!(
            matches!(&outcome, Err(EngineError::InputNotRegular)),
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

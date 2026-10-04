use crate::loopback::wait_product;
use arany::{Event, SessionId, StateRoot, Store, StoreError};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
use std::{
    path::Path,
    process::{Command, Stdio},
};

#[path = "disk_fault/enospc.rs"]
mod enospc;
#[path = "disk_fault/product.rs"]
mod product;

fn started(title: String) -> Event {
    Event::SessionStarted {
        title,
        workspace_identity: None,
    }
}

fn marker<'a>(output: &'a str, label: &str) -> &'a str {
    output
        .lines()
        .find_map(|line| line.split_once(label).map(|(_, value)| value.trim()))
        .unwrap_or_else(|| panic!("disk-fault helper marker {label} missing"))
}

fn read_only_store(path: &Path) -> Store {
    Store::open_read_only(StateRoot::open_existing(path).expect("existing State"))
        .expect("read-only Store")
}

#[test]
#[ignore = "supervised Linux file-size-limit helper"]
fn disk_fault_child() {
    let path = std::env::var_os("ARANY_TEST_STATE").expect("test State path");
    let limit = std::env::var("ARANY_TEST_LIMIT")
        .expect("test file-size limit")
        .parse::<u64>()
        .expect("numeric file-size limit");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("helper runtime");
    let store = Store::open(StateRoot::open_existing(Path::new(&path)).expect("existing State"))
        .expect("writable Store");
    setrlimit(
        Resource::Fsize,
        Rlimit {
            current: Some(limit),
            maximum: Some(limit),
        },
    )
    .expect("child-only file-size limit");
    assert_eq!(getrlimit(Resource::Fsize).current, Some(limit));

    let mut successful = 0;
    for _ in 0..256 {
        let id = SessionId::new();
        match runtime.block_on(store.append(id, started("x".repeat(128)))) {
            Ok(_) => successful += 1,
            Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(error, _)))
                if matches!(
                    error.code,
                    rusqlite::ErrorCode::DiskFull | rusqlite::ErrorCode::SystemIoFailure
                ) =>
            {
                runtime.block_on(store.close()).expect("Store shutdown");
                println!("FAULT_SESSION:{id}");
                println!("SUCCESS_COUNT:{successful}");
                return;
            }
            Err(error) => panic!("unexpected append failure: {error}"),
        }
    }
    panic!("file-size limit did not interrupt 256 bounded appends");
}

#[test]
#[ignore = "native Linux kernel file-size-limit release gate"]
fn kernel_file_size_limit_preserves_committed_prefix_and_recovers() {
    let temp = tempfile::tempdir().expect("private test root");
    let state = temp.path().join("state");
    let bootstrap_id = SessionId::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("parent runtime");
    let store =
        Store::open(StateRoot::admit(&state).expect("private State")).expect("bootstrap Store");
    runtime
        .block_on(store.append(bootstrap_id, started("bootstrap".into())))
        .expect("bootstrap Event");
    runtime.block_on(store.close()).expect("bootstrap close");
    let database = state.join("events.sqlite3");
    let limit = std::fs::metadata(&database).expect("journal size").len();
    assert!(limit > 0 && limit < 1024 * 1024, "small fixed test journal");

    let mut command = Command::new("/bin/sh");
    command
        .env_clear()
        .env(
            "ARANY_TEST_EXE",
            std::env::current_exe().expect("test binary"),
        )
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_LIMIT", limit.to_string())
        .current_dir(temp.path())
        .args([
            "-c",
            "trap '' XFSZ; exec \"$ARANY_TEST_EXE\" --ignored disk_fault_child --nocapture",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = wait_product(command.spawn().expect("limited helper process"));
    assert!(child.stdout.len() <= 4096 && child.stderr.len() <= 4096);
    assert!(
        child.status.success(),
        "limited helper failed: {}",
        String::from_utf8_lossy(&child.stderr)
    );
    let output = String::from_utf8(child.stdout).expect("helper output UTF-8");
    let failed_id = marker(&output, "FAULT_SESSION:")
        .parse::<SessionId>()
        .expect("failed typed Session ID");
    let successful = marker(&output, "SUCCESS_COUNT:")
        .parse::<i64>()
        .expect("successful Event count");
    assert!(successful > 0 && successful < 256, "committed fault prefix");

    let store = read_only_store(&state);
    let bootstrap = runtime
        .block_on(store.load_view(bootstrap_id))
        .expect("bootstrap replay")
        .expect("bootstrap Session");
    assert!(bootstrap.runs.is_empty());
    assert!(
        runtime
            .block_on(store.load_session(failed_id))
            .expect("failed Session lookup")
            .is_empty()
    );
    runtime.block_on(store.close()).expect("read-only close");

    let connection = rusqlite::Connection::open_with_flags(
        &database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .expect("read-only integrity connection");
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("committed Event count");
    assert_eq!(count, 1 + successful, "no partial failed Event");
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("SQLite integrity check");
    assert_eq!(integrity, "ok");
    drop(connection);

    let recovery_id = SessionId::new();
    let store = Store::open(StateRoot::open_existing(&state).expect("recovery State"))
        .expect("recovery Store");
    runtime
        .block_on(store.append(recovery_id, started("recovered".into())))
        .expect("post-fault append");
    runtime.block_on(store.close()).expect("recovery close");
    let store = read_only_store(&state);
    assert_eq!(
        runtime
            .block_on(store.load_session(recovery_id))
            .expect("recovered Event")
            .len(),
        1
    );
    runtime.block_on(store.close()).expect("final close");
}

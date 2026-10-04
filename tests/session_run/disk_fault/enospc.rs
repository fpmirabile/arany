use super::{read_only_store, started};
use crate::loopback::wait_product;
use arany::{SessionId, StateRoot, Store, StoreError};
use std::{
    fs::{File, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[path = "enospc/product.rs"]
mod product;

fn mount_private(mountpoint: &Path) {
    let mounted = Command::new("/usr/bin/mount")
        .env_clear()
        .args([
            "-t",
            "tmpfs",
            "-o",
            "size=4m,mode=0700,nr_inodes=1024",
            "tmpfs",
        ])
        .arg(mountpoint)
        .status()
        .expect("mount command");
    assert!(mounted.success(), "private tmpfs mount required");
}

fn enter_private_root(mountpoint: &Path) {
    rustix::process::chroot(mountpoint).expect("private mounted root");
    std::env::set_current_dir("/").expect("private root working directory");
}

fn fill_private_root() -> File {
    let mut filler = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open("/filler")
        .expect("private filler");
    let mut full = false;
    for _ in 0..=1024 {
        match filler.write_all(&[0x5a; 4096]) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::StorageFull => {
                full = true;
                break;
            }
            Err(error) => panic!("unexpected filler failure: {error}"),
        }
    }
    assert!(full, "bounded filler must reach kernel ENOSPC");
    assert_eq!(
        rustix::fs::statvfs("/")
            .expect("mounted filesystem capacity")
            .f_bavail,
        0,
        "no free blocks before SQLite append"
    );
    filler
}

#[test]
#[ignore = "supervised Linux mount-namespace ENOSPC helper"]
fn tmpfs_enospc_child() {
    let mountpoint = PathBuf::from(std::env::var_os("ARANY_TEST_MOUNT").expect("private mount"));
    mount_private(&mountpoint);
    enter_private_root(&mountpoint);

    let state = PathBuf::from("/state");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("helper runtime");
    let store =
        Store::open(StateRoot::admit(&state).expect("private State")).expect("writable Store");
    let bootstrap = SessionId::new();
    let accepted = SessionId::new();
    for (id, title) in [(bootstrap, "bootstrap"), (accepted, "accepted")] {
        runtime
            .block_on(store.append(id, started(title.into())))
            .expect("committed prefix");
    }

    let filler_path = PathBuf::from("/filler");
    let filler = fill_private_root();

    let failed = SessionId::new();
    match runtime.block_on(store.append(failed, started("x".repeat(128)))) {
        Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(error, _)))
            if error.code == rusqlite::ErrorCode::DiskFull => {}
        other => panic!("expected SQLite storage failure, got {other:?}"),
    }
    runtime.block_on(store.close()).expect("Store shutdown");
    drop(filler);
    std::fs::remove_file(&filler_path).expect("restore private capacity");

    let store = read_only_store(&state);
    for id in [bootstrap, accepted] {
        assert_eq!(
            runtime
                .block_on(store.load_session(id))
                .expect("prefix replay")
                .len(),
            1
        );
    }
    assert!(
        runtime
            .block_on(store.load_session(failed))
            .expect("failed Event lookup")
            .is_empty()
    );
    runtime.block_on(store.close()).expect("read-only close");

    let connection = rusqlite::Connection::open_with_flags(
        state.join("events.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .expect("read-only integrity connection");
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("committed Event count");
    assert_eq!(count, 2, "failed Event never committed");
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("SQLite integrity check");
    assert_eq!(integrity, "ok");
    drop(connection);

    let recovered = SessionId::new();
    let store = Store::open(StateRoot::open_existing(&state).expect("recovery State"))
        .expect("recovery Store");
    runtime
        .block_on(store.append(recovered, started("recovered".into())))
        .expect("recovery append");
    runtime.block_on(store.close()).expect("recovery close");
    let store = read_only_store(&state);
    assert_eq!(
        runtime
            .block_on(store.load_session(recovered))
            .expect("recovery replay")
            .len(),
        1
    );
    runtime.block_on(store.close()).expect("final close");
    println!("KERNEL_ENOSPC_REPLAY_OK");
}

#[test]
#[ignore = "native Linux private tmpfs ENOSPC release gate; requires unshare and mount"]
fn kernel_enospc_preserves_committed_prefix_and_recovers() {
    let temp = tempfile::tempdir().expect("private test root");
    let mountpoint = temp.path().join("volume");
    std::fs::create_dir(&mountpoint).expect("private mountpoint");
    let mut command = Command::new("/usr/bin/unshare");
    command
        .env_clear()
        .env("ARANY_TEST_MOUNT", &mountpoint)
        .args(["-Urm", "--"])
        .arg(std::env::current_exe().expect("test binary"))
        .args([
            "--ignored",
            "--exact",
            "disk_fault::enospc::tmpfs_enospc_child",
            "--nocapture",
        ])
        .current_dir(temp.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = wait_product(command.spawn().expect("private namespace helper"));
    assert!(output.stdout.len() <= 4096 && output.stderr.len() <= 4096);
    assert!(
        output.status.success(),
        "private ENOSPC helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output
            .stdout
            .windows(b"KERNEL_ENOSPC_REPLAY_OK".len())
            .any(|window| window == b"KERNEL_ENOSPC_REPLAY_OK")
    );
    assert_eq!(
        std::fs::read_dir(&mountpoint)
            .expect("host mountpoint")
            .count(),
        0,
        "private mount did not leak into the parent namespace"
    );
}

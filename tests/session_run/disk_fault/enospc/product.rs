use super::{enter_private_root, fill_private_root, mount_private};
use crate::loopback::{check_profile, read_request, send_response, wait_product, write_profile};
use arany::{Event, StateRoot, Store, create_session, rename_session};
use std::{
    fs::OpenOptions,
    io::ErrorKind,
    net::TcpListener,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn bind_read_only(source: &Path, target: &Path) {
    let mounted = Command::new("/usr/bin/mount")
        .env_clear()
        .arg("--bind")
        .arg(source)
        .arg(target)
        .status()
        .expect("private bind mount");
    assert!(mounted.success(), "private bind mount required");
    let remounted = Command::new("/usr/bin/mount")
        .env_clear()
        .args(["-o", "remount,bind,ro"])
        .arg(target)
        .status()
        .expect("read-only bind remount");
    assert!(remounted.success(), "read-only bind mount required");
}

fn mount_product_runtime(mountpoint: &Path) {
    let device_dir = mountpoint.join("dev");
    std::fs::create_dir(&device_dir).expect("private device directory");
    let null_target = device_dir.join("null");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&null_target)
        .expect("private null mountpoint");
    let null_mount = Command::new("/usr/bin/mount")
        .env_clear()
        .arg("--bind")
        .arg("/dev/null")
        .arg(&null_target)
        .status()
        .expect("private null bind");
    assert!(null_mount.success(), "private null device required");

    let bundle_target = mountpoint.join("etc/ssl/certs/ca-certificates.crt");
    std::fs::create_dir_all(bundle_target.parent().expect("certificate parent"))
        .expect("private certificate directory");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&bundle_target)
        .expect("private certificate mountpoint");
    bind_read_only(
        Path::new("/etc/ssl/certs/ca-certificates.crt"),
        &bundle_target,
    );
    symlink(
        "certs/ca-certificates.crt",
        mountpoint.join("etc/ssl/cert.pem"),
    )
    .expect("private certificate alias");

    let library_target = mountpoint.join("usr/lib");
    std::fs::create_dir_all(&library_target).expect("private library mountpoint");
    bind_read_only(Path::new("/usr/lib"), &library_target);
    symlink("usr/lib", mountpoint.join("lib64")).expect("private loader alias");

    let binary = Path::new(env!("CARGO_BIN_EXE_arany"));
    let binary_target = mountpoint.join(binary.strip_prefix("/").expect("absolute Cargo binary"));
    std::fs::create_dir_all(binary_target.parent().expect("binary parent"))
        .expect("private binary directory");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&binary_target)
        .expect("private binary mountpoint");
    bind_read_only(binary, &binary_target);
    assert!(
        matches!(
            OpenOptions::new().write(true).open(&binary_target),
            Err(error) if error.kind() == ErrorKind::ReadOnlyFilesystem
        ),
        "product bind must be read-only"
    );
}

#[test]
#[ignore = "supervised Linux shipped-exec ENOSPC helper"]
fn tmpfs_enospc_exec_child() {
    let mountpoint = PathBuf::from(std::env::var_os("ARANY_TEST_MOUNT").expect("private mount"));
    mount_private(&mountpoint);
    mount_product_runtime(&mountpoint);
    enter_private_root(&mountpoint);
    assert!(
        Path::new(env!("CARGO_BIN_EXE_arany")).is_file(),
        "mounted product binary missing after chroot"
    );
    assert!(
        Path::new("/lib64/ld-linux-x86-64.so.2").is_file(),
        "mounted product interpreter missing after chroot"
    );

    let workspace = PathBuf::from("/workspace");
    std::fs::create_dir(&workspace).expect("empty private Workspace");
    let state = PathBuf::from("/state");
    let listener = TcpListener::bind("127.0.0.1:0").expect("test-only loopback endpoint");
    write_profile(
        &state,
        listener.local_addr().expect("listener address").port(),
    );
    let conformance = thread::spawn(move || {
        listener.set_nonblocking(true).expect("bounded accept");
        for index in 0..3 {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "synthetic probe {index} missing");
                        thread::yield_now();
                    }
                    Err(error) => panic!("synthetic accept failed: {error}"),
                }
            };
            let body = read_request(&mut stream);
            let body: serde_json::Value =
                serde_json::from_slice(&body).expect("synthetic request JSON");
            let input: serde_json::Value =
                serde_json::from_str(body["input"].as_str().expect("semantic input"))
                    .expect("semantic JSON");
            assert!(
                input
                    .get("workspace_guidance")
                    .is_none_or(serde_json::Value::is_null)
            );
            let response = match index {
                0 => serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"ok"}}),
                1 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":["one read-only question"]}})
                }
                _ => serde_json::json!({"summary":"synthetic summary"}),
            };
            send_response(&mut stream, index, response);
        }
        listener
    });
    check_profile(&workspace, &state);
    let listener = conformance.join().expect("three data-free probes");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("helper runtime");
    let session_id = runtime
        .block_on(create_session(
            StateRoot::open_existing(&state).expect("admitted State"),
            workspace.clone(),
            Some("Fault probe".into()),
        ))
        .expect("committed Session");

    let filler = fill_private_root();
    let full_store = Store::open(StateRoot::open_existing(&state).expect("full-capacity State"))
        .expect("full-capacity Store open");
    runtime
        .block_on(full_store.close())
        .expect("full-capacity Store close");
    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .env_clear()
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .current_dir(&workspace)
        .args(["exec", "--state-dir", "/state", "--workspace", "/workspace"])
        .args([
            "--provider",
            "custom:local",
            "--model",
            "model-1",
            "--collaboration",
            "single",
            "--session-id",
        ])
        .arg(session_id.to_string())
        .arg("x".repeat(8 * 1024))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = wait_product(command.spawn().expect("exhausted product process"));
    assert_eq!(output.status.code(), Some(1), "ENOSPC product exit class");
    assert!(output.stdout.len() <= 4096 && output.stderr.len() <= 4096);
    assert_eq!(output.stdout, b"");
    assert_eq!(output.stderr, b"error: state storage failed\n");
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == ErrorKind::WouldBlock),
        "runtime Provider request must not leave the exhausted process"
    );
    drop(listener);

    drop(filler);
    std::fs::remove_file("/filler").expect("restore private capacity");
    let store = Store::open_read_only(StateRoot::open_existing(&state).expect("existing State"))
        .expect("read-only Store");
    let events = runtime
        .block_on(store.load_session(session_id))
        .expect("strict Event replay");
    assert_eq!(
        events.len(),
        1,
        "failed objective must not become canonical"
    );
    assert!(matches!(events[0].event, Event::SessionStarted { .. }));
    let view = runtime
        .block_on(store.load_view(session_id))
        .expect("Session reduction")
        .expect("committed Session");
    assert!(view.runs.is_empty(), "no fabricated terminal Run");
    runtime.block_on(store.close()).expect("read-only close");

    let connection = rusqlite::Connection::open_with_flags(
        state.join("events.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .expect("read-only integrity connection");
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("committed Event count");
    assert_eq!(count, 1);
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("SQLite integrity check");
    assert_eq!(integrity, "ok");
    drop(connection);

    runtime
        .block_on(rename_session(
            StateRoot::open_existing(&state).expect("recovery State"),
            workspace,
            session_id,
            "Recovered".into(),
        ))
        .expect("post-fault append");
    println!("KERNEL_ENOSPC_EXEC_OK");
}

#[test]
#[ignore = "native Linux private-tmpfs shipped-exec ENOSPC release gate"]
fn kernel_enospc_exec_reports_store_failure_without_egress() {
    let temp = tempfile::tempdir().expect("private test root");
    let mountpoint = temp.path().join("volume");
    std::fs::create_dir(&mountpoint).expect("private mountpoint");
    let mut command = Command::new("/usr/bin/unshare");
    command
        .env_clear()
        .env("ARANY_TEST_MOUNT", &mountpoint)
        .args(["-Urmpf", "--kill-child", "--mount-proc", "--"])
        .arg(std::env::current_exe().expect("test binary"))
        .args([
            "--ignored",
            "--exact",
            "disk_fault::enospc::product::tmpfs_enospc_exec_child",
            "--nocapture",
        ])
        .current_dir(temp.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = wait_product(command.spawn().expect("private product namespace"));
    assert!(output.stdout.len() <= 4096 && output.stderr.len() <= 4096);
    assert!(
        output.status.success(),
        "private ENOSPC product helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output
            .stdout
            .windows(b"KERNEL_ENOSPC_EXEC_OK".len())
            .any(|window| window == b"KERNEL_ENOSPC_EXEC_OK")
    );
    assert_eq!(
        std::fs::read_dir(&mountpoint)
            .expect("host mountpoint")
            .count(),
        0,
        "private mount did not leak into the parent namespace"
    );
}

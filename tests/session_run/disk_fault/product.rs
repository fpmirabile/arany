use crate::loopback::{check_profile, read_request, send_response, wait_product, write_profile};
use arany::{Event, StateRoot, Store, create_session, rename_session};
use std::{
    net::TcpListener,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
#[ignore = "native Linux shipped exec disk-fault release gate"]
fn limited_exec_reports_store_failure_without_an_answer() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("empty Workspace");
    let state = temp.path().join("state");
    let listener = TcpListener::bind("127.0.0.1:0").expect("test-only loopback endpoint");
    write_profile(
        &state,
        listener.local_addr().expect("listener address").port(),
    );

    let conformance = thread::spawn(move || {
        listener.set_nonblocking(true).expect("bounded accept");
        for index in 0..3 {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
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
        .expect("parent runtime");
    let session_id = runtime
        .block_on(create_session(
            StateRoot::open_existing(&state).expect("admitted State"),
            workspace.clone(),
            Some("Fault probe".into()),
        ))
        .expect("committed Session");
    let database = state.join("events.sqlite3");
    let limit = std::fs::metadata(&database).expect("journal size").len();
    assert!(limit > 0 && limit < 1024 * 1024, "small fixed journal");

    let mut command = Command::new("/bin/sh");
    command
        .env_clear()
        .env("ARANY_TEST_BINARY", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_SESSION", session_id.to_string())
        .env("ARANY_TEST_OBJECTIVE", "x".repeat(8 * 1024))
        .env("ARANY_TEST_LIMIT", limit.to_string())
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .current_dir(&workspace)
        .args([
            "-c",
            "trap '' XFSZ; exec /usr/bin/prlimit --fsize=\"$ARANY_TEST_LIMIT\" -- \"$ARANY_TEST_BINARY\" exec --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider custom:local --model model-1 --collaboration single --session-id \"$ARANY_TEST_SESSION\" \"$ARANY_TEST_OBJECTIVE\"",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = wait_product(command.spawn().expect("limited product process"));
    assert_eq!(output.status.code(), Some(1), "limited exec exit class");
    assert!(output.stdout.len() <= 4096 && output.stderr.len() <= 4096);
    assert_eq!(output.stdout, b"");
    assert_eq!(output.stderr, b"error: state storage failed\n");
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "runtime Provider request must not leave the limited process"
    );
    drop(listener);

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
        &database,
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
        .expect("unrestricted post-fault append");
}

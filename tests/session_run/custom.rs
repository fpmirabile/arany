use super::*;

#[test]
fn custom_profile_process_gate_proves_conformance_and_durable_runs() {
    use std::{
        io::{Read, Write},
        net::{SocketAddr, TcpListener},
        os::unix::fs::PermissionsExt,
        path::Path,
        process::{Output, Stdio},
        str::FromStr,
        thread,
        time::{Duration, Instant},
    };

    fn run_product(command: &mut Command) -> Output {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        super::loopback::wait_product(command.spawn().expect("product process"))
    }

    fn run_check(cwd: &Path, state: &Path, key: Option<&str>, proxy: Option<SocketAddr>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .current_dir(cwd)
            .args(["provider", "check", "local", "--state-dir"])
            .arg(state);
        if let Some(key) = key {
            command.env("ARANY_PROVIDER_LOCAL_KEY", key);
        }
        if let Some(proxy) = proxy {
            let proxy = format!("http://{proxy}");
            command
                .env("HTTP_PROXY", &proxy)
                .env("http_proxy", &proxy)
                .env("ALL_PROXY", &proxy)
                .env("NO_PROXY", "");
        }
        run_product(&mut command)
    }

    fn run_exec(cwd: &Path, state: &Path, key: Option<&str>, model: &str) -> Output {
        run_exec_policy(cwd, state, key, model, "single", None)
    }

    fn run_exec_policy(
        cwd: &Path,
        state: &Path,
        key: Option<&str>,
        model: &str,
        collaboration: &str,
        session_id: Option<SessionId>,
    ) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .current_dir(cwd)
            .args(["exec", "--state-dir"])
            .arg(state)
            .arg("--workspace")
            .arg(cwd)
            .args([
                "--provider",
                "custom:local",
                "--model",
                model,
                "--collaboration",
                collaboration,
                "answer",
            ]);
        if let Some(key) = key {
            command.env("ARANY_PROVIDER_LOCAL_KEY", key);
        }
        if let Some(session_id) = session_id {
            command.arg("--session-id").arg(session_id.to_string());
        }
        run_product(&mut command)
    }

    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    std::fs::write(workspace.join("private.txt"), b"OMITTED_WORKSPACE_CANARY")
        .expect("Workspace canary");
    std::fs::write(workspace.join("AGENTS.md"), b"EXPECTED_WORKSPACE_GUIDANCE")
        .expect("Workspace guidance");
    let state_path = temp.path().join("state");
    StateRoot::admit(&state_path).expect("state root");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback endpoint");
    let port = listener.local_addr().expect("listener address").port();
    let hostile_proxy = TcpListener::bind("127.0.0.1:0").expect("hostile proxy");
    hostile_proxy
        .set_nonblocking(true)
        .expect("nonblocking proxy observation");
    let proxy_addr = hostile_proxy.local_addr().expect("proxy address");
    let profile = serde_json::json!({
        "version": 1,
        "profiles": [{
            "name": "local",
            "protocol": "openai-responses",
            "endpoint": format!("http://127.0.0.1:{port}/v1/responses"),
            "model": "model-1",
            "credential_env": "ARANY_PROVIDER_LOCAL_KEY",
            "outcome_encoding": "json_schema",
            "privacy": "user_authorized",
            "max_output_tokens": 4096,
            "capability_evidence_version": 1
        }]
    });
    let profile_path = state_path.join("provider-profiles.json");
    std::fs::write(&profile_path, serde_json::to_vec(&profile).unwrap()).expect("profile file");
    std::fs::set_permissions(&profile_path, std::fs::Permissions::from_mode(0o600))
        .expect("private profile file");

    let server = thread::spawn(move || {
        listener.set_nonblocking(true).expect("bounded accept");
        for index in 0..9 {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "synthetic request {index} missing"
                        );
                        thread::yield_now();
                    }
                    Err(error) => panic!("accept failed: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("request deadline");
            let mut request = Vec::new();
            let (header_end, body_len) = loop {
                let mut chunk = [0; 4096];
                let count = stream.read(&mut chunk).expect("request bytes");
                assert!(count > 0 && request.len() + count <= 64 * 1024);
                request.extend_from_slice(&chunk[..count]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let header_end = end + 4;
                    let header = std::str::from_utf8(&request[..header_end]).expect("HTTP header");
                    let body_len = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .expect("Content-Length");
                    if request.len() >= header_end + body_len {
                        break (header_end, body_len);
                    }
                }
            };
            let header = std::str::from_utf8(&request[..header_end]).expect("HTTP header");
            assert!(header.starts_with("POST /v1/responses HTTP/1.1\r\n"));
            assert!(
                header
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-key")
            );
            let body: serde_json::Value =
                serde_json::from_slice(&request[header_end..header_end + body_len])
                    .expect("synthetic JSON");
            assert_eq!(body["model"], "model-1");
            assert_eq!(body["text"]["format"]["strict"], true);
            assert_eq!(
                body["max_output_tokens"],
                if index == 0 || index >= 3 { 4096 } else { 128 }
            );
            let input: serde_json::Value =
                serde_json::from_str(body["input"].as_str().expect("input string"))
                    .expect("semantic input");
            if (3..8).contains(&index) {
                assert_eq!(
                    input["phase"],
                    match index {
                        5 => "child_work",
                        6 => "root_synthesis",
                        _ => "root_plan",
                    }
                );
                assert_eq!(input["workspace_guidance"], "EXPECTED_WORKSPACE_GUIDANCE");
                if matches!(index, 3 | 4 | 6) {
                    assert_eq!(input["objective"], "answer");
                }
                if index == 7 {
                    assert_eq!(
                        input["history"],
                        serde_json::json!([{"user": "answer", "assistant": "verified answer"}])
                    );
                }
            } else {
                assert!(
                    input
                        .get("workspace_guidance")
                        .is_none_or(|value| value.is_null())
                );
            }
            assert!(
                !request
                    .windows(b"OMITTED_WORKSPACE_CANARY".len())
                    .any(|bytes| { bytes == b"OMITTED_WORKSPACE_CANARY" })
            );
            if index == 8 {
                write!(
                    stream,
                    "HTTP/1.1 302 Found\r\nLocation: http://{proxy_addr}/v1/responses\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .expect("synthetic redirect");
                continue;
            }
            let text = match index {
                0 => serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"ok"}}),
                1 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":["one read-only question"]}})
                }
                2 => serde_json::json!({"summary":"synthetic summary"}),
                3 => {
                    serde_json::json!({"outcome":{"type":"finish","summary":"verified summary","result":"verified answer"}})
                }
                4 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":["one read-only question"]}})
                }
                5 => {
                    serde_json::json!({"outcome":{"type":"finish","summary":"child summary","result":"child result"}})
                }
                6 => {
                    serde_json::json!({"outcome":{"type":"finish","summary":"team summary","result":"team answer"}})
                }
                7 => {
                    serde_json::json!({"outcome":{"type":"finish","summary":"unsafe summary","result":"test-key"}})
                }
                _ => unreachable!(),
            };
            let wire_text = if index == 7 {
                text.to_string().replace("test-key", "\\u0074est-key")
            } else {
                text.to_string()
            };
            let response = serde_json::json!({
                "id": format!("resp_{}", index + 1),
                "status": "completed",
                "model": "model-1",
                "output": [{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":wire_text}]}],
                "usage": {"input_tokens":20,"output_tokens":10}
            })
            .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            )
            .expect("synthetic response");
        }
    });

    let unverified = run_exec(&workspace, &state_path, Some("test-key"), "model-1");
    assert!(!unverified.status.success());
    assert_eq!(unverified.stdout, b"");
    assert_eq!(
        unverified.stderr,
        b"error: custom Provider evidence unavailable\n"
    );
    let checked = run_check(&workspace, &state_path, Some("test-key"), Some(proxy_addr));
    assert!(checked.status.success());
    assert_eq!(
        checked.stdout,
        b"Provider: local\nStatus: custom verified\n"
    );
    assert_eq!(checked.stderr, b"");
    assert!(matches!(
        hostile_proxy.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));

    let mut changed_profile = profile.clone();
    changed_profile["profiles"][0]["endpoint"] =
        format!("http://127.0.0.1:{port}/other/responses").into();
    std::fs::write(&profile_path, serde_json::to_vec(&changed_profile).unwrap())
        .expect("changed profile");
    let changed = run_exec(&workspace, &state_path, Some("test-key"), "model-1");
    assert!(!changed.status.success());
    assert_eq!(changed.stdout, b"");
    assert_eq!(
        changed.stderr,
        b"error: custom Provider evidence unavailable\n"
    );
    std::fs::write(&profile_path, serde_json::to_vec(&profile).unwrap()).expect("restore profile");

    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
        .expect("inspect evidence tuple");
    let (addresses, expiry): (String, i64) = connection
        .query_row(
            "SELECT addresses, expires_at_ms FROM provider_evidence WHERE profile_name='local'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("current evidence tuple");
    connection
        .execute(
            "UPDATE provider_evidence SET addresses=?1 WHERE profile_name='local'",
            [format!("[\"127.0.0.2:{port}\"]")],
        )
        .expect("changed address evidence");
    drop(connection);
    let changed_address = run_exec(&workspace, &state_path, Some("test-key"), "model-1");
    assert!(!changed_address.status.success());
    assert_eq!(changed_address.stdout, b"");
    assert_eq!(
        changed_address.stderr,
        b"error: custom Provider evidence unavailable\n"
    );
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
        .expect("restore evidence tuple");
    connection
        .execute(
            "UPDATE provider_evidence SET addresses=?1, expires_at_ms=1 WHERE profile_name='local'",
            [addresses],
        )
        .expect("expired evidence");
    drop(connection);
    let expired = run_exec(&workspace, &state_path, Some("test-key"), "model-1");
    assert!(!expired.status.success());
    assert_eq!(expired.stdout, b"");
    assert_eq!(
        expired.stderr,
        b"error: custom Provider evidence unavailable\n"
    );
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
        .expect("restore evidence expiry");
    connection
        .execute(
            "UPDATE provider_evidence SET expires_at_ms=?1 WHERE profile_name='local'",
            [expiry],
        )
        .expect("restore expiry");
    let event_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("pre-Run Event count");
    assert_eq!(event_count, 0);
    drop(connection);

    let wrong_model = run_exec(&workspace, &state_path, Some("test-key"), "model-2");
    assert!(!wrong_model.status.success());
    assert_eq!(wrong_model.stdout, b"");
    assert_eq!(
        wrong_model.stderr,
        b"error: selected custom Provider model does not match its profile\n"
    );
    let no_key = run_exec(&workspace, &state_path, None, "model-1");
    assert!(!no_key.status.success());
    assert_eq!(no_key.stdout, b"");
    assert_eq!(
        no_key.stderr,
        b"error: custom Provider credential unavailable\n"
    );

    let success = run_exec(&workspace, &state_path, Some("test-key"), "model-1");
    assert!(success.status.success());
    let continued_session_id = SessionId::from_str(
        std::str::from_utf8(&success.stderr)
            .expect("first receipt UTF-8")
            .lines()
            .next()
            .expect("first Session receipt")
            .strip_prefix("Session: ")
            .expect("first Session ID"),
    )
    .expect("first Session ID format");
    let team = run_exec_policy(
        &workspace,
        &state_path,
        Some("test-key"),
        "model-1",
        "team",
        None,
    );
    let reflected = run_exec_policy(
        &workspace,
        &state_path,
        Some("test-key"),
        "model-1",
        "single",
        Some(continued_session_id),
    );
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
        .expect("inspect pre-recheck Events");
    let events_before_recheck: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("pre-recheck Event count");
    let evidence_before_recheck: i64 = connection
        .query_row("SELECT COUNT(*) FROM provider_evidence", [], |row| {
            row.get(0)
        })
        .expect("pre-recheck evidence count");
    assert_eq!(evidence_before_recheck, 1);
    drop(connection);
    let redirected = run_check(&workspace, &state_path, Some("test-key"), Some(proxy_addr));
    server.join().expect("synthetic server completed");
    assert!(!redirected.status.success());
    assert_eq!(redirected.stdout, b"");
    assert_eq!(
        redirected.stderr,
        b"error: custom Provider conformance failed\n"
    );
    assert!(matches!(
        hostile_proxy.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    let denied_after_recheck = run_exec(&workspace, &state_path, Some("test-key"), "model-1");
    assert!(!denied_after_recheck.status.success());
    assert_eq!(denied_after_recheck.stdout, b"");
    assert_eq!(
        denied_after_recheck.stderr,
        b"error: custom Provider evidence unavailable\n"
    );
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
        .expect("inspect post-recheck Events");
    let events_after_recheck: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("post-recheck Event count");
    assert_eq!(events_after_recheck, events_before_recheck);
    drop(connection);
    assert!(success.status.success());
    assert_eq!(success.stdout, b"Answer:\n  verified answer\n");
    let receipt = std::str::from_utf8(&success.stderr).expect("receipt UTF-8");
    let receipt: Vec<_> = receipt.lines().collect();
    assert_eq!(receipt.len(), 4);
    let session_id =
        SessionId::from_str(receipt[0].strip_prefix("Session: ").unwrap()).expect("Session ID");
    let run_id = RunId::from_str(receipt[1].strip_prefix("Run: ").unwrap()).expect("Run ID");
    assert_eq!(receipt[2], "Status: finished");
    assert_eq!(receipt[3], "Provider: custom verified");
    assert_eq!(
        success.stderr,
        format!(
            "Session: {session_id}\nRun: {run_id}\nStatus: finished\nProvider: custom verified\n"
        )
        .as_bytes()
    );
    assert!(team.status.success());
    assert_eq!(team.stdout, b"Answer:\n  team answer\n");
    let team_receipt = std::str::from_utf8(&team.stderr).expect("team receipt UTF-8");
    let team_receipt: Vec<_> = team_receipt.lines().collect();
    assert_eq!(team_receipt.len(), 4);
    let team_session_id = SessionId::from_str(team_receipt[0].strip_prefix("Session: ").unwrap())
        .expect("team Session ID");
    assert_eq!(team_receipt[2], "Status: finished");
    assert_eq!(team_receipt[3], "Provider: custom verified");
    assert!(!reflected.status.success());
    assert_eq!(reflected.stdout, b"");
    let reflected_receipt = std::str::from_utf8(&reflected.stderr).expect("failure receipt UTF-8");
    let reflected_receipt: Vec<_> = reflected_receipt.lines().collect();
    assert_eq!(reflected_receipt.len(), 4);
    let reflected_session_id =
        SessionId::from_str(reflected_receipt[0].strip_prefix("Session: ").unwrap())
            .expect("failed Session ID");
    let reflected_run_id = RunId::from_str(reflected_receipt[1].strip_prefix("Run: ").unwrap())
        .expect("failed Run ID");
    assert_eq!(reflected_session_id, session_id);
    assert_eq!(
        reflected.stderr,
        format!(
            "Session: {reflected_session_id}\nRun: {reflected_run_id}\nStatus: failed\nProvider: custom verified\n"
        )
        .as_bytes()
    );
    let diagnostic_path = state_path.join("development.log");
    if cfg!(debug_assertions) {
        let log = std::fs::read_to_string(&diagnostic_path).expect("automatic development log");
        assert!(log.contains("runtime stage=DiagnosticsEnabled"));
        assert!(log.contains("provider phase=RootPlan disposition=InvalidResponse"));
        assert!(log.contains("run disposition=Failed"));
        for forbidden in [
            "test-key",
            "OMITTED_WORKSPACE_CANARY",
            "EXPECTED_WORKSPACE_GUIDANCE",
            "verified answer",
            "team answer",
            "model-1",
        ] {
            assert!(
                !log.contains(forbidden),
                "sensitive data entered development log"
            );
        }
    } else {
        assert!(
            !diagnostic_path.exists(),
            "optimized process must not create debug log"
        );
    }
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state reopen"))
        .expect("read-only Store");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let events = runtime
        .block_on(store.load_session(session_id))
        .expect("canonical Events");
    let view = SessionView::replay(session_id, &events)
        .expect("replay")
        .expect("Session");
    assert_eq!(view.runs.len(), 2);
    assert_eq!(view.runs[0].id, run_id);
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(
        view.runs[0].agents[0].provider_calls[0].wire_provenance,
        Some(arany::ProviderWireProvenance::ResponsesCompletedStoreFalseRequested)
    );
    let config = view.runs[0].config.as_ref().expect("Run config");
    assert_eq!(config.provider, "custom:local");
    assert_eq!(config.model, "model-1");
    assert_eq!(config.provider_concurrency, 1);
    let provenance = config
        .custom_profile_provenance
        .as_ref()
        .expect("verified custom provenance");
    assert_eq!(
        provenance.endpoint,
        format!("http://127.0.0.1:{port}/v1/responses")
    );
    assert_eq!(provenance.capability_evidence_version, 1);
    let team_events = runtime
        .block_on(store.load_session(team_session_id))
        .expect("team Events");
    let team_view = SessionView::replay(team_session_id, &team_events)
        .expect("team replay")
        .expect("team Session");
    assert_eq!(team_view.runs.len(), 1);
    assert_eq!(team_view.runs[0].status, RunStatus::Finished);
    assert_eq!(team_view.runs[0].agents.len(), 2);
    assert_eq!(
        team_view.runs[0]
            .agents
            .iter()
            .map(|agent| agent.provider_calls.len())
            .sum::<usize>(),
        3
    );
    assert!(
        team_view.runs[0]
            .agents
            .iter()
            .flat_map(|agent| &agent.provider_calls)
            .all(|call| call.wire_provenance
                == Some(arany::ProviderWireProvenance::ResponsesCompletedStoreFalseRequested))
    );
    assert_eq!(
        team_view.runs[0]
            .config
            .as_ref()
            .unwrap()
            .provider_concurrency,
        1
    );
    let reflected_events = runtime
        .block_on(store.load_session(reflected_session_id))
        .expect("failed Run Events");
    let reflected_view = SessionView::replay(reflected_session_id, &reflected_events)
        .expect("failed Run replay")
        .expect("failed Session");
    assert_eq!(reflected_view.runs[1].id, reflected_run_id);
    assert_eq!(reflected_view.runs[1].status, RunStatus::Failed);
    assert!(reflected_view.runs[1].assistant_message.is_none());
    assert_eq!(
        reflected_view.runs[1].agents[0].provider_calls[0].wire_provenance,
        None
    );
    runtime
        .block_on(store.close())
        .expect("close read-only Store");
    let journal_before = std::fs::read(state_path.join("events.sqlite3"))
        .expect("journal before read-only inspection");
    for (run_id, events, view) in [
        (run_id, &events, &view),
        (team_view.runs[0].id, &team_events, &team_view),
        (reflected_run_id, &reflected_events, &reflected_view),
    ] {
        let selected: Vec<_> = events
            .iter()
            .filter(|event| event.run_id == Some(run_id))
            .cloned()
            .collect();
        assert!(!selected.is_empty());
        for (format, output) in [
            ("text", arany::Output::Text),
            ("jsonl", arany::Output::Jsonl),
        ] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
            command
                .env_clear()
                .current_dir(&workspace)
                .args(["show", "--state-dir"])
                .arg(&state_path)
                .args(["--output", format, &run_id.to_string()]);
            let shown = run_product(&mut command);
            assert!(shown.status.success(), "show rejected valid Run {run_id}");
            assert_eq!(shown.stderr, b"");
            assert_eq!(
                shown.stdout,
                arany::render_session(view, &selected, output).as_bytes(),
                "show selected different committed facts for Run {run_id}"
            );
        }
    }
    for (selector, error) in [
        (
            RunId::new().to_string(),
            b"error: Session or Run not found\n".as_slice(),
        ),
        (
            "not-an-id".to_owned(),
            b"error: invalid Session or Run ID\n".as_slice(),
        ),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .current_dir(&workspace)
            .args(["show", "--state-dir"])
            .arg(&state_path)
            .args(["--output", "jsonl", &selector]);
        let rejected = run_product(&mut command);
        assert!(!rejected.status.success());
        assert_eq!(rejected.stdout, b"");
        assert_eq!(rejected.stderr, error);
    }
    assert!(
        std::fs::read(state_path.join("events.sqlite3")).expect("journal after inspection")
            == journal_before,
        "show changed the canonical journal"
    );
    let connection =
        rusqlite::Connection::open(state_path.join("events.sqlite3")).expect("inspect evidence");
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM provider_evidence WHERE profile_name='local'",
            [],
            |row| row.get(0),
        )
        .expect("persisted evidence");
    assert_eq!(count, 0);
    drop(connection);
    let database_bytes = std::fs::read(state_path.join("events.sqlite3")).expect("database bytes");
    for artifact in [
        success.stdout.as_slice(),
        success.stderr.as_slice(),
        team.stdout.as_slice(),
        team.stderr.as_slice(),
        reflected.stdout.as_slice(),
        reflected.stderr.as_slice(),
        database_bytes.as_slice(),
    ] {
        assert!(
            !artifact
                .windows(b"test-key".len())
                .any(|part| part == b"test-key")
        );
    }

    let unavailable = run_check(&workspace, &state_path, None, None);
    assert!(!unavailable.status.success());
    assert_eq!(unavailable.stdout, b"");
    assert_eq!(
        unavailable.stderr,
        b"error: custom Provider credential unavailable\n"
    );
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
        .expect("inspect invalidated evidence");
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM provider_evidence", [], |row| {
            row.get(0)
        })
        .expect("evidence count");
    assert_eq!(count, 0);

    connection
        .execute(
            "UPDATE events SET run_id=?1 WHERE session_id=?2 AND run_id=?3",
            rusqlite::params![
                run_id.to_string(),
                team_session_id.to_string(),
                team_view.runs[0].id.to_string()
            ],
        )
        .expect("inject a Run ID shared by two Sessions");
    drop(connection);
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state reopen"))
        .expect("ambiguous-owner read-only Store");
    for session in [session_id, team_session_id] {
        let events = runtime
            .block_on(store.load_session(session))
            .expect("each ambiguous-owner Session remains valid");
        let view = SessionView::replay(session, &events)
            .expect("each Session strictly replays")
            .expect("Session exists");
        assert_eq!(view.runs[0].id, run_id);
    }
    runtime
        .block_on(store.close())
        .expect("close ambiguous Store");
    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .env_clear()
        .current_dir(&workspace)
        .args(["show", "--state-dir"])
        .arg(&state_path)
        .args(["--output", "jsonl", &run_id.to_string()]);
    let ambiguous = run_product(&mut command);
    assert!(!ambiguous.status.success());
    assert_eq!(ambiguous.stdout, b"");
    assert_eq!(
        ambiguous.stderr,
        b"error: Session history unavailable or invalid\n"
    );
    #[cfg(target_os = "linux")]
    {
        let export_state = temp.path().join("export-state");
        let export_id = runtime
            .block_on(create_session(
                StateRoot::admit(&export_state).expect("export State"),
                workspace.clone(),
                Some("Bounded export".into()),
            ))
            .expect("export Session");
        for index in 0..24 {
            runtime
                .block_on(rename_session(
                    StateRoot::open_existing(&export_state).expect("export State"),
                    workspace.clone(),
                    export_id,
                    format!("Export {index:02} {}", "x".repeat(100)),
                ))
                .expect("export title Event");
        }
        let store = Store::open_read_only(StateRoot::open_existing(&export_state).unwrap())
            .expect("closed export Store");
        let events = runtime.block_on(store.load_session(export_id)).unwrap();
        let view = SessionView::replay(export_id, &events)
            .expect("strict export replay")
            .expect("export Session");
        runtime.block_on(store.close()).expect("close export Store");
        let expected = arany::render_session(&view, &events, arany::Output::Jsonl);
        assert!(expected.len() < 64 * 1024);
        let (stdout_reader, stdout_writer) = rustix::pipe::pipe().expect("export pipe");
        let capacity =
            rustix::pipe::fcntl_setpipe_size(&stdout_reader, 4096).expect("reduced export pipe");
        assert!(
            expected.len() > capacity,
            "export must exceed pipe capacity"
        );
        let (stderr_reader, stderr_writer) = rustix::pipe::pipe().expect("error pipe");
        rustix::pipe::fcntl_setpipe_size(&stderr_reader, 4096).expect("reduced error pipe");
        let mut child = super::loopback::ChildGuard::new(
            Command::new(env!("CARGO_BIN_EXE_arany"))
                .env_clear()
                .current_dir(&workspace)
                .args(["show", "--output", "jsonl", "--state-dir"])
                .arg(&export_state)
                .arg(export_id.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout_writer))
                .stderr(Stdio::from(stderr_writer))
                .spawn()
                .expect("bounded export process"),
        );
        child.child().stdout = Some(stdout_reader.into());
        child.child().stderr = Some(stderr_reader.into());
        let exported = super::loopback::wait_product(child.take());
        assert!(exported.status.success());
        assert!(exported.stderr.is_empty());
        assert_eq!(exported.stdout, expected.as_bytes());
        let store = Store::open_read_only(StateRoot::open_existing(&export_state).unwrap())
            .expect("reopen export Store");
        assert_eq!(
            runtime.block_on(store.load_session(export_id)).unwrap(),
            events
        );
        runtime
            .block_on(store.close())
            .expect("close export replay");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn custom_effort_profile_pins_selected_effort_after_synthetic_conformance() {
    use super::loopback::{read_request, send_response, wait_product, write_profile};
    use std::{
        net::TcpListener,
        os::unix::fs::PermissionsExt,
        process::Stdio,
        str::FromStr,
        thread,
        time::{Duration, Instant},
    };

    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback endpoint");
    write_profile(&state, listener.local_addr().unwrap().port());
    let path = state.join("provider-profiles.json");
    let mut profile: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    profile["profiles"][0]["capability_evidence_version"] = serde_json::json!(2);
    profile["profiles"][0]["efforts"] = serde_json::json!(["low", "high"]);
    std::fs::write(&path, serde_json::to_vec(&profile).unwrap()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let server = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        for index in 0..6 {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "request {index} missing");
                        thread::yield_now();
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            let body: serde_json::Value =
                serde_json::from_slice(&read_request(&mut stream)).unwrap();
            assert_eq!(body["model"], "model-1");
            assert_eq!(body["text"]["format"]["strict"], true);
            assert_eq!(
                body.get("reasoning")
                    .map(|value| value["effort"].as_str().unwrap()),
                match index {
                    3 => Some("low"),
                    4 | 5 => Some("high"),
                    _ => None,
                }
            );
            let outcome = match index {
                1 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":["read-only question"]}})
                }
                2 => serde_json::json!({"summary":"synthetic summary"}),
                _ => {
                    serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"answer"}})
                }
            };
            send_response(&mut stream, index, outcome);
        }
    });

    let mut check = Command::new(env!("CARGO_BIN_EXE_arany"));
    check
        .env_clear()
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .current_dir(&workspace)
        .args(["provider", "check", "local", "--state-dir"])
        .arg(&state)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let checked = wait_product(check.spawn().unwrap());
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );

    let mut rejected = Command::new(env!("CARGO_BIN_EXE_arany"));
    rejected
        .env_clear()
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .current_dir(&workspace)
        .args(["exec", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "--provider",
            "custom:local",
            "--model",
            "model-1",
            "--effort",
            "max",
            "answer",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let rejected = wait_product(rejected.spawn().unwrap());
    assert!(!rejected.status.success());
    assert_eq!(rejected.stdout, b"");
    assert_eq!(
        rejected.stderr,
        b"error: selected custom Provider effort is not conformed for its profile\n"
    );

    let mut run = Command::new(env!("CARGO_BIN_EXE_arany"));
    run.env_clear()
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .current_dir(&workspace)
        .args(["exec", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "--provider",
            "custom:local",
            "--model",
            "model-1",
            "--effort",
            "high",
            "answer",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = wait_product(run.spawn().unwrap());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().expect("synthetic server");

    assert_eq!(output.stdout, b"Answer:\n  answer\n");
    let receipt = std::str::from_utf8(&output.stderr).unwrap();
    let session_id = SessionId::from_str(
        receipt
            .lines()
            .next()
            .unwrap()
            .strip_prefix("Session: ")
            .unwrap(),
    )
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        let events = store.load_session(session_id).await.unwrap();
        let view = SessionView::replay(session_id, &events).unwrap().unwrap();
        assert_eq!(view.runs.len(), 1);
        assert_eq!(view.runs[0].status, RunStatus::Finished);
        let config = view.runs[0].config.as_ref().unwrap();
        assert_eq!(config.effort, Some(arany::Effort::High));
        assert_eq!(
            config
                .custom_profile_provenance
                .as_ref()
                .unwrap()
                .capability_evidence_version,
            2
        );
        store.close().await.unwrap();
    });
}

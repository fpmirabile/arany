use super::*;
use arany::{GoalId, GoalStatus};
use std::{
    io::Read, net::TcpListener, process::Stdio, str::FromStr, sync::mpsc, thread, time::Duration,
};

#[test]
fn sigint_cancels_exec_run_and_leaves_its_goal_resumable() {
    interrupted_exec_leaves_its_goal_resumable("-INT", None, "text");
}

#[test]
fn sigint_jsonl_names_the_cancelled_run_and_leaves_its_goal_resumable() {
    interrupted_exec_leaves_its_goal_resumable("-INT", None, "jsonl");
}

#[test]
fn sigterm_stops_exec_run_with_a_bounded_error_and_leaves_its_goal_resumable() {
    interrupted_exec_leaves_its_goal_resumable("-TERM", Some("terminated by SIGTERM"), "text");
}

fn interrupted_exec_leaves_its_goal_resumable(
    signal: &str,
    stop_error: Option<&str>,
    format: &str,
) {
    let jsonl = format == "jsonl";
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    StateRoot::admit(&state).expect("state root");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback endpoint");
    let port = listener.local_addr().expect("listener address").port();
    super::loopback::write_profile(&state, port);
    listener.set_nonblocking(true).expect("bounded accept");
    let (held_tx, held_rx) = mpsc::channel();

    // Three conformance requests precede the Run; the Run request is held open until exec drops it.
    thread::spawn(move || {
        for index in 0..3 {
            let mut stream = crate::process::accept_with_deadline(
                &listener,
                Duration::from_secs(30),
                "conformance request",
            );
            let body: serde_json::Value =
                serde_json::from_slice(&super::loopback::read_request(&mut stream))
                    .expect("request JSON");
            let streaming = body["stream"] == true;
            let text = match index {
                0 => serde_json::json!("ok"),
                1 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"one read-only question","profile":null}]}})
                }
                _ => serde_json::json!(r#"synthetic summary"#),
            };
            super::loopback::send_response_for(&mut stream, index, text, streaming);
        }
        let mut stream =
            crate::process::accept_with_deadline(&listener, Duration::from_secs(30), "Run request");
        super::loopback::read_request(&mut stream);
        held_tx.send(()).expect("observed Run request");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("hold deadline");
        // Returns when exec drops the connection after cancellation.
        let _ = stream.read(&mut [0; 1]);
    });

    super::loopback::check_profile(&workspace, &state);

    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
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
            "--collaboration",
            "single",
            "--output",
            format,
            "answer",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = super::loopback::ChildGuard::new(command.spawn().expect("exec process"));
    held_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("Provider request before interrupt");
    let pid = child.child().id().to_string();
    assert!(
        Command::new("kill")
            .args([signal, &pid])
            .status()
            .expect("send SIGINT")
            .success()
    );
    let output = super::process::capture(child.child(), Duration::from_secs(10), 64 * 1024);

    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    if jsonl {
        assert!(!output.stdout.is_empty());
        assert!(
            stderr.contains("error: Run cancelled\n"),
            "stderr: {stderr}"
        );
        assert!(!stderr.contains("Status:"), "stderr: {stderr}");
    } else {
        assert!(output.stdout.is_empty());
        assert!(stderr.contains("Status: cancelled\n"), "stderr: {stderr}");
    }
    match stop_error {
        Some(message) => assert!(
            stderr.ends_with(&format!("error: {message}\n")),
            "stderr: {stderr}"
        ),
        None => assert_eq!(
            stderr.matches("error:").count(),
            usize::from(jsonl),
            "stderr: {stderr}"
        ),
    }
    let pause = stderr
        .lines()
        .find(|line| line.starts_with("Goal paused; resume with arany exec --session-id "))
        .expect("pause line");
    let words: Vec<&str> = pause.split_whitespace().collect();
    let session_id = SessionId::from_str(words[7]).expect("Session ID");
    assert_eq!(words[8], "--resume-goal");
    let goal_id = GoalId::from_str(words[9]).expect("Goal ID");

    let store = Store::open_read_only(StateRoot::open_existing(&state).expect("state reopen"))
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
    assert_eq!(view.runs.len(), 1);
    assert_eq!(view.runs[0].status, RunStatus::Cancelled);
    if !jsonl {
        assert!(stderr.contains(&format!("Run: {}\n", view.runs[0].id)));
    }
    assert_eq!(
        view.goal(goal_id).expect("paused Goal").status,
        GoalStatus::Interrupted
    );
}

const APPROVAL_DEATH_WAIT: Duration = Duration::from_secs(20);
const APPROVAL_DEATH_OUTPUT_BYTES: usize = 64 * 1024;
const APPROVAL_DEATH_STACK_BYTES: usize = 8 * 1024 * 1024;
const APPROVAL_DEATH_ROOT: &str = "ARANY_APPROVAL_DEATH_ROOT";

#[test]
#[ignore = "supervised approval-wait process-death helper"]
fn approval_wait_death_helper() {
    use arany::test_support::scenario::{ScriptedProvider, request, tool_outcome};
    let root = std::path::PathBuf::from(std::env::var_os(APPROVAL_DEATH_ROOT).unwrap());
    let workspace = root.join("workspace");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let engine = engine_support::open(
            StateRoot::open_existing(&root.join("state")).unwrap(),
            arany::EngineConfig {
                guard_executable: Some(std::env::current_exe().unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        let session = engine
            .create_session(workspace.clone(), Some("Approval death".into()))
            .await
            .unwrap();
        let provider = std::sync::Arc::new(ScriptedProvider::new(vec![tool_outcome(
            arany::ToolCall::Read {
                path: "facts.txt".into(),
                offset: None,
                limit: Some(32),
            },
        )]));
        let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
        let mut input = request(&workspace, "Read the facts");
        input.session_id = Some(session);
        input.title = None;
        input.include_paths.clear();
        let (result, ()) = tokio::join!(
            engine_support::run_with(
                &engine,
                &provider,
                input,
                arany::RunControls {
                    tools: arany::ToolGrant::Configured,
                    approvals: Some(approvals),
                    ..Default::default()
                }
            ),
            async {
                let approval = inbox.next().await.expect("Ask reached the human channel");
                assert_eq!(provider.requests.lock().unwrap().len(), 1);
                println!("approval-ready {session}");
                std::io::Write::flush(&mut std::io::stdout()).unwrap();
                std::future::pending::<()>().await;
                drop(approval);
            }
        );
        result.expect("approval helper Run");
    });
}

#[test]
fn death_during_approval_leaves_no_started_effect_and_resume_reproposes() {
    use std::io::BufRead;
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("facts.txt"), "Fact").unwrap();
    let state = temp.path().join("state");
    StateRoot::admit(&state).unwrap();
    write_approval_death_settings(&state, true);
    let mut child = super::loopback::ChildGuard::new(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "exec_interrupt::approval_wait_death_helper",
                "--nocapture",
            ])
            .env_clear()
            .env("RUST_MIN_STACK", APPROVAL_DEATH_STACK_BYTES.to_string())
            .env(APPROVAL_DEATH_ROOT, temp.path())
            .current_dir(&workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = child.child().stdout.take().unwrap();
    let (ready, received) = mpsc::sync_channel(1);
    let reader = thread::spawn(move || {
        let mut lines = Vec::new();
        for line in std::io::BufReader::new(stdout.take(APPROVAL_DEATH_OUTPUT_BYTES as u64)).lines()
        {
            let line = line.unwrap();
            if let Some((_, session)) = line.split_once("approval-ready ") {
                ready.send(session.parse::<SessionId>().unwrap()).unwrap();
            }
            lines.push(line);
        }
        lines
    });
    let session = received.recv_timeout(APPROVAL_DEATH_WAIT);
    if child.child().try_wait().expect("helper status").is_none() {
        child.child().kill().expect("stop approval helper");
    }
    let output = super::process::capture(
        child.child(),
        APPROVAL_DEATH_WAIT,
        APPROVAL_DEATH_OUTPUT_BYTES,
    );
    let lines = reader.join().unwrap();
    let session = session.unwrap_or_else(|error| {
        panic!(
            "approval helper did not reach Ask: {error:?}; status={:?}; stdout={lines:?}; stderr={:?}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert!(!output.status.success());
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("approval-ready "))
            .count(),
        1
    );
    assert!(lines.iter().any(|line| line == "running 1 test"));
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(resume_after_approval_death(&state, &workspace, session));
}

fn write_approval_death_settings(state: &std::path::Path, ask: bool) {
    use std::os::unix::fs::PermissionsExt;
    for (name, bytes) in [
        ("tools.json", br#"{"version":1,"workspace_paths":["facts.txt"],"write":false,"commands":[],"skills":[],"mcp":[]}"#.as_slice()),
        ("permissions.json", if ask { br#"{"permissions":{"ask":["Read"]}}"#.as_slice() } else { b"{}".as_slice() }),
    ] {
        std::fs::write(state.join(name), bytes).unwrap();
        std::fs::set_permissions(state.join(name), std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

async fn resume_after_approval_death(
    state: &std::path::Path,
    workspace: &std::path::Path,
    session: SessionId,
) {
    use arany::test_support::scenario::{ScriptedProvider, request, tool_outcome};
    let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
    let events = store.load_session(session).await.unwrap();
    let view = store.load_view(session).await.unwrap().unwrap();
    store.close().await.unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event.event, Event::ToolProposed { .. }))
    );
    assert!(!events.iter().any(|event| matches!(
        event.event,
        Event::ToolStarted { .. } | Event::ToolFinished { .. }
    )));
    assert_eq!(view.runs[0].tools[0].started_sequence, None);
    assert_eq!(
        view.run_stop(&view.runs[0]),
        Some(arany::RunStop::Interrupted)
    );
    let goal = view.interrupted_goal_id().unwrap();
    write_approval_death_settings(state, false);
    let engine = engine_support::open(
        StateRoot::open_existing(state).unwrap(),
        arany::EngineConfig {
            guard_executable: Some(std::env::current_exe().unwrap()),
            ..Default::default()
        },
    )
    .unwrap();
    let provider = std::sync::Arc::new(ScriptedProvider::new(vec![
        tool_outcome(arany::ToolCall::Read {
            path: "facts.txt".into(),
            offset: None,
            limit: Some(32),
        }),
        arany::test_support::goals::finish_with("done"),
    ]));
    let mut input = request(workspace, "Read the facts");
    input.include_paths.clear();
    input.session_id = Some(session);
    input.title = None;
    input.goal = arany::GoalRequest::resume(goal, true);
    let result = engine_support::run_with(
        &engine,
        &provider,
        input,
        arany::RunControls {
            tools: arany::ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    engine.close().await.unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert_eq!(result.run.tools.len(), 1);
    assert_eq!(
        result.run.tools[0]
            .observation
            .as_ref()
            .unwrap()
            .disposition,
        arany::ToolDisposition::Succeeded
    );
    let (_, reopened) = arany::test_support::goals::replayed(state, session).await;
    assert!(reopened.runs[0].tools[0].observation.is_none());
    assert_eq!(reopened.runs.last().unwrap().tools, result.run.tools);
}

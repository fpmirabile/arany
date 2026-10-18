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
    let (held_tx, held_rx) = mpsc::channel();

    // Three conformance requests precede the Run; the Run request is held open until exec drops it.
    thread::spawn(move || {
        for index in 0..3 {
            let (mut stream, _) = listener.accept().expect("conformance request");
            let body: serde_json::Value =
                serde_json::from_slice(&super::loopback::read_request(&mut stream))
                    .expect("request JSON");
            let streaming = body["stream"] == true;
            let text = match index {
                0 => serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"ok"}}),
                1 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"one read-only question","profile":null}]}})
                }
                _ => serde_json::json!({"summary":"synthetic summary"}),
            };
            super::loopback::send_response_for(&mut stream, index, text, streaming);
        }
        let (mut stream, _) = listener.accept().expect("Run request");
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

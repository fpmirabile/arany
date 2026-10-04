use super::{TmuxServer, capture_pane, redacted_tail};
use crate::loopback::{
    check_profile_with_binary, read_request, send_response, wait_product, write_profile,
};
use arany::{
    AgentDisposition, AgentRole, AgentRunId, CollaborationPolicy, Event, RunConfig, RunDisposition,
    RunId, RunStatus, SessionId, StateRoot, Store, resume_session,
};
use std::{
    io::Read,
    net::{TcpListener, TcpStream},
    os::unix::fs::{MetadataExt, PermissionsExt},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const HISTORY_RUNS: usize = 1600;
const REQUEST_LIMIT: usize = 1024 * 1024;

fn resumed_tail_layout(screen: &[u8]) -> bool {
    let Ok(screen) = std::str::from_utf8(screen) else {
        return false;
    };
    let rows: Vec<_> = screen.lines().collect();
    rows.iter().all(|row| row.chars().count() <= 80)
        && rows
            .get(rows.len().saturating_sub(4))
            .is_some_and(|row| row.starts_with("─ Ask Arany "))
        && rows
            .get(rows.len().saturating_sub(2))
            .is_some_and(|row| row.contains("Enter sends") && row.contains("Ctrl+O newline"))
        && rows
            .last()
            .is_some_and(|row| row.starts_with("finished · custom:local/model-1"))
}

async fn seed_history(state: &std::path::Path, workspace: &std::path::Path) -> SessionId {
    let metadata = std::fs::metadata(workspace).expect("Workspace identity");
    let session_id = SessionId::new();
    let store = Store::open(StateRoot::admit(state).expect("private State")).expect("Store");
    store
        .append(
            session_id,
            Event::SessionStarted {
                title: "Near-cap fixture".into(),
                workspace_identity: Some((metadata.dev(), metadata.ino())),
            },
        )
        .await
        .expect("Session start");
    let objective_body = "0123456789abcdef".repeat(511);
    let answer_body = "0123456789abcdef".repeat(2047);
    for index in 0..HISTORY_RUNS {
        let run_id = RunId::new();
        let agent_run_id = AgentRunId::new();
        let objective = format!("objective {index:04} {objective_body}");
        let answer = format!("answer {index:04} {answer_body}");
        for event in [
            Event::MessageAccepted {
                run_id,
                text: objective,
                images: Vec::new(),
            },
            Event::RunStarted {
                run_id,
                config: RunConfig {
                    provider: "scripted".into(),
                    model: "test-model".into(),
                    effort: None,
                    custom_profile_provenance: None,
                    saved_api_account_id: None,
                    chatgpt_provenance: None,
                    output_token_bound: arany::OutputTokenBound::ProviderEnforced,
                    policy: CollaborationPolicy::Single,
                    output_token_cap: 4096,
                    provider_concurrency: 1,
                    workspace_device: metadata.dev(),
                    workspace_inode: metadata.ino(),
                    instruction_digest: None,
                    include_digests: Vec::new(),
                    history_run_ids: Vec::new(),
                    excluded_history_runs: 0,
                    context_usage: None,
                    compaction_event_sequence: None,
                    compaction_content_digest: None,
                    tool_policy: None,
                },
            },
            Event::AgentSpawned {
                run_id,
                agent_run_id,
                role: AgentRole::Primary,
                ordinal: 0,
                objective: None,
            },
            Event::AgentFinished {
                run_id,
                agent_run_id,
                disposition: AgentDisposition::Finished,
                summary: Some("done".into()),
                result: Some(answer.clone()),
            },
            Event::MessageCommitted {
                run_id,
                text: answer,
            },
            Event::RunFinished {
                run_id,
                disposition: RunDisposition::Finished,
            },
        ] {
            store.append(session_id, event).await.expect("valid Event");
        }
    }
    store.close().await.expect("closed Store");
    session_id
}

fn read_bounded_request(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("request timeout");
    let mut request = Vec::new();
    let mut expected = None;
    loop {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).expect("request bytes");
        assert!(count > 0 && request.len() + count <= REQUEST_LIMIT);
        request.extend_from_slice(&chunk[..count]);
        if expected.is_none()
            && let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n")
        {
            let header_end = end + 4;
            let header = std::str::from_utf8(&request[..header_end]).expect("HTTP header");
            assert!(header.starts_with("POST /v1/responses HTTP/1.1\r\n"));
            let body_len = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .expect("Content-Length");
            assert!(header_end + body_len <= REQUEST_LIMIT);
            expected = Some(header_end + body_len);
        }
        if expected.is_some_and(|length| request.len() >= length) {
            assert!(
                request
                    .windows(b"latency-probe".len())
                    .any(|part| part == b"latency-probe")
            );
            return;
        }
    }
}

#[tokio::test]
#[ignore = "native Linux near-cap active keyboard measurement; requires /usr/bin/tmux"]
async fn active_keyboard_stays_visible_with_near_cap_history() {
    let executable = super::test_product_executable();
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let exit_path = temp.path().join("exit");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback Provider");
    write_profile(
        &state,
        listener.local_addr().expect("Provider address").port(),
    );
    let (request_sender, request_ready) = mpsc::channel();
    let (release_sender, release_ready) = mpsc::channel();
    let provider = thread::spawn(move || {
        listener.set_nonblocking(true).expect("bounded accept");
        for index in 0..5 {
            let deadline = Instant::now() + Duration::from_secs(30);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "synthetic request missing");
                        thread::yield_now();
                    }
                    Err(error) => panic!("synthetic accept: {error}"),
                }
            };
            if index < 3 {
                read_request(&mut stream);
                let text = match index {
                    0 => {
                        serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"ok"}})
                    }
                    1 => serde_json::json!({"outcome":{"type":"delegate","children":["child"]}}),
                    _ => serde_json::json!({"summary":"summary"}),
                };
                send_response(&mut stream, index, text);
            } else if index == 3 {
                read_bounded_request(&mut stream);
                request_sender.send(()).expect("active Run request");
                release_ready
                    .recv_timeout(Duration::from_secs(60))
                    .expect("release primary response");
                send_response(
                    &mut stream,
                    index,
                    serde_json::json!({"outcome":{"type":"delegate","children":["child"]}}),
                );
            } else {
                read_request(&mut stream);
                request_sender.send(()).expect("child Run request");
                release_ready
                    .recv_timeout(Duration::from_secs(60))
                    .expect("release child response");
            }
        }
    });
    check_profile_with_binary(&workspace, &state, &executable);
    let session_id = seed_history(&state, &workspace).await;

    let wrapper = temp.path().join("tmux-pane.sh");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nbefore=$(stty -g)\n\"$ARANY_TEST_EXE\" --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\" --provider custom:local --model model-1 --no-color\nresult=$?\nafter=$(stty -g)\nprintf '%s\\n%s\\n%s\\n' \"$result\" \"$before\" \"$after\" > \"$ARANY_TEST_EXIT\"\nexit \"$result\"\n",
    )
    .expect("tmux pane wrapper");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))
        .expect("private executable wrapper");
    let server = TmuxServer {
        socket: temp.path().join("tmux.sock"),
    };
    let mut start = Command::new("/usr/bin/tmux");
    start
        .env_clear()
        .env("TERM", "xterm")
        .env("SHELL", "/bin/sh")
        .env("PATH", "/usr/bin:/bin")
        .env("ARANY_TEST_EXE", &executable)
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_SESSION", session_id.to_string())
        .env("ARANY_TEST_EXIT", &exit_path)
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .arg("-S")
        .arg(&server.socket)
        .args([
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-x",
            "80",
            "-y",
            "24",
            "-s",
            "arany-test",
        ])
        .arg(&wrapper)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    assert!(
        wait_product(start.spawn().expect("tmux server start"))
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if capture_pane(&server).is_some_and(|screen| resumed_tail_layout(&screen)) {
            break;
        }
        assert!(
            Instant::now() < deadline && !exit_path.exists(),
            "near-cap resumed composer missing: {}",
            redacted_tail(&capture_pane(&server).unwrap_or_default())
        );
        thread::yield_now();
    }
    let pane_pid = server.run(&[
        "display-message",
        "-p",
        "-t",
        "arany-test:0.0",
        "#{pane_pid}",
    ]);
    assert!(pane_pid.status.success(), "test-owned pane PID");
    let pane_pid = String::from_utf8(pane_pid.stdout)
        .expect("pane PID UTF-8")
        .trim()
        .parse::<u32>()
        .expect("pane PID");
    let product_pid = super::product_child_of_executable(pane_pid, &executable);
    let product_guard = super::ProductGuard::for_executable(product_pid, state.clone(), executable);
    assert!(product_guard.is_owned_and_running(), "private Arany child");
    assert!(
        server
            .run(&["send-keys", "-l", "-t", "arany-test:0.0", "latency-probe"])
            .status
            .success()
    );
    let submit_start = Instant::now();
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Enter"])
            .status
            .success()
    );
    request_ready
        .recv_timeout(Duration::from_secs(30))
        .expect("active Provider request");
    let admission = submit_start.elapsed();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server)
            .is_some_and(|screen| String::from_utf8_lossy(&screen).contains("working"))
        {
            break;
        }
        assert!(Instant::now() < deadline, "active Run not visible");
        thread::yield_now();
    }

    let mut samples = Vec::new();
    let draft = "QWERTYUIOP";
    for (index, character) in draft.char_indices() {
        if index == 5 {
            assert!(
                server
                    .run(&["send-keys", "-t", "arany-test:0.0", "PageUp"])
                    .status
                    .success()
            );
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if capture_pane(&server)
                    .is_some_and(|screen| String::from_utf8_lossy(&screen).contains("History"))
                {
                    break;
                }
                assert!(Instant::now() < deadline, "history navigation not visible");
                thread::yield_now();
            }
            release_sender.send(()).expect("release primary delegation");
            request_ready
                .recv_timeout(Duration::from_secs(20))
                .expect("child Provider request");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if capture_pane(&server).is_some_and(|screen| {
                    let text = String::from_utf8_lossy(&screen);
                    text.contains("History")
                        && text.contains("QWERT")
                        && text.contains("child 1 · working")
                }) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "delegation lost draft or history mode"
                );
                thread::yield_now();
            }
        }
        let start = Instant::now();
        assert!(
            server
                .run(&[
                    "send-keys",
                    "-l",
                    "-t",
                    "arany-test:0.0",
                    &character.to_string()
                ])
                .status
                .success()
        );
        let expected = &draft[..index + character.len_utf8()];
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if capture_pane(&server)
                .is_some_and(|screen| String::from_utf8_lossy(&screen).contains(expected))
            {
                samples.push(start.elapsed());
                break;
            }
            assert!(Instant::now() < deadline, "active draft key not visible");
            thread::yield_now();
        }
    }
    samples.sort();
    println!(
        "history_active_tmux runs={HISTORY_RUNS} transcript_bytes=65528000 admission={admission:?} key_samples={} key_p50={:?} key_max={:?}",
        samples.len(),
        samples[(samples.len() - 1) / 2],
        samples[samples.len() - 1]
    );
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-l"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.contains(draft) && !text.contains("History")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "live tail did not retain draft: {}",
            redacted_tail(&capture_pane(&server).unwrap_or_default())
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-c"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.contains(draft) && text.contains("Draft retained:") && !text.contains("History")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cancelled Run did not retain draft: {}",
            redacted_tail(&capture_pane(&server).unwrap_or_default())
        );
        thread::yield_now();
    }
    release_sender
        .send(())
        .expect("release synthetic connection");
    provider.join().expect("synthetic Provider server");
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-c"])
            .status
            .success()
    );
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "/quit", "Enter"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(marker) = std::fs::read_to_string(&exit_path) {
            let lines: Vec<_> = marker.lines().collect();
            if lines.len() == 3 {
                assert_eq!(lines[0], "0", "product exit success");
                assert_eq!(lines[1], lines[2], "terminal settings restored");
                break;
            }
        }
        assert!(Instant::now() < deadline, "product did not exit");
        thread::yield_now();
    }
    let view = resume_session(
        StateRoot::open_existing(&state).expect("read-only State"),
        workspace,
        session_id,
    )
    .await
    .expect("strict Session replay");
    assert_eq!(view.runs.len(), HISTORY_RUNS + 1, "draft did not submit");
    let last = view.runs.last().expect("active Run");
    assert_eq!(last.objective, "latency-probe");
    assert_eq!(last.status, RunStatus::Cancelled);
    assert_eq!(last.agents.len(), 2, "committed child delegation");
    assert!(last.assistant_message.is_none());
    assert!(!product_guard.is_owned_and_running(), "product exited");
}

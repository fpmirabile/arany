use super::super::loopback::send_response_for;
use super::*;
use arany::{Event, GoalStatus};

const NOTE: &str = "use the smaller fixture";
const DELEGATE: &str = r#"{"outcome":{"type":"delegate","children":[{"objective":"synthetic child","profile":null}]}}"#;

struct Attached {
    process: ChildGuard,
    input: Arc<Mutex<ChildStdin>>,
    stages: mpsc::Receiver<(usize, Vec<u8>)>,
    reader: thread::JoinHandle<Vec<u8>>,
}

// Linear mode: each typed line is one canonical line, so `\n` is Enter.
fn spawn_linear_run(
    state: &Path,
    workspace: &Path,
    objective: &str,
    patterns: &[&[u8]],
) -> Attached {
    let command = format!(
        "trap ':' INT; printf 'SHELL_PID:%s\\n' \"$$\"; \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider custom:local --model model-1 {objective}; exit \"$?\""
    );
    let mut attached =
        crate::isolation::account_isolated_script(state.parent().expect("fixture root"));
    attached
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", state)
        .env("ARANY_TEST_WORKSPACE", workspace)
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .env("SHELL", "/bin/sh")
        .env("TERM", "dumb")
        .current_dir(workspace)
        .args(["-q", "-e", "-c", &command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut attached = ChildGuard::new(attached.spawn().expect("attached PTY process"));
    let input = Arc::new(Mutex::new(
        attached.child().stdin.take().expect("PTY input"),
    ));
    let reader_input = Arc::clone(&input);
    let mut output = attached.child().stdout.take().expect("PTY output");
    let patterns: Vec<Vec<u8>> = patterns.iter().map(|pattern| pattern.to_vec()).collect();
    let (stage_sender, stages) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut declined = false;
        let mut bytes = Vec::new();
        let mut next = 0;
        let mut search_from = 0;
        loop {
            let mut chunk = [0; 4096];
            let count = output.read(&mut chunk).expect("PTY output bytes");
            if count == 0 {
                break;
            }
            assert!(bytes.len() + count <= 64 * 1024, "bounded PTY output");
            bytes.extend_from_slice(&chunk[..count]);
            crate::process::decline_workspace_consent(
                &mut *reader_input.lock().expect("fixture input"),
                &bytes,
                &mut declined,
            );
            while next < patterns.len() {
                let Some(offset) = bytes[search_from..]
                    .windows(patterns[next].len())
                    .position(|part| part == patterns[next])
                else {
                    break;
                };
                search_from += offset + patterns[next].len();
                stage_sender
                    .send((next, bytes.clone()))
                    .expect("stage receiver");
                next += 1;
            }
        }
        bytes
    });
    Attached {
        process: attached,
        input,
        stages,
        reader,
    }
}

fn stage(stages: &mpsc::Receiver<(usize, Vec<u8>)>, expected: usize, what: &str) -> Vec<u8> {
    let (index, bytes) = stages
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|error| panic!("{what} {error}"));
    assert_eq!(index, expected, "{what}");
    bytes
}

// Accepts within a deadline; the accepted socket is blocking again for read_request.
fn accept_within(listener: &TcpListener) -> std::net::TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    listener.set_nonblocking(true).expect("bounded accept");
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "synthetic Provider request missing"
                );
                thread::yield_now();
            }
            Err(error) => panic!("accept failed: {error}"),
        }
    };
    stream.set_nonblocking(false).expect("blocking request");
    stream
}

fn semantic_input(stream: &mut std::net::TcpStream) -> serde_json::Value {
    let body = read_request(stream);
    assert!(
        !body
            .windows(b"OMITTED_CANARY".len())
            .any(|part| part == b"OMITTED_CANARY")
    );
    let wire: serde_json::Value = serde_json::from_slice(&body).expect("Responses request");
    assert_eq!(wire["model"], "model-1");
    crate::semantic::input(&wire)
}

// The provider check consumes three requests: finish, delegate and summary.
fn answer_provider_check(stream: &mut std::net::TcpStream, index: usize) {
    let text = match index {
        0 => serde_json::json!("ok"),
        1 => serde_json::from_str(DELEGATE).expect("Delegate JSON"),
        _ => serde_json::json!(r#"synthetic summary"#),
    };
    send_response_for(stream, index, text, index < 2);
}

fn fixture_root(temp: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    std::fs::write(workspace.join("private.txt"), b"OMITTED_CANARY")
        .expect("omitted Workspace input");
    (workspace, temp.path().join("state"))
}

fn replayed(state: &Path, workspace: &Path) -> (Vec<arany::EventEnvelope>, SessionView) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only replay runtime");
    let sessions = runtime
        .block_on(arany::list_sessions(
            StateRoot::open_existing(state).expect("state reopen"),
            workspace.to_path_buf(),
        ))
        .expect("list committed Sessions");
    assert_eq!(sessions.len(), 1);
    let store = Store::open_read_only(StateRoot::open_existing(state).expect("state reopen"))
        .expect("read-only Store");
    let events = runtime
        .block_on(store.load_session(sessions[0].id))
        .expect("Events");
    let view = SessionView::replay(sessions[0].id, &events)
        .expect("strict replay")
        .expect("Session");
    (events, view)
}

#[test]
fn steering_note_during_attached_run_reaches_synthesis_and_is_confirmed() {
    const NOTE: &str = "use the\nsmaller fixture";
    let temp = tempfile::tempdir().expect("private test root");
    let (workspace, state) = fixture_root(&temp);
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback Provider");
    write_profile(
        &state,
        listener.local_addr().expect("Provider address").port(),
    );
    let (child_ready_sender, child_ready) = mpsc::channel();
    let (release_sender, release_child) = mpsc::channel::<()>();
    // Calls 0-2 are the provider check; then 3 primary Delegate, 4 held child, 5 synthesis.
    let server = thread::spawn(move || {
        for index in 0..6 {
            let mut stream = accept_within(&listener);
            let input = semantic_input(&mut stream);
            if index < 3 {
                answer_provider_check(&mut stream, index);
                continue;
            }
            match index {
                3 => {
                    assert_eq!(input["objective"], "steer");
                    assert!(
                        input
                            .get("steering_notes")
                            .is_none_or(serde_json::Value::is_null)
                    );
                    send_response_for(
                        &mut stream,
                        index,
                        serde_json::from_str(DELEGATE).expect("Delegate JSON"),
                        false,
                    );
                }
                4 => {
                    child_ready_sender.send(()).expect("held child gate");
                    release_child
                        .recv_timeout(Duration::from_secs(10))
                        .expect("release held child");
                    send_response_for(&mut stream, index, serde_json::json!("child result"), false);
                }
                _ => {
                    // The semantic wire helper projects each physical line separately.
                    assert_eq!(
                        input["steering_notes"],
                        serde_json::json!(["use the", "smaller fixture"])
                    );
                    send_response_for(&mut stream, index, serde_json::json!("synthesized"), false);
                }
            }
        }
    });
    check_profile(&workspace, &state);

    let mut attached = spawn_linear_run(
        &state,
        &workspace,
        "steer",
        &[
            b"SHELL_PID:",
            b"Agent: primary; state: active",
            b"Draft: 8 characters; Enter offers it as a steering note",
            b"Notice: Steering note waits for the agent's next step\r\n",
            b"Notice: Working \xc2\xb7 request ",
            b"Steering note sent to the agent",
            b"Input:",
        ],
    );
    let shell_text =
        String::from_utf8_lossy(&stage(&attached.stages, 0, "shell PID stage")).into_owned();
    let shell_pid = transcript_field(&shell_text, "SHELL_PID:")
        .parse::<u32>()
        .expect("shell PID");
    let product_pid = product_child_of(shell_pid);
    let _product = ProductGuard::new(product_pid, state.clone());
    stage(&attached.stages, 1, "active Run stage");
    child_ready
        .recv_timeout(Duration::from_secs(10))
        .expect("child call reached the Provider");
    write_input(
        &attached.input,
        b"\x1b[200~ignored\n/goal pause\n\x1b[201~\nuse the\\\n",
    );
    stage(
        &attached.stages,
        2,
        "newline retained without a steering offer",
    );
    write_input(&attached.input, b"smaller fixture\n");
    stage(&attached.stages, 3, "steering offer notice");
    // Controls keep running while the note waits; only plain text lines are offers.
    write_input(&attached.input, b"/status\n");
    stage(&attached.stages, 4, "active status while the note waits");
    release_sender.send(()).expect("release child call");
    stage(&attached.stages, 5, "steering confirmation");
    stage(&attached.stages, 6, "idle prompt after Run");
    write_input(&attached.input, b"/exit\n");
    let result = wait_product(attached.process.take());
    let output = attached.reader.join().expect("PTY output reader");
    assert!(result.status.success(), "attached exit after steering");
    assert!(
        output
            .windows(b"Steering note sent to the agent".len())
            .any(|part| part == b"Steering note sent to the agent")
    );
    server.join().expect("synthetic Provider conversation");

    let (events, view) = replayed(&state, &workspace);
    assert_eq!(view.runs.len(), 1);
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(view.runs[0].steering_notes, [NOTE]);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::GoalSteeringNoteRecorded { .. }))
            .count(),
        1
    );
}

#[test]
fn pause_during_attached_run_waits_for_the_call_and_pauses_the_goal() {
    let temp = tempfile::tempdir().expect("private test root");
    let (workspace, state) = fixture_root(&temp);
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback Provider");
    write_profile(
        &state,
        listener.local_addr().expect("Provider address").port(),
    );
    let (primary_ready_sender, primary_ready) = mpsc::channel();
    let (release_sender, release_primary) = mpsc::channel::<()>();
    let (stop_sender, stop) = mpsc::channel::<()>();
    // Calls 0-2 are the provider check; call 3 is the held primary. Any later request is a violation.
    let server = thread::spawn(move || {
        for index in 0..4 {
            let mut stream = accept_within(&listener);
            let input = semantic_input(&mut stream);
            if index < 3 {
                answer_provider_check(&mut stream, index);
                continue;
            }
            assert_eq!(input["objective"], "pause");
            primary_ready_sender.send(()).expect("held primary gate");
            release_primary
                .recv_timeout(Duration::from_secs(10))
                .expect("release held primary call");
            send_response_for(
                &mut stream,
                index,
                serde_json::from_str(DELEGATE).expect("Delegate JSON"),
                false,
            );
        }
        stop.recv_timeout(Duration::from_secs(15))
            .expect("product exit observed");
        listener
            .set_nonblocking(true)
            .expect("bounded final accept");
        listener.accept().is_ok()
    });
    check_profile(&workspace, &state);

    let mut attached = spawn_linear_run(
        &state,
        &workspace,
        "pause",
        &[
            b"SHELL_PID:",
            b"Agent: primary; state: active",
            b"Notice: Steering note waits for the agent's next step\r\n",
            b"Pausing at the next step; Ctrl+C cancels now",
            b"Status: cancelled",
            b"Goal paused; use /goal resume to continue.",
            b"Draft retained: 23 characters; Enter submits",
            b"Input:",
        ],
    );
    let shell_text =
        String::from_utf8_lossy(&stage(&attached.stages, 0, "shell PID stage")).into_owned();
    let shell_pid = transcript_field(&shell_text, "SHELL_PID:")
        .parse::<u32>()
        .expect("shell PID");
    let product_pid = product_child_of(shell_pid);
    let _product = ProductGuard::new(product_pid, state.clone());
    stage(&attached.stages, 1, "active Run stage");
    primary_ready
        .recv_timeout(Duration::from_secs(10))
        .expect("primary call reached the Provider");
    write_input(&attached.input, format!("{NOTE}\n").as_bytes());
    stage(&attached.stages, 2, "steering offer notice");
    write_input(&attached.input, b"/goal pause\n");
    stage(&attached.stages, 3, "pausing notice");
    release_sender.send(()).expect("release held primary call");
    stage(&attached.stages, 4, "cancelled receipt");
    stage(&attached.stages, 5, "goal resume notice");
    stage(&attached.stages, 6, "undelivered note returned as draft");
    // Idle Ctrl+C clears the retained draft so the exit line is not appended to it.
    write_input(&attached.input, b"\x03");
    stage(&attached.stages, 7, "idle prompt after clearing draft");
    write_input(&attached.input, b"/exit\n");
    let result = wait_product(attached.process.take());
    let output = attached.reader.join().expect("PTY output reader");
    assert!(result.status.success(), "attached exit after pause");
    let count = |text: &[u8]| {
        output
            .windows(text.len())
            .filter(|part| *part == text)
            .count()
    };
    assert_eq!(
        count(b"Notice: Pausing at the next step; Ctrl+C cancels now"),
        1,
        "linear pause prints one pausing notice"
    );

    stop_sender.send(()).expect("stop Provider observation");
    assert!(
        !server.join().expect("synthetic Provider conversation"),
        "no child or follow-up Provider request after pause"
    );

    let (events, view) = replayed(&state, &workspace);
    assert_eq!(view.runs.len(), 1);
    assert_eq!(view.runs[0].status, RunStatus::Cancelled);
    assert_eq!(view.goals.len(), 1);
    assert_eq!(view.goals[0].status, GoalStatus::Interrupted);
    assert!(
        view.runs[0].steering_notes.is_empty(),
        "undelivered note is not recorded"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, Event::ToolStarted { .. })),
        "a paused Run dispatches no Tool"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::ProviderCallRecorded { .. }))
            .count(),
        1
    );
}

#[test]
fn held_linear_draft_yields_to_controls_and_returns_at_run_end() {
    let temp = tempfile::tempdir().expect("private test root");
    let (workspace, state) = fixture_root(&temp);
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback Provider");
    write_profile(
        &state,
        listener.local_addr().expect("Provider address").port(),
    );
    let note = "a".repeat(4000);
    let expected_note = note.clone();
    let (child_ready_sender, child_ready) = mpsc::channel();
    let (release_child_sender, release_child) = mpsc::channel::<()>();
    let (release_synthesis_sender, release_synthesis) = mpsc::channel::<()>();
    // Calls 0-2 are the provider check; then 3 primary Delegate, 4 held child, 5 held synthesis.
    let server = thread::spawn(move || {
        for index in 0..6 {
            let mut stream = accept_within(&listener);
            let input = semantic_input(&mut stream);
            if index < 3 {
                answer_provider_check(&mut stream, index);
                continue;
            }
            match index {
                3 => {
                    assert_eq!(input["objective"], "steer");
                    send_response_for(
                        &mut stream,
                        index,
                        serde_json::from_str(DELEGATE).expect("Delegate JSON"),
                        false,
                    );
                }
                4 => {
                    child_ready_sender.send(()).expect("held child gate");
                    release_child
                        .recv_timeout(Duration::from_secs(10))
                        .expect("release held child");
                    send_response_for(&mut stream, index, serde_json::json!("child result"), false);
                }
                _ => {
                    assert_eq!(input["steering_notes"], serde_json::json!([expected_note]));
                    release_synthesis
                        .recv_timeout(Duration::from_secs(10))
                        .expect("release held synthesis");
                    send_response_for(&mut stream, index, serde_json::json!("synthesized"), false);
                }
            }
        }
    });
    check_profile(&workspace, &state);

    let mut attached = spawn_linear_run(
        &state,
        &workspace,
        "steer",
        &[
            b"SHELL_PID:",
            b"Agent: primary; state: active",
            b"Steering note waits for the agent's next step",
            b"Steering note sent to the agent",
            b"No room for more steering notes this Run",
            b"Notice: Working",
            b"A draft is already retained; line not kept",
            b"A draft is already retained; line not kept",
            b"Draft retained: 4200 characters; Enter submits",
            b"Input:",
            b"Input:",
        ],
    );
    let shell_text =
        String::from_utf8_lossy(&stage(&attached.stages, 0, "shell PID stage")).into_owned();
    let shell_pid = transcript_field(&shell_text, "SHELL_PID:")
        .parse::<u32>()
        .expect("shell PID");
    let product_pid = product_child_of(shell_pid);
    let _product = ProductGuard::new(product_pid, state.clone());
    stage(&attached.stages, 1, "active Run stage");
    child_ready
        .recv_timeout(Duration::from_secs(10))
        .expect("child call reached the Provider");
    write_input(&attached.input, format!("{note}\n").as_bytes());
    stage(&attached.stages, 2, "steering offer notice");
    release_child_sender.send(()).expect("release child call");
    stage(
        &attached.stages,
        3,
        "steering confirmation while synthesis is held",
    );
    // The 4000-byte note fills most of the Run's 8 KiB, so 4200 more characters are refused.
    let mut draft = vec![b'b'; 3000];
    draft.push(0x04);
    write_input(&attached.input, &draft);
    let mut line = vec![b'b'; 1200];
    line.push(b'\n');
    write_input(&attached.input, &line);
    stage(&attached.stages, 4, "RunFull refusal keeps the draft aside");
    write_input(&attached.input, b"/status\n");
    stage(&attached.stages, 5, "status runs while the draft is held");
    write_input(&attached.input, b"x\n");
    stage(&attached.stages, 6, "text line is not kept");
    let mut continued = vec![b'c'; 4000];
    continued.push(0x04);
    write_input(&attached.input, &continued);
    stage(&attached.stages, 7, "continued text is not kept");
    release_synthesis_sender
        .send(())
        .expect("release synthesis call");
    stage(&attached.stages, 8, "held draft returned at Run end");
    stage(&attached.stages, 9, "idle prompt after Run");
    // Idle Ctrl+C clears the retained draft so the exit line is not appended to it.
    write_input(&attached.input, b"\x03");
    stage(&attached.stages, 10, "idle prompt after clearing draft");
    write_input(&attached.input, b"/exit\n");
    let result = wait_product(attached.process.take());
    let _ = attached.reader.join().expect("PTY output reader");
    assert!(result.status.success(), "attached exit after held draft");
    server.join().expect("synthetic Provider conversation");

    let (events, view) = replayed(&state, &workspace);
    assert_eq!(view.runs.len(), 1);
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(view.runs[0].steering_notes, [note.as_str()]);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::GoalSteeringNoteRecorded { .. }))
            .count(),
        1
    );
}

use super::loopback::{ChildGuard, read_request, send_response, wait_product, write_profile};
use super::session_picker::{checkpoint_screen, pump, screen_rows, tail, wait_for_screen};
use arany::{RunStatus, SessionId, SessionView, StateRoot, Store};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

#[test]
fn screen_reader_agent_inspector_opens_and_closes_without_provider_access() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let text = inspect_agents(&state, &workspace, None);
    assert!(text.contains("No Runs yet"));
    assert!(text.contains("History: 0 agents in 0 recent Runs; 0 older Runs"));
    assert!(text.contains("No AgentRuns yet"));
    assert!(text.contains("Next Run: single"));
    assert!(text.contains("Agent inspection closed"));
    assert!(!text.contains("\x1b[?1000h"));
}

#[tokio::test]
async fn editor_inspector_retains_observation_derived_paths_and_digests_after_cancel() {
    let observed = arany::test_support::worker_teams::run_case(
        std::path::Path::new(env!("CARGO_BIN_EXE_arany")),
        "command_cancel",
    )
    .await;
    let state = observed.directory.path().join("state");
    let workspace = observed.directory.path().join("workspace");
    arany::set_session_defaults(
        StateRoot::open_existing(&state).expect("fixture State"),
        workspace.clone(),
        observed.session,
        arany::SessionDefaults {
            provider: Some("openai".into()),
            ..observed.view.defaults.clone()
        },
    )
    .await
    .expect("inspector provider defaults");
    let (before, _) = arany::test_support::goals::replayed(&state, observed.session).await;
    let text = inspect_agents(&state, &workspace, Some(observed.session));
    let report = &observed.view.runs[0].worker_history()[0].changed_paths;
    assert!(text.contains("Changed paths (untrusted"));
    assert!(text.contains("notes/0"));
    let row: serde_json::Value = serde_json::from_str(report.lines().nth(1).unwrap()).unwrap();
    assert!(text.contains(row["after_sha256"].as_str().unwrap()));
    assert!(text.contains("Lease released: true; verified: false"));
    assert!(text.contains("Publication: Confirmed"));
    for removed in [
        "integration:",
        "discarded:",
        "Private outcome unknown:",
        "cleanup confirmed:",
    ] {
        assert!(!text.contains(removed), "retired worker field: {removed}");
    }
    let (events, _) = arany::test_support::goals::replayed(&state, observed.session).await;
    assert_eq!(events, before);
}

fn inspect_agents(
    state: &std::path::Path,
    workspace: &std::path::Path,
    session: Option<SessionId>,
) -> String {
    let mut command = inspector_command(state, workspace, session);
    let mut child = ChildGuard::new(command.spawn().expect("attached PTY process"));
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
    let (sender, receiver) = mpsc::channel();
    let mut consent_input =
        std::fs::File::from(rustix::io::dup(&input).expect("owned fixture input"));
    let reader = thread::spawn(move || {
        let mut declined = false;
        let mut bytes = Vec::new();
        let mut stages: Vec<&[u8]> = vec![b"Input:", b"Agent choice: n next"];
        if session.is_some() {
            stages.push(b"Agent choice: n next");
        }
        stages.push(b"Notice: Agent inspection closed");
        let mut next = 0;
        let mut consumed = 0;
        loop {
            let mut chunk = [0; 4096];
            let count = output.read(&mut chunk).expect("PTY output bytes");
            if count == 0 {
                break;
            }
            assert!(bytes.len() + count <= 32 * 1024, "bounded PTY output");
            bytes.extend_from_slice(&chunk[..count]);
            crate::process::decline_workspace_consent(&mut consent_input, &bytes, &mut declined);
            if next < stages.len()
                && let Some(offset) = bytes[consumed..]
                    .windows(stages[next].len())
                    .position(|part| part == stages[next])
            {
                consumed += offset + stages[next].len();
                sender.send(next).expect("stage receiver");
                next += 1;
            }
        }
        bytes
    });
    let stage = |expected| {
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|error| panic!("PTY stage {expected}: {error}")),
            expected
        );
    };
    stage(0);
    input.write_all(b"/agents\n").expect("open inspector");
    stage(1);
    if session.is_some() {
        input.write_all(b"n\n").expect("select the editor");
        stage(2);
    }
    input.write_all(b"q\n").expect("close inspector");
    stage(if session.is_some() { 3 } else { 2 });
    input.write_all(b"/exit\n").expect("exit attached mode");
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.child().try_wait().expect("product status") {
            assert!(status.success(), "attached process exited successfully");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "product exceeded parent deadline"
        );
        thread::yield_now();
    }
    let output = reader.join().expect("PTY output reader");
    let text = String::from_utf8_lossy(&output);
    text.into_owned()
}

fn inspector_command(
    state: &std::path::Path,
    workspace: &std::path::Path,
    session: Option<SessionId>,
) -> Command {
    let invocation = if session.is_some() {
        "exec \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\" --provider openai"
    } else {
        "exec \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider openai"
    };
    let mut command =
        crate::isolation::account_isolated_script(state.parent().expect("fixture root"));
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", state)
        .env("ARANY_TEST_WORKSPACE", workspace)
        .env(
            "ARANY_TEST_SESSION",
            session.map(|id| id.to_string()).unwrap_or_default(),
        )
        .env("TERM", "dumb")
        .current_dir(workspace)
        .args(["-q", "-e", "-c", invocation, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

#[test]
fn agent_inspector_preserves_progress_and_transient_mouse_selection() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    std::fs::write(workspace.join("private.txt"), b"OMITTED_CANARY")
        .expect("omitted Workspace input");
    let state = temp.path().join("state");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback endpoint");
    let port = listener.local_addr().expect("listener address").port();
    write_profile(&state, port);

    let (active_request, request_ready) = mpsc::channel();
    let (release_run, released) = mpsc::channel();
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).expect("bounded accept");
        for index in 3..5 {
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
            let body = read_request(&mut stream);
            assert!(
                !body
                    .windows(b"OMITTED_CANARY".len())
                    .any(|part| part == b"OMITTED_CANARY")
            );
            let wire: serde_json::Value = serde_json::from_slice(&body).expect("Responses request");
            assert_eq!(wire["model"], "model-1");
            assert!(wire.get("text").is_none());
            assert_eq!(wire["max_output_tokens"], 4096);
            let input: serde_json::Value = crate::semantic::input(&wire);
            assert!(!input.to_string().contains("/agents"));
            match index {
                3 => assert_eq!(input["objective"], "prior"),
                _ => {
                    assert_eq!(input["objective"], "current");
                    active_request.send(()).expect("active request gate");
                    released
                        .recv_timeout(Duration::from_secs(10))
                        .expect("release active response");
                }
            }
            let text = match index {
                3 => {
                    serde_json::json!("prior answer")
                }
                _ => {
                    serde_json::json!("current answer")
                }
            };
            send_response(&mut stream, index, text);
        }
    });

    let mut first = Command::new(env!("CARGO_BIN_EXE_arany"));
    first
        .env_clear()
        .envs(crate::process::account_fixture(
            state.with_file_name("account"),
        ))
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .current_dir(&workspace)
        .args(["exec", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args(["--provider", "custom:local", "--model", "model-1", "prior"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let first = wait_product(first.spawn().expect("prior Run process"));
    assert!(first.status.success(), "prior Run finished");
    assert_eq!(first.stdout, b"Answer:\n  prior answer\n");
    let first_receipt = String::from_utf8(first.stderr).expect("prior receipt");
    let session_id = first_receipt
        .lines()
        .filter_map(|line| line.strip_prefix("Session: "))
        .find_map(|id| id.parse::<SessionId>().ok())
        .expect("Session ID");

    opt_into_delegation(&state, &workspace, session_id);
    let mut attached =
        crate::isolation::account_isolated_script(state.parent().expect("fixture root"));
    attached
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_SESSION", session_id.to_string())
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .env("TERM", "dumb")
        .current_dir(&workspace)
        .args([
            "-q",
            "-e",
            "-c",
            "exec \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\" --provider custom:local --model model-1 current",
            "/dev/null",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut attached = ChildGuard::new(attached.spawn().expect("attached PTY process"));
    let mut input = attached.child().stdin.take().expect("PTY input");
    let mut output = attached.child().stdout.take().expect("PTY output");
    let (stage_sender, stages) = mpsc::channel();
    let mut consent_input =
        std::fs::File::from(rustix::io::dup(&input).expect("owned fixture input"));
    let reader = thread::spawn(move || {
        let mut declined = false;
        let mut bytes = Vec::new();
        let patterns: [&[u8]; 8] = [
            b"Agent: primary; state: active",
            b"Agent choice: n next",
            b"Agent 2/2: primary; finished",
            // The reprint rearms, so the next choice waits for its prompt.
            b"Agent choice: n next",
            b"Notice: Run is active; Ctrl+C cancels",
            b"Agent choice: n next",
            b"z",
            b"Status: finished",
        ];
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
            crate::process::decline_workspace_consent(&mut consent_input, &bytes, &mut declined);
            while next < patterns.len() {
                let Some(offset) = bytes[search_from..]
                    .windows(patterns[next].len())
                    .position(|part| part == patterns[next])
                else {
                    break;
                };
                search_from += offset + patterns[next].len();
                stage_sender.send(next).expect("stage receiver");
                next += 1;
            }
        }
        bytes
    });
    let stage = |expected| {
        assert_eq!(
            stages
                .recv_timeout(Duration::from_secs(10))
                .expect("PTY stage"),
            expected
        );
    };
    request_ready
        .recv_timeout(Duration::from_secs(10))
        .expect("in-flight Provider request");
    stage(0);
    input
        .write_all(b"/agents\n")
        .expect("open active inspector");
    stage(1);
    input.write_all(b"n\n").expect("select recent AgentRun");
    stage(2);
    stage(3);
    input.write_all(b"q\n").expect("close active inspector");
    stage(4);
    input
        .write_all(b"/agents\n")
        .expect("reopen active inspector");
    stage(5);
    input.write_all(b"z").expect("partial inspector choice");
    stage(6);
    release_run.send(()).expect("release active response");
    stage(7);
    input.write_all(b"/exit\n").expect("exit attached mode");
    drop(input);
    let result = wait_product(attached.take());
    let output = reader.join().expect("PTY output reader");
    server.join().expect("synthetic server completed");
    assert!(
        result.status.success(),
        "attached process exited successfully"
    );
    assert_eq!(result.stderr, b"");
    let transcript = String::from_utf8(output).expect("linear transcript UTF-8");
    assert!(transcript.contains("Objective: prior"));
    assert!(transcript.contains("Current Run: auto · up to 3 children; topology locked"));
    assert!(transcript.contains("Arany · Answer:\r\n  current answer"));
    assert!(!transcript.contains("OMITTED_CANARY"));
    assert!(!transcript.contains("test-key"));
    assert!(!transcript.contains("\x1b[?1000h"));

    inline_inspector(&workspace, &state, session_id, 80);
    inline_inspector(&workspace, &state, session_id, 16);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only replay runtime");
    let store = Store::open_read_only(StateRoot::open_existing(&state).expect("state reopen"))
        .expect("read-only Store");
    let events = runtime
        .block_on(store.load_session(session_id))
        .expect("Events");
    let view = SessionView::replay(session_id, &events)
        .expect("strict replay")
        .expect("Session");
    assert_eq!(view.runs.len(), 2);
    assert_eq!(view.runs[0].objective, "prior");
    assert_eq!(view.runs[1].objective, "current");
    assert_eq!(view.runs[1].status, RunStatus::Finished);
    assert_eq!(view.runs[1].agents.len(), 1);
    assert_eq!(
        view.runs[1].assistant_message.as_deref(),
        Some("current answer")
    );
    runtime.block_on(store.close()).expect("close Store");
}

fn inline_inspector(
    workspace: &std::path::Path,
    state: &std::path::Path,
    id: SessionId,
    width: u16,
) {
    let no_color = if width < 20 { "--no-color " } else { "" };
    let command = format!(
        "stty rows 24 cols {width}; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" --classic {no_color}--state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
    );
    let mut command_line =
        crate::isolation::account_isolated_script(state.parent().expect("fixture root"));
    command_line
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", state)
        .env("ARANY_TEST_WORKSPACE", workspace)
        .env("ARANY_TEST_SESSION", id.to_string())
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(workspace)
        .args(["-q", "-e", "-c", &command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut attached = ChildGuard::new(command_line.spawn().expect("inline inspector process"));
    let mut input = attached.child().stdin.take().expect("PTY input");
    let mut output = attached.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
    let mut transcript = Vec::new();
    let mut history = Vec::new();
    let mut capture_from = 0;
    let mut answered = crate::session_picker::PtyResponses::default();
    wait_for_screen(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        (0, width.into(), 24),
        "Ask Arany",
    );
    capture_from = checkpoint_screen(
        &mut transcript,
        &mut answered,
        &mut history,
        (width.into(), 24),
        capture_from,
    );
    let opened_at = capture_from;
    input.write_all(b"/agents\r").expect("open inspector");
    wait_for_screen(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        (opened_at, width.into(), 24),
        // The newest Run's details name its objective; 16 columns keep this prefix.
        "Objective: curr",
    );
    // The inspector takes keys only once its armed frame is drawn.
    let deadline = Instant::now() + Duration::from_secs(10);
    while crate::armed_selector(&transcript, opened_at, width.into(), 24).is_none() {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "armed inspector missing: {}",
            tail(&transcript[opened_at..])
        );
        thread::yield_now();
    }
    capture_from = checkpoint_screen(
        &mut transcript,
        &mut answered,
        &mut history,
        (width.into(), 24),
        capture_from,
    );
    let checkpoint = capture_from;
    input.write_all(b"\x1b[B").expect("select prior AgentRun");
    wait_for_screen(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        (checkpoint, width.into(), 24),
        "Objective: prior",
    );
    capture_from = checkpoint_screen(
        &mut transcript,
        &mut answered,
        &mut history,
        (width.into(), 24),
        capture_from,
    );
    let checkpoint = capture_from;
    input.write_all(b"\x1b").expect("close inspector");
    wait_for_screen(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        (checkpoint, width.into(), 24),
        "Agent ins",
    );
    capture_from = checkpoint_screen(
        &mut transcript,
        &mut answered,
        &mut history,
        (width.into(), 24),
        capture_from,
    );
    input.write_all(b"/exit\r").expect("exit attached mode");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        if attached
            .child()
            .try_wait()
            .expect("inspector status")
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "inline inspector did not exit: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    let result = wait_product(attached.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(result.status.success(), "inline inspector exited cleanly");
    assert_eq!(result.stderr, b"");
    checkpoint_screen(
        &mut transcript,
        &mut answered,
        &mut history,
        (width.into(), 24),
        capture_from,
    );
    let transcript = String::from_utf8(history).expect("inline transcript UTF-8");
    let before = transcript
        .lines()
        .find_map(|line| line.strip_prefix("TTY_BEFORE:"))
        .expect("initial terminal settings")
        .trim_end_matches('\r');
    let after = transcript
        .split_once("TTY_AFTER:")
        .expect("restored terminal marker")
        .1
        .lines()
        .next()
        .expect("restored terminal settings")
        .trim_end_matches('\r');
    assert_eq!(before, after, "terminal restored after inspector");
    assert!(!transcript.contains("\x1b[?1000h"));
    assert!(!transcript.contains("\x1b[?1049h"));
    assert!(!transcript.contains("OMITTED_CANARY"));
    assert!(!transcript.contains("test-key"));
}

fn replayed(state: &std::path::Path, id: SessionId) -> (Vec<arany::EventEnvelope>, SessionView) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only replay runtime");
    let store = Store::open_read_only(StateRoot::open_existing(state).expect("state reopen"))
        .expect("read-only Store");
    let events = runtime.block_on(store.load_session(id)).expect("Events");
    let view = SessionView::replay(id, &events)
        .expect("strict replay")
        .expect("Session");
    runtime.block_on(store.close()).expect("close Store");
    (events, view)
}

// New conversations default to single; the resumed Run opts into delegation like `/agents auto`.
fn opt_into_delegation(state: &std::path::Path, workspace: &std::path::Path, id: SessionId) {
    let (_, view) = replayed(state, id);
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("defaults runtime")
        .block_on(arany::set_session_defaults(
            StateRoot::open_existing(state).expect("fixture State"),
            workspace.to_path_buf(),
            id,
            arany::SessionDefaults {
                policy: arany::CollaborationPolicy::Auto {
                    max_active_children: 3,
                },
                ..view.defaults
            },
        ))
        .expect("delegation opt-in");
}

#[test]
fn agent_chats_switch_to_each_child_and_back_without_changing_history() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback endpoint");
    write_profile(
        &state,
        listener.local_addr().expect("listener address").port(),
    );
    // A Run whose primary delegates to two children.
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).expect("bounded accept");
        for index in 0..4 {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "request {index} missing");
                        thread::yield_now();
                    }
                    Err(error) => panic!("accept failed: {error}"),
                }
            };
            let wire: serde_json::Value =
                serde_json::from_slice(&read_request(&mut stream)).expect("Responses request");
            let input = crate::semantic::input(&wire);
            let text = match input["phase"].as_str() {
                Some("root_plan") => {
                    serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"first child task","profile":null},{"objective":"second child task","profile":null}]}})
                }
                Some("child_work") => serde_json::json!(format!(
                    "result of {}\n\n> - nested **result**\n>\n>   ~~~\n>   nested code\n>   ~~~",
                    input["objective"].as_str().expect("child objective")
                )),
                _ => serde_json::json!("team answer"),
            };
            send_response(&mut stream, index, text);
        }
    });
    let mut team = Command::new(env!("CARGO_BIN_EXE_arany"));
    team.env_clear()
        .envs(crate::process::account_fixture(
            state.with_file_name("account"),
        ))
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
            "auto",
            "team task",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let team = wait_product(team.spawn().expect("team Run process"));
    server.join().expect("synthetic server completed");
    assert!(team.status.success(), "team Run finished");
    assert_eq!(team.stdout, b"Answer:\n  team answer\n");
    let session_id = String::from_utf8(team.stderr)
        .expect("team receipt")
        .lines()
        .filter_map(|line| line.strip_prefix("Session: "))
        .find_map(|id| id.parse::<SessionId>().ok())
        .expect("Session ID");
    let (events, before) = replayed(&state, session_id);
    assert_eq!(before.runs[0].agents.len(), 3);

    for classic in [false, true] {
        agent_chat_journey(&workspace, &state, session_id, classic);
    }
    linear_agent_chats(&workspace, &state, session_id);

    // Only resume's inherited defaults may follow; viewing adds no Run, note or child Event.
    let (after_events, after) = replayed(&state, session_id);
    assert_eq!(after_events[..events.len()], events[..]);
    assert!(
        after_events[events.len()..].iter().all(|envelope| matches!(
            envelope.event,
            arany::Event::SessionDefaultChanged { .. }
        ) && envelope.run_id.is_none()
            && envelope.agent_run_id.is_none()),
        "{:?}",
        &after_events[events.len()..]
    );
    assert_eq!(after.runs, before.runs);
    assert_eq!(after.goals, before.goals);
}

fn agent_chat_journey(
    workspace: &std::path::Path,
    state: &std::path::Path,
    id: SessionId,
    classic: bool,
) {
    let size = (80, 24);
    let mode = if classic { "--classic " } else { "" };
    let command = format!(
        "stty rows 24 cols 80; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" {mode}--state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
    );
    let mut command_line =
        crate::isolation::account_isolated_script(state.parent().expect("fixture root"));
    command_line
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", state)
        .env("ARANY_TEST_WORKSPACE", workspace)
        .env("ARANY_TEST_SESSION", id.to_string())
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(workspace)
        .args(["-q", "-e", "-c", &command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut attached = ChildGuard::new(command_line.spawn().expect("agent chat process"));
    let mut input = attached.child().stdin.take().expect("PTY input");
    let mut output = attached.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
    let mut transcript = Vec::new();
    let mut history = Vec::new();
    let mut answered = crate::session_picker::PtyResponses::default();
    let mut capture_from = 0;
    let context = if classic { "Classic" } else { "Fullscreen" };
    // Decoding stops at the last cursor hide or show that ends a Ratatui frame, so a partial one never releases keys.
    let mut step = |keys: &[u8], needles: &[&str], absent: &[&str]| {
        capture_from = checkpoint_screen(
            &mut transcript,
            &mut answered,
            &mut history,
            (size.0, size.1),
            capture_from,
        );
        let from = capture_from;
        input.write_all(keys).expect("PTY keys");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            let frame_end = transcript[from..]
                .windows(6)
                .rposition(|part| part == b"\x1b[?25l" || part == b"\x1b[?25h")
                .map(|offset| from + offset + 6);
            let screen =
                frame_end.map(|end| screen_rows(&transcript[..end], size.0, size.1).join("\n"));
            if let Some(screen) = screen.as_ref().filter(|screen| {
                needles.iter().all(|needle| {
                    crate::process::pty_text_matches(screen.as_bytes(), needle.as_bytes())
                        .next()
                        .is_some()
                }) && !absent.iter().any(|text| {
                    crate::process::pty_text_matches(screen.as_bytes(), text.as_bytes())
                        .next()
                        .is_some()
                })
            }) {
                break screen.clone();
            }
            assert!(
                Instant::now() < deadline,
                "{context} missing {needles:?} or showing {absent:?}: {screen:?} {}",
                tail(&transcript[from..])
            );
            thread::yield_now();
        }
    };
    // The composer title marks the primary chat; only an armed child view shows its key hint.
    let primary = "Ask Arany";
    let child = "viewing child";
    let armed = "agents · Esc primary";
    step(b"", &[primary, "Alt+Left/Right agents"], &[child]);
    // A draft keeps Alt+Right with the composer; Backspace then empties it.
    step(b"x", &["> x"], &[child]);
    step(
        b"\x1b[1;3C\x7f",
        &[primary, "Alt+Left/Right agents"],
        &[child, "> x"],
    );
    step(
        b"\x1b[1;3C",
        &[
            "viewing child 1 (read-only)",
            armed,
            "Assignment:",
            "first child task",
            "result of first child task",
            "│ • nested result",
            "│   nested code",
        ],
        &[primary],
    );
    step(
        b"\x1b[1;3C",
        &[
            "viewing child 2 (read-only)",
            armed,
            "result of second child task",
        ],
        &[primary, "first child task"],
    );
    // The ring wraps from the last child to the primary, and backwards to the last child.
    step(b"\x1b[1;3C", &[primary], &[child]);
    step(
        b"\x1b[1;3D",
        &["viewing child 2 (read-only)", armed],
        &[primary],
    );
    // Escape returns without cancelling anything; Classic marks the boundary in scrollback.
    let returned: &[&str] = if classic {
        &[primary, "Back to primary"]
    } else {
        &[primary]
    };
    step(b"\x1b", returned, &[child]);
    // `/agents` searches locally; no match activates nothing, and Enter opens the focused child.
    let picker = "Enter open · Esc close";
    step(
        b"/agents\r",
        &[picker, "Type to search 3 agents", "› 1. Run 1: primary"],
        &[child],
    );
    step(
        b"zz",
        &[
            picker,
            "Search: zz · no matching agents",
            "No matching agents",
        ],
        &["› 1."],
    );
    step(
        b"\r\x7f\x7f",
        &[picker, "Type to search 3 agents"],
        &[child],
    );
    step(
        b"second",
        &[
            "Search: second · 1 of 3 agents",
            "› 1. Run 1: child 2; finished · second child task",
        ],
        &[child],
    );
    step(
        b"\r",
        &[
            "viewing child 2 (read-only)",
            armed,
            "result of second child task",
        ],
        &[picker, primary],
    );
    step(b"\x1b", returned, &[child]);
    step(b"/agents\r", &[picker, "› 1. Run 1: primary"], &[child]);
    let screen = step(
        b"\rqueued-submit\rqueued-draft",
        &[primary, "Alt+Left/Right agents"],
        &[picker, child, "queued-submit", "queued-draft"],
    );
    let composer = screen.lines().find(|row| row.starts_with("> "));
    assert_eq!(
        composer.map(str::trim_end),
        Some("> Ask a question, or type / for commands"),
        "{screen}"
    );
    step(b"x", &["> x"], &["queued-submit", "queued-draft"]);
    step(b"\x7f", &["Alt+Left/Right agents"], &["> x"]);
    let from = checkpoint_screen(
        &mut transcript,
        &mut answered,
        &mut history,
        (size.0, size.1),
        capture_from,
    );
    input.write_all(b"/exit\r").expect("exit attached mode");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        if attached.child().try_wait().expect("chat status").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{context} chat did not exit: {}",
            tail(&transcript[from..])
        );
        thread::yield_now();
    }
    let result = wait_product(attached.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(result.status.success(), "{context} chat exited cleanly");
    history.extend_from_slice(&transcript[from..]);
    let text = String::from_utf8_lossy(&history);
    let setting = |marker: &str| {
        text.split_once(marker)
            .and_then(|(_, rest)| rest.lines().next())
            .map(|line| line.trim_end_matches('\r').to_owned())
            .unwrap_or_else(|| panic!("{context} {marker} missing"))
    };
    assert_eq!(
        setting("TTY_BEFORE:"),
        setting("TTY_AFTER:"),
        "{context} terminal restored"
    );
    assert!(!text.contains("test-key"));
    if classic {
        assert!(!text.contains("\x1b[?1000h") && !text.contains("\x1b[?1049h"));
        // Switching appended each child once and never repeated the primary's settled answer.
        let (scrollback, screen) = crate::terminal_grid(&history, size.0, size.1);
        let rows = scrollback.iter().chain(&screen).collect::<Vec<_>>();
        assert_eq!(
            rows.iter()
                .filter(|row| row.contains("team answer"))
                .count(),
            1,
            "{rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("Viewing child 1 (read-only)"))
        );
    }
}

fn linear_agent_chats(workspace: &std::path::Path, state: &std::path::Path, id: SessionId) {
    let mut command =
        crate::isolation::account_isolated_script(state.parent().expect("fixture root"));
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", state)
        .env("ARANY_TEST_WORKSPACE", workspace)
        .env("ARANY_TEST_SESSION", id.to_string())
        .env("TERM", "dumb")
        .current_dir(workspace)
        .args([
            "-q",
            "-e",
            "-c",
            "exec \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\"",
            "/dev/null",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("linear chat process"));
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
    let (sender, receiver) = mpsc::channel();
    let mut consent_input =
        std::fs::File::from(rustix::io::dup(&input).expect("owned fixture input"));
    let reader = thread::spawn(move || {
        let mut declined = false;
        let mut bytes = Vec::new();
        let prompt: &[u8] = b"Agent chat: number opens";
        let picker: &[u8] = b"Agent choice: n next";
        let stages: [&[u8]; 24] = [
            b"Input:",
            b"second child task\r\nAgent chat: number opens",
            b"Viewing child 2 (read-only)",
            prompt,
            b"Notice: Invalid agent choice; type a number, n, p or q",
            prompt,
            b"Viewing primary; its conversation is above",
            prompt,
            b"Viewing child 1 (read-only)",
            prompt,
            b"Back to primary",
            // The primary prompt follows the return barrier, so later input reaches the composer.
            b"Input:",
            b"Type to search 3 agents",
            picker,
            b"Search: zz \xc2\xb7 no matching agents",
            picker,
            b"Notice: No agent focused; choose a number first",
            picker,
            b"1. Run 1: child 2; finished \xc2\xb7 second child task (focused)",
            picker,
            b"Viewing child 2 (read-only)",
            prompt,
            b"Back to primary",
            b"Input:",
        ];
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
            crate::process::decline_workspace_consent(&mut consent_input, &bytes, &mut declined);
            while next < stages.len() {
                let Some(offset) = bytes[search_from..]
                    .windows(stages[next].len())
                    .position(|part| part == stages[next])
                else {
                    break;
                };
                search_from += offset + stages[next].len();
                sender.send(next).expect("stage receiver");
                next += 1;
            }
        }
        bytes
    });
    let stage = |expected| {
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(10))
                .expect("linear agent chat stage"),
            expected
        );
    };
    stage(0);
    input
        .write_all(b"/transcript agents\n")
        .expect("list agent chats");
    stage(1);
    // Every choice waits for the prompt that follows its armed view.
    for (keys, stages) in [
        (b"3\n".as_slice(), [2, 3]),
        (b"hello\n".as_slice(), [4, 5]),
        (b"n\n".as_slice(), [6, 7]),
        (b"n\n".as_slice(), [8, 9]),
    ] {
        input.write_all(keys).expect("agent chat choice");
        stages.into_iter().for_each(stage);
    }
    input.write_all(b"q\n").expect("return to primary");
    stage(10);
    stage(11);
    // Every picker choice waits for the prompt that follows its rearmed reprint.
    for (keys, stages) in [
        (b"/agents\n".as_slice(), [12, 13]),
        (b"/zz\n".as_slice(), [14, 15]),
        (b"o\n".as_slice(), [16, 17]),
        (b"/second\n".as_slice(), [18, 19]),
        (b"o\n".as_slice(), [20, 21]),
    ] {
        input.write_all(keys).expect("agent picker choice");
        stages.into_iter().for_each(stage);
    }
    input.write_all(b"q\n").expect("return to primary again");
    stage(22);
    stage(23);
    input.write_all(b"/exit\n").expect("exit linear chat");
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.child().try_wait().expect("linear status") {
            assert!(status.success(), "linear chat exited successfully");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "linear chat exceeded parent deadline"
        );
        thread::yield_now();
    }
    let text = String::from_utf8_lossy(&reader.join().expect("linear output reader")).into_owned();
    assert!(
        text.contains("Agent result:\r\n  result of first child task"),
        "{text}"
    );
    assert!(text.contains("  > - nested **result**\r\n"), "{text}");
    assert!(text.contains("  >   nested code\r\n"), "{text}");
    assert!(
        text.contains("Viewing primary; its conversation is above"),
        "{text}"
    );
    assert_eq!(
        text.matches("Agent chats in this Run:").count(),
        1,
        "{text}"
    );
    assert!(
        !text.contains("\x1b["),
        "linear output has no cursor rewriting"
    );
}

// Conformance, one prior Run, then per journey a team Run whose child calls and synthesis wait
// until the test releases them; custom routes call one child at a time, in spawn order.
fn gated_team_server(
    listener: TcpListener,
    journeys: usize,
    messaging: bool,
    requested: mpsc::Sender<&'static str>,
    release: mpsc::Receiver<()>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        listener.set_nonblocking(true).expect("bounded accept");
        for index in 0..1 + journeys * 4 {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "request {index} missing");
                        thread::yield_now();
                    }
                    Err(error) => panic!("accept failed: {error}"),
                }
            };
            let wire: serde_json::Value =
                serde_json::from_slice(&read_request(&mut stream)).expect("Responses request");
            let input = crate::semantic::input(&wire);
            let text = {
                let mut current = input.clone();
                current["history"] = serde_json::Value::Null;
                current.to_string()
            };
            let targeted = messaging
                && input["phase"] == "child_work"
                && input["objective"] == "second child task";
            assert_eq!(
                text.contains("/literal guidance"),
                targeted,
                "only the selected child's request receives its note"
            );
            let whole = input.to_string();
            assert!(!whole.contains("late note") && !whole.contains("queued note"));

            let gate = |name| {
                requested.send(name).expect("request observer");
                release
                    .recv_timeout(Duration::from_secs(20))
                    .expect("released response");
            };
            let text = match (index, input["phase"].as_str()) {
                (0, _) => serde_json::json!("prior answer"),
                (_, Some("root_plan")) => {
                    serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"first child task","profile":null},{"objective":"second child task","profile":null}]}})
                }
                (_, Some("child_work")) => {
                    let objective = input["objective"].as_str().expect("child objective");
                    gate(if objective == "first child task" {
                        "first"
                    } else {
                        "second"
                    });
                    serde_json::json!(format!("result of {objective}"))
                }
                _ => {
                    gate("synthesis");
                    serde_json::json!("team answer")
                }
            };
            send_response(&mut stream, index, text);
        }
    })
}

#[test]
fn agent_chats_follow_a_running_child_from_committed_events_only() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback endpoint");
    write_profile(
        &state,
        listener.local_addr().expect("listener address").port(),
    );
    let (requests, requested) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let server = gated_team_server(listener, 5, false, requests, released);
    let mut prior = Command::new(env!("CARGO_BIN_EXE_arany"));
    prior
        .env_clear()
        .envs(crate::process::account_fixture(
            state.with_file_name("account"),
        ))
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .current_dir(&workspace)
        .args(["exec", "--state-dir"])
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args(["--provider", "custom:local", "--model", "model-1", "prior"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let prior = wait_product(prior.spawn().expect("prior Run process"));
    assert!(prior.status.success(), "prior Run finished");
    let session_id = String::from_utf8(prior.stderr)
        .expect("prior receipt")
        .lines()
        .filter_map(|line| line.strip_prefix("Session: "))
        .find_map(|id| id.parse::<SessionId>().ok())
        .expect("Session ID");
    let gates = Gates { requested, release };
    for classic in [false, true] {
        live_agent_chat_journey(&workspace, &state, session_id, classic, &gates);
    }
    for classic in [false, true] {
        live_agent_picker_journey(&workspace, &state, session_id, classic, &gates);
    }
    live_linear_agent_chat(&workspace, &state, session_id, &gates, false);
    server.join().expect("synthetic server completed");

    // Following a child only read committed facts: each team Run closed normally, with no note.
    let (_, view) = replayed(&state, session_id);
    assert_eq!(view.runs.len(), 6);
    for run in &view.runs[1..] {
        assert_eq!(run.status, RunStatus::Finished);
        assert_eq!(run.assistant_message.as_deref(), Some("team answer"));
        assert_eq!(run.agents.len(), 3);
        assert!(run.steering_notes.is_empty());
    }
}

#[test]
fn exit_is_refused_while_a_run_is_active_and_succeeds_once_it_ends() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace directory");
    let state = temp.path().join("state");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    write_profile(
        &state,
        listener.local_addr().expect("listener address").port(),
    );
    let (requests, requested) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let server = gated_team_server(listener, 1, false, requests, released);
    let prior = wait_product(
        Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .envs(crate::process::account_fixture(
                state.with_file_name("account"),
            ))
            .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
            .current_dir(&workspace)
            .args(["exec", "--state-dir"])
            .arg(&state)
            .arg("--workspace")
            .arg(&workspace)
            .args(["--provider", "custom:local", "--model", "model-1", "prior"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the prior Run"),
    );
    assert!(prior.status.success());
    let id = String::from_utf8(prior.stderr)
        .expect("prior stderr is UTF-8")
        .lines()
        .find_map(|line| {
            line.strip_prefix("Session: ")
                .and_then(|id| id.parse::<SessionId>().ok())
        })
        .expect("prior Run reports its Session");
    let gates = Gates { requested, release };
    let (mut attached, mut pty) = live_pty(&workspace, &state, id, false);
    pty.await_request(&gates, "first");
    pty.frame(0, &["Alt+Left/Right agents"], &[]);
    gates.release.send(()).expect("release the first child");
    pty.await_request(&gates, "second");
    gates.release.send(()).expect("release the second child");
    pty.await_request(&gates, "synthesis");
    // The synthesis is held, so the Run is active: `/exit` is refused until it is released.
    let release = gates.release.clone();
    exit_live_after_refusal(&mut attached, &mut pty, move || {
        release.send(()).expect("release the synthesis");
    });
    server.join().expect("server thread finishes");
}

#[test]
fn selected_child_messaging_journey() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace directory");
    let state = temp.path().join("state");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    write_profile(
        &state,
        listener.local_addr().expect("listener address").port(),
    );
    let (requests, requested) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let server = gated_team_server(listener, 3, true, requests, released);
    let prior = wait_product(
        Command::new(env!("CARGO_BIN_EXE_arany"))
            .env_clear()
            .envs(crate::process::account_fixture(
                state.with_file_name("account"),
            ))
            .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
            .current_dir(&workspace)
            .args(["exec", "--state-dir"])
            .arg(&state)
            .arg("--workspace")
            .arg(&workspace)
            .args(["--provider", "custom:local", "--model", "model-1", "prior"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the prior Run"),
    );
    assert!(prior.status.success());
    let id = String::from_utf8(prior.stderr)
        .expect("prior stderr is UTF-8")
        .lines()
        .find_map(|line| {
            line.strip_prefix("Session: ")
                .and_then(|id| id.parse::<SessionId>().ok())
        })
        .expect("prior Run reports its Session");
    let gates = Gates { requested, release };
    for classic in [false, true] {
        let (mut attached, mut pty) = live_pty(&workspace, &state, id, classic);
        let total = if classic { 7 } else { 4 };
        pty.await_request(&gates, "first");
        pty.frame(0, &["Alt+Left/Right agents", "child 2"], &[]);
        pty.step(
            b"/agents\r",
            &[&format!("Type to search {total} agents"), "Enter open"],
            &[],
        );
        pty.step(
            b"second",
            &["Search: second", &format!("of {total} agents"), "child 2"],
            &[],
        );
        pty.step(
            b"\rqueued note\r",
            &["viewing child 2 (message)", "agents · Esc primary"],
            &["queued note"],
        );
        pty.step(b"/literal guidance", &["> /literal guidance"], &[]);
        pty.step(b"\r", &["Note waiting for child 2's next step"], &[]);
        pty.step(b"\x1b", &["Ask Arany"], &["viewing child"]);
        pty.step(
            b"\x1b[1;3C",
            &["viewing child 1 (message)", "agents · Esc primary"],
            &[],
        );
        pty.step(b"late note", &["> late note"], &[]);
        pty.step(b"\r", &["Note waiting for child 1's next step"], &[]);
        pty.step(b"\x1b", &["Ask Arany"], &["viewing child"]);
        pty.step(
            b"/transcript agents 3\r",
            &[
                "viewing child 2 (message)",
                "/literal guidance",
                "Note waiting for child 2's next step",
            ],
            &[],
        );
        let from = pty.capture_from;
        gates.release.send(()).expect("server accepts the release");
        pty.await_request(&gates, "second");
        pty.frame(
            from,
            &[
                "Steering note recorded for child 2",
                "viewing child 2 (message)",
            ],
            &[],
        );
        pty.step(b"draft", &["> draft"], &[]);
        pty.step(b"\x1b", &["Ask Arany"], &["viewing child"]);
        pty.step(
            b"/transcript agents 2\r",
            &[
                "viewing child 1 (read-only)",
                "> late note",
                "Note not delivered; child draft retained",
            ],
            &[],
        );
        pty.step(b"\r", &["This child is read-only; draft retained"], &[]);
        pty.step(b"\x7f", &["Read-only"], &["> late note"]);
        pty.step(
            b"\x1b[1;3C",
            &[
                "viewing child 2 (message)",
                "> draft",
                "agents · Esc primary",
            ],
            &[],
        );
        gates.release.send(()).expect("server accepts the release");
        pty.await_request(&gates, "synthesis");
        pty.frame(
            pty.capture_from,
            &["viewing child 2 (read-only)", "> draft"],
            &[],
        );
        gates.release.send(()).expect("server accepts the release");
        pty.step(b"\x1b", &["team answer", "Ask Arany"], &["viewing child"]);
        exit_live(&mut attached, &mut pty);
    }
    live_linear_agent_chat(&workspace, &state, id, &gates, true);
    server.join().expect("server thread finishes");
    let (events, view) = replayed(&state, id);
    assert_eq!(view.runs.len(), 4);
    assert_eq!(view.goals.len(), 4);
    let notes: Vec<_> = events
        .iter()
        .filter(|event| matches!(event.event, arany::Event::AgentSteeringNoteRecorded { .. }))
        .collect();
    assert_eq!(notes.len(), 3);
    for run in &view.runs[1..] {
        assert_eq!(run.status, RunStatus::Finished);
        assert_eq!(run.agents.len(), 3);
        assert!(run.steering_notes.is_empty());
        assert!(run.agents[1].steering_notes.is_empty());
        assert_eq!(run.agents[2].steering_notes, ["/literal guidance"]);
        assert_eq!(run.agents[2].grant, Some(arany::ChildGrant::NoTools));
        assert!(!events.iter().any(|event| event.run_id == Some(run.id)
            && matches!(event.event, arany::Event::ToolStarted { .. })));
    }
}

struct Gates {
    requested: mpsc::Receiver<&'static str>,
    release: mpsc::Sender<()>,
}

const LIVE_SIZE: (usize, usize) = (80, 24);

// Decode at cursor markers; Classic resizing also emits them before the replacement frame.
struct LivePty {
    input: std::process::ChildStdin,
    output: std::process::ChildStdout,
    transcript: Vec<u8>,
    history: Vec<u8>,
    answered: crate::session_picker::PtyResponses,
    capture_from: usize,
    context: &'static str,
}

impl LivePty {
    fn pump(&mut self) {
        pump(
            &mut self.output,
            &mut self.input,
            &mut self.transcript,
            &mut self.answered,
        );
    }

    fn frame_ends(&self, from: usize) -> Vec<usize> {
        self.transcript[from..]
            .windows(6)
            .enumerate()
            .filter(|(_, part)| *part == b"\x1b[?25l" || *part == b"\x1b[?25h")
            .map(|(offset, _)| from + offset + 6)
            .collect()
    }

    fn screen(&self, end: usize) -> String {
        screen_rows(&self.transcript[..end], LIVE_SIZE.0, LIVE_SIZE.1).join("\n")
    }

    // After a view switch, name the armed hint row, or keys sent next can be dropped.
    fn frame(&mut self, from: usize, needles: &[&str], absent: &[&str]) -> usize {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.pump();
            let end = self.frame_ends(from).last().copied();
            if let Some(end) = end {
                let screen = self.screen(end);
                if needles.iter().all(|needle| {
                    crate::process::pty_text_matches(screen.as_bytes(), needle.as_bytes())
                        .next()
                        .is_some()
                }) && !absent.iter().any(|text| {
                    crate::process::pty_text_matches(screen.as_bytes(), text.as_bytes())
                        .next()
                        .is_some()
                }) {
                    return end;
                }
            }
            assert!(
                Instant::now() < deadline,
                "{} missing {needles:?} or showing {absent:?}: {:?} {}",
                self.context,
                end.map(|end| self.screen(end)),
                tail(&self.transcript[from..])
            );
            thread::yield_now();
        }
    }

    fn step(&mut self, keys: &[u8], needles: &[&str], absent: &[&str]) -> usize {
        self.capture_from = checkpoint_screen(
            &mut self.transcript,
            &mut self.answered,
            &mut self.history,
            LIVE_SIZE,
            self.capture_from,
        );
        let from = self.capture_from;
        self.input.write_all(keys).expect("PTY keys");
        self.frame(from, needles, absent)
    }

    fn await_request(&mut self, gates: &Gates, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.pump();
            match gates.requested.try_recv() {
                Ok(name) => {
                    assert_eq!(name, expected, "{}", self.context);
                    return;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => panic!("synthetic server stopped"),
            }
            assert!(
                Instant::now() < deadline,
                "{} awaited the {expected} request",
                self.context
            );
            thread::yield_now();
        }
    }

    // The decoded screen of every complete frame after `start`; none repeats its predecessor,
    // since only a changed displayed fact may redraw.
    fn frames_since(&self, start: usize) -> Vec<String> {
        let mut previous = self.screen(start);
        self.frame_ends(start)
            .into_iter()
            .map(|end| {
                let screen = self.screen(end);
                assert_ne!(
                    screen, previous,
                    "{} redrew an unchanged view",
                    self.context
                );
                previous.clone_from(&screen);
                screen
            })
            .collect()
    }
}

// A Classic or Fullscreen attached process that resumes the Session and starts a team Run.
fn live_pty(
    workspace: &std::path::Path,
    state: &std::path::Path,
    id: SessionId,
    classic: bool,
) -> (ChildGuard, LivePty) {
    opt_into_delegation(state, workspace, id);
    let mode = if classic { "--classic " } else { "" };
    let command = format!(
        "stty rows 24 cols 80; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" {mode}--state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\" --provider custom:local --model model-1 'team task'; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
    );
    let mut command_line =
        crate::isolation::account_isolated_script(state.parent().expect("fixture root"));
    command_line
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", state)
        .env("ARANY_TEST_WORKSPACE", workspace)
        .env("ARANY_TEST_SESSION", id.to_string())
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(workspace)
        .args(["-q", "-e", "-c", &command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut attached = ChildGuard::new(command_line.spawn().expect("live agent chat process"));
    let input = attached.child().stdin.take().expect("PTY input");
    let output = attached.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
    let pty = LivePty {
        input,
        output,
        transcript: Vec::new(),
        history: Vec::new(),
        answered: crate::session_picker::PtyResponses::default(),
        capture_from: 0,
        context: if classic { "Classic" } else { "Fullscreen" },
    };
    (attached, pty)
}

// Exits through `/exit` and checks that the terminal settings were restored.
fn exit_live(attached: &mut ChildGuard, pty: &mut LivePty) {
    exit_live_after_refusal(attached, pty, || {});
}

// `on_refused` runs once a refusal of `/exit` is on screen, before the retry.
fn exit_live_after_refusal(
    attached: &mut ChildGuard,
    pty: &mut LivePty,
    mut on_refused: impl FnMut(),
) {
    let refused = "unavailable while a Run is active";
    let count = |screen: &str, text: &str| {
        crate::process::pty_text_matches(screen.as_bytes(), text.as_bytes()).count()
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut from = pty.capture_from;
    // The `/exit` refusal notice persists, so resend after a newer refusal reaches idle.
    'exit: loop {
        from = checkpoint_screen(
            &mut pty.transcript,
            &mut pty.answered,
            &mut pty.history,
            LIVE_SIZE,
            from,
        );
        pty.capture_from = from;
        let earlier = count(&pty.screen(pty.transcript.len()), refused);
        pty.input.write_all(b"/exit\r").expect("exit attached mode");
        let mut was_refused = false;
        let mut inspected = 0;
        loop {
            pty.pump();
            if attached.child().try_wait().expect("chat status").is_some() {
                break 'exit;
            }
            for end in pty.frame_ends(from).into_iter().skip(inspected) {
                inspected += 1;
                let screen = pty.screen(end);
                if count(&screen, refused) > earlier && !was_refused {
                    was_refused = true;
                    on_refused();
                }
                if was_refused && count(&screen, "ready ·") > 0 {
                    continue 'exit;
                }
            }
            assert!(
                Instant::now() < deadline,
                "{} live chat did not exit: {}",
                pty.context,
                tail(&pty.transcript[from..])
            );
            thread::yield_now();
        }
    }
    let result = wait_product(attached.take());
    pty.pump();
    assert!(
        result.status.success(),
        "{} chat exited cleanly",
        pty.context
    );
    pty.history.extend_from_slice(&pty.transcript[from..]);
    let text = String::from_utf8_lossy(&pty.history);
    let setting = |marker: &str| {
        text.split_once(marker)
            .and_then(|(_, rest)| rest.lines().next())
            .map(|line| line.trim_end_matches('\r').to_owned())
            .unwrap_or_else(|| panic!("{} {marker} missing", pty.context))
    };
    assert_eq!(setting("TTY_BEFORE:"), setting("TTY_AFTER:"));
    assert!(!text.contains("test-key"));
}

fn live_agent_chat_journey(
    workspace: &std::path::Path,
    state: &std::path::Path,
    id: SessionId,
    classic: bool,
    gates: &Gates,
) {
    let (mut attached, mut pty) = live_pty(workspace, state, id, classic);
    let armed = "agents · Esc primary";
    pty.await_request(gates, "first");
    // The shelf lists both children once their spawns are committed and reduced.
    let from = pty.capture_from;
    pty.frame(from, &["child 2 ·", "Alt+Left/Right agents"], &[]);
    pty.step(
        b"\x1b[1;3C",
        &[
            "viewing child 1 (message)",
            armed,
            "first child task",
            "child 1 is working",
        ],
        &[],
    );
    let second: &[&str] = &[
        "viewing child 2 (message)",
        armed,
        "second child task",
        "child 2 is working",
    ];
    let shown = pty.step(b"\x1b[1;3C", second, &[]);
    // The sibling finishes and the selected child starts: nothing shown changes, so nothing draws.
    gates.release.send(()).expect("release the first child");
    pty.await_request(gates, "second");
    for screen in pty.frames_since(shown) {
        assert!(screen.contains("viewing child 2 (message)"), "{screen}");
        assert!(!screen.contains("result of first child task"), "{screen}");
    }
    // The sibling's committed result is there once its own chat is opened.
    pty.step(
        b"\x1b[1;3D",
        &[
            "viewing child 1 (read-only)",
            armed,
            "result of first child task",
            "child 1 finished",
        ],
        &[],
    );
    let absent: &[&str] = if classic {
        &[]
    } else {
        &["result of first child task"]
    };
    let shown = pty.step(b"\x1b[1;3C", second, absent);
    // The selected child's own result and status arrive without reopening its chat.
    gates.release.send(()).expect("release the second child");
    pty.frame(
        shown,
        &[
            "viewing child 2 (read-only)",
            armed,
            "result of second child task",
            "child 2 finished",
        ],
        absent,
    );
    pty.await_request(gates, "synthesis");
    let frames = pty.frames_since(shown);
    assert!(
        frames
            .iter()
            .all(|screen| screen.contains("viewing child 2 (")),
        "{frames:?}"
    );
    gates.release.send(()).expect("release the synthesis");
    // Escape returns to the primary without cancelling; its answer follows the Run's end.
    pty.step(
        b"\x1b",
        &[
            "team answer",
            "Ask Arany",
            "> Ask a question, or type / for commands",
        ],
        &["viewing child", "agents · Esc primary"],
    );
    exit_live(&mut attached, &mut pty);
    if classic {
        // Live records were appended once each, below the chat's read-only boundary.
        let (scrollback, screen) = crate::terminal_grid(&pty.history, LIVE_SIZE.0, LIVE_SIZE.1);
        let rows = scrollback.iter().chain(&screen).collect::<Vec<_>>();
        let count = |needle: &str| rows.iter().filter(|row| row.contains(needle)).count();
        assert_eq!(count("result of second child task"), 1, "{rows:?}");
        assert_eq!(count("Viewing child 2 (message)"), 2, "{rows:?}");
        // The Fullscreen journey's Run, then this one after the return, each once.
        assert_eq!(count("team answer"), 2, "{rows:?}");
    }
}

// `/agents` during a team Run: committed changes while the user searches never retarget the focus.
fn live_agent_picker_journey(
    workspace: &std::path::Path,
    state: &std::path::Path,
    id: SessionId,
    classic: bool,
    gates: &Gates,
) {
    let (mut attached, mut pty) = live_pty(workspace, state, id, classic);
    let picker = "Enter open · Esc close";
    let focused = |pty: &LivePty, end: usize| {
        pty.screen(end)
            .lines()
            .find(|row| row.starts_with("› ") && row.contains(". Run "))
            .map(str::to_owned)
            .unwrap_or_default()
    };
    pty.await_request(gates, "first");
    let from = pty.capture_from;
    pty.frame(from, &["child 2 ·", "Alt+Left/Right agents"], &[]);
    pty.step(b"/agents\r", &[picker, "Current Run: 3 agents"], &[]);
    let end = pty.step(
        b"\rqueued-steer\rqueued-draft",
        &["Ask Arany", "Alt+Left/Right agents"],
        &[picker, "queued-steer", "queued-draft"],
    );
    let screen = pty.screen(end);
    let composer = screen.lines().find(|row| row.starts_with("> "));
    assert_eq!(
        composer.map(str::trim_end),
        Some("> Ask a question, or type / for commands"),
        "{screen}"
    );
    pty.step(b"x", &["> x"], &["queued-steer", "queued-draft"]);
    pty.step(b"\x7f", &["Alt+Left/Right agents"], &["> x"]);
    pty.step(b"/agents\r", &[picker, "Current Run: 3 agents"], &[]);
    pty.step(b"active", &["Search: active · 3 of"], &[]);
    let end = pty.step(b"\x1b[B\x1b[B", &["› 3. Run "], &[]);
    assert!(
        focused(&pty, end).contains("child 2; active"),
        "{}",
        pty.screen(end)
    );
    // The first child finishes and leaves the matches; the focus stays on child 2, now second.
    let from = pty.capture_from;
    gates.release.send(()).expect("release the first child");
    pty.await_request(gates, "second");
    let end = pty.frame(from, &["Search: active · 2 of", "› 2. Run "], &[]);
    assert!(
        focused(&pty, end).contains("child 2; active"),
        "{}",
        pty.screen(end)
    );
    pty.step(
        b"\r",
        &[
            "viewing child 2 (message)",
            "agents · Esc primary",
            "second child task",
            "child 2 is working",
        ],
        &[picker],
    );
    gates.release.send(()).expect("release the second child");
    pty.await_request(gates, "synthesis");
    gates.release.send(()).expect("release the synthesis");
    pty.step(
        b"\x1b",
        &[
            "team answer",
            "Ask Arany",
            "> Ask a question, or type / for commands",
        ],
        &["viewing child", "agents · Esc primary"],
    );
    exit_live(&mut attached, &mut pty);
}

fn live_linear_agent_chat(
    workspace: &std::path::Path,
    state: &std::path::Path,
    id: SessionId,
    gates: &Gates,
    messaging: bool,
) {
    let mut command =
        crate::isolation::account_isolated_script(state.parent().expect("fixture root"));
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", state)
        .env("ARANY_TEST_WORKSPACE", workspace)
        .env("ARANY_TEST_SESSION", id.to_string())
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .env("TERM", "dumb")
        .current_dir(workspace)
        .args([
            "-q",
            "-e",
            "-c",
            "exec \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\" --provider custom:local --model model-1 'team task'",
            "/dev/null",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("live linear chat process"));
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
    let (sender, receiver) = mpsc::channel();
    let mut consent_input =
        std::fs::File::from(rustix::io::dup(&input).expect("owned fixture input"));
    let prompt: &[u8] = b"Agent chat: number opens";
    let mut stages: Vec<&[u8]> = vec![
        b"Agent: child 2;",
        b"Viewing child 2 (message)",
        b"child 2 is working; its committed activity appears here",
        prompt,
        b"Viewing child 1 (read-only)",
        b"result of first child task",
        prompt,
        b"Viewing child 2 (message)",
        prompt,
        b"New activity from child 2:\r\nAgent result:\r\n  result of second child task\r\nchild 2 finished",
        prompt,
        b"Back to primary",
        b"team answer",
        b"Input:",
    ];
    if messaging {
        stages[9] = b"New activity from child 2:";
        stages.insert(4, b"Notice: Note waiting for child 2's next step");
        stages.insert(5, prompt);
        stages.insert(6, b"Steering note recorded for child 2");
        stages.insert(7, prompt);
    }
    let reader = thread::spawn(move || {
        let mut declined = false;
        let mut bytes = Vec::new();
        let mut offsets = Vec::new();
        let mut search_from = 0;
        loop {
            let mut chunk = [0; 4096];
            let count = output.read(&mut chunk).expect("PTY output bytes");
            if count == 0 {
                break;
            }
            assert!(bytes.len() + count <= 64 * 1024, "bounded PTY output");
            bytes.extend_from_slice(&chunk[..count]);
            crate::process::decline_workspace_consent(&mut consent_input, &bytes, &mut declined);
            while let Some(stage) = stages.get(offsets.len()) {
                let Some(matched) =
                    crate::process::pty_text_matches(&bytes[search_from..], stage).next()
                else {
                    break;
                };
                search_from += matched.end;
                offsets.push(search_from);
                sender.send(offsets.len() - 1).expect("stage receiver");
            }
        }
        (bytes, offsets)
    });
    let stage = |expected| {
        let actual = receiver
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|error| panic!("live linear agent chat stage {expected}: {error:?}"));
        assert_eq!(actual, expected);
    };
    let request = |expected| {
        assert_eq!(
            gates
                .requested
                .recv_timeout(Duration::from_secs(10))
                .expect("gated request"),
            expected
        );
    };
    request("first");
    stage(0);
    input
        .write_all(b"/transcript agents 3\n")
        .expect("open the second child");
    (1..=3).for_each(stage);
    if messaging {
        input
            .write_all(b"/message /literal guidance\n")
            .expect("offer child note");
        (4..=5).for_each(stage);
    }
    gates.release.send(()).expect("release the first child");
    request("second");
    if messaging {
        (6..=7).for_each(stage);
    }
    let shift = usize::from(messaging) * 4;
    input.write_all(b"p\n").expect("previous agent");
    (4 + shift..=6 + shift).for_each(stage);
    input.write_all(b"n\n").expect("next agent");
    (7 + shift..=8 + shift).for_each(stage);
    gates.release.send(()).expect("release the second child");
    (9 + shift..=10 + shift).for_each(stage);
    request("synthesis");
    gates.release.send(()).expect("release the synthesis");
    input.write_all(b"q\n").expect("return to primary");
    (11 + shift..=13 + shift).for_each(stage);
    input.write_all(b"/exit\n").expect("exit linear chat");
    drop(input);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.child().try_wait().expect("linear status") {
            assert!(status.success(), "live linear chat exited successfully");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "live linear chat exceeded deadline"
        );
        thread::yield_now();
    }
    let (bytes, offsets) = reader.join().expect("linear output reader");
    let text = String::from_utf8_lossy(&bytes);
    // While child 2 was shown, its sibling's committed result printed nothing.
    let second_shown =
        String::from_utf8_lossy(&bytes[offsets[1]..offsets[4 + usize::from(messaging) * 4]]);
    assert!(!second_shown.contains("result of first"), "{second_shown}");
    assert_eq!(
        text.matches("result of second child task").count(),
        1,
        "{text}"
    );
    assert!(
        !text.contains("\x1b["),
        "linear output has no cursor rewriting"
    );
}

use super::loopback::{
    ChildGuard, check_profile, read_request, send_response, wait_product, write_profile,
};
use super::session_picker::{pump, tail};
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
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("TERM", "dumb")
        .current_dir(&workspace)
        .args([
            "-q",
            "-e",
            "-c",
            "exec \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider openai",
            "/dev/null",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("attached PTY process"));
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let stages: [&[u8]; 3] = [
            b"Input:",
            b"Agent choice: n next",
            b"Notice: Agent inspection closed",
        ];
        let mut next = 0;
        loop {
            let mut chunk = [0; 4096];
            let count = output.read(&mut chunk).expect("PTY output bytes");
            if count == 0 {
                break;
            }
            assert!(bytes.len() + count <= 32 * 1024, "bounded PTY output");
            bytes.extend_from_slice(&chunk[..count]);
            if next < stages.len()
                && bytes
                    .windows(stages[next].len())
                    .any(|part| part == stages[next])
            {
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
                .expect("PTY stage"),
            expected
        );
    };
    stage(0);
    input.write_all(b"/agents\n").expect("open inspector");
    stage(1);
    input.write_all(b"q\n").expect("close inspector");
    stage(2);
    input.write_all(b"/quit\n").expect("exit attached mode");
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
    assert!(text.contains("No Runs yet"));
    assert!(text.contains("History: 0 agents in 0 recent Runs; 0 older Runs"));
    assert!(text.contains("No AgentRuns yet"));
    assert!(text.contains("Next Run: auto · up to 3 children"));
    assert!(text.contains("Agent inspection closed"));
    assert!(!text.contains("\x1b[?1000h"));
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
        for index in 0..5 {
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
            assert_eq!(wire["text"]["format"]["strict"], true);
            assert_eq!(
                wire["max_output_tokens"],
                if index == 1 || index == 2 { 128 } else { 4096 }
            );
            let input: serde_json::Value =
                serde_json::from_str(wire["input"].as_str().expect("semantic input text"))
                    .expect("semantic input");
            assert!(!input.to_string().contains("/agents"));
            match index {
                0..=2 => {
                    assert!(
                        input
                            .get("workspace_guidance")
                            .is_none_or(serde_json::Value::is_null)
                    );
                }
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
                0 => serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"ok"}}),
                1 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":["synthetic child"]}})
                }
                2 => serde_json::json!({"summary":"synthetic summary"}),
                3 => {
                    serde_json::json!({"outcome":{"type":"finish","summary":"prior summary","result":"prior answer"}})
                }
                _ => {
                    serde_json::json!({"outcome":{"type":"finish","summary":"current summary","result":"current answer"}})
                }
            };
            send_response(&mut stream, index, text);
        }
    });

    check_profile(&workspace, &state);

    let mut first = Command::new(env!("CARGO_BIN_EXE_arany"));
    first
        .env_clear()
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

    let mut attached = Command::new("/usr/bin/script");
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
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let patterns: [&[u8]; 7] = [
            b"Agent: primary; state: active",
            b"Agent choice: n next",
            b"Agent 2/2: primary; finished",
            b"Notice: Run in progress... Ctrl+C cancels",
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
    input.write_all(b"q\n").expect("close active inspector");
    stage(3);
    input
        .write_all(b"/agents\n")
        .expect("reopen active inspector");
    stage(4);
    input.write_all(b"z").expect("partial inspector choice");
    stage(5);
    release_run.send(()).expect("release active response");
    stage(6);
    input.write_all(b"/quit\n").expect("exit attached mode");
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

fn wait_for_pty(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut usize,
    after: usize,
    pattern: &[u8],
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(output, input, transcript, answered);
        if transcript[after..]
            .windows(pattern.len())
            .any(|part| part == pattern)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "inline agent inspector stage missing: {}",
            tail(transcript)
        );
        thread::yield_now();
    }
}

fn inline_inspector(
    workspace: &std::path::Path,
    state: &std::path::Path,
    id: SessionId,
    width: u16,
) {
    let no_color = if width < 20 { "--no-color " } else { "" };
    let command = format!(
        "stty rows 24 cols {width}; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" {no_color}--state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
    );
    let mut command_line = Command::new("/usr/bin/script");
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
    let mut answered = 0;
    wait_for_pty(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        0,
        b"Ask Arany",
    );
    let picker_start = transcript.len();
    input.write_all(b"/agents\r").expect("open inspector");
    wait_for_pty(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        0,
        b"\x1b[?1000h",
    );
    let checkpoint = if width < 20 {
        wait_for_pty(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            picker_start,
            b"Esc Up/Dn P",
        );
        for cue in [b"Esc".as_slice(), b"Up/Dn"] {
            assert!(
                transcript[picker_start..]
                    .windows(cue.len())
                    .any(|part| part == cue),
                "narrow inspector cue missing: {}",
                tail(&transcript)
            );
        }
        let checkpoint = transcript.len();
        input.write_all(b"\x1b[B").expect("select prior AgentRun");
        wait_for_pty(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            checkpoint,
            b"prior",
        );
        let checkpoint = transcript.len();
        input.write_all(b"\x1b").expect("close inspector");
        checkpoint
    } else {
        let checkpoint = transcript.len();
        input
            .write_all(b"\x1b[<35;1;13M")
            .expect("hover older AgentRun");
        wait_for_pty(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            checkpoint,
            b"prior",
        );
        let checkpoint = transcript.len();
        input
            .write_all(b"\x1b[<64;1;13M")
            .expect("wheel to newest AgentRun");
        wait_for_pty(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            checkpoint,
            b"current",
        );
        let checkpoint = transcript.len();
        input
            .write_all(b"\x1b[<0;1;13M")
            .expect("click older AgentRun");
        wait_for_pty(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            checkpoint,
            b"prior",
        );
        let checkpoint = transcript.len();
        input
            .write_all(b"\x1b[<65;1;2M\x1b[<35;1;12M")
            .expect("off-list wheel then hover newest AgentRun");
        wait_for_pty(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            checkpoint,
            b"current",
        );
        let checkpoint = transcript.len();
        input
            .write_all(b"\x1b[<35;1;13M")
            .expect("hover older AgentRun again");
        wait_for_pty(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            checkpoint,
            b"prior",
        );
        input.write_all(b"\r").expect("close inspector");
        checkpoint
    };
    wait_for_pty(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        checkpoint,
        b"\x1b[?1000l",
    );
    input.write_all(b"/quit\r").expect("exit attached mode");
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
        assert!(Instant::now() < deadline, "inline inspector did not exit");
        thread::yield_now();
    }
    let result = wait_product(attached.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(result.status.success(), "inline inspector exited cleanly");
    assert_eq!(result.stderr, b"");
    let transcript = String::from_utf8(transcript).expect("inline transcript UTF-8");
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
    assert_eq!(transcript.matches("\x1b[?1000h").count(), 1);
    assert_eq!(transcript.matches("\x1b[?1000l").count(), 1);
    assert!(!transcript.contains("\x1b[?1049h"));
    assert!(!transcript.contains("OMITTED_CANARY"));
    assert!(!transcript.contains("test-key"));
}

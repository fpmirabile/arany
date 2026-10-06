use super::loopback::{
    ChildGuard, check_profile, read_request, send_response, wait_product, write_profile,
};
use super::process::BoundedOutput;
use arany::{RunStatus, SessionId, SessionView, StateRoot, Store};
use rustix::process::{Pid, Signal, kill_process};
use std::{
    fs::File,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::{ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
enum ActiveExit {
    Signal(Signal, &'static str),
    ProviderFailure,
    CtrlC,
    HelpThenCtrlC,
    TypoThenCtrlC,
    SecondCtrlC,
    SuspendThenSignal,
    PtyLoss,
}

fn product_state(pid: Pid) -> char {
    let status = std::fs::read_to_string(format!("/proc/{}/status", pid.as_raw_pid()))
        .expect("product process status");
    status
        .lines()
        .find_map(|line| {
            line.strip_prefix("State:")
                .and_then(|state| state.trim().chars().next())
        })
        .expect("product process state")
}

fn stdin_nonblocking(pid: Pid) -> bool {
    let fdinfo = std::fs::read_to_string(format!("/proc/{}/fdinfo/0", pid.as_raw_pid()))
        .expect("product stdin flags");
    let flags = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("flags:").map(str::trim))
        .expect("stdin flags field");
    let flags = u32::from_str_radix(flags, 8).expect("octal stdin flags");
    flags & 0o4000 != 0
}

pub(super) fn tty_settings(pid: Pid) -> String {
    let stdin =
        File::open(format!("/proc/{}/fd/0", pid.as_raw_pid())).expect("active product PTY slave");
    let output = Command::new("/usr/bin/stty")
        .arg("-g")
        .env_clear()
        .stdin(Stdio::from(stdin))
        .bounded_output_with_input()
        .expect("read active PTY settings");
    assert!(output.status.success(), "active PTY settings available");
    String::from_utf8(output.stdout)
        .expect("PTY settings UTF-8")
        .trim()
        .to_owned()
}

pub(super) fn wait_stopped(pid: Pid) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if product_state(pid) == 'T' {
            return;
        }
        assert!(Instant::now() < deadline, "product did not suspend");
        thread::yield_now();
    }
}

fn write_input(input: &Arc<Mutex<ChildStdin>>, bytes: &[u8]) {
    input
        .lock()
        .expect("PTY input lock")
        .write_all(bytes)
        .expect("PTY input bytes");
}

fn continue_draft(input: &Arc<Mutex<ChildStdin>>, byte: u8, count: usize) {
    assert!(count <= 4094, "bounded physical line");
    let mut segment = vec![byte; count];
    segment.push(4);
    write_input(input, &segment);
}

pub(super) fn product_child_of(shell_pid: u32) -> Pid {
    product_child_of_executable(shell_pid, Path::new(env!("CARGO_BIN_EXE_arany")))
}

pub(super) fn product_child_of_executable(shell_pid: u32, product: &Path) -> Pid {
    let children_path = format!("/proc/{shell_pid}/task/{shell_pid}/children");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(children) = std::fs::read_to_string(&children_path) {
            for raw in children.split_ascii_whitespace() {
                let Ok(raw_pid) = raw.parse::<i32>() else {
                    continue;
                };
                if std::fs::read_link(format!("/proc/{raw_pid}/exe"))
                    .ok()
                    .as_deref()
                    == Some(product)
                {
                    return Pid::from_raw(raw_pid).expect("positive product PID");
                }
            }
        }
        assert!(Instant::now() < deadline, "active product child not found");
        thread::yield_now();
    }
}

pub(super) fn transcript_field<'a>(text: &'a str, label: &str) -> &'a str {
    text.lines()
        .filter_map(|line| line.trim_end_matches('\r').strip_prefix(label))
        .next()
        .unwrap_or_else(|| {
            panic!(
                "transcript field {label} missing; output: {}",
                redacted_tail(text.as_bytes())
            )
        })
}

fn redacted_tail(output: &[u8]) -> String {
    String::from_utf8_lossy(output)
        .replace("test-key", "[redacted]")
        .replace("OMITTED_CANARY", "[redacted]")
        .chars()
        .rev()
        .take(1024)
        .collect::<String>()
        .chars()
        .rev()
        .flat_map(char::escape_default)
        .collect()
}

fn active_exit_during_provider_call(exit: ActiveExit, inline: bool) {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    std::fs::write(workspace.join("private.txt"), b"OMITTED_CANARY")
        .expect("omitted Workspace input");
    let state = temp.path().join("state");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback endpoint");
    write_profile(
        &state,
        listener.local_addr().expect("listener address").port(),
    );
    let (request_sender, request_ready) = mpsc::channel();
    let (closed_sender, socket_closed) = mpsc::channel();
    let (failure_sender, failure_ready) = mpsc::channel();
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).expect("bounded accept");
        for index in 0..4 {
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
            let input: serde_json::Value =
                serde_json::from_str(wire["input"].as_str().expect("semantic input text"))
                    .expect("semantic input");
            if index == 3 {
                assert_eq!(input["objective"], "cancel");
                request_sender.send(()).expect("active request gate");
                if matches!(exit, ActiveExit::ProviderFailure) {
                    failure_ready
                        .recv_timeout(Duration::from_secs(10))
                        .expect("release synthetic failure");
                    stream
                        .write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 8\r\nConnection: close\r\n\r\ntest-key")
                        .expect("synthetic failure response");
                    return;
                }
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("cancel deadline");
                let mut byte = [0];
                assert_eq!(stream.read(&mut byte).expect("cancelled socket"), 0);
                closed_sender.send(()).expect("socket closure observation");
                return;
            }
            assert!(
                input
                    .get("workspace_guidance")
                    .is_none_or(serde_json::Value::is_null)
            );
            let text = match index {
                0 => serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"ok"}}),
                1 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":["synthetic child"]}})
                }
                _ => serde_json::json!({"summary":"synthetic summary"}),
            };
            send_response(&mut stream, index, text);
        }
    });
    check_profile(&workspace, &state);

    let command = if inline {
        "trap ':' INT; stty rows 24 cols 80; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider custom:local --model model-1 cancel; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
    } else {
        "trap ':' INT; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider custom:local --model model-1 cancel; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
    };
    let mut attached = Command::new("/usr/bin/script");
    attached
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .env("SHELL", "/bin/sh")
        .env("TERM", if inline { "xterm" } else { "dumb" })
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut attached = ChildGuard::new(attached.spawn().expect("attached PTY process"));
    let input = Arc::new(Mutex::new(
        attached.child().stdin.take().expect("PTY input"),
    ));
    let reader_input = Arc::clone(&input);
    let awaiting_resume_query = Arc::new(AtomicBool::new(false));
    let reader_awaiting_resume_query = Arc::clone(&awaiting_resume_query);
    let mut output = attached.child().stdout.take().expect("PTY output");
    let (stage_sender, stages) = mpsc::channel();
    let (resume_query_sender, resume_queries) = mpsc::channel();
    let expect_idle = matches!(
        exit,
        ActiveExit::CtrlC
            | ActiveExit::HelpThenCtrlC
            | ActiveExit::TypoThenCtrlC
            | ActiveExit::ProviderFailure
    );
    let expect_failure = matches!(exit, ActiveExit::ProviderFailure);
    let expect_feedback =
        expect_idle && (inline || matches!(exit, ActiveExit::CtrlC | ActiveExit::ProviderFailure));
    let expect_draft = inline && matches!(exit, ActiveExit::ProviderFailure | ActiveExit::CtrlC);
    let expected_status = if expect_failure {
        "failed"
    } else {
        "cancelled"
    };
    let expect_help = matches!(exit, ActiveExit::HelpThenCtrlC);
    let expect_typo = matches!(exit, ActiveExit::TypoThenCtrlC);
    let expect_second_ctrl_c = matches!(exit, ActiveExit::SecondCtrlC);
    let expect_resume = matches!(exit, ActiveExit::SuspendThenSignal) && !inline;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut patterns: Vec<&[u8]> = vec![
            b"SHELL_PID:",
            if inline {
                b"TTY_BEFORE:"
            } else {
                b"Agent: primary; state: active"
            },
        ];
        if expect_draft {
            patterns.push(b"pX");
        }
        if expect_resume {
            patterns.push(b"Draft: 6000 characters; Enter retains until Run ends");
            patterns.push(b"Presentation: screen-reader");
            patterns.push(b"Notice: Error: invalid or overlong terminal line; draft unchanged\r\n");
            patterns.push(b"Draft: 8192 characters; Enter retains until Run ends");
        }
        if expect_second_ctrl_c {
            patterns.push(b"Cancelling Run...");
        }
        if expect_typo {
            patterns.push(b"Notice: Error: Unknown command; did you mean /provider?\r\n");
            patterns.push(b"Notice: Error: Usage: /status\r\n");
            patterns.push(b"Notice: Error: Usage: /help\r\n");
            patterns.push(b"Notice: Error: Usage: /exit\r\n");
            patterns.push(b"Notice: Provider: custom:local; locked for this run\r\n");
            patterns.push(b"Notice: Working \xc2\xb7 request ");
            patterns.push(b"\r\n");
        }
        if expect_help {
            patterns.push(if inline {
                b"Commands"
            } else {
                b"Command: /exit; Exit Session; during Run: view-only\r\n"
            });
        }
        patterns.push(if expect_failure {
            b"Status: failed"
        } else {
            b"Status: cancelled"
        });
        if expect_feedback {
            patterns.push(if expect_failure {
                b"unavailable."
            } else {
                b"cancelled."
            });
        }
        if expect_draft {
            patterns.push(b"Draft retained: 6 characters; Enter submits");
        }
        if expect_idle {
            patterns.push(if inline { b"\x1b[?25h" } else { b"Input:" });
        }
        if expect_draft {
            patterns.push(b"Commands");
            patterns.push(b"close");
            patterns.push(b"\x1b[?25h");
        }
        patterns.push(b"TTY_AFTER:");
        let mut next = 0;
        let mut search_from = 0;
        let mut answered_cursor_queries = 0;
        loop {
            let mut chunk = [0; 4096];
            let count = output.read(&mut chunk).expect("PTY output bytes");
            if count == 0 {
                break;
            }
            assert!(bytes.len() + count <= 64 * 1024, "bounded PTY output");
            bytes.extend_from_slice(&chunk[..count]);
            if inline {
                let queries = bytes.windows(4).filter(|part| *part == b"\x1b[6n").count();
                assert!(queries <= 32, "bounded cursor-position queries");
                while answered_cursor_queries < queries {
                    if reader_awaiting_resume_query.load(Ordering::Acquire) {
                        resume_query_sender
                            .send(())
                            .expect("resumed cursor query stage");
                    }
                    write_input(&reader_input, b"\x1b[24;1R");
                    answered_cursor_queries += 1;
                }
            }
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
    let shell_stage = stages
        .recv_timeout(Duration::from_secs(10))
        .expect("shell PID stage");
    assert_eq!(shell_stage.0, 0);
    let shell_text = String::from_utf8_lossy(&shell_stage.1);
    let shell_pid = transcript_field(&shell_text, "SHELL_PID:")
        .parse::<u32>()
        .expect("shell PID");
    let product_pid = product_child_of(shell_pid);
    let product = acquisition::ProductGuard::new(product_pid, state.clone());
    if let Err(error) = request_ready.recv_timeout(Duration::from_secs(10)) {
        let result = wait_product(attached.take());
        let output = reader.join().expect("PTY output reader");
        panic!(
            "Provider request gate {error}: status {:?}; output: {}",
            result.status,
            redacted_tail(&output)
        );
    }
    assert_eq!(
        stages
            .recv_timeout(Duration::from_secs(10))
            .expect("working stage")
            .0,
        1
    );
    let active_settings = inline.then(|| tty_settings(product_pid));
    let mut suspended_settings = None;
    if expect_draft {
        write_input(&input, b"/helX\x1b[Dp");
        assert_eq!(
            stages
                .recv_timeout(Duration::from_secs(10))
                .expect("editable in-flight draft stage")
                .0,
            2
        );
    }
    match exit {
        ActiveExit::ProviderFailure => {
            failure_sender
                .send(())
                .expect("release failed Provider call");
        }
        ActiveExit::Signal(signal, _) => {
            kill_process(product_pid, signal).expect("deliver termination signal");
        }
        ActiveExit::CtrlC => write_input(&input, b"\x03"),
        ActiveExit::HelpThenCtrlC => {
            write_input(&input, if inline { b"/help\r" } else { b"/help\n" });
            let stage = match stages.recv_timeout(Duration::from_secs(10)) {
                Ok(stage) => stage,
                Err(error) => {
                    kill_process(product_pid, Signal::TERM).expect("stop stalled help test");
                    let result = wait_product(attached.take());
                    let output = reader.join().expect("PTY output reader");
                    panic!(
                        "active help stage {error}; status {:?}; output: {}",
                        result.status,
                        redacted_tail(&output)
                    );
                }
            };
            assert_eq!(stage.0, 2);
            write_input(&input, b"\x03");
        }
        ActiveExit::TypoThenCtrlC => {
            write_input(&input, b"/provder\n");
            assert_eq!(
                stages
                    .recv_timeout(Duration::from_secs(10))
                    .expect("rejected active command stage")
                    .0,
                2
            );
            for (expected, command) in [
                (3, b"/status unused\n".as_slice()),
                (4, b"/help unused\n".as_slice()),
                (5, b"/exit unused\n".as_slice()),
            ] {
                write_input(&input, command);
                assert_eq!(
                    stages
                        .recv_timeout(Duration::from_secs(10))
                        .expect("rejected unexpected active argument stage")
                        .0,
                    expected
                );
            }
            write_input(&input, b"/provider\n");
            assert_eq!(
                stages
                    .recv_timeout(Duration::from_secs(10))
                    .expect("next active command stage")
                    .0,
                6
            );
            write_input(&input, b"/status\n");
            for expected in [7, 8] {
                assert_eq!(
                    stages
                        .recv_timeout(Duration::from_secs(10))
                        .expect("complete current-Run status stage")
                        .0,
                    expected
                );
            }
            write_input(&input, b"\x03");
        }
        ActiveExit::SecondCtrlC => {
            let lock = rusqlite::Connection::open(state.join("events.sqlite3"))
                .expect("open cancellation gate");
            lock.execute_batch("BEGIN EXCLUSIVE")
                .expect("hold cancellation append");
            write_input(&input, b"\x03");
            assert_eq!(
                stages
                    .recv_timeout(Duration::from_secs(10))
                    .expect("first cancellation notice stage")
                    .0,
                2
            );
            write_input(&input, b"\x03");
            let deadline = Instant::now() + Duration::from_secs(3);
            while stdin_nonblocking(product_pid) {
                assert!(
                    Instant::now() < deadline,
                    "second Ctrl+C did not release input"
                );
                thread::yield_now();
            }
            lock.execute_batch("COMMIT")
                .expect("release cancellation append");
        }
        ActiveExit::SuspendThenSignal if inline => {
            kill_process(product_pid, Signal::TSTP).expect("deliver inline SIGTSTP");
            wait_stopped(product_pid);
            let stopped_settings = tty_settings(product_pid);
            assert_ne!(
                stopped_settings,
                *active_settings.as_ref().expect("active inline settings"),
                "raw mode released before stop"
            );
            suspended_settings = Some(stopped_settings);
            awaiting_resume_query.store(true, Ordering::Release);
            kill_process(product_pid, Signal::CONT).expect("resume inline Run");
            resume_queries
                .recv_timeout(Duration::from_secs(10))
                .expect("inline terminal reacquisition query");
            assert_eq!(
                tty_settings(product_pid),
                *active_settings.as_ref().expect("active inline settings"),
                "raw mode reacquired after resume"
            );
            kill_process(product_pid, Signal::TERM).expect("terminate resumed inline Run");
        }
        ActiveExit::SuspendThenSignal => {
            continue_draft(&input, b'a', 3000);
            continue_draft(&input, b'b', 3000);
            assert_eq!(
                stages
                    .recv_timeout(Duration::from_secs(10))
                    .expect("retained draft stage")
                    .0,
                2
            );
            assert!(stdin_nonblocking(product_pid), "active canonical reader");
            kill_process(product_pid, Signal::TSTP).expect("deliver SIGTSTP");
            wait_stopped(product_pid);
            assert!(
                !stdin_nonblocking(product_pid),
                "reader released before stop"
            );
            kill_process(product_pid, Signal::CONT).expect("resume active Run");
            assert_eq!(
                stages
                    .recv_timeout(Duration::from_secs(10))
                    .expect("resumed presentation stage")
                    .0,
                3
            );
            assert!(
                stdin_nonblocking(product_pid),
                "reader reacquired after resume"
            );
            continue_draft(&input, b'c', 2193);
            assert_eq!(
                stages
                    .recv_timeout(Duration::from_secs(10))
                    .expect("retained draft overflow stage")
                    .0,
                4
            );
            continue_draft(&input, b'd', 2192);
            assert_eq!(
                stages
                    .recv_timeout(Duration::from_secs(10))
                    .expect("full retained draft stage")
                    .0,
                5
            );
            kill_process(product_pid, Signal::TERM).expect("terminate resumed Run");
        }
        ActiveExit::PtyLoss => {
            attached.child().kill().expect("close PTY master");
            attached.take().wait().expect("reap PTY owner");
            let output = reader.join().expect("PTY output reader");
            if let Err(error) = socket_closed.recv_timeout(Duration::from_secs(3)) {
                let owned = product.is_owned_and_running();
                let process = if owned {
                    format!(
                        "state={} wait={}",
                        product_state(product_pid),
                        std::fs::read_to_string(format!(
                            "/proc/{}/wchan",
                            product_pid.as_raw_pid()
                        ))
                        .unwrap_or_else(|_| "unavailable".into())
                        .trim()
                    )
                } else {
                    "exited".into()
                };
                panic!(
                    "Provider socket remained open after PTY loss: {error}; product {process}; output: {}",
                    redacted_tail(&output)
                );
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while product.is_owned_and_running() {
                assert!(
                    Instant::now() < deadline,
                    "active product survived PTY loss"
                );
                thread::yield_now();
            }
            server.join().expect("synthetic Provider socket closed");
            assert!(
                !output
                    .windows(b"Answer:".len())
                    .any(|part| part == b"Answer:")
            );
            assert!(
                !output
                    .windows(b"OMITTED_CANARY".len())
                    .any(|part| part == b"OMITTED_CANARY")
            );
            assert!(
                !output
                    .windows(b"test-key".len())
                    .any(|part| part == b"test-key")
            );
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("read-only replay runtime");
            let sessions = runtime
                .block_on(arany::list_sessions(
                    StateRoot::open_existing(&state).expect("state reopen"),
                    workspace,
                ))
                .expect("list committed Sessions");
            assert_eq!(sessions.len(), 1);
            let store =
                Store::open_read_only(StateRoot::open_existing(&state).expect("state reopen"))
                    .expect("read-only Store");
            let events = runtime
                .block_on(store.load_session(sessions[0].id))
                .expect("Events");
            let view = SessionView::replay(sessions[0].id, &events)
                .expect("strict replay")
                .expect("Session");
            assert_eq!(view.runs.len(), 1);
            assert_eq!(view.runs[0].status, RunStatus::Cancelled);
            assert!(view.runs[0].assistant_message.is_none());
            return;
        }
    }
    assert_eq!(
        stages
            .recv_timeout(Duration::from_secs(10))
            .expect("terminal receipt stage")
            .0,
        usize::from(expect_draft)
            + if expect_resume {
                6
            } else if expect_second_ctrl_c {
                3
            } else if expect_typo {
                9
            } else if expect_help {
                3
            } else {
                2
            }
    );
    if expect_feedback {
        let feedback = match stages.recv_timeout(Duration::from_secs(10)) {
            Ok(stage) => stage,
            Err(error) => {
                kill_process(product_pid, Signal::TERM).expect("stop stalled feedback test");
                let result = wait_product(attached.take());
                let output = reader.join().expect("PTY output reader");
                panic!(
                    "unsuccessful Run chat feedback {error}; status {:?}; output: {}",
                    result.status,
                    redacted_tail(&output)
                );
            }
        };
        assert_eq!(
            feedback.0,
            usize::from(expect_draft) + if expect_help { 4 } else { 3 }
        );
    }
    if expect_idle {
        if expect_draft {
            assert_eq!(
                stages
                    .recv_timeout(Duration::from_secs(10))
                    .expect("retained draft status cue after chat feedback")
                    .0,
                5
            );
        }
        assert_eq!(
            stages
                .recv_timeout(Duration::from_secs(10))
                .expect("idle input stage")
                .0,
            usize::from(expect_feedback)
                + 2 * usize::from(expect_draft)
                + if expect_typo {
                    10
                } else if expect_help {
                    4
                } else {
                    3
                }
        );
        if expect_draft {
            write_input(&input, b"\x1b[3~\r");
            for expected in [7, 8] {
                assert_eq!(
                    stages
                        .recv_timeout(Duration::from_secs(10))
                        .expect("retained draft and caret open local help after correction")
                        .0,
                    expected
                );
            }
            write_input(&input, b"\x1b");
            assert_eq!(
                stages
                    .recv_timeout(Duration::from_secs(10))
                    .expect("composer restored after local help")
                    .0,
                9
            );
        }
        write_input(&input, if inline { b"/quit\r" } else { b"/quit\n" });
    }
    let restored_stage = match stages.recv_timeout(Duration::from_secs(10)) {
        Ok(stage) => stage,
        Err(error) => {
            if product.is_owned_and_running() {
                let _ = kill_process(product_pid, Signal::TERM);
            }
            let result = wait_product(attached.take());
            let output = reader.join().expect("PTY output reader");
            panic!(
                "terminal restoration stage {error}; status {:?}; output: {}",
                result.status,
                redacted_tail(&output)
            );
        }
    };
    let result = wait_product(attached.take());
    let output = reader.join().expect("PTY output reader");
    server.join().expect("synthetic server completed");
    let preview = redacted_tail(&output);
    assert_eq!(
        restored_stage.0,
        usize::from(expect_feedback)
            + 5 * usize::from(expect_draft)
            + if expect_resume {
                7
            } else if expect_typo {
                11
            } else if expect_help {
                5
            } else if expect_second_ctrl_c || expect_idle {
                4
            } else {
                3
            },
        "status: {:?}; output: {preview}",
        result.status
    );
    assert_eq!(result.status.success(), expect_idle, "attached exit class");
    assert_eq!(result.stderr, b"");
    let transcript = String::from_utf8(output).expect("PTY transcript UTF-8");
    assert_eq!(
        transcript_field(&transcript, "TTY_BEFORE:"),
        transcript_field(&transcript, "TTY_AFTER:"),
        "terminal mode restored"
    );
    if let Some(settings) = active_settings {
        assert_ne!(
            settings,
            transcript_field(&transcript, "TTY_BEFORE:"),
            "inline Run acquired raw terminal mode"
        );
    }
    if let Some(settings) = suspended_settings {
        assert_eq!(
            settings,
            transcript_field(&transcript, "TTY_BEFORE:"),
            "original terminal mode restored before stop"
        );
    }
    match exit {
        ActiveExit::Signal(_, signal_name) => {
            assert!(transcript.contains(&format!("error: terminated by {signal_name}")));
        }
        ActiveExit::SuspendThenSignal => {
            assert!(transcript.contains("error: terminated by SIGTERM"))
        }
        ActiveExit::SecondCtrlC => {
            assert!(transcript.contains("error: forced shutdown after second Ctrl+C"))
        }
        ActiveExit::CtrlC
        | ActiveExit::HelpThenCtrlC
        | ActiveExit::TypoThenCtrlC
        | ActiveExit::ProviderFailure => {}
        ActiveExit::PtyLoss => unreachable!("PTY loss returned after replay"),
    }
    assert!(transcript.contains(&format!("Status: {expected_status}")));
    if expect_feedback {
        let receipt = transcript
            .find(&format!("Status: {expected_status}"))
            .expect("terminal receipt");
        let mut after = receipt;
        if expect_failure {
            for word in ["Provider", "unavailable.", "Check", "the", "connection"] {
                let offset = transcript[after..]
                    .find(word)
                    .unwrap_or_else(|| panic!("missing failure cause word {word}: {preview}"));
                after += offset + word.len();
            }
        }
        for word in ["No", "answer", "was", "committed."] {
            let offset = transcript[after..]
                .find(word)
                .unwrap_or_else(|| panic!("missing chat feedback word {word}: {preview}"));
            after += offset + word.len();
        }
        assert!(
            transcript[receipt..].contains(match (inline, expect_failure) {
                (true, true) => "Arany · error:",
                (true, false) => "Arany · notice:",
                (false, true) => "Notice: Error: Run failed.",
                (false, false) => "Notice: Run cancelled.",
            }),
            "labeled unsuccessful outcome: {preview}"
        );
    }
    if !inline {
        assert_eq!(
            transcript.matches("\r\nYou:\r\n  cancel\r\n").count(),
            1,
            "linear committed objective is labeled once"
        );
        if expect_help {
            let help = transcript
                .split_once("Help: local commands;")
                .expect("linear help")
                .1;
            let during_run = help
                .split_once("Status: cancelled")
                .expect("cancelled receipt")
                .0;
            assert!(
                !during_run.contains("Input:"),
                "help must not release active input ownership: {preview}"
            );
            for name in arany::command_completions("/") {
                assert_eq!(
                    during_run
                        .lines()
                        .filter(|line| line.starts_with(&format!("Command: /{name};"))
                            || line.starts_with(&format!("Command: /{name} ")))
                        .count(),
                    1,
                    "active linear help must expose every compiled control: {name}"
                );
            }
            assert!(during_run.contains("during Run: locked"));
            assert!(during_run.contains("during Run: view-only"));
            assert!(!during_run.contains('\u{1b}'));
        }
    }
    if expect_resume {
        assert_eq!(
            transcript
                .matches("Notice: Error: invalid or overlong terminal line; draft unchanged\r\n")
                .count(),
            1,
            "one labeled overflow error preserves the retained draft: {preview}"
        );
    }
    assert!(!transcript.contains("Answer:"));
    assert!(!transcript.contains("OMITTED_CANARY"));
    assert!(!transcript.contains("test-key"));
    let receipt_start = transcript[..transcript
        .find(&format!("Status: {expected_status}"))
        .expect("terminal receipt status")]
        .rfind("Session: ")
        .expect("receipt Session field");
    let session_id = transcript[receipt_start..]
        .lines()
        .next()
        .expect("receipt Session line")
        .trim_end_matches('\r')
        .strip_prefix("Session: ")
        .expect("receipt Session label")
        .parse::<SessionId>()
        .expect("Session ID");
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
    assert_eq!(view.runs.len(), 1);
    assert_eq!(
        view.runs[0].status,
        if expect_failure {
            RunStatus::Failed
        } else {
            RunStatus::Cancelled
        }
    );
    assert!(view.runs[0].assistant_message.is_none());
    assert_eq!(view.runs[0].agents.len(), 1);
    assert_eq!(view.runs[0].agents[0].provider_calls.len(), 1);
    if expect_typo {
        assert_eq!(
            view.defaults,
            arany::SessionDefaults {
                provider: Some("custom:local".into()),
                model: Some("model-1".into()),
                ..arany::SessionDefaults::default()
            },
            "rejected arguments and pinned inspection preserve defaults"
        );
        assert_eq!(
            events
                .iter()
                .filter(|envelope| matches!(
                    envelope.event,
                    arany::Event::SessionDefaultChanged { .. }
                ))
                .count(),
            1,
            "only startup selection changes defaults"
        );
        for command in ["status", "help", "exit"] {
            assert_eq!(
                transcript
                    .matches(&format!("Notice: Error: Usage: /{command}\r\n"))
                    .count(),
                1,
                "one complete rejection before the next physical command"
            );
        }
        let usage = view.runs[0]
            .config
            .as_ref()
            .and_then(|config| config.context_usage.as_ref())
            .expect("current Run footprint");
        let status = format!(
            "Notice: Working · request {}% of local limit · 1 agent. Includes chat, instructions, files and images; not model tokens. Details: /agents\r\n",
            usage.utilization_percent(),
        );
        assert_eq!(transcript.matches(&status).count(), 1);
        assert!(!transcript.contains("Notice: Preparing message · Ctrl+C cancels"));
    }
    if expect_failure {
        assert_eq!(
            view.runs[0].agents[0].provider_calls[0].disposition,
            arany::ProviderCallDisposition::Unavailable
        );
    }
    let receipt = format!(
        "Session: {session_id}\r\nRun: {}\r\nStatus: {expected_status}\r\nProvider: custom verified\r\n",
        view.runs[0].id
    );
    assert_eq!(
        transcript.matches(&receipt).count(),
        1,
        "one committed receipt"
    );
    runtime.block_on(store.close()).expect("close Store");
}

#[test]
fn termination_signals_during_provider_call_restore_terminal_and_replay_cancelled_run() {
    for (signal, name) in [(Signal::TERM, "SIGTERM"), (Signal::HUP, "SIGHUP")] {
        active_exit_during_provider_call(ActiveExit::Signal(signal, name), false);
    }
}

#[test]
fn ctrl_c_during_provider_call_returns_to_session_after_cancelled_receipt() {
    active_exit_during_provider_call(ActiveExit::CtrlC, false);
}

#[test]
fn ctrl_c_over_active_help_still_cancels_the_run() {
    for inline in [true, false] {
        active_exit_during_provider_call(ActiveExit::HelpThenCtrlC, inline);
    }
}

#[test]
fn failed_and_cancelled_attached_runs_keep_draft_without_an_answer_or_retry() {
    for exit in [ActiveExit::ProviderFailure, ActiveExit::CtrlC] {
        for inline in [true, false] {
            active_exit_during_provider_call(exit, inline);
        }
    }
}

#[test]
fn rejected_active_screen_reader_line_does_not_join_next_command() {
    active_exit_during_provider_call(ActiveExit::TypoThenCtrlC, false);
}

#[test]
fn second_ctrl_c_during_cancellation_restores_terminal_and_replays_committed_run() {
    active_exit_during_provider_call(ActiveExit::SecondCtrlC, false);
}

#[test]
fn in_flight_suspend_restores_reader_and_bounded_draft() {
    active_exit_during_provider_call(ActiveExit::SuspendThenSignal, false);
}

#[test]
fn inline_sigterm_restores_raw_mode_during_active_run() {
    active_exit_during_provider_call(ActiveExit::Signal(Signal::TERM, "SIGTERM"), true);
}

#[test]
fn lost_pty_during_provider_call_cancels_without_answer() {
    active_exit_during_provider_call(ActiveExit::PtyLoss, true);
}

#[test]
fn inline_in_flight_suspend_releases_and_reacquires_raw_mode() {
    active_exit_during_provider_call(ActiveExit::SuspendThenSignal, true);
}

#[path = "active_terminal/output_window.rs"]
mod output_window;

#[path = "active_terminal/broken_stderr.rs"]
mod broken_stderr;

#[path = "active_terminal/acquisition.rs"]
mod acquisition;
pub(super) use acquisition::ProductGuard;

#[path = "active_terminal/panic.rs"]
mod panic;

#[cfg(target_os = "linux")]
#[path = "active_terminal/tmux.rs"]
mod tmux;

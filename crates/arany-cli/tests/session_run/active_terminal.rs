use super::loopback::{
    ChildGuard, check_profile, read_request, send_response, wait_product, write_profile,
};
use super::process::BoundedOutput;
use arany::{RunStatus, SessionId, SessionView, StateRoot, Store};
use rustix::process::{Pid, Signal, kill_process};
use std::{
    collections::VecDeque,
    fs::File,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::{ChildStdin, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

const DRAFT_SEGMENT_BYTES: usize = 3000;
const RETAINED_DRAFT_BYTES: usize = 2 * DRAFT_SEGMENT_BYTES;
const DRAFT_STAGE_TIMEOUT: Duration = Duration::from_secs(10);
const ACTIVE_PTY_OUTPUT_BYTES: usize = arany::MAX_USER_MESSAGE_BYTES + 64 * 1024;

#[derive(Clone, Copy)]
enum ActiveExit {
    Signal(Signal, &'static str),
    ProviderFailure,
    CtrlC,
    Escape,
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

fn reader_nonblocking(pid: Pid) -> bool {
    let process = std::path::PathBuf::from(format!("/proc/{}", pid.as_raw_pid()));
    let terminal = std::fs::read_link(process.join("fd/0")).expect("product input terminal");
    for entry in std::fs::read_dir(process.join("fd")).expect("product descriptors") {
        let entry = entry.expect("descriptor entry");
        let name = entry.file_name();
        if matches!(name.to_str(), Some("0" | "1" | "2")) {
            continue;
        }
        let target = match std::fs::read_link(entry.path()) {
            Ok(target) => target,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("product descriptor identity unavailable: {error}"),
        };
        if target != terminal {
            continue;
        }
        let fdinfo = match std::fs::read_to_string(process.join("fdinfo").join(name)) {
            Ok(info) => info,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("product reader flags unavailable: {error}"),
        };
        let flags = fdinfo
            .lines()
            .find_map(|line| line.strip_prefix("flags:").map(str::trim))
            .expect("reader flags field");
        if u32::from_str_radix(flags, 8).expect("octal reader flags") & 0o4000 != 0 {
            return true;
        }
    }
    false
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
    try_product_child_of_executable(shell_pid, product).expect("active product child not found")
}

pub(super) fn try_product_child_of_executable(shell_pid: u32, product: &Path) -> Option<Pid> {
    const MAX_DESCENDANT_DEPTH: usize = 8;
    const MAX_DESCENDANTS: usize = 64;
    const MAX_CHILDREN_BYTES: usize = 1024;

    let root_pid = i32::try_from(shell_pid).ok()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut pending = VecDeque::new();
        pending.push_back((root_pid, 0));
        let mut seen = vec![root_pid];
        let mut visited = 0;
        while let Some((parent_pid, depth)) = pending.pop_front() {
            if depth >= MAX_DESCENDANT_DEPTH {
                continue;
            }
            let children_path = format!("/proc/{parent_pid}/task/{parent_pid}/children");
            let mut children = String::new();
            let Ok(bytes_read) = File::open(children_path).and_then(|file| {
                file.take(u64::try_from(MAX_CHILDREN_BYTES + 1).expect("bounded child list limit"))
                    .read_to_string(&mut children)
            }) else {
                continue;
            };
            if bytes_read <= MAX_CHILDREN_BYTES {
                for raw in children.split_ascii_whitespace() {
                    if visited >= MAX_DESCENDANTS {
                        break;
                    }
                    let Ok(raw_pid) = raw.parse::<i32>() else {
                        continue;
                    };
                    if raw_pid <= 0 || seen.contains(&raw_pid) {
                        continue;
                    }
                    visited += 1;
                    seen.push(raw_pid);
                    if std::fs::read_link(format!("/proc/{raw_pid}/exe"))
                        .ok()
                        .as_deref()
                        == Some(product)
                    {
                        return Some(Pid::from_raw(raw_pid).expect("positive product PID"));
                    }
                    pending.push_back((raw_pid, depth + 1));
                }
            }
            if visited >= MAX_DESCENDANTS {
                break;
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
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

pub(crate) fn collect_nonblocking(
    reader: &mut impl Read,
    bytes: &mut Vec<u8>,
    limit: usize,
) -> bool {
    loop {
        let mut chunk = [0; 4096];
        match reader.read(&mut chunk) {
            Ok(0) => return true,
            Ok(count) => {
                assert!(bytes.len() + count <= limit, "bounded PTY transcript");
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return false,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("PTY read failed: {error}"),
        }
    }
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
    let (release_sender, release_ready) = mpsc::channel();
    let expect_resume = matches!(exit, ActiveExit::SuspendThenSignal) && !inline;
    let continued_draft_counts: Vec<_> = (RETAINED_DRAFT_BYTES + DRAFT_SEGMENT_BYTES
        ..arany::MAX_USER_MESSAGE_BYTES)
        .step_by(DRAFT_SEGMENT_BYTES)
        .collect();
    let overflow_stage = 4 + continued_draft_counts.len();
    let peer_gate_timeout = if expect_resume {
        DRAFT_STAGE_TIMEOUT * u32::try_from(overflow_stage + 3).expect("bounded draft stages")
    } else {
        DRAFT_STAGE_TIMEOUT
    };
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
            assert!(wire.get("text").is_none());
            if index == 2 {
                crate::loopback::assert_compaction_request(&wire);
            }
            let input: serde_json::Value = crate::semantic::input(&wire);
            if index == 3 {
                assert_eq!(input["objective"], "cancel");
                request_sender.send(()).expect("active request gate");
                if expect_resume || matches!(exit, ActiveExit::ProviderFailure) {
                    release_ready
                        .recv_timeout(peer_gate_timeout)
                        .expect("release held Provider call");
                }
                if matches!(exit, ActiveExit::ProviderFailure) {
                    // A Retry-After past the action deadline ends the call without a same-route retry.
                    stream
                        .write_all(b"HTTP/1.1 503 Unavailable\r\nRetry-After: 86400\r\nContent-Length: 8\r\nConnection: close\r\n\r\ntest-key")
                        .expect("synthetic failure response");
                    return;
                }
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("cancel deadline");
                let mut byte = [0];
                assert_eq!(
                    stream
                        .read_exact(&mut byte)
                        .expect_err("cancelled socket returned response bytes")
                        .kind(),
                    std::io::ErrorKind::UnexpectedEof,
                    "cancelled socket must close"
                );
                closed_sender.send(()).expect("socket closure observation");
                return;
            }
            assert!(
                input
                    .get("workspace_guidance")
                    .is_none_or(serde_json::Value::is_null)
            );
            let text = match index {
                0 => serde_json::json!("ok"),
                1 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"synthetic child","profile":null}]}})
                }
                _ => serde_json::json!(r#"synthetic summary"#),
            };
            if index == 2 {
                assert_eq!(text, "synthetic summary", "plain-text compaction reply");
            }
            send_response(&mut stream, index, text);
        }
    });
    check_profile(&workspace, &state);

    let command = if inline {
        "trap ':' INT; stty rows 24 cols 80; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" --classic --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider custom:local --model model-1 cancel; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
    } else {
        "trap ':' INT; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider custom:local --model model-1 cancel; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
    };
    let mut attached =
        crate::isolation::account_isolated_script(state.parent().expect("fixture root"));
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
    let mut output = attached.child().stdout.take().expect("PTY output");
    let (stage_sender, stages) = mpsc::channel();
    let (paste_sender, paste_ready) = mpsc::sync_channel(1);
    let expect_idle = matches!(
        exit,
        ActiveExit::CtrlC
            | ActiveExit::Escape
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
    let continued_draft_patterns: Vec<_> = continued_draft_counts
        .iter()
        .map(|count| format!("Draft: {count} characters; Enter offers it as a steering note"))
        .collect();
    // Classic returns to chat with a native notice instead of a shell receipt.
    let classic_idle = inline && expect_idle;
    let classic_label = if expect_failure {
        "Arany \u{b7} error:"
    } else {
        "Arany \u{b7} notice:"
    };
    let reader = thread::spawn(move || {
        let mut declined = false;
        let mut answered = 0;
        let mut keyboard_answered = 0;
        let mut bytes = Vec::new();
        let mut paste_sender = expect_draft.then_some(paste_sender);
        let retained_draft_pattern =
            format!("Draft: {RETAINED_DRAFT_BYTES} characters; Enter offers it as a steering note");
        let full_draft_pattern = format!(
            "Draft: {} characters; Enter offers it as a steering note",
            arany::MAX_USER_MESSAGE_BYTES
        );
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
            patterns.push(retained_draft_pattern.as_bytes());
            patterns.push(b"Presentation: screen-reader");
            patterns.extend(continued_draft_patterns.iter().map(String::as_bytes));
            patterns.push(b"Notice: Error: invalid or overlong terminal line; draft unchanged\r\n");
            patterns.push(full_draft_pattern.as_bytes());
        }
        if expect_second_ctrl_c {
            patterns.push(b"Cancelling Run...");
        }
        if expect_typo {
            patterns.push(b"Notice: Error: Unknown command; did you mean /provider?\r\n");
            patterns.push(b"Notice: Error: Usage: /status\r\n");
            patterns.push(b"Notice: Error: Usage: /help\r\n");
            patterns.push(b"Notice: Error: Usage: /exit\r\n");
            // The PTY may repeat CR before LF, so the whole line is matched here, not by `contains`.
            patterns.push(
                "Notice: Model: model-1 \u{b7} default; /model changes are unavailable while a Run is active; Ctrl+C cancels\r\n"
                    .as_bytes(),
            );
            patterns.push(b"Context left: ");
            patterns.push(b" (Arany request budget at admission)\r\n/usage for details\r\n");
        }
        if expect_help {
            patterns.push(if inline {
                // Only armed help has this token; cursor moves can replace the space before close.
                b"Enter/Esc"
            } else {
                b"Command: /exit; Exit Session; during Run: view-only\r\n"
            });
        }
        patterns.push(if classic_idle {
            classic_label.as_bytes()
        } else if expect_failure {
            b"Status: failed"
        } else {
            b"Status: cancelled"
        });
        if expect_feedback {
            patterns.push(if expect_failure {
                b"unavailable."
            } else {
                b"continue."
            });
        }
        if expect_draft {
            patterns.push(b"Draft retained: 9222 characters; Enter submits");
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
        loop {
            let mut chunk = [0; 4096];
            let count = output.read(&mut chunk).expect("PTY output bytes");
            if count == 0 {
                break;
            }
            assert!(
                bytes.len() + count <= ACTIVE_PTY_OUTPUT_BYTES,
                "bounded PTY output"
            );
            bytes.extend_from_slice(&chunk[..count]);
            crate::process::decline_workspace_consent(
                &mut *reader_input.lock().expect("fixture input"),
                &bytes,
                &mut declined,
            );
            let queries = bytes.windows(4).filter(|part| *part == b"\x1b[6n").count();
            assert!(
                inline || queries == 0,
                "linear ownership must not query cursor input"
            );
            assert!(queries <= 32, "bounded cursor queries");
            crate::reply_positioned_cursor_queries(
                &mut *reader_input.lock().expect("fixture input"),
                &bytes,
                &mut answered,
            );
            keyboard::answer_supported_queries(
                &mut *reader_input.lock().expect("fixture input"),
                &bytes,
                &mut keyboard_answered,
            );
            if paste_sender.is_some()
                && super::session_picker::screen_contains(
                    &bytes,
                    80,
                    24,
                    "Pasted Content 9216 chars",
                )
            {
                paste_sender.take().unwrap().send(()).unwrap();
            }
            while next < patterns.len() {
                let Some(matched) =
                    crate::process::pty_text_matches(&bytes[search_from..], patterns[next]).next()
                else {
                    break;
                };
                search_from += matched.end;
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
        let pasted = format!("@not-admitted /exit {}", "p".repeat(9196));
        assert_eq!(pasted.len(), 9 * 1024);
        write_input(&input, format!("\x1b[200~{pasted}\x1b[201~").as_bytes());
        paste_ready
            .recv_timeout(Duration::from_secs(10))
            .expect("chip rendered before the Run settles");
    }
    match exit {
        ActiveExit::ProviderFailure => {
            release_sender
                .send(())
                .expect("release failed Provider call");
        }
        ActiveExit::Signal(signal, _) => {
            kill_process(product_pid, signal).expect("deliver termination signal");
        }
        ActiveExit::CtrlC => write_input(&input, b"\x03"),
        ActiveExit::Escape => write_input(&input, b"\x1b"),
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
            write_input(&input, b"/model\n");
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
            while reader_nonblocking(product_pid) {
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
            kill_process(product_pid, Signal::CONT).expect("resume inline Run");
            let deadline = Instant::now() + Duration::from_secs(10);
            while tty_settings(product_pid)
                != *active_settings.as_ref().expect("active inline settings")
            {
                assert!(
                    Instant::now() < deadline,
                    "inline terminal did not reacquire"
                );
                thread::yield_now();
            }
            assert_eq!(
                tty_settings(product_pid),
                *active_settings.as_ref().expect("active inline settings"),
                "raw mode reacquired after resume"
            );
            kill_process(product_pid, Signal::TERM).expect("terminate resumed inline Run");
        }
        ActiveExit::SuspendThenSignal => {
            continue_draft(&input, b'a', DRAFT_SEGMENT_BYTES);
            continue_draft(&input, b'b', DRAFT_SEGMENT_BYTES);
            assert_eq!(
                stages
                    .recv_timeout(DRAFT_STAGE_TIMEOUT)
                    .expect("retained draft stage")
                    .0,
                2
            );
            assert!(reader_nonblocking(product_pid), "active canonical reader");
            kill_process(product_pid, Signal::TSTP).expect("deliver SIGTSTP");
            wait_stopped(product_pid);
            assert!(
                !reader_nonblocking(product_pid),
                "reader released before stop"
            );
            kill_process(product_pid, Signal::CONT).expect("resume active Run");
            assert_eq!(
                stages
                    .recv_timeout(DRAFT_STAGE_TIMEOUT)
                    .expect("resumed presentation stage")
                    .0,
                3
            );
            assert!(
                reader_nonblocking(product_pid),
                "reader reacquired after resume"
            );
            for index in 0..continued_draft_counts.len() {
                continue_draft(&input, b'c', DRAFT_SEGMENT_BYTES);
                assert_eq!(
                    stages
                        .recv_timeout(DRAFT_STAGE_TIMEOUT)
                        .expect("continued retained draft stage")
                        .0,
                    4 + index
                );
            }
            let remaining = arany::MAX_USER_MESSAGE_BYTES
                - continued_draft_counts
                    .last()
                    .copied()
                    .unwrap_or(RETAINED_DRAFT_BYTES);
            continue_draft(&input, b'c', remaining + 1);
            assert_eq!(
                stages
                    .recv_timeout(DRAFT_STAGE_TIMEOUT)
                    .expect("retained draft overflow stage")
                    .0,
                overflow_stage
            );
            continue_draft(&input, b'd', remaining);
            assert_eq!(
                stages
                    .recv_timeout(DRAFT_STAGE_TIMEOUT)
                    .expect("full retained draft stage")
                    .0,
                overflow_stage + 1
            );
            release_sender
                .send(())
                .expect("release resumed Provider call");
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
                overflow_stage + 2
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
            write_input(&input, b"\x1b[D\x1b[3~\x1b[3~\r");
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
        write_input(&input, if inline { b"/exit\r" } else { b"/exit\n" });
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
                overflow_stage + 3
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
    keyboard::assert_keyboard_restored(&output, inline);
    let transcript = String::from_utf8(output).expect("PTY transcript UTF-8");
    let restored = transcript.rfind("\x1b[?25h").unwrap_or(0);
    assert!(
        !transcript[restored..].contains("\x1b[6n"),
        "terminal restoration must not query input after stopping its reader"
    );
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
        | ActiveExit::Escape
        | ActiveExit::HelpThenCtrlC
        | ActiveExit::TypoThenCtrlC
        | ActiveExit::ProviderFailure => {}
        ActiveExit::PtyLoss => unreachable!("PTY loss returned after replay"),
    }
    let receipt_label = if classic_idle {
        classic_label.to_owned()
    } else {
        format!("Status: {expected_status}")
    };
    assert!(transcript.contains(&receipt_label));
    if expect_feedback {
        let receipt = transcript.find(&receipt_label).expect("terminal receipt");
        let mut after = receipt;
        if expect_failure {
            for word in ["Provider", "unavailable.", "Check", "the", "connection"] {
                let offset = transcript[after..]
                    .find(word)
                    .unwrap_or_else(|| panic!("missing failure cause word {word}: {preview}"));
                after += offset + word.len();
            }
        }
        // A cancelled plain prompt pauses its Goal, so only the resume notice follows the receipt.
        if expect_failure {
            for word in ["No", "answer", "was", "committed."] {
                let offset = transcript[after..]
                    .find(word)
                    .unwrap_or_else(|| panic!("missing chat feedback word {word}: {preview}"));
                after += offset + word.len();
            }
        }
        assert!(
            transcript[receipt..].contains(match (inline, expect_failure) {
                (true, _) => classic_label,
                (false, true) => "Notice: Error: Run failed.",
                (false, false) => "Notice: Goal paused; use /goal resume to continue.",
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
            for name in arany_cli::terminal::command_completions("/") {
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
    assert!(
        !transcript.contains("@not-admitted"),
        "unsent payload stays collapsed and undisclosed"
    );
    assert!(!transcript.contains("test-key"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only replay runtime");
    let session_id = if classic_idle {
        let sessions = runtime
            .block_on(arany::list_sessions(
                StateRoot::open_existing(&state).expect("state reopen"),
                workspace.clone(),
            ))
            .expect("list committed Sessions");
        assert_eq!(sessions.len(), 1);
        sessions[0].id
    } else {
        let receipt_start = transcript[..transcript
            .find(&receipt_label)
            .expect("terminal receipt status")]
            .rfind("Session: ")
            .expect("receipt Session field");
        transcript[receipt_start..]
            .lines()
            .next()
            .expect("receipt Session line")
            .trim_end_matches('\r')
            .strip_prefix("Session: ")
            .expect("receipt Session label")
            .parse::<SessionId>()
            .expect("Session ID")
    };
    let store = Store::open_read_only(StateRoot::open_existing(&state).expect("state reopen"))
        .expect("read-only Store");
    let events = runtime
        .block_on(store.load_session(session_id))
        .expect("Events");
    let view = SessionView::replay(session_id, &events)
        .expect("strict replay")
        .expect("Session");
    assert!(
        events.iter().all(|event| !event
            .event
            .payload()
            .expect("payload")
            .contains("@not-admitted")),
        "unsubmitted paste never enters the journal"
    );
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
            "Context left: {} (Arany request budget at admission)\r\n/usage for details\r\n",
            arany_cli::presentation::format_bytes(u64::from(
                usage.budget_bytes.saturating_sub(usage.used_bytes)
            )),
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
        usize::from(!classic_idle),
        "one committed shell receipt outside Classic chat"
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
fn escape_during_classic_provider_call_cancels_and_leaves_the_goal_paused() {
    active_exit_during_provider_call(ActiveExit::Escape, true);
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

#[path = "active_terminal/tool_group.rs"]
pub(crate) mod tool_group;

#[path = "active_terminal/acquisition.rs"]
mod acquisition;
pub(super) use acquisition::ProductGuard;

#[path = "active_terminal/panic.rs"]
mod panic;

#[path = "active_terminal/steering.rs"]
mod steering;

#[path = "active_terminal/clipboard_copy.rs"]
mod clipboard_copy;

#[cfg(target_os = "linux")]
#[path = "active_terminal/tmux.rs"]
mod tmux;

#[test]
fn classic_dialog_takes_its_content_height_and_closes_under_the_latest_history() {
    use super::session_picker::{PtyResponses, pump, tail};
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let id = crate::seed_transcript_session(&state, &workspace);
    let shell = "stty rows 50 cols 80; printf 'SHELL_PID:%s\\n' \"$$\"; \"$ARANY_TEST_EXE\" --classic --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\"; result=$?; exit \"$result\"";
    let mut command = crate::isolation::account_isolated_script(temp.path());
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_SESSION", id.to_string())
        .env("TERM", "xterm")
        .env("SHELL", "/bin/sh")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().unwrap());
    let mut input = child.child().stdin.take().unwrap();
    let mut output = child.child().stdout.take().unwrap();
    let flags = fcntl_getfl(&output).unwrap();
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).unwrap();
    let mut transcript = Vec::new();
    let mut answered = PtyResponses::default();
    // The latest composer caret row (0-based) drawn after `from`, decoded with the screen.
    let caret = |transcript: &[u8], from: usize| {
        let marker = b"\x1b[?25h\x1b[";
        let start = from
            + transcript[from..]
                .windows(marker.len())
                .rposition(|part| part == marker)?
            + marker.len();
        let digits = transcript[start..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        transcript[start + digits..].starts_with(b";3H").then(|| {
            let row = String::from_utf8_lossy(&transcript[start..start + digits])
                .parse::<usize>()
                .unwrap();
            (
                row - 1,
                crate::terminal_grid(&transcript[..start + digits + 3], 80, 50),
            )
        })
    };
    let objectives = |(scrollback, screen): &(Vec<String>, Vec<String>)| {
        scrollback
            .iter()
            .chain(screen)
            .filter(|row| row.contains("Synthetic objective"))
            .count()
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let (draft_row, idle) = loop {
        if let Some(found) = caret(&transcript, 0) {
            break found;
        }
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "idle composer: {}",
            tail(&transcript)
        );
        thread::yield_now();
    };
    let text = String::from_utf8_lossy(&transcript);
    let pid = product_child_of(transcript_field(&text, "SHELL_PID:").parse().unwrap());
    let _product = ProductGuard::new(pid, state.clone());
    super::assert_history_above(&idle.1, draft_row - 1, "idle Classic at 50 rows");
    let history = objectives(&idle);
    assert!(history > 0, "seeded history visible or in scrollback");

    let opened_at = transcript.len();
    input.write_all(b"\x0b").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let dialog = loop {
        if let Some(screen) = crate::armed_selector(&transcript, opened_at, 80, 50) {
            break screen;
        }
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "armed quick actions: {}",
            tail(&transcript[opened_at..])
        );
        thread::yield_now();
    };
    let top = dialog
        .iter()
        .position(|row| row.starts_with("Quick actions"))
        .expect("quick actions header");
    // The header, five actions and footer take only their content height, right under history.
    super::assert_history_above(&dialog, top, "open dialog");
    assert!(dialog[top + 5].contains("Usage · reported tokens"));
    assert!(dialog[top + 6].contains("1-5 or Up/Down · Enter open · Esc return"));
    assert_eq!(top + 7, 50, "no reserved rows below the dialog: {dialog:?}");

    let closed_at = transcript.len();
    input.write_all(b"\x1b").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let (draft_row, closed) = loop {
        if let Some(found) = caret(&transcript, closed_at) {
            break found;
        }
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "composer after the dialog: {}",
            tail(&transcript[closed_at..])
        );
        thread::yield_now();
    };
    // Closing shrinks to the composer at the dialog's top, still under the same history.
    assert_eq!(
        draft_row - 1,
        top,
        "composer keeps the dialog top: {:?}",
        closed.1
    );
    super::assert_history_above(&closed.1, top, "closed dialog");
    assert!(
        closed.1[top + 4..].iter().all(|row| row.trim().is_empty()),
        "no stale dialog rows below the composer: {:?}",
        closed.1
    );
    assert_eq!(
        objectives(&closed),
        history,
        "scrollback is intact and never duplicated"
    );

    input.write_all(b"/exit\r").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.child().try_wait().unwrap().is_none() {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "Classic exit: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    assert!(wait_product(child.take()).status.success());
}

#[test]
fn explicit_classic_uses_native_scrollback_and_dumps_bounded_details() {
    use super::session_picker::{PtyResponses, pump, tail};
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let id = crate::seed_transcript_session(&state, &workspace);
    assert!(!state.join("ui-preferences.json").exists());
    let shell = "stty rows 24 cols 80; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" --classic --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\"; result=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$result\"";
    let mut command = crate::isolation::account_isolated_script(temp.path());
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_SESSION", id.to_string())
        .env("TERM", "xterm")
        .env("SHELL", "/bin/sh")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().unwrap());
    let mut input = child.child().stdin.take().unwrap();
    let mut output = child.child().stdout.take().unwrap();
    let flags = fcntl_getfl(&output).unwrap();
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).unwrap();
    let mut transcript = Vec::new();
    let mut answered = PtyResponses::default();
    let wait = |mut output: &mut dyn Read,
                mut input: &mut dyn Write,
                transcript: &mut Vec<u8>,
                answered: &mut PtyResponses,
                from: usize,
                needle: &[u8]| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !transcript[from..]
            .windows(needle.len())
            .any(|part| part == needle)
        {
            pump(&mut output, &mut input, transcript, answered);
            assert!(
                Instant::now() < deadline,
                "Classic PTY: {}",
                tail(transcript)
            );
            thread::yield_now();
        }
        pump(&mut output, &mut input, transcript, answered);
    };
    // The idle composer caret, found by the composer border wherever the viewport sits.
    let wait_caret = |mut output: &mut dyn Read,
                      mut input: &mut dyn Write,
                      transcript: &mut Vec<u8>,
                      answered: &mut PtyResponses,
                      from: usize| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !super::composer_caret(transcript, from, 80, 24)
            .is_some_and(|(column, _)| column == 3)
        {
            pump(&mut output, &mut input, transcript, answered);
            assert!(
                Instant::now() < deadline,
                "Classic composer caret: {}",
                tail(&transcript[from..])
            );
            thread::yield_now();
        }
    };
    wait(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        0,
        b"TTY_BEFORE:",
    );
    let text = String::from_utf8_lossy(&transcript);
    let pid = product_child_of(transcript_field(&text, "SHELL_PID:").parse().unwrap());
    let _product = ProductGuard::new(pid, state.clone());
    wait_caret(&mut output, &mut input, &mut transcript, &mut answered, 0);
    let first_query = transcript
        .windows(4)
        .position(|part| part == b"\x1b[6n")
        .expect("Classic startup cursor query");
    assert!(
        transcript[..first_query].ends_with(b"\x1b[24;1H"),
        "Classic reserves rows below any existing shell output"
    );
    // Idle Classic takes only its content, so the composer border sits under history.
    let (_, screen) = super::composer_caret(&transcript, 0, 80, 24).expect("idle composer");
    let border = screen
        .iter()
        .rposition(|row| row.starts_with("─ Ask Arany"))
        .expect("composer border");
    super::assert_history_above(&screen, border, "idle Classic");
    // The transcript gesture follows the ignored page keys in reader order.
    let from = transcript.len();
    input.write_all(b"\x1b[5~\x1b[6~\x0f").unwrap();
    wait(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        from,
        b"reached;",
    );
    wait_caret(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        from,
    );
    let dump = &transcript[from..];
    let heading = dump
        .windows(b"Transcript".len())
        .position(|part| part == b"Transcript")
        .unwrap();
    let prelude = dump[..heading].to_vec();
    assert_eq!(
        dump.windows(b"DETAIL_CANARY".len())
            .filter(|part| *part == b"DETAIL_CANARY")
            .count(),
        16
    );
    assert!(!dump.windows(5).any(|part| part == b"\x1b]52;"));
    for gesture in [b"\x0f".as_slice(), b"/transcript\r"] {
        let from = transcript.len();
        input.write_all(gesture).unwrap();
        wait(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"reached;",
        );
        wait_caret(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
        );
        if gesture == b"\x0f" {
            let dump = &transcript[from..];
            let heading = dump
                .windows(b"Transcript".len())
                .position(|part| part == b"Transcript")
                .unwrap();
            assert_eq!(
                &dump[..heading],
                prelude,
                "page keys must produce no output"
            );
        }
    }
    for (command, needle) in [
        (&b"/resume\r"[..], &b"Resume \xc2\xb7"[..]),
        (b"/agents\r", b"Last"),
        (b"/help\r", b"local controls"),
    ] {
        let from = transcript.len();
        input.write_all(command).unwrap();
        wait(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            needle,
        );
        // A selector takes keys only once its armed frame is drawn.
        let deadline = Instant::now() + Duration::from_secs(10);
        while crate::armed_selector(&transcript, from, 80, 24).is_none() {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            assert!(
                Instant::now() < deadline,
                "armed {}: {}",
                String::from_utf8_lossy(command).escape_debug(),
                tail(&transcript[from..])
            );
            thread::yield_now();
        }
        let close_from = transcript.len();
        input.write_all(b"\x1b").unwrap();
        wait_caret(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            close_from,
        );
        // Closing shrinks the viewport by clearing only its own rows, never the whole screen.
        assert!(
            !transcript[close_from..]
                .windows(4)
                .any(|part| part == b"\x1b[2J"),
            "settled history must not be cleared"
        );
    }
    let exit_from = transcript.len();
    input.write_all(b"/exit\r").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.child().try_wait().unwrap().is_none() {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(Instant::now() < deadline, "Classic exit");
        thread::yield_now();
    }
    assert!(wait_product(child.take()).status.success());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    super::assert_classic_owned_clear(&transcript, exit_from);
    let text = String::from_utf8_lossy(&transcript);
    for mode in [1049, 1000, 1002, 1003, 1006, 1007, 1015] {
        assert!(
            !text.contains(&format!("\x1b[?{mode}h")),
            "Classic enabled {mode}"
        );
    }
    assert_eq!(
        transcript_field(&text, "TTY_BEFORE:"),
        transcript_field(&text, "TTY_AFTER:")
    );
}

#[path = "active_terminal/keyboard.rs"]
mod keyboard;

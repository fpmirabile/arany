use super::{ProductGuard, product_child_of, redacted_tail, transcript_field, tty_settings};
use crate::loopback::{
    ChildGuard, check_profile, read_request, send_response, wait_product, write_profile,
};
use arany::{RunStatus, SessionId, SessionView, StateRoot, Store};
use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    pty::{OpenptFlags, grantpt, ioctl_tiocgptpeer, openpt, ptsname, unlockpt},
    termios::{Winsize, tcsetwinsize},
};
use std::{
    fs::File,
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn collect_nonblocking(reader: &mut impl Read, bytes: &mut Vec<u8>, limit: usize) -> bool {
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

#[test]
fn broken_stderr_during_active_run_restores_raw_mode_and_cancels_without_answer() {
    for compaction in [false, true] {
        run_broken_stderr_case(compaction);
    }
}

fn run_broken_stderr_case(compaction: bool) {
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
    let (fault_sender, fault_ready) = mpsc::channel();
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).expect("bounded accept");
        for index in 0..if compaction { 5 } else { 4 } {
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
            if index == if compaction { 4 } else { 3 } {
                if compaction {
                    let wire: serde_json::Value =
                        serde_json::from_slice(&body).expect("compaction request");
                    let input: serde_json::Value =
                        serde_json::from_str(wire["input"].as_str().expect("input text"))
                            .expect("semantic compaction input");
                    assert_eq!(wire["max_output_tokens"], 1024);
                    assert_eq!(input["items"].as_array().expect("history items").len(), 1);
                    assert!(input.get("workspace_guidance").is_none());
                    assert!(input.get("includes").is_none());
                }
                request_sender.send(()).expect("active request gate");
                fault_ready
                    .recv_timeout(Duration::from_secs(10))
                    .expect("renderer fault gate");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("cancel deadline");
                let mut byte = [0];
                assert_eq!(
                    stream.read(&mut byte).unwrap_or_else(|error| {
                        panic!("cancelled socket, compaction={compaction}: {error}")
                    }),
                    0,
                    "cancelled socket, compaction={compaction}"
                );
                return;
            }
            let text = match index {
                0 => serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"ok"}}),
                1 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":["synthetic child"]}})
                }
                2 => serde_json::json!({"summary":"synthetic summary"}),
                _ => {
                    serde_json::json!({"outcome":{"type":"finish","summary":"committed summary","result":"committed answer"}})
                }
            };
            send_response(&mut stream, index, text);
        }
    });
    check_profile(&workspace, &state);

    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)
        .expect("stderr PTY master");
    grantpt(&master).expect("grant stderr PTY");
    unlockpt(&master).expect("unlock stderr PTY");
    let slave = ioctl_tiocgptpeer(
        &master,
        OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC,
    )
    .expect("stderr PTY slave");
    tcsetwinsize(
        &slave,
        Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .expect("stderr PTY size");
    let slave_path = ptsname(&master, Vec::new())
        .expect("stderr PTY path")
        .into_string()
        .expect("UTF-8 PTY path");
    let mut stderr_master = File::from(master);
    let mut stderr_probe = File::from(slave);
    let flags = fcntl_getfl(&stderr_master).expect("stderr master flags");
    fcntl_setfl(&stderr_master, flags | OFlags::NONBLOCK).expect("nonblocking stderr master");

    let command = "stty rows 24 cols 80; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; exec 2>\"$ARANY_TEST_STDERR_PTY\"; \"$ARANY_TEST_EXE\" --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider custom:local --model model-1 cancel; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\"";
    let mut attached = Command::new("/usr/bin/script");
    attached
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_STDERR_PTY", &slave_path)
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut attached = ChildGuard::new(attached.spawn().expect("attached PTY process"));
    let mut input = attached.child().stdin.take().expect("PTY input");
    let mut output = attached.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout pipe flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking stdout pipe");
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut answered_queries = 0;
    let mut compaction_sent = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        collect_nonblocking(&mut output, &mut stdout_bytes, 64 * 1024);
        collect_nonblocking(&mut stderr_master, &mut stderr_bytes, 64 * 1024);
        let queries = stdout_bytes
            .windows(4)
            .chain(stderr_bytes.windows(4))
            .filter(|part| *part == b"\x1b[6n")
            .count();
        assert!(queries <= 32, "bounded cursor-position queries");
        while answered_queries < queries {
            input.write_all(b"\x1b[24;1R").expect("cursor response");
            answered_queries += 1;
        }
        let receipt = b"Status: finished";
        if compaction
            && !compaction_sent
            && let Some(receipt_at) = stderr_bytes
                .windows(receipt.len())
                .position(|part| part == receipt)
            && stderr_bytes[receipt_at..]
                .windows(6)
                .any(|part| part == b"\x1b[?25h")
        {
            input
                .write_all(b"/compact\r")
                .expect("manual compaction after completed Run");
            compaction_sent = true;
        }
        match request_ready.try_recv() {
            Ok(()) => break,
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => panic!("active request gate closed"),
        }
        assert!(Instant::now() < deadline, "active request not reached");
        thread::yield_now();
    }
    collect_nonblocking(&mut stderr_master, &mut stderr_bytes, 64 * 1024);
    let progress = if compaction {
        b"Compacting".as_slice()
    } else {
        b"working".as_slice()
    };
    assert!(
        stderr_bytes
            .windows(progress.len())
            .any(|part| part == progress)
            && stderr_bytes.windows(2).any(|part| part == b"\x1b["),
        "inline rendering reached stderr PTY; stderr: {}; stdout: {}",
        redacted_tail(&stderr_bytes),
        redacted_tail(&stdout_bytes)
    );
    let stdout_text = String::from_utf8_lossy(&stdout_bytes);
    let shell_pid = transcript_field(&stdout_text, "SHELL_PID:")
        .parse::<u32>()
        .expect("shell PID");
    let before = transcript_field(&stdout_text, "TTY_BEFORE:").to_owned();
    let product_pid = product_child_of(shell_pid);
    let _product_guard = ProductGuard::new(product_pid, state.clone());
    assert_ne!(tty_settings(product_pid), before, "inline raw mode active");

    drop(stderr_master);
    let flags = fcntl_getfl(&stderr_probe).expect("stderr probe flags");
    fcntl_setfl(&stderr_probe, flags | OFlags::NONBLOCK).expect("nonblocking stderr probe");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match stderr_probe.write(b"x") {
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) => {
                break;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("stderr probe failed: {error}"),
        }
        assert!(Instant::now() < deadline, "stderr did not fail writes");
        thread::yield_now();
    }
    input
        .write_all(b"x")
        .expect("trigger redraw after stderr fault");
    fault_sender.send(()).expect("renderer fault triggered");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        collect_nonblocking(&mut output, &mut stdout_bytes, 64 * 1024);
        if attached
            .child()
            .try_wait()
            .expect("attached status")
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "broken stderr did not end attached process"
        );
        thread::yield_now();
    }
    let result = wait_product(attached.take());
    collect_nonblocking(&mut output, &mut stdout_bytes, 64 * 1024);
    server.join().expect("synthetic server completed");
    assert!(
        !result.status.success(),
        "broken renderer exits unsuccessfully"
    );
    assert_eq!(result.stderr, b"");
    let transcript = String::from_utf8(stdout_bytes).expect("PTY transcript UTF-8");
    let after = transcript
        .split_once("TTY_AFTER:")
        .expect("restored terminal marker")
        .1
        .lines()
        .next()
        .expect("restored terminal settings")
        .trim_end_matches('\r');
    assert_eq!(
        transcript_field(&transcript, "TTY_BEFORE:"),
        after,
        "raw mode restored after broken stderr: {}",
        redacted_tail(transcript.as_bytes())
    );
    if compaction {
        assert!(compaction_sent);
        assert_eq!(transcript.matches("committed answer").count(), 1);
    } else {
        assert!(!transcript.contains("Answer:"));
    }

    let connection = rusqlite::Connection::open_with_flags(
        state.join("events.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("read-only Session ID lookup");
    let session_id: String = connection
        .query_row("SELECT session_id FROM events LIMIT 1", [], |row| {
            row.get(0)
        })
        .expect("Session Event");
    drop(connection);
    let session_id = session_id.parse::<SessionId>().expect("typed Session ID");
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
    assert!(
        view.compactions.is_empty(),
        "interrupted compaction commits no result in this fixture"
    );
    if compaction {
        assert_eq!(view.runs[0].status, RunStatus::Finished);
        assert_eq!(
            view.runs[0].assistant_message.as_deref(),
            Some("committed answer")
        );
    } else {
        assert_eq!(view.runs[0].status, RunStatus::Cancelled);
        assert!(view.runs[0].assistant_message.is_none());
    }
    runtime.block_on(store.close()).expect("close Store");
}

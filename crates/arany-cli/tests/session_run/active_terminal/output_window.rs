use super::*;

// Classic inserts the confirmed answer above its viewport while retaining ownership.
const NATIVE_ANSWER: &[u8] = "\u{25cf} \x1b[1mArany:".as_bytes();

#[test]
fn sigterm_during_restored_output_preserves_receipt_and_terminal() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    std::fs::write(workspace.join("private.txt"), b"OMITTED_CANARY")
        .expect("omitted Workspace input");
    let state = temp.path().join("state");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback Provider");
    write_profile(
        &state,
        listener.local_addr().expect("Provider address").port(),
    );
    listener.set_nonblocking(true).expect("bounded accept");
    let (request_sender, request_ready) = mpsc::channel();
    let (response_sender, response_ready) = mpsc::channel();
    let server = thread::spawn(move || {
        let mut stream = crate::process::accept_with_deadline(
            &listener,
            Duration::from_secs(30),
            "Provider request",
        );
        let body = read_request(&mut stream);
        assert!(
            !body
                .windows(b"OMITTED_CANARY".len())
                .any(|part| part == b"OMITTED_CANARY")
        );
        request_sender.send(()).expect("active Provider gate");
        response_ready
            .recv_timeout(Duration::from_secs(10))
            .expect("response release");
        let text = serde_json::json!("A".repeat(32 * 1024));
        super::super::loopback::send_response_for(&mut stream, 0, text, true);
    });

    let command = "trap ':' INT; stty rows 24 cols 80; printf 'SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; \"$ARANY_TEST_EXE\" --classic --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider custom:local --model model-1 finish; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";
    let mut attached =
        crate::isolation::account_isolated_script(state.parent().expect("fixture root"));
    attached
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
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
    assert_eq!(
        rustix::pipe::fcntl_setpipe_size(&output, 4096).expect("bounded output pipe"),
        4096
    );
    let (stage_sender, stages) = mpsc::channel();
    let (resume_sender, resume_reader) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut declined = false;
        let mut bytes = Vec::new();
        let mut working_reported = false;
        let mut answer_reported = false;
        let mut answered_queries = 0;
        loop {
            let mut chunk = [0; 1024];
            let count = output.read(&mut chunk).expect("PTY output");
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
            let queries = bytes.windows(4).filter(|part| *part == b"\x1b[6n").count();
            assert!(queries <= 32, "bounded cursor-position queries");
            crate::reply_positioned_cursor_queries(
                &mut *reader_input.lock().expect("fixture input"),
                &bytes,
                &mut answered_queries,
            );
            if !working_reported
                && bytes
                    .windows(b"TTY_BEFORE:".len())
                    .position(|part| part == b"TTY_BEFORE:")
                    .is_some_and(|start| bytes[start..].windows(2).any(|part| part == b"\r\n"))
            {
                stage_sender
                    .send((0, bytes.clone()))
                    .expect("working stage");
                working_reported = true;
            }
            if !answer_reported
                && bytes
                    .windows(NATIVE_ANSWER.len())
                    .any(|part| part == NATIVE_ANSWER)
            {
                stage_sender.send((1, bytes.clone())).expect("answer stage");
                answer_reported = true;
                resume_reader
                    .recv_timeout(Duration::from_secs(10))
                    .expect("resume output reader");
            }
        }
        bytes
    });
    request_ready
        .recv_timeout(Duration::from_secs(10))
        .expect("active Provider request");
    let working = stages
        .recv_timeout(Duration::from_secs(10))
        .expect("working output stage");
    assert_eq!(working.0, 0);
    let working_text = String::from_utf8_lossy(&working.1);
    let shell_pid = transcript_field(&working_text, "SHELL_PID:")
        .parse::<u32>()
        .expect("shell PID");
    let before = transcript_field(&working_text, "TTY_BEFORE:").to_owned();
    let product_pid = product_child_of(shell_pid);
    assert_ne!(tty_settings(product_pid), before, "inline raw mode active");
    response_sender.send(()).expect("release Provider response");
    let answer = stages
        .recv_timeout(Duration::from_secs(10))
        .expect("restored output stage");
    assert_eq!(answer.0, 1);
    assert_ne!(
        tty_settings(product_pid),
        before,
        "Classic retains terminal ownership during native output"
    );
    kill_process(product_pid, Signal::TERM).expect("signal during restored output");
    resume_sender.send(()).expect("resume output drain");
    let result = wait_product(attached.take());
    let output = reader.join().expect("PTY output reader");
    server.join().expect("synthetic Provider server");
    assert!(!result.status.success(), "signal exits attached Session");
    assert_eq!(result.stderr, b"");
    let transcript = String::from_utf8(output).expect("PTY transcript UTF-8");
    assert_eq!(transcript_field(&transcript, "TTY_AFTER:"), before);
    assert!(transcript.contains("error: terminated by SIGTERM"));
    assert!(
        !transcript.contains("Status: "),
        "Classic keeps no shell receipt"
    );
    assert!(!transcript.contains("OMITTED_CANARY"));
    assert!(!transcript.contains("test-key"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("replay runtime");
    let sessions = runtime
        .block_on(arany::list_sessions(
            StateRoot::open_existing(&state).expect("state reopen"),
            workspace,
        ))
        .expect("list committed Sessions");
    assert_eq!(sessions.len(), 1);
    let session_id = sessions[0].id;
    let store = Store::open_read_only(StateRoot::open_existing(&state).expect("state reopen"))
        .expect("read-only Store");
    let events = runtime
        .block_on(store.load_session(session_id))
        .expect("persisted Events");
    let view = SessionView::replay(session_id, &events)
        .expect("strict replay")
        .expect("Session");
    assert_eq!(view.runs.len(), 1);
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(
        view.runs[0].assistant_message.as_ref().map(String::len),
        Some(32 * 1024)
    );
    runtime.block_on(store.close()).expect("close Store");
}

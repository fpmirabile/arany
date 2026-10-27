use super::*;

#[test]
fn inline_quick_selector_returns_to_the_same_draft_and_caret() {
    for merged_overflow in [false, true] {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state = temp.path().join("state");
        for index in 0..16 {
            std::fs::write(
                workspace.join(format!("included-{index}.txt")),
                b"synthetic",
            )
            .unwrap();
        }
        let outside = temp.path().join("outside.txt");
        std::fs::write(&outside, b"OUTSIDE_INPUT_CANARY").unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("linkZ")).unwrap();
        let shell = "stty rows 24 cols 40; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; \"$ARANY_TEST_EXE\" --classic --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider openai --model gpt-5.4 --no-color $ARANY_TEST_INCLUDES; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";
        let includes = if merged_overflow {
            (0..16)
                .map(|index| format!("--include included-{index}.txt"))
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            String::new()
        };
        let mut command =
            crate::isolation::account_isolated_script(state.parent().expect("fixture root"));
        command
            .env_clear()
            .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
            .env("ARANY_TEST_STATE", &state)
            .env("ARANY_TEST_INCLUDES", includes)
            .env("ARANY_TEST_WORKSPACE", &workspace)
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm")
            .env("OPENAI_API_KEY", "synthetic-never-sent-key")
            .current_dir(&workspace)
            .args(["-q", "-e", "-c", shell, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ChildGuard::new(command.spawn().expect("attached PTY process"));
        let mut input = child.child().stdin.take().expect("PTY input");
        let mut output = child.child().stdout.take().expect("PTY output");
        let flags = fcntl_getfl(&output).expect("stdout flags");
        fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
        let mut transcript = Vec::new();
        let mut answered = crate::session_picker::PtyResponses::default();
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"Ask Arany",
        );
        let selector_at = transcript.len();
        input
            .write_all("a🙂b\x1b[D\x0b".as_bytes())
            .expect("draft and selector");
        wait_armed(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            selector_at,
        );
        let after_menu = transcript.len();
        input
            .write_all(b"\x1b[200~INERT_MODAL_PASTE\r/exit\x03\x1b[201~\x1b")
            .expect("ignore paste in selector and close without changing draft");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            after_menu,
            b"Ask Arany",
        );
        input.write_all(b"X").expect("edit returned draft");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"Xb",
        );
        let selector_at = transcript.len();
        input.write_all(b"\x0b").expect("open quick selector");
        wait_armed(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            selector_at,
        );
        let inspector_at = transcript.len();
        input.write_all(b"\x1b[B\r").expect("open Agent inspector");
        // With no Runs the inspector has no rows; its key hint is the arming cue.
        wait_for_frame(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            (inspector_at, 40, 24),
            |screen| {
                screen.iter().any(|row| row.contains("No Runs yet"))
                    && screen.iter().any(|row| row.contains("Esc close"))
            },
        );
        let after_inspector = transcript.len();
        input.write_all(b"\x1b").expect("close inspector");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            after_inspector,
            b"Ask Arany",
        );
        input.write_all(b"Y").expect("edit returned draft");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"Yb",
        );
        let usage_at = transcript.len();
        input.write_all(b"\x0b").expect("usage quick selector");
        wait_armed(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            usage_at,
        );
        input
            .write_all(b"5")
            .expect("inspect usage over Unicode draft");
        wait_for_frame(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            (usage_at, 40, 24),
            |screen| {
                screen.iter().any(|row| row.contains("no calls yet"))
                    && screen.iter().any(|row| row.contains("Esc close"))
            },
        );
        let after_usage = transcript.len();
        input.write_all(b"\x1b").expect("close usage");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            after_usage,
            b"Ask Arany",
        );
        input.write_all(b"U").expect("edit at retained usage caret");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"Ub",
        );
        let after_inspection = transcript.len();
        input.write_all(b"\x0b").expect("reopen quick selector");
        wait_armed(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            after_inspection,
        );
        input
            .write_all(b"\x1b[B\x1b[B\r")
            .expect("choose guarded Session action");
        wait_for_after(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            after_inspection,
            b"Submit",
        );
        input.write_all(b"Z").expect("edit after guarded action");
        wait_for(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            b"Zb",
        );
        let cases = if merged_overflow {
            vec![("Check @extra.txtZ", b"includes;".as_slice())]
        } else {
            vec![
                ("Check @missing.txtZ", b"exist;".as_slice()),
                ("Check @linkZ", b"symbolic".as_slice()),
            ]
        };
        for (draft, diagnostic) in cases {
            let from = transcript.len();
            input.write_all(b"\x03").unwrap();
            input.write_all(draft.as_bytes()).unwrap();
            input.write_all(b"\x1b[D\r").unwrap();
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                diagnostic,
            );
            let after_diagnostic = from
                + transcript[from..]
                    .windows(diagnostic.len())
                    .position(|part| part == diagnostic)
                    .unwrap()
                + diagnostic.len();
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                after_diagnostic,
                b"\x1b[?25h",
            );
            let from = transcript.len();
            input.write_all(b"X").unwrap();
            wait_for_after(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                b"XZ",
            );
        }
        input
            .write_all(b"\x03/exit\r")
            .expect("clear draft and quit");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            if child.child().try_wait().expect("process status").is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "PTY exit: {}", tail(&transcript));
            thread::yield_now();
        }
        let status = child.take().wait().expect("process exit");
        assert!(status.success(), "product failed: {}", tail(&transcript));
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        let text = String::from_utf8_lossy(&transcript);
        let marker = |name: &str| {
            text.lines()
                .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
                .expect("TTY marker")
                .to_owned()
        };
        assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
        assert!(
            !text.contains("INERT_MODAL_PASTE"),
            "ignored modal paste entered the draft"
        );
        assert!(
            !text[text.find("Ask Arany").expect("initial composer")..]
                .contains("Do you trust this folder?"),
            "invalid inputs must be rejected before trust or Run admission"
        );
        if state.join("events.sqlite3").exists() {
            let connection = rusqlite::Connection::open_with_flags(
                state.join("events.sqlite3"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let count: i64 = connection
                .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
                .unwrap();
            assert_eq!(count, 0, "invalid inputs must not materialize a Session");
        }
        let enabled = text.matches("\x1b[?2004h").count();
        let disabled = text.matches("\x1b[?2004l").count();
        assert!(
            enabled > 0 && enabled == disabled,
            "paste mode was not restored"
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("read-only runtime");
        runtime.block_on(async {
            let root = StateRoot::open_existing(&state).expect("existing State");
            let sessions = list_sessions(root, workspace.clone())
                .await
                .expect("listed Sessions");
            assert!(
                sessions.is_empty(),
                "selector navigation and retained drafts save no conversation"
            );
        });
    }
}

fn session_picker_close_case(quick: bool) {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let saved_id = saved_conversation(&state, &workspace, SessionDefaults::default());
    let shell = "stty rows 24 cols 40; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; \"$ARANY_TEST_EXE\" --classic --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider openai --no-color; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\"";
    let mut command =
        crate::isolation::account_isolated_script(state.parent().expect("fixture root"));
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("attached PTY process"));
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
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
        (0, 40, 24),
        "Ask Arany",
    );
    capture_from = checkpoint_screen(
        &mut transcript,
        &mut answered,
        &mut history,
        (40, 24),
        capture_from,
    );
    let mut before_picker = capture_from;
    if quick {
        input.write_all(b"\x0b").expect("quick actions");
        wait_armed(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            before_picker,
        );
        capture_from = checkpoint_screen(
            &mut transcript,
            &mut answered,
            &mut history,
            (40, 24),
            capture_from,
        );
        before_picker = capture_from;
        input.write_all(b"\x1b[B\x1b[B\r").expect("Session action");
    } else {
        input.write_all(b"/resume\r").expect("Session command");
    }
    wait_for_frame(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        (before_picker, 40, 24),
        |screen| {
            screen.iter().any(|row| row.contains("1 in this Workspace"))
                && screen.iter().any(|row| row.starts_with("› 1."))
        },
    );
    capture_from = checkpoint_screen(
        &mut transcript,
        &mut answered,
        &mut history,
        (40, 24),
        capture_from,
    );
    let before_close = capture_from;
    input.write_all(b"\x1b").expect("close picker");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        let recent = &transcript[before_close..];
        assert!(
            !screen_contains(&transcript, 40, 24, "failed"),
            "picker close failed: {}",
            tail(&transcript)
        );
        if screen_contains(&transcript, 40, 24, "closed")
            && recent.windows(6).any(|part| part == b"\x1b[?25h")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "picker close: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    capture_from = checkpoint_screen(
        &mut transcript,
        &mut answered,
        &mut history,
        (40, 24),
        capture_from,
    );
    input.write_all(b"/exit\r").expect("exit");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        if child.child().try_wait().expect("process status").is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "PTY exit: {}", tail(&transcript));
        thread::yield_now();
    }
    let status = child.take().wait().expect("process exit");
    assert!(status.success(), "product failed: {}", tail(&transcript));
    checkpoint_screen(
        &mut transcript,
        &mut answered,
        &mut history,
        (40, 24),
        capture_from,
    );
    let text = String::from_utf8_lossy(&history);
    let marker = |name: &str| {
        text.lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
            .expect("TTY marker")
            .to_owned()
    };
    assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("replay runtime")
        .block_on(async {
            let sessions =
                list_sessions(StateRoot::open_existing(&state).expect("State"), workspace)
                    .await
                    .expect("closed history");
            assert_eq!(
                sessions.len(),
                1,
                "opening and closing resume must not save the empty composer"
            );
            assert_eq!(sessions[0].id, saved_id);
        });
}

#[test]
fn inline_session_picker_closes_after_slash_command() {
    session_picker_close_case(false);
}

#[test]
fn inline_session_picker_closes_after_quick_action() {
    session_picker_close_case(true);
}

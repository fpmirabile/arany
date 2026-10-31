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
        let mut pty = Pty::new(
            crate::product::PtyLaunch::new(
                crate::product::Isolation::Passwd(crate::isolation::account_wrapper(
                    state.parent().expect("fixture root"),
                )),
                shell,
            )
            .session(&state, &workspace)
            .env("ARANY_TEST_INCLUDES", includes)
            .term("xterm")
            .env("OPENAI_API_KEY", "synthetic-never-sent-key")
            .spawn(),
        );
        pty.wait_for(b"Ask Arany");
        let selector_at = pty.send("a🙂b\x1b[D\x0b".as_bytes());
        wait_armed(&mut pty, selector_at);
        pty.send_and_wait(
            b"\x1b[200~INERT_MODAL_PASTE\r/exit\x03\x1b[201~\x1b",
            b"Ask Arany",
        );
        pty.send(b"X");
        pty.wait_for(b"Xb");
        let selector_at = pty.send(b"\x0b");
        wait_armed(&mut pty, selector_at);
        let inspector_at = pty.send(b"\x1b[B\r");
        // With no Runs the inspector has no rows; its key hint is the arming cue.
        wait_for_frame(&mut pty, (inspector_at, 40, 24), |screen| {
            screen.iter().any(|row| row.contains("No Runs yet"))
                && screen.iter().any(|row| row.contains("Esc close"))
        });
        pty.send_and_wait(b"\x1b", b"Ask Arany");
        pty.send(b"Y");
        pty.wait_for(b"Yb");
        let usage_at = pty.send(b"\x0b");
        wait_armed(&mut pty, usage_at);
        pty.send(b"5");
        wait_for_frame(&mut pty, (usage_at, 40, 24), |screen| {
            screen.iter().any(|row| row.contains("no calls yet"))
                && screen.iter().any(|row| row.contains("Esc close"))
        });
        pty.send_and_wait(b"\x1b", b"Ask Arany");
        pty.send(b"U");
        pty.wait_for(b"Ub");
        let after_inspection = pty.send(b"\x0b");
        wait_armed(&mut pty, after_inspection);
        pty.send(b"\x1b[B\x1b[B\r");
        pty.wait_for_after(after_inspection, b"Submit");
        pty.send(b"Z");
        pty.wait_for(b"Zb");
        let cases = if merged_overflow {
            vec![("Check @extra.txtZ", b"includes;".as_slice())]
        } else {
            vec![
                ("Check @missing.txtZ", b"exist;".as_slice()),
                ("Check @linkZ", b"symbolic".as_slice()),
            ]
        };
        for (draft, diagnostic) in cases {
            let from = pty.send(b"\x03");
            pty.send(draft.as_bytes());
            pty.send(b"\x1b[D\r");
            pty.wait_for_after(from, diagnostic);
            let after_diagnostic = from
                + pty.transcript[from..]
                    .windows(diagnostic.len())
                    .position(|part| part == diagnostic)
                    .unwrap()
                + diagnostic.len();
            pty.wait_for_after(after_diagnostic, b"\x1b[?25h");
            pty.send_and_wait(b"X", b"XZ");
        }
        pty.send(b"\x03/exit\r");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pty.pump();
            if pty
                .child
                .child()
                .try_wait()
                .expect("process status")
                .is_some()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "PTY exit: {}",
                tail(&pty.transcript)
            );
            thread::yield_now();
        }
        let status = pty.child.take().wait().expect("process exit");
        assert!(
            status.success(),
            "product failed: {}",
            tail(&pty.transcript)
        );
        pty.pump();
        let text = String::from_utf8_lossy(&pty.transcript);
        assert_tty_restored(&text);
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
    let mut pty = Pty::new(
        crate::product::PtyLaunch::new(
            crate::product::Isolation::Passwd(crate::isolation::account_wrapper(
                state.parent().expect("fixture root"),
            )),
            shell,
        )
        .session(&state, &workspace)
        .term("xterm")
        .spawn(),
    );
    let mut history = Vec::new();
    let mut capture_from = 0;
    pty.wait_for_screen((0, 40, 24), "Ask Arany");
    capture_from = pty.checkpoint_screen(&mut history, (40, 24), capture_from);
    let mut before_picker = capture_from;
    if quick {
        pty.send(b"\x0b");
        wait_armed(&mut pty, before_picker);
        capture_from = pty.checkpoint_screen(&mut history, (40, 24), capture_from);
        before_picker = capture_from;
        pty.send(b"\x1b[B\x1b[B\r");
    } else {
        pty.send(b"/resume\r");
    }
    wait_for_frame(&mut pty, (before_picker, 40, 24), |screen| {
        screen.iter().any(|row| row.contains("1 in this Workspace"))
            && screen.iter().any(|row| row.starts_with("› 1."))
    });
    capture_from = pty.checkpoint_screen(&mut history, (40, 24), capture_from);
    let before_close = capture_from;
    pty.send(b"\x1b");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        pty.pump();
        let recent = &pty.transcript[before_close..];
        assert!(
            !screen_contains(&pty.transcript, 40, 24, "failed"),
            "picker close failed: {}",
            tail(&pty.transcript)
        );
        if screen_contains(&pty.transcript, 40, 24, "closed")
            && recent.windows(6).any(|part| part == b"\x1b[?25h")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "picker close: {}",
            tail(&pty.transcript)
        );
        thread::yield_now();
    }
    capture_from = pty.checkpoint_screen(&mut history, (40, 24), capture_from);
    pty.send(b"/exit\r");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        pty.pump();
        if pty
            .child
            .child()
            .try_wait()
            .expect("process status")
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "PTY exit: {}",
            tail(&pty.transcript)
        );
        thread::yield_now();
    }
    let status = pty.child.take().wait().expect("process exit");
    assert!(
        status.success(),
        "product failed: {}",
        tail(&pty.transcript)
    );
    pty.checkpoint_screen(&mut history, (40, 24), capture_from);
    let text = String::from_utf8_lossy(&history);
    assert_tty_restored(&text);
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

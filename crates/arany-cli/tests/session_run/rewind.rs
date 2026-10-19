use super::{
    active_terminal::{ProductGuard, product_child_of, transcript_field},
    loopback::ChildGuard,
    session_picker::{PtyResponses, pump, tail},
};
use arany::{Event, EventEnvelope, StateRoot, Store};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn wait(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut PtyResponses,
    from: usize,
    needle: &[u8],
) {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        pump(output, input, transcript, answered);
        if transcript[from..]
            .windows(needle.len())
            .any(|s| s == needle)
        {
            return;
        }
        assert!(
            Instant::now() < end,
            "rewind PTY missing {}: {}",
            tail(needle),
            tail(transcript)
        );
        std::thread::yield_now();
    }
}

#[test]
fn rewind_pty_classic_fullscreen_narrow_and_linear_restore_a_prompt_without_submitting() {
    for (mode, width) in [("--classic", 80), ("", 40), ("--screen-reader", 60)] {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = root.path().join("state");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let source = runtime.block_on(async {
            let engine = super::engine_support::open(
                StateRoot::admit(&state).unwrap(),
                arany::EngineConfig::default(),
            )
            .unwrap();
            let provider = std::sync::Arc::new(
                arany::test_support::scenario::ScriptedProvider::new(vec![
                    arany::test_support::goals::finish_with("First"),
                    arany::test_support::goals::finish_with("Second"),
                ])
                .with_profile("openai", "gpt-5.4"),
            );
            let mut id = None;
            for prompt in ["First prompt", "Second prompt"] {
                let mut request = arany::test_support::scenario::request(&workspace, prompt);
                request.include_paths.clear();
                if id.is_some() {
                    request.title = None;
                }
                request.session_id = id;
                id = Some(
                    super::engine_support::run_to_end(&engine, &provider, request)
                        .await
                        .unwrap()
                        .session_id,
                );
            }
            engine.close().await.unwrap();
            id.unwrap()
        });
        let before = runtime.block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let events = store.load_session(source).await.unwrap();
            store.close().await.unwrap();
            events
        });
        let shell = format!(
            "trap : INT; stty rows 24 cols {width}; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" {mode} --no-color --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\" --provider openai --model gpt-5.4; code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$code\""
        );
        let mut command: Command = super::isolation::account_isolated_script(root.path());
        command
            .env_clear()
            .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
            .env("ARANY_TEST_STATE", &state)
            .env("ARANY_TEST_WORKSPACE", &workspace)
            .env("ARANY_TEST_SESSION", source.to_string())
            .env("OPENAI_API_KEY", "synthetic-test-key")
            .env("TERM", "xterm-256color")
            .env("SHELL", "/bin/sh")
            .current_dir(&workspace)
            .args(["-q", "-e", "-c", &shell, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ChildGuard::new(command.spawn().unwrap());
        let mut input = child.child().stdin.take().unwrap();
        let mut output = child.child().stdout.take().unwrap();
        fcntl_setfl(&output, fcntl_getfl(&output).unwrap() | OFlags::NONBLOCK).unwrap();
        let mut transcript = Vec::new();
        let mut answered = PtyResponses::default();
        let linear = mode == "--screen-reader";
        wait(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            0,
            if linear { b"Input:\r\n" } else { b"\x1b[?25h" },
        );
        let shell_pid = transcript_field(&String::from_utf8_lossy(&transcript), "SHELL_PID:")
            .parse::<u32>()
            .unwrap();
        let _product = ProductGuard::new(product_child_of(shell_pid), state.clone());
        if !linear {
            let from = transcript.len();
            input.write_all(b"saved draft\x1b").unwrap();
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                b"Esc again",
            );
            let from = transcript.len();
            input.write_all(b"\x1b").unwrap();
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                b"history;",
            );
            let from = transcript.len();
            input.write_all(b"\x1b[A").unwrap();
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                b"Hsaved",
            );
            input.write_all(b"\x03").unwrap();
        }
        let from = transcript.len();
        input.write_all(b"/rewind\r").unwrap();
        wait(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"Choose a checkpoint",
        );
        if !linear {
            let from = transcript.len();
            input.write_all(b"\x1b").unwrap();
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                b"cancelled",
            );
            let from = transcript.len();
            input.write_all(b"\x1b").unwrap();
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                b"Esc again",
            );
            let from = transcript.len();
            input.write_all(b"\x1b").unwrap();
            wait(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                from,
                b"Choose a checkpoint",
            );
        }
        let from = transcript.len();
        input
            .write_all(if linear { b"2\r" } else { b"\r" })
            .unwrap();
        wait(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"conversation",
        );
        let from = transcript.len();
        input
            .write_all(if linear {
                b"Restore conversation\r"
            } else {
                b"\x1b[A\r"
            })
            .unwrap();
        wait(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"Second prompt",
        );
        if linear {
            assert!(String::from_utf8_lossy(&transcript[from..]).contains("Draft (not sent):"));
        }
        input.write_all(b"\x03").unwrap();
        let settle = Instant::now() + Duration::from_millis(500);
        while Instant::now() < settle {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            std::thread::yield_now();
        }
        let from = transcript.len();
        input.write_all(b"/exit\r").unwrap();
        wait(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            b"TTY_AFTER:",
        );
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            if let Some(status) = child.child().try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < end, "PTY exit deadline");
            std::thread::yield_now();
        }
        let text = String::from_utf8_lossy(&transcript);
        assert_eq!(
            transcript_field(&text, "TTY_BEFORE:"),
            transcript_field(&text, "TTY_AFTER:")
        );
        runtime.block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let after = store.load_session(source).await.unwrap();
            assert_eq!(after[..before.len()], before[..]);
            assert!(matches!(
                after[before.len()..],
                [] | [EventEnvelope {
                    event: Event::SessionDefaultChanged { .. },
                    ..
                }]
            ));
            store.close().await.unwrap();
            let sessions =
                arany::list_sessions(StateRoot::open_existing(&state).unwrap(), workspace.clone())
                    .await
                    .unwrap();
            assert_eq!(sessions.len(), 2);
            let fork = sessions
                .iter()
                .find(|session| session.id != source)
                .unwrap();
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let view = store.load_view(fork.id).await.unwrap().unwrap();
            assert_eq!(view.runs.len(), 1);
            assert!(view.goals.is_empty());
            store.close().await.unwrap();
        });
    }
}

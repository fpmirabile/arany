use super::active_terminal::{ProductGuard, tty_settings};
use super::loopback::write_profile;
use super::session_picker::{
    checkpoint_screen, pump, screen_contains, screen_rows, tail, wait_for_screen,
};
use arany::{SessionId, StateRoot, Store, create_session, list_sessions};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::process::{Pid, Signal, kill_process};
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const NATIVE_TEST_KEY: &str = "synthetic-native-choice-key";

fn product_child_of(shell_pid: u32) -> Pid {
    let children_path = format!("/proc/{shell_pid}/task/{shell_pid}/children");
    let product = Path::new(env!("CARGO_BIN_EXE_arany"));
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
        assert!(
            Instant::now() < deadline,
            "model-browser product child not found"
        );
        thread::yield_now();
    }
}

#[test]
fn screen_reader_model_catalog_selects_exact_profile_without_provider_egress() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    write_profile(&state, 9321);
    let path = state.join("provider-profiles.json");
    let mut profile: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("profile file")).expect("profile JSON");
    profile["profiles"][0]["efforts"] = serde_json::json!(["low", "high"]);
    std::fs::write(&path, serde_json::to_vec(&profile).expect("profile JSON"))
        .expect("updated profile file");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let session_id = runtime.block_on(async {
        create_session(
            StateRoot::admit(&state).expect("private State"),
            workspace.clone(),
            None,
        )
        .await
        .expect("unselected Session")
    });

    let mut child = crate::product::PtyLaunch::new(
        crate::product::Isolation::Passwd(crate::isolation::isolated_wrapper(temp.path())),
        "exec \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\"",
    )
    .executable("/arany")
    .session(&state, &workspace)
    .env("ARANY_TEST_SESSION", session_id.to_string())
    .spawn();
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = crate::product::Blocking(child.child().stdout.take().expect("PTY output"));
    let (sender, receiver) = mpsc::channel();
    let mut consent_input = std::fs::File::from(rustix::io::dup(&input).unwrap());
    let reader = thread::spawn(move || {
        let mut declined = false;
        let mut bytes = Vec::new();
        let stages: [&str; 26] = [
            "Input:",
            "Command: /exit",
            "Notice: Select /provider first",
            "Notice: Provider: custom:local; set /model",
            "Notice: Error: Unknown command; did you mean /model?",
            "Choose number [effort|default], n next, p previous, or q close:",
            "Notice: invalid model or effort choice",
            "Notice: Model: model-1",
            "Choose number [effort|default], n next, p previous, or q close:",
            "Notice: Model catalog closed",
            "Notice: Provider: openai; set /model",
            "Notice: Model: gpt-linear-guard",
            "Notice: Model: gpt-linear-guard",
            "Notice: Error: selected native API credentials or workspace configuration unavailable; draft retained (29 characters)",
            "Input:",
            "Input:",
            "Notice: Model: gpt-6-astra",
            "Provider unavailable",
            "Notice: Error: Session has an active operation; selection unchanged. Try again when it finishes",
            "Notice: Provider: custom:local; set /model",
            "Choose number [effort|default], n next, p previous, or q close:",
            "Notice: Error: Session has an active operation; selection unchanged. Try again when it finishes",
            "Choose number [effort|default], n next, p previous, or q close:",
            "Notice: Model: model-1",
            "Notice: Provider: openai; set /model",
            "Notice: Model: gpt-6-astra",
        ];
        let mut next = 0;
        let mut start = 0;
        loop {
            let mut chunk = [0; 4096];
            let count = output.read(&mut chunk).expect("PTY output bytes");
            if count == 0 {
                break;
            }
            assert!(bytes.len() + count <= 32 * 1024, "bounded PTY output");
            bytes.extend_from_slice(&chunk[..count]);
            super::process::decline_workspace_consent(&mut consent_input, &bytes, &mut declined);
            while let Some(line_end) = bytes[start..].iter().position(|byte| *byte == b'\n') {
                let end = start + line_end + 1;
                // Decode the line without its feed: on a one-row screen a line feed scrolls the
                // text off, as an emulator would.
                if next < stages.len()
                    && screen_contains(&bytes[start..start + line_end], 1024, 1, stages[next])
                {
                    sender.send(next).expect("stage receiver");
                    next += 1;
                }
                start = end;
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
    input.write_all(b"/help\n").expect("complete linear help");
    stage(1);
    for (keys, expected) in [
        (&b"/model\n"[..], 1),
        (&b"/provider custom:local\n"[..], 2),
        (&b"/models\n"[..], 3),
        (&b"/model\n"[..], 4),
        (&b"1 max\n"[..], 5),
        (&b"1 high\n"[..], 6),
        (&b"/model\n"[..], 7),
        (&b"q\n"[..], 8),
        (&b"/provider openai\n"[..], 9),
        (&b"/model gpt-linear-guard\n"[..], 10),
        (&b"/model gpt-linear-guard high\n"[..], 11),
        (&b"unaccepted physical objective\n"[..], 12),
    ] {
        input.write_all(keys).expect("linear model journey");
        stage(expected + 1);
    }
    stage(14);
    input
        .write_all(b"\x03")
        .expect("clear the restored unaccepted draft");
    stage(15);
    input
        .write_all(b"/model gpt-6-astra high\n")
        .expect("choose model after clearing");
    stage(16);
    input.write_all(b"/model\n").expect("open model catalog");
    stage(17);
    let held = File::open(state.join(format!("session-{session_id}.lock"))).expect("Session lock");
    let prefix = || {
        runtime.block_on(async {
            let store = Store::open_read_only(StateRoot::open_existing(&state).expect("State"))
                .expect("read-only Store");
            let events = store
                .load_session(session_id)
                .await
                .expect("Session prefix");
            store.close().await.expect("close Store");
            events
        })
    };
    rustix::fs::flock(&held, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .expect("held operation");
    let before = prefix();
    input
        .write_all(b"/model gpt-6-astra low\n")
        .expect("busy atomic tuple");
    stage(18);
    assert_eq!(prefix(), before);
    rustix::fs::flock(&held, rustix::fs::FlockOperation::Unlock).expect("release operation");
    input
        .write_all(b"/provider custom:local\n")
        .expect("custom selection");
    stage(19);
    rustix::fs::flock(&held, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .expect("held catalog operation");
    let before = prefix();
    input.write_all(b"/model\n").expect("busy catalog");
    stage(20);
    input.write_all(b"1 low\n").expect("busy atomic selection");
    stage(21);
    assert_eq!(prefix(), before);
    rustix::fs::flock(&held, rustix::fs::FlockOperation::Unlock).expect("release catalog");
    input.write_all(b"/model\n").expect("reopen catalog");
    stage(22);
    input.write_all(b"1 low\n").expect("choice after unlock");
    stage(23);
    input
        .write_all(b"/provider openai\n")
        .expect("restore Provider");
    stage(24);
    input
        .write_all(b"/model gpt-6-astra high\n")
        .expect("restore atomic defaults");
    stage(25);
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
    assert!(!output.contains(&0x1b));
    let text = String::from_utf8(output).expect("UTF-8 transcript");
    for name in arany_cli::terminal::command_completions("/") {
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with(&format!("Command: /{name};"))
                    || line.starts_with(&format!("Command: /{name} ")))
                .count(),
            1,
            "linear help must show each compiled entry completely: {name}"
        );
    }
    assert!(text.contains("Command: /model <model-id> [effort|default];"));
    assert!(text.contains("during Run: locked"));
    assert!(text.contains("during Run: view-only"));
    assert!(text.contains(
        "Choice 1: model-1; selected effort low; exact profile; effort provider default, low, high"
    ));
    assert!(!text.contains("Setup: Choose model effort"));
    assert_eq!(
        text.matches("Notice: Error: Model catalog unavailable: Provider unavailable. Review the selected account or retry /model.\r\n")
            .count(),
        1,
        "catalog failure needs a textual error and recovery: {}",
        tail(text.as_bytes())
    );
    let session_id = text
        .lines()
        .filter_map(|line| line.trim_end_matches('\r').strip_prefix("Session: "))
        .find_map(|value| value.parse::<SessionId>().ok())
        .expect("final Session ID");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only runtime");
    runtime.block_on(async {
        let root = StateRoot::open_existing(&state).expect("existing State");
        let store = Store::open_read_only(root).expect("read-only Store");
        let view = store
            .load_view(session_id)
            .await
            .expect("valid Session history")
            .expect("selected Session");
        assert_eq!(view.defaults.provider.as_deref(), Some("openai"));
        assert_eq!(view.defaults.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(view.defaults.effort, Some(arany::Effort::High));
        assert!(view.runs.is_empty());
        let events = store
            .load_session(session_id)
            .await
            .expect("committed defaults");
        let custom_efforts = events
            .iter()
            .filter_map(|envelope| match &envelope.event {
                arany::Event::SessionDefaultChanged { defaults }
                    if defaults.provider.as_deref() == Some("custom:local")
                        && defaults.effort.is_some() =>
                {
                    defaults.effort
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            custom_efforts,
            [arany::Effort::High, arany::Effort::Low],
            "invalid input and cancellation create no default change"
        );
        let native_defaults = events
            .iter()
            .filter_map(|envelope| match &envelope.event {
                arany::Event::SessionDefaultChanged { defaults }
                    if defaults.provider.as_deref() == Some("openai") =>
                {
                    Some((defaults.model.as_deref(), defaults.effort))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            native_defaults,
            [
                (None, None),
                (Some("gpt-linear-guard"), Some(arany::Effort::Low)),
                (Some("gpt-linear-guard"), Some(arany::Effort::High)),
                (Some("gpt-6-astra"), Some(arany::Effort::High)),
                (None, None),
                (Some("gpt-6-astra"), Some(arany::Effort::High))
            ],
            "atomic tuples and rejected busy selections preserve exact history"
        );
        store.close().await.expect("close Store");
    });
}

#[test]
fn inline_model_catalog_preserves_terminal_and_pins_selected_default() {
    for width in [16, 20, 24, 40] {
        inline_model_catalog_at_width(width);
    }
}

fn inline_model_catalog_at_width(width: u16) {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let saved_id = super::slash_completion::saved_conversation(
        &state,
        &workspace,
        arany::SessionDefaults::default(),
    );
    write_profile(&state, 9321);
    let profile_path = state.join("provider-profiles.json");
    let mut profile: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&profile_path).expect("profile file"))
            .expect("profile JSON");
    profile["profiles"][0]["efforts"] = serde_json::json!(["low", "high"]);
    std::fs::write(
        &profile_path,
        serde_json::to_vec(&profile).expect("profile JSON"),
    )
    .expect("updated profile file");
    let command = format!(
        "stty rows 24 cols {width}; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" --classic --no-color --provider custom:local --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume \"$ARANY_TEST_SESSION\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
    );
    let mut attached = crate::product::PtyLaunch::new(
        crate::product::Isolation::Passwd(crate::isolation::account_wrapper(
            state.parent().expect("fixture root"),
        )),
        command.as_str(),
    )
    .session(&state, &workspace)
    .env("ARANY_TEST_SESSION", saved_id.to_string())
    .env("OPENAI_API_KEY", NATIVE_TEST_KEY)
    .term("xterm")
    .spawn();
    let mut input = attached.child().stdin.take().expect("PTY input");
    let mut output = attached.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout pipe flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking stdout pipe");
    let mut transcript = Vec::new();
    let mut history = Vec::new();
    let mut capture_from = 0;
    let mut answered = crate::session_picker::PtyResponses::default();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !screen_contains(&transcript, width.into(), 24, "Ask Ara") {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "composer missing: {}",
            tail(&transcript)
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
    let before_menu = capture_from;
    input
        .write_all(b"note\x0b")
        .expect("draft and quick actions");
    let deadline = Instant::now() + Duration::from_secs(10);
    // Selectors take keys only once their armed frame is drawn.
    while crate::armed_selector(&transcript, before_menu, width.into(), 24).is_none() {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "quick actions missing: {}",
            tail(&transcript)
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
    let before_catalog = capture_from;
    input.write_all(b"\r").expect("open catalog");
    let model_label: &str = if width < 20 { "mo…-1" } else { "model-1" };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let complete = crate::armed_selector(&transcript, before_catalog, width.into(), 24)
            .is_some_and(|screen| screen.iter().any(|row| row.contains(model_label)));
        if complete {
            break;
        }
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "model catalog missing: {}",
            tail(&transcript)
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
    assert!(screen_contains(
        &transcript,
        width.into(),
        24,
        "Models · exact"
    ));
    let filtered_at = capture_from;
    input.write_all(b"z").expect("filter model catalog");
    wait_for_screen(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        (filtered_at, width.into(), 24),
        "No matches",
    );
    capture_from = checkpoint_screen(
        &mut transcript,
        &mut answered,
        &mut history,
        (width.into(), 24),
        capture_from,
    );
    let restored_at = capture_from;
    input
        .write_all(b"\r\x7f")
        .expect("no-match Enter and Backspace filter");
    wait_for_screen(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        (restored_at, width.into(), 24),
        model_label,
    );
    capture_from = checkpoint_screen(
        &mut transcript,
        &mut answered,
        &mut history,
        (width.into(), 24),
        capture_from,
    );
    let before_selection = capture_from;
    input.write_all(b"\r").expect("select model");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript[before_selection..]
        .windows(b"\x1b[?25h".len())
        .any(|part| part == b"\x1b[?25h")
    {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "model choice missing: {}",
            tail(&transcript)
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
    let before_edit = capture_from;
    input.write_all(b"X").expect("edit returned draft");
    // The catalog redraws the composer caret too, so wait until its hint is gone.
    let composer_ready = |transcript: &[u8], from: usize, draft: &str, column: usize| {
        crate::composer_caret(transcript, from, width.into(), 24).is_some_and(|(caret, screen)| {
            caret == column
                && screen.iter().any(|row| row.starts_with(draft))
                && !screen
                    .iter()
                    .any(|row| row.contains("L/R effort") || row.contains("Left/Right reasoning"))
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !composer_ready(&transcript, before_edit, "> noteX", 8) {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "draft not restored: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }

    let native_pid = (width == 40).then(|| {
        let text = String::from_utf8_lossy(&history);
        let shell = text
            .lines()
            .find_map(|line| line.strip_prefix("SHELL_PID:"))
            .expect("shell PID")
            .trim_end_matches('\r')
            .parse()
            .expect("shell PID number");
        product_child_of(shell)
    });
    let _native_guard = native_pid.map(|pid| ProductGuard::new(pid, state.clone()));
    // A digit sent with Ctrl+K arrives before the selector arms and is discarded.
    for (keys, focused, label) in [
        (
            &b"\x1b[D\x0b1"[..],
            "› 1. Models",
            "pre-frame digit discarded",
        ),
        (b"\x1b[B", "› 2. Agents", "armed selector takes later keys"),
    ] {
        capture_from = checkpoint_screen(
            &mut transcript,
            &mut answered,
            &mut history,
            (width.into(), 24),
            capture_from,
        );
        let start = capture_from;
        input.write_all(keys).expect(label);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !crate::armed_selector(&transcript, start, width.into(), 24)
            .is_some_and(|screen| screen.iter().any(|row| row.starts_with(focused)))
        {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            assert!(
                Instant::now() < deadline,
                "{label} at {width} columns: {}",
                tail(&transcript[start..])
            );
            thread::yield_now();
        }
    }
    {
        let mut transition =
            |keys: &[u8], text: &str, caret: Option<usize>, hide_cursor: bool, label: &str| {
                capture_from = checkpoint_screen(
                    &mut transcript,
                    &mut answered,
                    &mut history,
                    (width.into(), 24),
                    capture_from,
                );
                let start = capture_from;
                input.write_all(keys).expect(label);
                let deadline = Instant::now() + Duration::from_secs(10);
                let before = screen_rows(&transcript[..start], width.into(), 24);
                loop {
                    // Each check needs a frame drawn after the keys, never the stale screen.
                    let complete = if hide_cursor {
                        crate::armed_selector(&transcript, start, width.into(), 24).is_some_and(
                            |screen| {
                                screen != before && screen.iter().any(|row| row.contains(text))
                            },
                        )
                    } else if let Some(column) = caret {
                        composer_ready(&transcript, start, text, column)
                    } else {
                        // Selection notices are inserted into native scrollback above the
                        // viewport, so they are read from the bytes drawn after the keys.
                        screen_rows(&transcript, width.into(), 24) != before
                            && composer_ready(&transcript, start, "> ", 3)
                            && visible_text_contains(&transcript[start..], text)
                    };
                    if complete {
                        break;
                    }
                    pump(&mut output, &mut input, &mut transcript, &mut answered);
                    assert!(
                        Instant::now() < deadline,
                        "{label} at {width} columns: {}",
                        tail(&transcript)
                    );
                    thread::yield_now();
                }
            };
        for (keys, text, caret, hide_cursor, label) in [
            (
                &b"1"[..],
                "<low>",
                None,
                true,
                "armed digit 1 opens the picker with a mid-draft caret",
            ),
            (
                &b"\x1b[C"[..],
                "high",
                None,
                true,
                "right changes focused reasoning",
            ),
            (
                &b"\x1b"[..],
                "> noteX",
                Some(7),
                false,
                "cancel returns draft",
            ),
            (
                &b"Y"[..],
                "> noteYX",
                Some(8),
                false,
                "cancel restores caret",
            ),
            (
                &b"\x1b[F\x7f\x7f\x7f\x7f\x7f\x7f/model\r"[..],
                "<low>",
                None,
                true,
                "slash model picker",
            ),
            (
                &b"\x1b[C\r"[..],
                "> ",
                Some(3),
                false,
                "atomic model and reasoning selection",
            ),
            (
                &b"/model\r"[..],
                "<high>",
                None,
                true,
                "reopen with current reasoning",
            ),
            (&b"\r"[..], "> ", Some(3), false, "current tuple remains"),
            (&b"/model\r"[..], "<high>", None, true, "reopen for cancel"),
            (&b"\x1b"[..], "> ", Some(3), false, "cancel keeps defaults"),
        ] {
            transition(keys, text, caret, hide_cursor, label);
        }
        if native_pid.is_some() {
            transition(
                b"/provider openai\r",
                "Provider: openai",
                None,
                false,
                "select synthetic native source",
            );
            transition(
                b"/model gpt-choice-guard high\r",
                "Model: gpt-choice-guard",
                None,
                false,
                "unknown tuple selects without probes",
            );
            transition(
                b"/provider custom:local\r",
                "Provider: custom:local",
                None,
                false,
                "restore exact profile",
            );
            transition(
                b"/model model-1 high\r",
                "Model: model-1",
                None,
                false,
                "restore exact tuple",
            );
        }
    }
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
        if let Some(status) = attached.child().try_wait().expect("product status") {
            assert!(status.success(), "attached process exited successfully");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "product did not exit: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    checkpoint_screen(
        &mut transcript,
        &mut answered,
        &mut history,
        (width.into(), 24),
        capture_from,
    );
    let output = String::from_utf8(history).expect("UTF-8 PTY transcript");
    let before = output
        .lines()
        .find_map(|line| line.strip_prefix("TTY_BEFORE:"))
        .expect("initial TTY settings")
        .trim_end_matches('\r');
    let after = output
        .split_once("TTY_AFTER:")
        .expect("restored terminal marker")
        .1
        .lines()
        .next()
        .expect("final TTY settings")
        .trim_end_matches('\r');
    assert_eq!(before, after, "catalog restored the terminal");
    assert!(!output.contains("\x1b[?1049h"));
    assert!(!output.contains("\x1b[?1000h"));
    assert!(!output.contains(NATIVE_TEST_KEY));
    assert!(!output.contains("Billable model check"));
    assert!(!output.contains("Accept cost"));
    assert!(
        !std::fs::read(state.join("events.sqlite3"))
            .expect("journal bytes")
            .windows(NATIVE_TEST_KEY.len())
            .any(|part| part == NATIVE_TEST_KEY.as_bytes())
    );
    let session_id = output
        .split_once("Session: ")
        .and_then(|(_, rest)| rest.lines().next())
        .and_then(|value| value.trim_end_matches('\r').parse::<SessionId>().ok())
        .expect("final Session ID");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only runtime");
    runtime.block_on(async {
        let root = StateRoot::open_existing(&state).expect("existing State");
        let store = Store::open_read_only(root).expect("read-only Store");
        let view = store
            .load_view(session_id)
            .await
            .expect("valid Session history")
            .expect("selected Session");
        assert_eq!(view.defaults.provider.as_deref(), Some("custom:local"));
        assert_eq!(view.defaults.model.as_deref(), Some("model-1"));
        assert_eq!(view.defaults.effort, Some(arany::Effort::High));
        assert!(view.runs.is_empty());
        store.close().await.expect("close Store");
    });
}

#[test]
fn inline_model_catalog_cancel_then_sigterm_restores_terminal_without_selection() {
    for (columns, rows) in [(80, 24), (16, 8)] {
        run_model_catalog_cancel_then_sigterm_case(columns, rows);
    }
}

fn run_model_catalog_cancel_then_sigterm_case(columns: u16, rows: u16) {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    write_profile(&state, 9321);
    let command = format!(
        "stty rows {rows} cols {columns}; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" --classic --provider custom:local --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
    );
    let mut attached = crate::product::PtyLaunch::new(
        crate::product::Isolation::Passwd(crate::isolation::account_wrapper(
            state.parent().expect("fixture root"),
        )),
        &command,
    )
    .session(&state, &workspace)
    .term("xterm")
    .spawn();
    let mut input = attached.child().stdin.take().expect("PTY input");
    let mut output = attached.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout pipe flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking stdout pipe");
    let mut transcript = Vec::new();
    let mut history = Vec::new();
    let mut capture_from = 0;
    let mut answered = crate::session_picker::PtyResponses::default();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !screen_contains(&transcript, columns.into(), rows.into(), "Ask Ara") {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "composer missing: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    let visible = String::from_utf8_lossy(&transcript);
    let shell_pid = visible
        .lines()
        .find_map(|line| line.strip_prefix("SHELL_PID:"))
        .expect("shell PID marker")
        .trim_end_matches('\r')
        .parse::<u32>()
        .expect("shell PID");
    let before = visible
        .lines()
        .find_map(|line| line.strip_prefix("TTY_BEFORE:"))
        .expect("initial TTY settings")
        .trim_end_matches('\r')
        .to_owned();
    let product_pid = product_child_of(shell_pid);
    let product_guard = ProductGuard::new(product_pid, state.clone());
    assert_ne!(tty_settings(product_pid), before, "raw mode active");

    let catalog_marker: &str = if columns == 16 {
        "Enter pick Esc"
    } else {
        "model-1"
    };
    capture_from = checkpoint_screen(
        &mut transcript,
        &mut answered,
        &mut history,
        (columns.into(), rows.into()),
        capture_from,
    );
    let opened_at = capture_from;
    input.write_all(b"/model\r").expect("open first catalog");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !crate::armed_selector(&transcript, opened_at, columns.into(), rows.into())
        .is_some_and(|screen| screen.iter().any(|row| row.contains(catalog_marker)))
    {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "first catalog missing: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    capture_from = checkpoint_screen(
        &mut transcript,
        &mut answered,
        &mut history,
        (columns.into(), rows.into()),
        capture_from,
    );
    let prior_len = capture_from;
    input.write_all(b"\x03").expect("cancel first browser");
    let closed_marker: &str = if columns == 16 {
        "Model catalog c"
    } else {
        "Model catalog closed"
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !screen_contains(&transcript, columns.into(), rows.into(), closed_marker)
        || !transcript[prior_len..]
            .windows(6)
            .any(|part| part == b"\x1b[?25h")
    {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "browser did not close: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    assert!(
        attached
            .child()
            .try_wait()
            .expect("live shell status")
            .is_none(),
        "Ctrl+C left the Session active"
    );
    assert_ne!(
        tty_settings(product_pid),
        before,
        "composer reacquired raw mode"
    );

    capture_from = checkpoint_screen(
        &mut transcript,
        &mut answered,
        &mut history,
        (columns.into(), rows.into()),
        capture_from,
    );
    let reopen_at = capture_from;
    input.write_all(b"/model\r").expect("open second catalog");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        if crate::armed_selector(&transcript, reopen_at, columns.into(), rows.into())
            .is_some_and(|screen| screen.iter().any(|row| row.contains(catalog_marker)))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "second catalog missing: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    assert_ne!(tty_settings(product_pid), before, "browser owns raw mode");
    assert!(
        product_guard.is_owned_and_running(),
        "private product child"
    );
    capture_from = checkpoint_screen(
        &mut transcript,
        &mut answered,
        &mut history,
        (columns.into(), rows.into()),
        capture_from,
    );
    kill_process(product_pid, Signal::TERM).expect("terminate model browser");
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        if let Some(status) = attached.child().try_wait().expect("script status") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "signal did not exit: {}",
            tail(&transcript)
        );
        thread::yield_now();
    };
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(!status.success(), "SIGTERM exits unsuccessfully");
    checkpoint_screen(
        &mut transcript,
        &mut answered,
        &mut history,
        (columns.into(), rows.into()),
        capture_from,
    );
    let transcript = String::from_utf8(history).expect("PTY transcript UTF-8");
    let after = transcript
        .split_once("TTY_AFTER:")
        .expect("restored terminal marker")
        .1
        .lines()
        .next()
        .expect("restored TTY settings")
        .trim_end_matches('\r');
    assert_eq!(before, after, "SIGTERM restored the exact TTY settings");
    assert!(transcript.contains("terminated by SIGTERM"));
    assert!(!transcript.contains("\x1b[?1049h"));
    assert!(!transcript.contains("\x1b[?1000h"));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only runtime");
    runtime.block_on(async {
        let sessions = list_sessions(
            StateRoot::open_existing(&state).expect("existing State"),
            workspace.clone(),
        )
        .await
        .expect("canonical Session list");
        assert!(
            sessions.is_empty(),
            "cancelled model navigation saves no empty conversation"
        );
    });
}

// Whether `text` was drawn in `frame`, ignoring cursor controls and the blank cells a
// diffed frame skips between words.
fn visible_text_contains(frame: &[u8], text: &str) -> bool {
    let mut visible = Vec::new();
    let mut cursor = 0;
    while cursor < frame.len() {
        if frame[cursor..].starts_with(b"\x1b[") {
            cursor += 2;
            while cursor < frame.len() && !(0x40..=0x7e).contains(&frame[cursor]) {
                cursor += 1;
            }
        } else if !frame[cursor].is_ascii_whitespace() {
            visible.push(frame[cursor]);
        }
        cursor += 1;
    }
    let text = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    visible.windows(text.len()).any(|part| part == text)
}

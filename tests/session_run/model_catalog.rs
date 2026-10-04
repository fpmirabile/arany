use super::active_terminal::ProductGuard;
use super::loopback::{ChildGuard, write_profile};
use super::process::BoundedOutput;
use super::session_picker::{pump, tail};
use arany::{SessionId, StateRoot, Store, create_session, list_sessions};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::process::{Pid, Signal, kill_process};
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
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

fn tty_settings(pid: Pid) -> String {
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
    profile["profiles"][0]["capability_evidence_version"] = serde_json::json!(2);
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

    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_SESSION", session_id.to_string())
        .env("TERM", "dumb")
        .current_dir(&workspace)
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
    let mut child = ChildGuard::new(command.spawn().expect("attached PTY process"));
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let stages: [&[u8]; 24] = [
            b"Input:", b"Command: /exit", b"Notice: Select /provider first",
            b"Notice: Provider: custom:local; set /model", b"Notice: Error: Usage: /models",
            b"Choose number [effort|default], n next, p previous, or q close:",
            b"Notice: invalid model or effort choice", b"Notice: Model: model-1",
            b"Choose number [effort|default], n next, p previous, or q close:",
            b"Notice: Model catalog closed", b"Notice: Provider: openai; set /model",
            b"Notice: Model: gpt-linear-guard", b"Notice: Model: gpt-linear-guard",
            b"Notice: Error: selected native API credentials or workspace configuration unavailable",
            b"Notice: Model: gpt-6-astra", b"Provider unavailable",
            b"Notice: Error: Session has an active operation; selection unchanged. Try again when it finishes",
            b"Notice: Provider: custom:local; set /model",
            b"Choose number [effort|default], n next, p previous, or q close:",
            b"Notice: Error: Session has an active operation; selection unchanged. Try again when it finishes",
            b"Choose number [effort|default], n next, p previous, or q close:",
            b"Notice: Model: model-1", b"Notice: Provider: openai; set /model", b"Notice: Model: gpt-6-astra",
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
            while next < stages.len()
                && let Some(position) = bytes[start..]
                    .windows(stages[next].len())
                    .position(|part| part == stages[next])
            {
                sender.send(next).expect("stage receiver");
                start += position + stages[next].len();
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
    input.write_all(b"/help\n").expect("complete linear help");
    stage(1);
    for (keys, expected) in [
        (&b"/models\n"[..], 1),
        (&b"/provider custom:local\n"[..], 2),
        (&b"/models extra\n"[..], 3),
        (&b"/model\n"[..], 4),
        (&b"1 max\n"[..], 5),
        (&b"1 high\n"[..], 6),
        (&b"/models\n"[..], 7),
        (&b"q\n"[..], 8),
        (&b"/provider openai\n"[..], 9),
        (&b"/model gpt-linear-guard\n"[..], 10),
        (&b"/model gpt-linear-guard high\n"[..], 11),
        (&b"unaccepted physical objective\n"[..], 12),
        (&b"/model gpt-6-astra high\n"[..], 13),
        (&b"/models\n"[..], 14),
    ] {
        input.write_all(keys).expect("linear model journey");
        stage(expected + 1);
    }
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
    stage(16);
    assert_eq!(prefix(), before);
    rustix::fs::flock(&held, rustix::fs::FlockOperation::Unlock).expect("release operation");
    input
        .write_all(b"/provider custom:local\n")
        .expect("custom selection");
    stage(17);
    rustix::fs::flock(&held, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .expect("held catalog operation");
    let before = prefix();
    input.write_all(b"/models\n").expect("busy catalog");
    stage(18);
    input.write_all(b"1 low\n").expect("busy atomic selection");
    stage(19);
    assert_eq!(prefix(), before);
    rustix::fs::flock(&held, rustix::fs::FlockOperation::Unlock).expect("release catalog");
    input.write_all(b"/models\n").expect("reopen catalog");
    stage(20);
    input.write_all(b"1 low\n").expect("choice after unlock");
    stage(21);
    input
        .write_all(b"/provider openai\n")
        .expect("restore Provider");
    stage(22);
    input
        .write_all(b"/model gpt-6-astra high\n")
        .expect("restore atomic defaults");
    stage(23);
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
    assert!(!output.contains(&0x1b));
    let text = String::from_utf8(output).expect("UTF-8 transcript");
    for name in arany::command_completions("/") {
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
        text.matches("Notice: Error: Model catalog unavailable: Provider unavailable. Review the selected account or retry /models.\r\n")
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
    write_profile(&state, 9321);
    let profile_path = state.join("provider-profiles.json");
    let mut profile: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&profile_path).expect("profile file"))
            .expect("profile JSON");
    profile["profiles"][0]["capability_evidence_version"] = serde_json::json!(2);
    profile["profiles"][0]["efforts"] = serde_json::json!(["low", "high"]);
    std::fs::write(
        &profile_path,
        serde_json::to_vec(&profile).expect("profile JSON"),
    )
    .expect("updated profile file");
    let command = format!(
        "stty rows 24 cols {width}; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" --no-color --provider custom:local --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
    );
    let mut attached = Command::new("/usr/bin/script");
    attached
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("OPENAI_API_KEY", NATIVE_TEST_KEY)
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", command.as_str(), "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut attached = ChildGuard::new(attached.spawn().expect("attached PTY process"));
    let mut input = attached.child().stdin.take().expect("PTY input");
    let mut output = attached.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout pipe flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking stdout pipe");
    let mut transcript = Vec::new();
    let mut answered = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript
        .windows(b"Ask Arany".len())
        .any(|part| part == b"Ask Arany")
    {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "composer missing: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    let before_menu = transcript.len();
    input
        .write_all(b"note\x0b")
        .expect("draft and quick actions");
    let menu_ready: &[u8] = if width < 20 {
        b"Esc"
    } else if width < 40 {
        b"Enter:open"
    } else {
        b"return"
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript[before_menu..]
        .windows(menu_ready.len())
        .any(|part| part == menu_ready)
    {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "quick actions missing: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    let before_catalog = transcript.len();
    input.write_all(b"\r").expect("open catalog");
    let model_label: &[u8] = if width < 20 {
        "mo…l-1".as_bytes()
    } else {
        b"model-1"
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let catalog = &transcript[before_catalog..];
        let complete = catalog
            .windows(model_label.len())
            .position(|part| part == model_label)
            .is_some_and(|position| {
                catalog[position + model_label.len()..]
                    .windows(b"\x1b[?25l".len())
                    .any(|part| part == b"\x1b[?25l")
            });
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
    let before_filter = transcript.len();
    input.write_all(b"z").expect("filter model catalog");
    let no_match_label: &[u8] = b"No matches";
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript[before_filter..]
        .windows(no_match_label.len())
        .any(|part| part == no_match_label)
    {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "empty filter missing: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    let before_clear = transcript.len();
    input
        .write_all(b"\r\x7f")
        .expect("no-match Enter and Backspace filter");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript[before_clear..]
        .windows(model_label.len())
        .any(|part| part == model_label)
    {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "filter did not restore model: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    let before_selection = transcript.len();
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
    let before_edit = transcript.len();
    input.write_all(b"X").expect("edit returned draft");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript[before_edit..]
        .windows(b"\x1b[22;7HX".len())
        .any(|part| part == b"\x1b[22;7HX")
    {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "draft not restored: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    let native_pid = (width == 40).then(|| {
        let text = String::from_utf8_lossy(&transcript);
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
    {
        let mut transition = |keys: &[u8], marker: &[u8], hide_cursor: bool, label: &str| {
            let start = transcript.len();
            input.write_all(keys).expect(label);
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let frame = &transcript[start..];
                let complete = frame_marker_end(frame, marker);
                if complete.is_some_and(|end| {
                    !hide_cursor
                        || frame[end..]
                            .windows(b"\x1b[?25l".len())
                            .any(|part| part == b"\x1b[?25l")
                }) {
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
        for (keys, marker, hide_cursor, label) in [
            (
                &b"\x1b[D\x0b\r"[..],
                &b"<low>"[..],
                true,
                "picker with mid-draft caret",
            ),
            (
                &b"\x1b[C"[..],
                &b"high"[..],
                true,
                "right changes focused reasoning",
            ),
            (
                &b"\x1b"[..],
                &b"\x1b[22;7H"[..],
                false,
                "cancel returns draft",
            ),
            (
                &b"Y"[..],
                &b"\x1b[22;7HY"[..],
                false,
                "cancel restores caret",
            ),
            (
                &b"\x1b[F\x7f\x7f\x7f\x7f\x7f\x7f/model\r"[..],
                &b"<low>"[..],
                true,
                "slash model picker",
            ),
            (
                &b"\x1b[C\r"[..],
                &b"\x1b[?25h"[..],
                false,
                "atomic model and reasoning selection",
            ),
            (
                &b"/model\r"[..],
                &b"<high>"[..],
                true,
                "reopen with current reasoning",
            ),
            (
                &b"\r"[..],
                &b"\x1b[?25h"[..],
                false,
                "current tuple remains",
            ),
            (&b"/model\r"[..], &b"<high>"[..], true, "reopen for cancel"),
            (
                &b"\x1b"[..],
                &b"\x1b[?25h"[..],
                false,
                "cancel keeps defaults",
            ),
        ] {
            transition(keys, marker, hide_cursor, label);
        }
        if native_pid.is_some() {
            transition(
                b"/provider openai\r",
                b"set /model",
                false,
                "select synthetic native source",
            );
            transition(
                b"/model gpt-choice-guard high\r",
                b"Model: gpt-choice-guard",
                false,
                "unknown tuple selects without probes",
            );
            transition(
                b"/provider custom:local\r",
                b"set /model",
                false,
                "restore exact profile",
            );
            transition(
                b"/model model-1 high\r",
                b"Model: model-1",
                false,
                "restore exact tuple",
            );
        }
    }
    input.write_all(b"/quit\r").expect("exit attached mode");
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
    let output = String::from_utf8(transcript).expect("UTF-8 PTY transcript");
    assert!(output.contains("Models · exact"));
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

fn frame_marker_end(frame: &[u8], marker: &[u8]) -> Option<usize> {
    if marker.contains(&b'\x1b') {
        return frame
            .windows(marker.len())
            .position(|part| part == marker)
            .map(|start| start + marker.len());
    }
    let mut visible = Vec::new();
    let mut ends = Vec::new();
    let mut cursor = 0;
    while cursor < frame.len() {
        if frame[cursor..].starts_with(b"\x1b[") {
            cursor += 2;
            while cursor < frame.len() && !(0x40..=0x7e).contains(&frame[cursor]) {
                cursor += 1;
            }
        } else if !frame[cursor].is_ascii_whitespace() {
            visible.push(frame[cursor]);
            ends.push(cursor + 1);
        }
        cursor += 1;
    }
    let marker = marker
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    visible
        .windows(marker.len())
        .position(|part| part == marker)
        .map(|start| ends[start + marker.len() - 1])
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
        "stty rows {rows} cols {columns}; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" --provider custom:local --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
    );
    let mut attached = Command::new("/usr/bin/script");
    attached
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", &command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut attached = ChildGuard::new(attached.spawn().expect("model-browser PTY process"));
    let mut input = attached.child().stdin.take().expect("PTY input");
    let mut output = attached.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout pipe flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking stdout pipe");
    let mut transcript = Vec::new();
    let mut answered = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript
        .windows(b"/help".len())
        .any(|part| part == b"/help")
    {
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

    let catalog_marker: &[u8] = if columns == 16 {
        b"Enter pick Esc"
    } else {
        b"model-1"
    };
    input.write_all(b"/models\r").expect("open first catalog");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !frame_marker_end(&transcript, catalog_marker).is_some_and(|end| {
        transcript[end..]
            .windows(b"\x1b[?25l".len())
            .any(|part| part == b"\x1b[?25l")
    }) {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "first catalog missing: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    let prior_len = transcript.len();
    input.write_all(b"\x03").expect("cancel first browser");
    let closed_marker: &[u8] = if columns == 16 {
        b"Model catalog c"
    } else {
        b"closed"
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript[prior_len..]
        .windows(closed_marker.len())
        .any(|part| part == closed_marker)
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

    let reopen_at = transcript.len();
    input.write_all(b"/models\r").expect("open second catalog");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        let frame = &transcript[reopen_at..];
        if frame_marker_end(frame, catalog_marker).is_some_and(|end| {
            frame[end..]
                .windows(b"\x1b[?25l".len())
                .any(|part| part == b"\x1b[?25l")
        }) {
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
    let transcript = String::from_utf8(transcript).expect("PTY transcript UTF-8");
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
        assert_eq!(sessions.len(), 1);
        let store = Store::open_read_only(StateRoot::open_existing(&state).expect("state reopen"))
            .expect("read-only Store");
        let view = store
            .load_view(sessions[0].id)
            .await
            .expect("valid Session replay")
            .expect("Session exists");
        assert_eq!(view.defaults.provider.as_deref(), Some("custom:local"));
        assert_eq!(view.defaults.model, None);
        assert_eq!(view.defaults.effort, None);
        assert!(view.runs.is_empty());
        store.close().await.expect("close Store");
    });
}

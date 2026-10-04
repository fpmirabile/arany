use super::active_terminal::{ProductGuard, product_child_of};
use super::loopback::{ChildGuard, wait_product};
use arany::{SessionView, StateRoot, Store, create_session};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use std::{
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub(super) fn tail(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .rev()
        .take(512)
        .collect::<String>()
        .chars()
        .rev()
        .flat_map(char::escape_default)
        .collect()
}

pub(super) fn pump(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut usize,
) {
    loop {
        let mut chunk = [0; 4096];
        match output.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                assert!(
                    transcript.len() + count <= 64 * 1024,
                    "bounded PTY transcript"
                );
                transcript.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("PTY read failed: {error}"),
        }
    }
    let queries = transcript
        .windows(4)
        .filter(|part| *part == b"\x1b[6n")
        .count();
    assert!(queries <= 32, "bounded cursor queries");
    while *answered < queries {
        input.write_all(b"\x1b[2;1R").expect("cursor response");
        *answered += 1;
    }
}

fn run_picker_case(
    name: &str,
    width: u16,
    selection: &[u8],
    older_selected: bool,
    stale_activation: bool,
) {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("Session runtime");
    let root = StateRoot::admit(&state).expect("state root");
    let older = runtime
        .block_on(create_session(
            StateRoot::open_existing(&state).expect("state reopen"),
            workspace.clone(),
            Some("Older".into()),
        ))
        .expect("older Session");
    let newer = runtime
        .block_on(create_session(
            StateRoot::open_existing(&state).expect("state reopen"),
            workspace.clone(),
            Some("Newer".into()),
        ))
        .expect("newer Session");
    drop(root);

    let no_color = if width < 20 || stale_activation {
        "--no-color "
    } else {
        ""
    };
    let command = format!(
        "stty rows 24 cols {width}; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" {no_color}--state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
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
    let mut attached = ChildGuard::new(attached.spawn().expect("picker process"));
    let mut input = attached.child().stdin.take().expect("PTY input");
    let mut output = attached.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout pipe flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking stdout pipe");
    let mut transcript = Vec::new();
    let mut answered = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript
        .windows(b"\x1b[?1000h".len())
        .any(|part| part == b"\x1b[?1000h")
    {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "mouse capture not enabled in {name}: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    let shell_pid = String::from_utf8_lossy(&transcript)
        .lines()
        .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
        .expect("picker shell PID")
        .parse()
        .expect("numeric picker shell PID");
    let _product = ProductGuard::new(product_child_of(shell_pid), state.clone());
    if width < 20 {
        let older_id = older.to_string();
        let newer_id = newer.to_string();
        let cues: [&[u8]; 5] = [
            b"Up/Dn",
            b"Enter",
            b"Esc",
            &older_id.as_bytes()[older_id.len() - 8..],
            &newer_id.as_bytes()[newer_id.len() - 8..],
        ];
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cues
            .iter()
            .all(|cue| transcript.windows(cue.len()).any(|part| part == *cue))
        {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            assert!(
                Instant::now() < deadline,
                "narrow picker frame incomplete in {name}: {}",
                tail(&transcript)
            );
            thread::yield_now();
        }
    }
    let selected_at = transcript.len();
    input.write_all(selection).expect("picker selection");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript[selected_at..]
        .windows(b"\x1b[?1000l".len())
        .any(|part| part == b"\x1b[?1000l")
        || !transcript[selected_at..]
            .windows(b"\x1b[?25h".len())
            .any(|part| part == b"\x1b[?25h")
    {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "mouse capture not disabled in {name}: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    if stale_activation {
        let original_mode = std::fs::metadata(&state)
            .expect("fixture State mode")
            .permissions();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755))
            .expect("make fixture State unsafe");
        let rejected_at = transcript.len();
        input
            .write_all(b"/sessions\r")
            .expect("reject unsafe listing");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            let recent = &transcript[rejected_at..];
            if recent
                .windows(b"unsafe".len())
                .any(|part| part == b"unsafe")
                && recent.windows(6).any(|part| part == b"\x1b[?25h")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "unsafe listing feedback: {}",
                tail(recent)
            );
            thread::yield_now();
        }
        std::fs::set_permissions(&state, original_mode).expect("restore fixture State mode");
        assert!(
            transcript[rejected_at..]
                .windows(b"Arany \xc2\xb7 error:".len())
                .any(|part| part == b"Arany \xc2\xb7 error:"),
            "unavailable Session listing needs an error heading: {}",
            tail(&transcript[rejected_at..(rejected_at + 512).min(transcript.len())])
        );
        let open_at = transcript.len();
        input
            .write_all(b"/sessions\r")
            .expect("open attached picker");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            let recent = &transcript[open_at..];
            if recent.windows(8).any(|part| part == b"\x1b[?1000h")
                && recent.windows(6).any(|part| part == b"\x1b[?25l")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "attached picker: {}",
                tail(recent)
            );
            thread::yield_now();
        }
        let original_workspace = temp.path().join("workspace-original");
        let replacement_workspace = temp.path().join("workspace-replacement");
        std::fs::rename(&workspace, &original_workspace).expect("retain original Workspace");
        std::fs::create_dir(&workspace).expect("replace fixture Workspace identity");
        let failed_at = transcript.len();
        input
            .write_all(b"\r")
            .expect("activate the stale Session row");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            let recent = &transcript[failed_at..];
            if recent
                .windows(b"Workspace".len())
                .any(|part| part == b"Workspace")
                && recent.windows(6).any(|part| part == b"\x1b[?25h")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "stale activation: {}",
                tail(recent)
            );
            thread::yield_now();
        }
        std::fs::rename(&workspace, &replacement_workspace).expect("retain replacement fixture");
        std::fs::rename(&original_workspace, &workspace).expect("restore original Workspace");
        assert!(
            transcript[failed_at..]
                .windows(b"Arany \xc2\xb7 error:".len())
                .any(|part| part == b"Arany \xc2\xb7 error:"),
            "rejected Session activation needs an error heading: {}",
            tail(&transcript[failed_at..(failed_at + 512).min(transcript.len())])
        );
    }
    input
        .write_all(b"/rename Mouse\r/quit\r")
        .expect("rename selected Session and exit");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        if attached
            .child()
            .try_wait()
            .expect("picker status")
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "picker did not exit in {name}: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    let result = wait_product(attached.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(
        result.status.success(),
        "picker {name} failed: {}",
        tail(&transcript)
    );
    assert_eq!(result.stderr, b"");
    let output_text = String::from_utf8(transcript).expect("PTY transcript UTF-8");
    let before = output_text
        .lines()
        .find_map(|line| line.strip_prefix("TTY_BEFORE:"))
        .expect("initial terminal settings")
        .trim_end_matches('\r');
    let after = output_text
        .split_once("TTY_AFTER:")
        .expect("restored terminal marker")
        .1
        .lines()
        .next()
        .expect("restored terminal settings")
        .trim_end_matches('\r');
    assert_eq!(before, after, "terminal restored after {name}");
    let capture_count = if stale_activation { 2 } else { 1 };
    assert_eq!(output_text.matches("\x1b[?1000h").count(), capture_count);
    assert_eq!(output_text.matches("\x1b[?1000l").count(), capture_count);
    assert!(!output_text.contains("\x1b[?1049h"));

    let store = Store::open_read_only(StateRoot::open_existing(&state).expect("state reopen"))
        .expect("read-only Store");
    let expected = if older_selected {
        [(older, "Mouse"), (newer, "Newer")]
    } else {
        [(older, "Older"), (newer, "Mouse")]
    };
    for (id, title) in expected {
        let events = runtime.block_on(store.load_session(id)).expect("Events");
        let view = SessionView::replay(id, &events)
            .expect("strict replay")
            .expect("Session");
        assert_eq!(view.title, title, "{name} chose the wrong Session");
        assert_eq!(events.len(), if title == "Mouse" { 2 } else { 1 });
        assert!(view.runs.is_empty());
    }
    runtime.block_on(store.close()).expect("close Store");
}

#[test]
fn session_picker_selects_visible_rows_and_restores_capture() {
    run_picker_case(
        "hover and stale activation",
        80,
        b"\x1b[<35;1;15M\r",
        true,
        true,
    );
    run_picker_case("wheel", 80, b"\x1b[<65;1;14M\r", true, false);
    run_picker_case("click", 80, b"\x1b[<0;1;15M", true, false);
    run_picker_case("off-surface wheel", 80, b"\x1b[<65;1;1M\r", false, false);
    run_picker_case("16-column keyboard", 16, b"\x1b[B\r", true, false);
}

fn screen_reader_entry(
    state: &std::path::Path,
    workspace: &std::path::Path,
    resume: bool,
) -> String {
    let entry = if resume {
        "--continue"
    } else {
        "--provider openai --model gpt-5.4"
    };
    let shell = format!(
        "before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" {entry}; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\""
    );
    let mut command = Command::new("/usr/bin/script");
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", state)
        .env("ARANY_TEST_WORKSPACE", workspace)
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(workspace)
        .args(["-q", "-e", "-c", &shell, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("screen-reader process"));
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout pipe flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking stdout pipe");
    let mut transcript = Vec::new();
    let mut answered = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript.windows(8).any(|part| part == b"Input:\r\n") {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "screen-reader input not ready: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    if !resume {
        let original_mode = std::fs::metadata(state)
            .expect("fixture State mode")
            .permissions();
        std::fs::set_permissions(state, std::fs::Permissions::from_mode(0o755))
            .expect("make fixture State unsafe");
        let rejected_at = transcript.len();
        input
            .write_all(b"/sessions\r")
            .expect("reject unsafe listing");
        let expected = b"Notice: Error: state directory unavailable or unsafe\r\nInput:\r\n";
        let deadline = Instant::now() + Duration::from_secs(10);
        while !transcript[rejected_at..]
            .windows(expected.len())
            .any(|part| part == expected)
        {
            pump(&mut output, &mut input, &mut transcript, &mut answered);
            assert!(
                Instant::now() < deadline,
                "linear listing error: {}",
                tail(&transcript[rejected_at..])
            );
            thread::yield_now();
        }
        std::fs::set_permissions(state, original_mode).expect("restore fixture State mode");
    }
    input.write_all(b"/quit\r").expect("quit Session");
    let result = wait_product(child.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(
        result.status.success(),
        "Session exit: {}",
        tail(&transcript)
    );
    assert_eq!(result.stderr, b"");
    assert_eq!(answered, 0, "screen-reader mode uses no cursor query");
    let text = String::from_utf8(transcript).expect("screen-reader UTF-8");
    let marker = |name: &str| {
        text.lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
            .expect("terminal marker")
            .to_owned()
    };
    assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
    assert!(
        !text.contains('\x1b'),
        "screen-reader output is control-free"
    );
    marker("Session: ")
}

#[test]
fn screen_reader_continue_reopens_the_empty_workspace_session() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");

    let created = screen_reader_entry(&state, &workspace, false);
    let continued = screen_reader_entry(&state, &workspace, true);
    assert_eq!(continued, created, "--continue must not create a Session");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only runtime");
    runtime.block_on(async {
        let root = StateRoot::open_existing(&state).expect("existing State");
        let sessions = arany::list_sessions(root, workspace.clone())
            .await
            .expect("Workspace Sessions");
        assert_eq!(sessions.len(), 1);
        let store = Store::open_read_only(StateRoot::open_existing(&state).expect("state reopen"))
            .expect("read-only Store");
        let events = store.load_session(sessions[0].id).await.expect("Events");
        let view = SessionView::replay(sessions[0].id, &events)
            .expect("strict replay")
            .expect("Session");
        assert!(view.runs.is_empty());
        assert_eq!(view.defaults.provider.as_deref(), Some("openai"));
        assert_eq!(view.defaults.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(events.len(), 2);
        store.close().await.expect("close Store");
    });
}

#[path = "session_picker/termination.rs"]
mod termination;

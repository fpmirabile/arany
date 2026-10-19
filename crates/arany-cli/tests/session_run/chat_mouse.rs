use super::active_terminal::{ProductGuard, product_child_of_executable};
use super::loopback::{ChildGuard, wait_product};
use super::session_picker::{PtyResponses, pump, tail};
use arany::{StateRoot, create_session};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    process::Stdio,
    thread,
    time::{Duration, Instant},
};

const MOUSE_MODES: [u32; 5] = [1000, 1002, 1003, 1006, 1015];

fn mouse_modes_on(bytes: &[u8]) -> Vec<u32> {
    let text = String::from_utf8_lossy(bytes);
    let mut on = Vec::new();
    let mut rest = text.as_ref();
    while let Some(start) = rest.find("\x1b[?") {
        rest = &rest[start + 3..];
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == ';'))
            .unwrap_or(rest.len());
        let enable = match rest[end..].chars().next() {
            Some('h') => true,
            Some('l') => false,
            _ => continue,
        };
        for mode in rest[..end].split(';').filter_map(|part| part.parse().ok()) {
            if MOUSE_MODES.contains(&mode) {
                on.retain(|active| *active != mode);
                if enable {
                    on.push(mode);
                }
            }
        }
    }
    on
}

fn wait_for(
    output: &mut impl std::io::Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut PtyResponses,
    from: usize,
    needles: &[&[u8]],
    label: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !needles.iter().all(|needle| {
        transcript[from..]
            .windows(needle.len())
            .any(|part| part == *needle)
    }) {
        pump(output, input, transcript, answered);
        assert!(
            Instant::now() < deadline,
            "{label}: {}",
            tail(&transcript[from..])
        );
        thread::yield_now();
    }
}

fn checkpoint(
    output: &mut impl std::io::Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut PtyResponses,
    label: &str,
    bytes: &[u8],
    needles: &[&[u8]],
) {
    let from = transcript.len();
    input.write_all(bytes).expect("chat input");
    wait_for(output, input, transcript, answered, from, needles, label);
    pump(output, input, transcript, answered);
    assert_eq!(
        mouse_modes_on(transcript),
        Vec::<u32>::new(),
        "mouse reporting active after {label}: {}",
        tail(transcript)
    );
}

#[test]
fn fullscreen_chat_keeps_mouse_reporting_off_at_every_returned_checkpoint() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("Session runtime");
    let root = StateRoot::admit(&state).expect("state root");
    runtime
        .block_on(create_session(
            StateRoot::open_existing(&state).expect("state reopen"),
            workspace.clone(),
            Some("Only".into()),
        ))
        .expect("Session");
    drop(root);

    let command = "stty rows 24 cols 80; printf 'SHELL_PID:%s\\n' \"$$\"; \"$ARANY_TEST_EXE\" --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume; exit_code=$?; exit \"$exit_code\"";
    let mut attached = crate::isolation::isolated_script(temp.path());
    attached
        .env_clear()
        .env("ARANY_TEST_EXE", "/arany")
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut attached = ChildGuard::new(attached.spawn().expect("chat process"));
    let mut input = attached.child().stdin.take().expect("PTY input");
    let mut output = attached.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout pipe flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking stdout pipe");
    let mut transcript = Vec::new();
    let mut answered = PtyResponses::default();

    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        0,
        &[b"\x1b[?1000h", b"\x1b[?1049h"],
        "fullscreen picker",
    );
    let shell_pid = String::from_utf8_lossy(&transcript)
        .lines()
        .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
        .expect("shell PID")
        .parse()
        .expect("numeric shell PID");
    let _product = ProductGuard::new(
        product_child_of_executable(shell_pid, std::path::Path::new("/arany")),
        state.clone(),
    );

    checkpoint(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "picker close",
        b"\r",
        &[b"\x1b[?25h\x1b[22;3H"],
    );
    checkpoint(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "draft redraw",
        b"x",
        &[b"\x1b[?25h\x1b[22;4H"],
    );
    checkpoint(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "draft cleared",
        b"\x7f",
        &[b"\x1b[?25h\x1b[22;3H"],
    );
    let from = transcript.len();
    input.write_all(b"/resume\r").expect("open picker");
    wait_for(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        from,
        &[b"\x1b[?1000h"],
        "picker rows capture transiently",
    );
    let mut active = mouse_modes_on(&transcript);
    active.sort_unstable();
    assert_eq!(active, MOUSE_MODES.to_vec());
    checkpoint(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "picker escape",
        b"\x1b",
        &[b"\x1b[?25h\x1b[22;3H"],
    );

    let original_mode = std::fs::metadata(&state).expect("State mode").permissions();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755))
        .expect("make State unsafe");
    checkpoint(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "listing error",
        b"/resume\r",
        &[
            b"state directory unavailable or unsafe",
            b"\x1b[?25h\x1b[22;3H",
        ],
    );
    std::fs::set_permissions(&state, original_mode).expect("restore State mode");
    checkpoint(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        "error cleared",
        b"y",
        &[b"\x1b[?25h\x1b[22;4H"],
    );

    input.write_all(b"\x7f/exit\r").expect("exit");
    let deadline = Instant::now() + Duration::from_secs(10);
    while attached.child().try_wait().expect("status").is_none() {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(Instant::now() < deadline, "exit: {}", tail(&transcript));
        thread::yield_now();
    }
    let result = wait_product(attached.take());
    assert!(
        result.status.success(),
        "chat failed: {}",
        tail(&transcript)
    );
}

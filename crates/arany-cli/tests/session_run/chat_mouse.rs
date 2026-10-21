use super::active_terminal::{ProductGuard, product_child_of_executable, wait_stopped};
use super::loopback::{ChildGuard, wait_product};
use super::session_picker::{PtyResponses, pump, screen_rows, tail};
use arany::{StateRoot, create_session};
use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    process::{Pid, Signal, kill_process},
};
use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{ChildStdin, ChildStdout, Stdio},
    thread,
    time::{Duration, Instant},
};

const CAPTURE: [u32; 3] = [1000, 1002, 1006];
const TRACKED: [u32; 6] = [1000, 1002, 1003, 1006, 1007, 1015];
const EMPTY_COMPOSER: &[u8] = b"\x1b[?25h\x1b[22;3H";

// Folds DEC private mode toggles, remembering every mode that was ever enabled.
#[derive(Clone, Default)]
struct Modes {
    active: Vec<u32>,
    enabled: Vec<u32>,
}

impl Modes {
    fn apply(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
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
                if TRACKED.contains(&mode) {
                    self.active.retain(|active| *active != mode);
                    if enable {
                        self.active.push(mode);
                        if !self.enabled.contains(&mode) {
                            self.enabled.push(mode);
                        }
                    }
                }
            }
        }
    }
}

// Fields drop in order: the product guard, then the PTY pipes, then the reaping child guard.
struct Chat {
    product: Option<(Pid, ProductGuard)>,
    output: ChildStdout,
    input: ChildStdin,
    child: ChildGuard,
    transcript: Vec<u8>,
    answered: PtyResponses,
    // Modes folded from captures that `fresh` discarded.
    folded: Modes,
}

impl Chat {
    fn start(root: &Path, state: &Path, workspace: &Path, env: &[(&str, &str)]) -> Self {
        let command = "stty rows 24 cols 80; printf 'SHELL_PID:%s\\n' \"$$\"; \"$ARANY_TEST_EXE\" --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --resume; exit_code=$?; exit \"$exit_code\"";
        let mut attached = crate::isolation::isolated_script(root);
        attached
            .env_clear()
            .env("ARANY_TEST_EXE", "/arany")
            .env("ARANY_TEST_STATE", state)
            .env("ARANY_TEST_WORKSPACE", workspace)
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm")
            .envs(env.iter().copied())
            .current_dir(workspace)
            .args(["-q", "-e", "-c", command, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ChildGuard::new(attached.spawn().expect("chat process"));
        let input = child.child().stdin.take().expect("PTY input");
        let output = child.child().stdout.take().expect("PTY output");
        let flags = fcntl_getfl(&output).expect("stdout pipe flags");
        fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking stdout pipe");
        Self {
            product: None,
            output,
            input,
            child,
            transcript: Vec::new(),
            answered: PtyResponses::default(),
            folded: Modes::default(),
        }
    }

    fn pump(&mut self) {
        pump(
            &mut self.output,
            &mut self.input,
            &mut self.transcript,
            &mut self.answered,
        );
    }

    fn wait(&mut self, from: usize, done: impl Fn(&[u8]) -> bool, label: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(&self.transcript[from..]) {
            self.pump();
            assert!(
                Instant::now() < deadline,
                "{label}: {}",
                tail(&self.transcript[from..])
            );
            thread::yield_now();
        }
    }

    fn wait_for(&mut self, from: usize, needles: &[&[u8]], label: &str) {
        self.wait(
            from,
            |bytes| {
                needles
                    .iter()
                    .all(|needle| bytes.windows(needle.len()).any(|part| part == *needle))
            },
            label,
        );
    }

    // A picker takes keys only once its armed frame is drawn.
    fn wait_armed(&mut self, from: usize, label: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while crate::armed_selector(&self.transcript, from, 80, 24).is_none() {
            self.pump();
            assert!(
                Instant::now() < deadline,
                "{label}: {}",
                tail(&self.transcript[from..])
            );
            thread::yield_now();
        }
    }

    fn adopt_product(&mut self, state: &Path) -> Pid {
        let shell_pid = String::from_utf8_lossy(&self.transcript)
            .lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix("SHELL_PID:"))
            .expect("shell PID")
            .parse()
            .expect("numeric shell PID");
        let pid = product_child_of_executable(shell_pid, Path::new("/arany"));
        self.product = Some((pid, ProductGuard::new(pid, state.to_owned())));
        pid
    }

    fn send(&mut self, bytes: &[u8]) -> usize {
        assert!(bytes.len() <= 1024, "bounded PTY input write");
        let from = self.transcript.len();
        self.input.write_all(bytes).expect("chat input");
        from
    }

    fn modes(&self) -> Modes {
        let mut modes = self.folded.clone();
        modes.apply(&self.transcript);
        modes
    }

    fn assert_active(&self, expected: &[u32], label: &str) {
        let mut active = self.modes().active;
        active.sort_unstable();
        assert_eq!(active, expected, "{label}: {}", tail(&self.transcript));
    }

    fn step(&mut self, label: &str, bytes: &[u8], needles: &[&[u8]], expected: &[u32]) {
        let from = self.send(bytes);
        self.wait_for(from, needles, label);
        self.pump();
        self.assert_active(expected, label);
    }

    // Starts a new capture so the 64 KiB pump bound covers the remaining steps.
    fn fresh(&mut self) {
        self.folded = self.modes();
        self.transcript.clear();
        self.answered = PtyResponses::default();
    }

    fn exit(mut self) -> Modes {
        self.send(b"/exit\r");
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.child.child().try_wait().expect("status").is_none() {
            self.pump();
            assert!(
                Instant::now() < deadline,
                "exit: {}",
                tail(&self.transcript)
            );
            thread::yield_now();
        }
        let result = wait_product(self.child.take());
        self.pump();
        assert!(
            result.status.success(),
            "chat failed: {}",
            tail(&self.transcript)
        );
        self.modes()
    }
}

fn session_fixture(fullscreen: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    if fullscreen {
        crate::seed_fullscreen(&state);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("Session runtime");
    runtime
        .block_on(create_session(
            StateRoot::admit(&state).expect("state root"),
            workspace.clone(),
            Some("Only".into()),
        ))
        .expect("Session");
    (temp, state, workspace)
}

#[test]
fn fullscreen_chat_captures_exact_mouse_modes_and_copies_a_drag_selection() {
    let (temp, state, workspace) = session_fixture(true);
    // A remote session skips native clipboards, so a copy can only be one OSC 52 sequence.
    let mut chat = Chat::start(
        temp.path(),
        &state,
        &workspace,
        &[("SSH_CONNECTION", "192.0.2.1 50000 192.0.2.2 22")],
    );
    chat.wait_for(0, &[b"\x1b[?1049h", b"\x1b[?1006h"], "fullscreen capture");
    chat.wait_armed(0, "armed fullscreen picker");
    let pid = chat.adopt_product(&state);
    chat.assert_active(&CAPTURE, "startup picker");

    chat.step("picker close", b"\r", &[EMPTY_COMPOSER], &CAPTURE);
    chat.step("draft redraw", b"x", &[b"\x1b[?25h\x1b[22;4H"], &CAPTURE);
    chat.step("draft cleared", b"\x7f", &[EMPTY_COMPOSER], &CAPTURE);
    let from = chat.send(b"/resume\r");
    chat.wait_armed(from, "armed reopened picker");
    chat.assert_active(&CAPTURE, "reopened picker");
    assert!(
        !chat.transcript[from..]
            .windows(4)
            .any(|part| part == b"\x1b[?1"),
        "pickers reuse the global capture: {}",
        tail(&chat.transcript[from..])
    );
    chat.step("picker escape", b"\x1b", &[EMPTY_COMPOSER], &CAPTURE);

    let original_mode = std::fs::metadata(&state).expect("State mode").permissions();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755))
        .expect("make State unsafe");
    // Fullscreen positions each word separately, so match the words of the error.
    chat.step(
        "listing error",
        b"/resume\r",
        &[b"directory", b"unavailable", b"unsafe", EMPTY_COMPOSER],
        &CAPTURE,
    );
    std::fs::set_permissions(&state, original_mode).expect("restore State mode");

    // Drag from the error heading past the end of its body; the heading and indent stay out.
    let screen = screen_rows(&chat.transcript, 80, 24);
    let heading = screen
        .iter()
        .rposition(|row| row.trim_end() == "Arany · error:")
        .unwrap_or_else(|| panic!("error heading on screen: {screen:#?}"));
    assert_eq!(
        screen[heading + 1].trim_end(),
        "  state directory unavailable or unsafe"
    );
    let (heading, body) = (heading + 1, heading + 2);
    let drag =
        format!("\x1b[<0;1;{heading}M\x1b[<32;40;{body}M\x1b[<32;80;{body}M\x1b[<0;80;{body}m");
    chat.step(
        "drag copy",
        drag.as_bytes(),
        &[b"\x1b]52;c;c3RhdGUgZGlyZWN0b3J5IHVuYXZhaWxhYmxlIG9yIHVuc2FmZQ==\x07"],
        &CAPTURE,
    );
    let copies = chat
        .transcript
        .windows(5)
        .filter(|part| *part == b"\x1b]52;")
        .count();
    assert_eq!(copies, 1, "one gesture writes one copy");
    chat.step(
        "draft after copy",
        b"y",
        &[b"\x1b[?25h\x1b[22;4H"],
        &CAPTURE,
    );
    chat.step("draft removed", b"\x7f", &[EMPTY_COMPOSER], &CAPTURE);

    // Suspension releases every mode before the process stops and restores them after.
    chat.fresh();
    let from = chat.send(b"\x1a");
    wait_stopped(pid);
    chat.wait(
        from,
        |bytes| bytes.windows(8).any(|part| part == b"\x1b[?1000l"),
        "capture released before stop",
    );
    chat.pump();
    chat.assert_active(&[], "stopped");
    let resumed_at = chat.transcript.len();
    kill_process(pid, Signal::CONT).expect("resume chat");
    chat.wait_for(
        resumed_at,
        &[b"\x1b[?1049h", b"\x1b[?1006h", EMPTY_COMPOSER],
        "capture reacquired",
    );
    chat.pump();
    chat.assert_active(&CAPTURE, "resumed");

    let modes = chat.exit();
    assert_eq!(modes.active, Vec::<u32>::new(), "modes restored on exit");
    let mut enabled = modes.enabled;
    enabled.sort_unstable();
    assert_eq!(enabled, CAPTURE, "never 1003, 1015 or 1007");
}

#[test]
fn classic_chat_enables_no_mouse_or_alternate_scroll_mode() {
    let (temp, state, workspace) = session_fixture(false);
    let mut chat = Chat::start(temp.path(), &state, &workspace, &[]);
    chat.wait_armed(0, "armed Classic picker");
    chat.adopt_product(&state);
    let from = chat.send(b"\r");
    chat.wait_for(from, &[b"\x1b[?25h"], "Classic composer");
    let modes = chat.exit();
    assert_eq!(modes.enabled, Vec::<u32>::new(), "Classic enabled a mode");
}

#[test]
fn disabled_mouse_fullscreen_uses_alternate_scroll_only() {
    let (temp, state, workspace) = session_fixture(true);
    let mut chat = Chat::start(
        temp.path(),
        &state,
        &workspace,
        &[("ARANY_DISABLE_MOUSE", "1")],
    );
    chat.wait_for(0, &[b"\x1b[?1049h", b"\x1b[?1007h"], "alternate scroll");
    chat.wait_armed(0, "armed fullscreen picker");
    chat.adopt_product(&state);
    chat.assert_active(&[1007], "startup picker");
    chat.step("picker close", b"\r", &[EMPTY_COMPOSER], &[1007]);
    let modes = chat.exit();
    assert_eq!(modes.active, Vec::<u32>::new(), "alternate scroll restored");
    assert_eq!(modes.enabled, [1007], "no mouse reporting mode");
}

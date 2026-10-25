use super::active_terminal::{ProductGuard, product_child_of_executable, wait_stopped};
use super::loopback::{ChildGuard, read_request, send_response, wait_product, write_profile};
use super::session_picker::{PtyResponses, pump, screen_rows, tail};
use arany::{StateRoot, create_session};
use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    process::{Pid, Signal, kill_process},
};
use std::{
    io::Write,
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{ChildStdin, ChildStdout, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const CAPTURE: [u32; 3] = [1000, 1002, 1006];
const CAPTURE_WITH_MOTION: [u32; 4] = [1000, 1002, 1003, 1006];
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
    reply_to_kitty: bool,
    kitty_reply_sent: bool,
}

impl Chat {
    fn start(root: &Path, state: &Path, workspace: &Path, env: &[(&str, &str)]) -> Self {
        Self::launch(root, state, workspace, env, "--resume", false)
    }

    // `arguments` follow the State and Workspace flags; `network` keeps loopback for a Provider.
    fn launch(
        root: &Path,
        state: &Path,
        workspace: &Path,
        env: &[(&str, &str)],
        arguments: &str,
        network: bool,
    ) -> Self {
        let command = format!(
            "stty rows 24 cols 80; printf 'SHELL_PID:%s\\n' \"$$\"; \"$ARANY_TEST_EXE\" --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" {arguments}; exit_code=$?; exit \"$exit_code\""
        );
        let mut attached = if network {
            crate::isolation::account_isolated_script(root)
        } else {
            crate::isolation::isolated_script(root)
        };
        attached
            .env_clear()
            .env("ARANY_TEST_EXE", "/arany")
            .env("ARANY_TEST_STATE", state)
            .env("ARANY_TEST_WORKSPACE", workspace)
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm")
            .envs(env.iter().copied())
            .current_dir(workspace)
            .args(["-q", "-e", "-c", &command, "/dev/null"])
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
            reply_to_kitty: false,
            kitty_reply_sent: false,
        }
    }

    fn pump(&mut self) {
        pump(
            &mut self.output,
            &mut self.input,
            &mut self.transcript,
            &mut self.answered,
        );
        if self.reply_to_kitty
            && !self.kitty_reply_sent
            && self
                .transcript
                .windows(b"\x1b_Gi=31,s=1,v=1,a=q".len())
                .any(|part| part == b"\x1b_Gi=31,s=1,v=1,a=q")
        {
            self.input
                .write_all(b"\x1b_Gi=31;OK\x1b\\\x1b[?0u\x1b[?62;52;c")
                .expect("answer Kitty graphics probe");
            self.kitty_reply_sent = true;
        }
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

    fn wait_provider(&mut self, ready: &mpsc::Receiver<()>, label: &str) {
        self.wait(
            0,
            |_| match ready.try_recv() {
                Ok(()) => true,
                Err(mpsc::TryRecvError::Empty) => false,
                Err(mpsc::TryRecvError::Disconnected) => panic!("{label}: provider disconnected"),
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

    fn exit(self) -> Modes {
        self.exit_with(b"/exit\r")
    }

    fn exit_with(mut self, keys: &[u8]) -> Modes {
        self.send(keys);
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

#[test]
fn kitty_probe_enables_any_motion_and_a_burst_keeps_following_keys() {
    let (temp, state, workspace) = session_fixture(true);
    let mut chat = Chat::start(temp.path(), &state, &workspace, &[]);
    chat.reply_to_kitty = true;
    chat.wait_for(
        0,
        &[b"\x1b_Gi=31,s=1,v=1,a=q", b"\x1b[?1003h"],
        "Kitty probe and any-motion capture",
    );
    chat.assert_active(&CAPTURE_WITH_MOTION, "Kitty capable terminal");
    chat.wait_armed(0, "armed fullscreen picker");
    chat.step(
        "picker close",
        b"\r",
        &[EMPTY_COMPOSER],
        &CAPTURE_WITH_MOTION,
    );

    let motion = b"\x1b[<35;40;12M";
    for _ in 0..50 {
        for _ in 0..4 {
            chat.send(motion);
        }
        chat.pump();
    }
    let from = chat.send(b"x");
    chat.wait_for(from, &[b"x"], "key after motion burst");
    chat.step(
        "burst draft cleared",
        b"\x7f",
        &[EMPTY_COMPOSER],
        &CAPTURE_WITH_MOTION,
    );
    let modes = chat.exit();
    assert_eq!(modes.active, Vec::<u32>::new(), "motion mode restored");
    assert!(modes.enabled.contains(&1003));
}

#[test]
fn kitty_hover_tracks_drag_leave_and_revalidates_composer_geometry() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    let (temp, state, workspace) = session_fixture(true);
    let image = arany::test_support::test_image();
    std::fs::write(
        workspace.join("preview.png"),
        STANDARD.decode(image.base64()).expect("test PNG bytes"),
    )
    .expect("workspace PNG");
    let mut chat = Chat::start(temp.path(), &state, &workspace, &[]);
    chat.reply_to_kitty = true;
    chat.wait_for(
        0,
        &[b"\x1b_Gi=31,s=1,v=1,a=q", b"\x1b[?1003h"],
        "Kitty probe and any-motion capture",
    );
    chat.wait_armed(0, "armed fullscreen picker");
    chat.step(
        "picker close",
        b"\r",
        &[EMPTY_COMPOSER],
        &CAPTURE_WITH_MOTION,
    );

    let attached = chat.send(b"\x1b[200~preview.png\x1b[201~");
    chat.wait(attached, has_image_chip, "image attachment chip");
    let (column, row) = image_chip_position(&chat.transcript);
    let transfer = b"\x1b_Ga=T";
    let deletion = b"\x1b_Ga=d,d=I,i=31";

    let before_enter = chat.transcript.len();
    let frames_before_enter = count_bytes(&chat.transcript, b"\x1b[?25h");
    chat.send(format!("\x1b[<35;{column};{row}M").as_bytes());
    chat.wait(
        before_enter,
        |bytes| count_bytes(bytes, transfer) == 1,
        "first Kitty preview transfer",
    );
    assert!(screen_has_preview(&chat.transcript), "preview frame drawn");
    assert_eq!(
        count_bytes(&chat.transcript, b"\x1b[?25h") - frames_before_enter,
        1,
        "enter draws one frame"
    );

    let before_drag = chat.transcript.len();
    let frames_before_drag = count_bytes(&chat.transcript, b"\x1b[?25h");
    let inside_column = column.saturating_add(2);
    let drag = format!(
        "\x1b[<35;{inside_column};{row}M\x1b[<0;{column};{row}M\x1b[<32;70;{row}M\x1b[<0;70;{row}m"
    );
    chat.send(drag.as_bytes());
    chat.wait(
        before_drag,
        |bytes| count_bytes(bytes, deletion) == 1,
        "drag out deletes Kitty preview",
    );
    assert!(
        !screen_has_preview(&chat.transcript),
        "leave repaints balloon cells"
    );
    assert_eq!(
        count_bytes(&chat.transcript, transfer),
        1,
        "no retransmit on drag"
    );
    assert_eq!(
        count_bytes(&chat.transcript, b"\x1b[?25h") - frames_before_drag,
        1,
        "within-chip motion is suppressed and drag leave draws one frame"
    );

    let (moved_column, moved_row) = image_chip_position(&chat.transcript);
    let before_reenter = chat.transcript.len();
    chat.send(format!("\x1b[<35;{moved_column};{moved_row}M").as_bytes());
    chat.wait(
        before_reenter,
        |bytes| count_bytes(bytes, transfer) == 1,
        "preview reentry after drag",
    );
    let previous_cursor = kitty_image_cursor(&chat.transcript[before_reenter..])
        .expect("first Kitty placement coordinates");

    let before_geometry = chat.transcript.len();
    let old_row = moved_row;
    let wrap = format!("\x1b[200~{}\x1b[201~", "a".repeat(80));
    chat.send(wrap.as_bytes());
    chat.wait(
        before_geometry,
        |bytes| count_bytes(bytes, deletion) == 1,
        "composer reflow invalidates old placement",
    );
    let (new_column, new_row) = image_chip_position(&chat.transcript);
    assert_ne!(new_row, old_row, "wrapped draft moves the image chip");
    assert!(
        !screen_has_preview(&chat.transcript),
        "stale pointer leaves preview closed"
    );

    let before_reposition = chat.transcript.len();
    chat.send(format!("\x1b[<35;{new_column};{new_row}M").as_bytes());
    chat.wait(
        before_reposition,
        |bytes| count_bytes(bytes, transfer) == 1,
        "preview follows the reflowed image chip",
    );
    let repositioned_cursor = kitty_image_cursor(&chat.transcript[before_reposition..])
        .expect("repositioned Kitty coordinates");
    assert_ne!(
        repositioned_cursor, previous_cursor,
        "image placement uses the updated geometry"
    );

    let before_picker = chat.transcript.len();
    chat.send(b"\x0b");
    chat.wait(
        before_picker,
        |bytes| count_bytes(bytes, deletion) == 1,
        "quick-action picker deletes the active preview",
    );
    assert!(
        !screen_has_preview(&chat.transcript),
        "picker clears preview frame"
    );
    chat.wait_armed(before_picker, "armed quick-action picker");
    let before_close = chat.transcript.len();
    chat.send(b"\x1b");
    chat.wait(
        0,
        |bytes| position(&bytes[before_close..], b"\x1b[?25h").is_some() && has_image_chip(bytes),
        "picker closes",
    );
    assert!(
        has_image_chip(&chat.transcript),
        "draft chip remains after picker"
    );
    assert_eq!(
        count_bytes(&chat.transcript, transfer),
        3,
        "close does not restore hover"
    );

    let (column, row) = image_chip_position(&chat.transcript);
    let before_final_enter = chat.transcript.len();
    chat.send(format!("\x1b[<35;{column};{row}M").as_bytes());
    chat.wait(
        before_final_enter,
        |bytes| count_bytes(bytes, transfer) == 1,
        "preview reentry after picker cleanup",
    );
    let before_clear = chat.transcript.len();
    chat.send(b"\x03");
    chat.wait(
        before_clear,
        |bytes| count_bytes(bytes, deletion) == 1,
        "removing the chip deletes the active preview",
    );
    let modes = chat.exit();
    assert_eq!(modes.active, Vec::<u32>::new(), "capture restored on exit");
}

#[test]
fn kitty_hover_redraws_through_active_run_and_compaction_input_loops() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde_json::json;

    let (temp, state, workspace) = session_fixture(true);
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback provider");
    write_profile(
        &state,
        listener.local_addr().expect("provider address").port(),
    );
    let (run_ready_sender, run_ready) = mpsc::channel();
    let (compact_ready_sender, compact_ready) = mpsc::channel();
    let (release_sender, release_receiver) = mpsc::channel();
    listener
        .set_nonblocking(true)
        .expect("bounded provider accept");
    let provider = thread::spawn(move || {
        let accept = |context| {
            crate::process::accept_with_deadline(&listener, Duration::from_secs(30), context)
        };
        for index in 0..3 {
            let mut stream = accept("provider conformance request");
            read_request(&mut stream);
            let text = match index {
                0 => json!("ok"),
                1 => {
                    json!({"outcome":{"type":"delegate","children":[{"objective":"synthetic child","profile":null}]}})
                }
                _ => json!("synthetic summary"),
            };
            send_response(&mut stream, index, text);
        }

        let mut run = accept("Run request");
        read_request(&mut run);
        run_ready_sender.send(()).expect("Run request observed");
        release_receiver
            .recv_timeout(Duration::from_secs(20))
            .expect("release Run response");
        send_response(&mut run, 0, json!("active answer"));

        let mut compact = accept("compaction request");
        let body = read_request(&mut compact);
        let request: serde_json::Value = serde_json::from_slice(&body).expect("compaction JSON");
        crate::loopback::assert_compaction_request(&request);
        compact_ready_sender
            .send(())
            .expect("compaction request observed");
        release_receiver
            .recv_timeout(Duration::from_secs(20))
            .expect("release compaction response");
        send_response(&mut compact, 1, json!("synthetic summary"));
    });
    crate::loopback::check_profile(&workspace, &state);

    let png = STANDARD
        .decode(arany::test_support::test_image().base64())
        .expect("test PNG bytes");
    std::fs::write(workspace.join("preview.png"), png).expect("workspace PNG");
    let mut chat = Chat::launch(
        temp.path(),
        &state,
        &workspace,
        &[
            ("ARANY_PROVIDER_LOCAL_KEY", "test-key"),
            ("SSH_CONNECTION", "192.0.2.1 50000 192.0.2.2 22"),
        ],
        "--resume --provider custom:local --model model-1",
        true,
    );
    chat.reply_to_kitty = true;
    chat.wait_for(
        0,
        &[b"\x1b_Gi=31,s=1,v=1,a=q", b"\x1b[?1003h"],
        "Kitty probe and any-motion capture",
    );
    chat.wait_armed(0, "armed fullscreen picker");
    chat.step(
        "picker close",
        b"\r",
        &[EMPTY_COMPOSER],
        &CAPTURE_WITH_MOTION,
    );

    chat.send(b"start work\r");
    chat.wait_provider(&run_ready, "Run provider request");
    let attach = chat.send(b"\x1b[200~preview.png\x1b[201~");
    chat.wait(attach, has_image_chip, "Run draft image chip");
    let (column, row) = image_chip_position(&chat.transcript);
    let (history_column, history_row) =
        screen_text_position(&chat.transcript, "start work").expect("Run objective in History");
    let transfer = b"\x1b_Ga=T";
    let deletion = b"\x1b_Ga=d,d=I,i=31";
    let clipboard_copy = b"\x1b]52;c;";
    let from_run = chat.transcript.len();
    let selection = format!(
        "\x1b[<0;{history_column};{history_row}M\x1b[<32;{column};{row}M\x1b[<0;{column};{row}m"
    );
    chat.send(selection.as_bytes());
    chat.wait(
        from_run,
        |bytes| {
            count_bytes(bytes, transfer) >= 1
                && bytes
                    .windows(clipboard_copy.len())
                    .any(|part| part == clipboard_copy)
        },
        "Run History drag crossing chip renders and copies",
    );
    chat.send(format!("\x1b[<35;70;{row}M").as_bytes());
    chat.wait(
        from_run,
        |bytes| count_bytes(bytes, deletion) >= 1,
        "Run input loop deletes on leave",
    );
    release_sender.send(()).expect("release Run response");
    chat.wait(
        0,
        |bytes| {
            screen_text_position(bytes, "active answer").is_some()
                && screen_text_position(bytes, "Draft retained:").is_some()
        },
        "Run returns to idle with the retained draft",
    );
    let clear = chat.send(b"\x03");
    chat.wait(
        0,
        |bytes| position(&bytes[clear..], EMPTY_COMPOSER).is_some() && !has_image_chip(bytes),
        "Run draft cleared",
    );

    chat.send(b"/compact\r");
    chat.wait_provider(&compact_ready, "compaction provider request");
    let attach = chat.send(b"\x1b[200~preview.png\x1b[201~");
    chat.wait(attach, has_image_chip, "compaction draft image chip");
    let (column, row) = image_chip_position(&chat.transcript);
    let (history_column, history_row) =
        screen_text_position(&chat.transcript, "start work").expect("compaction History objective");
    let from_compaction = chat.transcript.len();
    let selection = format!(
        "\x1b[<0;{history_column};{history_row}M\x1b[<32;{column};{row}M\x1b[<0;{column};{row}m"
    );
    chat.send(selection.as_bytes());
    chat.wait(
        from_compaction,
        |bytes| {
            count_bytes(bytes, transfer) >= 1
                && bytes
                    .windows(clipboard_copy.len())
                    .any(|part| part == clipboard_copy)
        },
        "compaction History drag crossing chip renders and copies",
    );
    chat.send(format!("\x1b[<35;70;{row}M").as_bytes());
    chat.wait(
        from_compaction,
        |bytes| count_bytes(bytes, deletion) >= 1,
        "compaction input loop deletes on leave",
    );
    release_sender
        .send(())
        .expect("release compaction response");
    provider.join().expect("provider thread");
    chat.wait(
        0,
        |bytes| screen_text_position(bytes, "Compaction saved").is_some(),
        "compaction completes",
    );
    let clear = chat.send(b"\x03");
    chat.wait_for(clear, &[EMPTY_COMPOSER], "compaction draft cleared");
    chat.exit();
}

fn count_bytes(bytes: &[u8], needle: &[u8]) -> usize {
    bytes
        .windows(needle.len())
        .filter(|part| *part == needle)
        .count()
}

fn visible_screen(transcript: &[u8]) -> Vec<String> {
    let sanitized = strip_kitty_control_strings(transcript);
    screen_rows(&sanitized, 80, 24)
}

fn strip_kitty_control_strings(bytes: &[u8]) -> Vec<u8> {
    let mut sanitized = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while let Some(start) = rest.windows(3).position(|part| part == b"\x1b_G") {
        sanitized.extend_from_slice(&rest[..start]);
        rest = &rest[start + 3..];
        let Some(end) = rest.windows(2).position(|part| part == b"\x1b\\") else {
            return sanitized;
        };
        rest = &rest[end + 2..];
    }
    sanitized.extend_from_slice(rest);
    sanitized
}

fn has_image_chip(transcript: &[u8]) -> bool {
    let marker = b"\x1b[?25h";
    let Some(frame_end) = transcript
        .windows(marker.len())
        .rposition(|part| part == marker)
    else {
        return false;
    };
    visible_screen(&transcript[..frame_end])
        .iter()
        .any(|line| line.contains("[Image #1]"))
}

fn screen_text_position(transcript: &[u8], text: &str) -> Option<(usize, usize)> {
    visible_screen(transcript)
        .iter()
        .enumerate()
        .find_map(|(row, line)| line.find(text).map(|column| (column + 1, row + 1)))
}

fn image_chip_position(transcript: &[u8]) -> (usize, usize) {
    visible_screen(transcript)
        .iter()
        .enumerate()
        .find_map(|(row, line)| line.find("[Image #1]").map(|column| (column + 1, row + 1)))
        .unwrap_or_else(|| panic!("image chip on screen: {}", tail(transcript)))
}

fn screen_has_preview(transcript: &[u8]) -> bool {
    visible_screen(transcript)
        .iter()
        .any(|line| line.contains("Image 1: PNG"))
}

fn kitty_image_cursor(bytes: &[u8]) -> Option<(usize, usize)> {
    let marker = b"\x1b[s";
    let start = bytes
        .windows(marker.len())
        .rposition(|part| part == marker)?;
    let sequence = &bytes[start + marker.len()..];
    let end = sequence.iter().position(|byte| *byte == b'H')?;
    let mut coordinates = sequence[..end]
        .strip_prefix(b"\x1b[")?
        .split(|byte| *byte == b';');
    Some((
        std::str::from_utf8(coordinates.next()?)
            .ok()?
            .parse()
            .ok()?,
        std::str::from_utf8(coordinates.next()?)
            .ok()?
            .parse()
            .ok()?,
    ))
}

fn session_fixture(fullscreen: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    if !fullscreen {
        crate::seed_classic(&state);
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
fn default_fullscreen_chat_captures_exact_mouse_modes_and_copies_a_drag_selection() {
    let (temp, state, workspace) = session_fixture(true);
    assert!(!state.join("ui-preferences.json").exists());
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
    assert_no_kitty_probe(&chat.transcript);
    let modes = chat.exit();
    assert_eq!(modes.enabled, Vec::<u32>::new(), "Classic enabled a mode");
}

#[test]
fn disabled_mouse_fullscreen_uses_alternate_scroll_only() {
    for value in ["", "1"] {
        let (temp, state, workspace) = session_fixture(true);
        let mut chat = Chat::start(
            temp.path(),
            &state,
            &workspace,
            &[("ARANY_DISABLE_MOUSE", value)],
        );
        chat.wait_for(0, &[b"\x1b[?1049h", b"\x1b[?1007h"], "alternate scroll");
        chat.wait_armed(0, "armed fullscreen picker");
        assert_no_kitty_probe(&chat.transcript);
        chat.adopt_product(&state);
        chat.assert_active(&[1007], "startup picker");
        chat.step("picker close", b"\r", &[EMPTY_COMPOSER], &[1007]);
        let modes = chat.exit();
        assert_eq!(modes.active, Vec::<u32>::new(), "alternate scroll restored");
        assert_eq!(modes.enabled, [1007], "no mouse reporting mode: {value:?}");
    }
}

#[test]
fn tmux_suppresses_the_kitty_graphics_probe() {
    let (temp, state, workspace) = session_fixture(true);
    let mut chat = Chat::start(
        temp.path(),
        &state,
        &workspace,
        &[("TMUX", "session,pane,server")],
    );
    chat.wait_armed(0, "armed fullscreen picker inside tmux");
    assert_no_kitty_probe(&chat.transcript);
    chat.adopt_product(&state);
    let modes = chat.exit_with(b"\x1b");
    assert_eq!(
        modes.active,
        Vec::<u32>::new(),
        "modes restored on cancellation"
    );
}

#[test]
fn linear_chat_suppresses_the_kitty_graphics_probe() {
    let (temp, state, workspace) = session_fixture(true);
    let mut chat = Chat::start(temp.path(), &state, &workspace, &[("TERM", "dumb")]);
    chat.wait_for(
        0,
        &[
            b"Resume:",
            b"Choice 1: Only",
            b"Choose number, exact ID, n next, p previous, or q close:",
        ],
        "linear resume picker",
    );
    assert_no_kitty_probe(&chat.transcript);
    chat.adopt_product(&state);
    let from = chat.send(b"1\r");
    chat.wait_for(from, &[b"Input:"], "linear composer");
    chat.exit();
}

#[test]
fn screen_reader_chat_suppresses_the_kitty_graphics_probe() {
    let (temp, state, workspace) = session_fixture(true);
    let mut chat = Chat::launch(
        temp.path(),
        &state,
        &workspace,
        &[],
        "--screen-reader",
        false,
    );
    chat.wait_for(0, &[b"Input:\r\n"], "screen-reader startup");
    assert_no_kitty_probe(&chat.transcript);
    chat.exit();
}

fn assert_no_kitty_probe(bytes: &[u8]) {
    assert!(
        !bytes
            .windows(b"\x1b_Gi=31,s=1,v=1,a=q".len())
            .any(|part| part == b"\x1b_Gi=31,s=1,v=1,a=q"),
        "excluded terminal session sent the Kitty graphics query: {}",
        tail(bytes)
    );
}

fn position(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes.windows(needle.len()).position(|part| part == needle)
}

const OMITTED: &str = "Earlier Runs omitted; arany show prints the whole conversation";

// The lines `[` appended to the primary screen, decoded tall enough to keep the scrolled-off
// part, or None until the last line is the return cue.
fn dumped_lines(bytes: &[u8]) -> Option<Vec<String>> {
    let start = position(bytes, b"\x1b[?1049l")?;
    let mut lines = screen_rows(&bytes[start..], 80, 512)
        .into_iter()
        .map(|row| row.trim_end().to_owned())
        .collect::<Vec<_>>();
    let end = lines.iter().rposition(|line| !line.is_empty())?;
    lines.truncate(end + 1);
    (lines[end] == "Enter returns to Arany").then_some(lines)
}

// Checks the dump: capture and the alternate screen end before the bounded, escaped text.
fn assert_bounded_dump(bytes: &[u8]) {
    let lines = dumped_lines(bytes).expect("dump return cue");
    let released = [b"\x1b[?1049l".as_slice(), b"\x1b[?1000l"]
        .into_iter()
        .map(|mode| position(bytes, mode).expect("mode released by the dump"))
        .max()
        .expect("released modes");
    assert!(
        !screen_rows(&bytes[..released], 80, 24)
            .iter()
            .any(|row| row.contains("Arany transcript:")),
        "modes are released before the text"
    );
    assert_eq!(lines[..2], ["Arany transcript:", OMITTED], "{lines:#?}");
    // 17 seeded Runs and the active one: the 16 newest whole Runs, read like chat.
    let count = |expected: &str| lines.iter().filter(|line| *line == expected).count();
    assert_eq!(count("You:"), 16, "{lines:#?}");
    assert_eq!(count("  Synthetic objective"), 15);
    assert_eq!(count("DETAIL_CANARY \\u{001b}]52;c;inert\\u{0007}"), 15);
    assert_eq!(count("  Synthetic answer"), 15);
    let active = lines
        .iter()
        .rposition(|line| line == "You:")
        .expect("active Run");
    assert_eq!(lines[active + 1], "  stream");
    assert!(
        lines[active..].iter().all(|line| line != "Arany:"),
        "the active Run has no committed answer: {lines:#?}"
    );
    assert_eq!(
        position(bytes, b"\x1b]52"),
        None,
        "no raw clipboard sequence"
    );
    assert_eq!(
        position(bytes, b"\x1b[?1049h"),
        None,
        "no redraw during the dump"
    );
}

#[test]
fn fullscreen_pager_dumps_bounded_transcript_to_scrollback_while_a_run_streams() {
    const ANSWER: &str = "STREAM_CANARY";
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let id = crate::seed_transcript_session(&state, &workspace);
    let provider = super::loopback::held_stream_provider(&state, ANSWER);
    super::loopback::check_profile(&workspace, &state);
    let mut chat = Chat::launch(
        temp.path(),
        &state,
        &workspace,
        &[("ARANY_PROVIDER_LOCAL_KEY", "test-key")],
        &format!("--resume {id} --provider custom:local --model model-1 stream"),
        true,
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while provider.streaming.try_recv().is_err() {
        chat.pump();
        assert!(
            Instant::now() < deadline,
            "streamed Run call: {}",
            tail(&chat.transcript)
        );
        thread::yield_now();
    }
    let pid = chat.adopt_product(&state);

    // The pager replaces the composer with detailed History above its key footer.
    let pager = |bytes: &[u8]| {
        screen_rows(bytes, 80, 24)[23].starts_with("Transcript · / find · n/N · j/k · g/G")
    };
    chat.send(b"\x0f");
    chat.wait(0, pager, "pager footer");
    chat.pump();
    chat.assert_active(&CAPTURE, "pager during the Run");
    chat.fresh();
    let from = chat.send(b"[");
    chat.wait(from, |bytes| dumped_lines(bytes).is_some(), "dump");
    chat.pump();
    chat.assert_active(&[], "dump released capture");
    assert_bounded_dump(&chat.transcript);

    // The Run finishes its stream while the dump waits; nothing redraws until Enter.
    provider.release.send(()).expect("release the stream");
    let deadline = Instant::now() + Duration::from_secs(20);
    while provider.consumed.try_recv().is_err() {
        chat.pump();
        assert!(
            Instant::now() < deadline,
            "Run consumed its stream during the dump: {}",
            tail(&chat.transcript)
        );
        thread::yield_now();
    }
    chat.pump();
    assert_eq!(position(&chat.transcript, b"\x1b[?1049h"), None);
    chat.assert_active(&[], "dump waits for Enter");

    let back = chat.send(b"\r");
    chat.wait(
        back,
        |bytes| {
            let rows = screen_rows(bytes, 80, 24);
            position(bytes, b"\x1b[?1049h").is_some()
                && pager(bytes)
                && rows.iter().enumerate().skip(1).any(|(index, row)| {
                    row.trim_end() == format!("  {ANSWER}")
                        && rows[index - 1].trim_end() == "● Arany:"
                })
        },
        "pager returns with the finished answer",
    );
    chat.pump();
    chat.assert_active(&CAPTURE, "Enter reacquires capture");
    provider.peer.join().expect("held stream Provider");

    // Suspension during a dump restores the shell and returns to the pager on SIGCONT.
    chat.fresh();
    let from = chat.send(b"[");
    chat.wait(from, |bytes| dumped_lines(bytes).is_some(), "second dump");
    chat.pump();
    chat.assert_active(&[], "second dump released capture");
    let suspended_at = chat.send(b"\x1a");
    wait_stopped(pid);
    chat.pump();
    chat.assert_active(&[], "stopped during the dump");
    let resumed_at = chat.transcript.len();
    assert_eq!(
        position(&chat.transcript[suspended_at..], b"\x1b[2J"),
        None,
        "suspension leaves the dump on the primary screen"
    );
    kill_process(pid, Signal::CONT).expect("resume chat");
    // The resumed footer names the resume, so detail rows show that the pager reopened.
    chat.wait(
        resumed_at,
        |bytes| {
            position(bytes, b"\x1b[?1049h").is_some()
                && screen_rows(bytes, 80, 24)
                    .iter()
                    .any(|row| row.starts_with("Arany · commentary:"))
        },
        "pager after SIGCONT",
    );
    chat.pump();
    chat.assert_active(&CAPTURE, "resumed pager");
    chat.step("pager closed", b"q", &[EMPTY_COMPOSER], &CAPTURE);

    // An active Run drops an empty-draft Ctrl+D, so wait for the settled `ready`.
    chat.wait(
        0,
        |bytes| screen_rows(bytes, 80, 24)[23].starts_with("ready"),
        "settled Run before Ctrl+D",
    );
    let modes = chat.exit_with(b"\x04");
    assert_eq!(modes.active, Vec::<u32>::new(), "modes restored on exit");
    let mut enabled = modes.enabled;
    enabled.sort_unstable();
    assert_eq!(enabled, CAPTURE, "never 1003, 1015 or 1007");
}

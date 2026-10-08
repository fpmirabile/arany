use super::acquisition::ProductGuard;
use super::{
    product_child_of_executable, redacted_tail, transcript_field, tty_settings, wait_stopped,
};
use crate::loopback::{
    ChildGuard, check_profile_with_binary, read_request, send_response, wait_product, write_profile,
};
use arany::{StateRoot, list_sessions, resume_session};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::process::{Signal, kill_process};
use std::{
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

#[path = "tmux/history_latency.rs"]
mod history_latency;

struct TmuxServer {
    socket: PathBuf,
}

impl TmuxServer {
    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new("/usr/bin/tmux");
        command
            .env_clear()
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        wait_product(command.spawn().expect("tmux command"))
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let Ok(mut child) = Command::new("/usr/bin/tmux")
            .env_clear()
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if child.try_wait().ok().flatten().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                break;
            }
            thread::yield_now();
        }
        let _ = child.wait();
    }
}

fn capture_pane(server: &TmuxServer) -> Option<Vec<u8>> {
    let result = server.run(&["capture-pane", "-p", "-t", "arany-test:0.0"]);
    if !result.status.success() {
        return None;
    }
    assert!(result.stdout.len() <= 8 * 1024, "bounded tmux pane capture");
    Some(result.stdout)
}

fn decline_initial_consent(server: &TmuxServer, screen: &[u8], declined: &mut bool) {
    if *declined {
        return;
    }
    let visible: String = String::from_utf8_lossy(screen)
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if visible.contains("Doyoutrust")
        && (visible.contains("Ctrl+Cexit") || visible.contains("^Cexit"))
    {
        let dismissed = server.run(&["send-keys", "-t", "arany-test:0.0", "Escape"]);
        assert!(dismissed.status.success(), "dismiss fixture folder consent");
        *declined = true;
    }
}

fn capture_scrollback(server: &TmuxServer) -> Option<Vec<u8>> {
    let result = server.run(&["capture-pane", "-p", "-S", "-", "-t", "arany-test:0.0"]);
    if !result.status.success() {
        return None;
    }
    assert!(
        result.stdout.len() <= 16 * 1024,
        "bounded tmux scrollback capture"
    );
    Some(result.stdout)
}

fn live_tail_layout(screen: &[u8], width: usize) -> bool {
    let Ok(screen) = std::str::from_utf8(screen) else {
        return false;
    };
    let rows: Vec<_> = screen.lines().collect();
    let composer = rows.iter().position(|row| row.contains("Ask Arany"));
    let status = rows.iter().position(|row| {
        let row = row.trim_start();
        row.starts_with("ready") || row.starts_with("Session ")
    });
    rows.iter().all(|row| row.chars().count() <= width)
        && matches!((composer, status), (Some(composer), Some(status)) if status == composer + 3 && status + 1 == rows.len())
}

fn active_layout_with_draft(screen: &[u8]) -> bool {
    let Ok(screen) = std::str::from_utf8(screen) else {
        return false;
    };
    let rows: Vec<_> = screen.lines().collect();
    let Some(composer) = rows.iter().rposition(|row| row.contains("Ask Arany")) else {
        return false;
    };
    composer > 0
        && rows[composer - 1]
            .trim_start()
            .starts_with("primary · working")
        && rows
            .get(composer + 1)
            .is_some_and(|row| row.contains("pending"))
        && rows
            .get(composer + 2)
            .is_some_and(|row| row.contains("draft"))
        && rows
            .get(composer + 3)
            .is_some_and(|row| row.contains("Enter keeps draft"))
        && rows.get(composer + 4).is_some_and(|row| {
            let row = row.trim_start();
            row.starts_with("working") && row.contains("model-1")
        })
}

fn idle_multiline_layout(screen: &[u8]) -> bool {
    let Ok(screen) = std::str::from_utf8(screen) else {
        return false;
    };
    let rows: Vec<_> = screen.lines().collect();
    let Some(composer) = rows.iter().rposition(|row| row.contains("Ask Arany")) else {
        return false;
    };
    rows.get(composer + 1)
        .is_some_and(|row| row.contains("first"))
        && rows
            .get(composer + 2)
            .is_some_and(|row| row.contains("second"))
        && rows.get(composer + 4).is_some_and(|row| {
            let row = row.trim_start();
            row.starts_with("ready") || row.starts_with("Session ")
        })
}

fn read_trimmed(path: &Path) -> String {
    std::fs::read_to_string(path)
        .expect("tmux pane marker")
        .trim()
        .to_owned()
}

fn pump_pty(
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
                    "bounded tmux PTY output"
                );
                transcript.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("tmux PTY read failed: {error}"),
        }
    }
    let queries = transcript
        .windows(4)
        .filter(|part| *part == b"\x1b[6n")
        .count();
    assert!(queries <= 32, "bounded tmux cursor queries");
    while *answered < queries {
        input.write_all(b"\x1b[24;1R").expect("cursor response");
        *answered += 1;
    }
}

fn test_product_executable() -> PathBuf {
    let executable = std::env::var_os("ARANY_RELEASE_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_arany")));
    assert!(executable.is_absolute(), "absolute product binary path");
    let executable = executable.canonicalize().expect("product binary exists");
    assert!(executable.is_file(), "product binary is a regular file");
    executable
}

#[test]
#[ignore = "native Linux tmux release gate; requires /usr/bin/tmux"]
fn inline_composer_restores_tmux_pane_and_replays_empty_session() {
    let executable = test_product_executable();
    for (no_color_flag, no_color_env) in [(true, false), (false, true), (false, false)] {
        inline_composer_tmux_case(&executable, no_color_flag, no_color_env);
    }
}

fn inline_composer_tmux_case(executable: &Path, no_color_flag: bool, no_color_env: bool) {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let before_path = temp.path().join("before");
    let after_path = temp.path().join("after");
    let exit_path = temp.path().join("exit");
    let wrapper = temp.path().join("tmux-pane.sh");
    let color_flag = if no_color_flag { " --no-color" } else { "" };
    std::fs::write(
        &wrapper,
        format!("#!/bin/sh\nstty -g > \"$ARANY_TEST_BEFORE\"\n\"$ARANY_TEST_EXE\" --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider openai --model gpt-5.4{color_flag}\nresult=$?\nstty -g > \"$ARANY_TEST_AFTER\"\nprintf '%s\\n' \"$result\" > \"$ARANY_TEST_EXIT\"\nexit \"$result\"\n"),
    )
    .expect("tmux pane wrapper");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))
        .expect("private executable wrapper");

    let server = TmuxServer {
        socket: temp.path().join("tmux.sock"),
    };
    let mut start =
        crate::process::account_isolated_command(temp.path(), "/usr/bin/tmux", executable);
    start
        .env_clear()
        .env("TERM", "xterm")
        .env("SHELL", "/bin/sh")
        .env("PATH", "/usr/bin:/bin")
        .env("ARANY_TEST_EXE", executable)
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_BEFORE", &before_path)
        .env("ARANY_TEST_AFTER", &after_path)
        .env("ARANY_TEST_EXIT", &exit_path)
        .env("OPENAI_API_KEY", "synthetic-test-key")
        .arg("-S")
        .arg(&server.socket)
        .args([
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-x",
            "80",
            "-y",
            "24",
            "-s",
            "arany-test",
        ])
        .arg(&wrapper)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if no_color_env {
        start.env("NO_COLOR", "1");
    }
    let started = wait_product(start.spawn().expect("tmux server start"));
    assert!(
        started.status.success(),
        "tmux pane started: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    let mut declined = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(screen) = capture_pane(&server) {
            decline_initial_consent(&server, &screen, &mut declined);
            if live_tail_layout(&screen, 80) {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "inline tmux composer/status not visible"
        );
        thread::yield_now();
    }
    let startup = capture_pane(&server).expect("idle startup pane");
    let startup = String::from_utf8(startup).expect("idle startup UTF-8");
    eprintln!(
        "Idle terminal capture (flag no-color={no_color_flag}, env no-color={no_color_env}):\n{startup}"
    );
    assert!(startup.contains("Describe a task to begin."));
    assert!(
        startup
            .lines()
            .last()
            .is_some_and(|row| row.starts_with("ready · ") && row.contains("gpt-5.4")),
        "startup status prioritizes state and model"
    );
    let styled = server.run(&["capture-pane", "-p", "-e", "-t", "arany-test:0.0"]);
    assert!(styled.status.success(), "styled pane capture");
    assert!(
        styled.stdout.len() <= 16 * 1024,
        "bounded styled pane capture"
    );
    let cyan = b"\x1b[38;5;6m";
    assert_eq!(
        styled.stdout.windows(cyan.len()).any(|bytes| bytes == cyan),
        !(no_color_flag || no_color_env),
        "cyan accents match the selected color mode"
    );
    for (width, height) in [
        (50, 24),
        (40, 24),
        (24, 24),
        (20, 24),
        (16, 24),
        (16, 8),
        (80, 24),
    ] {
        let resized = server.run(&[
            "resize-window",
            "-x",
            &width.to_string(),
            "-y",
            &height.to_string(),
            "-t",
            "arany-test:0",
        ]);
        assert!(resized.status.success(), "tmux window resize");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let pane = server.run(&[
                "display-message",
                "-p",
                "-t",
                "arany-test:0.0",
                "#{pane_width}x#{pane_height}",
            ]);
            if let Some(screen) = capture_pane(&server)
                && pane.status.success()
                && pane.stdout == format!("{width}x{height}\n").as_bytes()
                && live_tail_layout(&screen, width)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "inline layout at {width} columns"
            );
            thread::yield_now();
        }
        if width == 20 || width == 16 {
            let styled = server.run(&["capture-pane", "-p", "-e", "-t", "arany-test:0.0"]);
            assert!(styled.status.success(), "styled narrow pane capture");
            assert!(
                styled.stdout.len() <= 16 * 1024,
                "bounded styled pane capture"
            );
            assert_eq!(
                styled.stdout.windows(cyan.len()).any(|bytes| bytes == cyan),
                !(no_color_flag || no_color_env),
                "narrow accents match the selected color mode"
            );
        }
        if width == 16 && height == 8 {
            let screen = capture_pane(&server).expect("compact terminal review capture");
            eprintln!(
                "16x8 terminal capture:\n{}",
                String::from_utf8_lossy(&screen)
            );
        }
        if width == 16 && height == 24 {
            let opened = server.run(&["send-keys", "-t", "arany-test:0.0", "C-k"]);
            assert!(opened.status.success(), "open narrow quick actions");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(screen) = capture_pane(&server)
                    && String::from_utf8_lossy(&screen).contains("Up/Dn Enter Esc")
                {
                    break;
                }
                assert!(Instant::now() < deadline, "narrow quick actions visible");
                thread::yield_now();
            }
            let closed = server.run(&["send-keys", "-t", "arany-test:0.0", "Escape"]);
            assert!(closed.status.success(), "close narrow quick actions");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(screen) = capture_pane(&server)
                    && live_tail_layout(&screen, width)
                {
                    break;
                }
                assert!(Instant::now() < deadline, "narrow composer restored");
                thread::yield_now();
            }
            if no_color_flag {
                let typed = server.run(&["send-keys", "-l", "-t", "arany-test:0.0", "/help"]);
                assert!(typed.status.success(), "type narrow help command");
                let submitted = server.run(&["send-keys", "-t", "arany-test:0.0", "Enter"]);
                assert!(submitted.status.success(), "open narrow help");
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if let Some(screen) = capture_pane(&server) {
                        let screen = String::from_utf8_lossy(&screen);
                        if screen.contains("Commands 1/16")
                            && screen.contains("> /help")
                            && screen.contains("Up/Dn Enter Esc")
                        {
                            break;
                        }
                    }
                    assert!(Instant::now() < deadline, "narrow help not visible");
                    thread::yield_now();
                }
                let moved = server.run(&["send-keys", "-t", "arany-test:0.0", "End"]);
                assert!(moved.status.success(), "navigate to last command");
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if let Some(screen) = capture_pane(&server) {
                        let screen = String::from_utf8_lossy(&screen);
                        if screen.contains("Commands 16/16")
                            && screen.contains("> /exit")
                            && screen.contains("Exit Session")
                        {
                            break;
                        }
                    }
                    assert!(
                        Instant::now() < deadline,
                        "last narrow help row not visible"
                    );
                    thread::yield_now();
                }
                let closed = server.run(&["send-keys", "-t", "arany-test:0.0", "Escape"]);
                assert!(closed.status.success(), "close narrow help");
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if let Some(screen) = capture_pane(&server)
                        && live_tail_layout(&screen, width)
                    {
                        break;
                    }
                    assert!(Instant::now() < deadline, "composer after narrow help");
                    thread::yield_now();
                }
            }
        }
    }
    let draft = "Please review the model, effort, and errors before sending.";
    let resized = server.run(&[
        "resize-window",
        "-x",
        "40",
        "-y",
        "24",
        "-t",
        "arany-test:0",
    ]);
    assert!(resized.status.success(), "resize before wrapped draft");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| live_tail_layout(&screen, 40)) {
            break;
        }
        assert!(Instant::now() < deadline, "empty resized composer ready");
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-l", "-t", "arany-test:0.0", draft])
            .status
            .success()
    );
    let wrapped_layout = |screen: &[u8]| {
        let screen = String::from_utf8_lossy(screen);
        let rows: Vec<_> = screen.lines().collect();
        let Some(composer) = rows.iter().rposition(|row| row.contains("Ask Arany")) else {
            return false;
        };
        composer + 5 == rows.len()
            && rows[composer + 1] == format!("> {}", &draft[..38])
            && rows[composer + 2].trim_start() == &draft[38..]
            && rows[composer + 3].contains("Ctrl+O newline")
            && rows[composer + 4].starts_with("ready")
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| wrapped_layout(&screen)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "ordinary draft wraps into two rows"
        );
        thread::yield_now();
    }
    eprintln!(
        "Wrapped draft capture:\n{}",
        String::from_utf8_lossy(&capture_pane(&server).expect("wrapped capture"))
    );
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Left"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let cursor = server.run(&[
            "display-message",
            "-p",
            "-t",
            "arany-test:0.0",
            "#{cursor_x},#{cursor_y}",
        ]);
        if cursor.status.success()
            && cursor.stdout == format!("{},21\n", 2 + (draft.len() - 1) % 38).as_bytes()
        {
            break;
        }
        assert!(Instant::now() < deadline, "wrapped caret moved left");
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-k"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            String::from_utf8_lossy(&screen).contains("Up/Down · Enter open · Esc return")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "selector opened over wrapped draft"
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Escape"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| wrapped_layout(&screen)) {
            break;
        }
        assert!(Instant::now() < deadline, "same wrapped draft restored");
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-l", "-t", "arany-test:0.0", "!"])
            .status
            .success()
    );
    let edited = format!("{}!.", &draft[..draft.len() - 1]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server)
            .is_some_and(|screen| String::from_utf8_lossy(&screen).contains(&edited[38..]))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "selector restored the mid-draft caret"
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["resize-window", "-x", "16", "-y", "8", "-t", "arany-test:0"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(screen) = capture_pane(&server) {
            let screen = String::from_utf8_lossy(&screen);
            let rows: Vec<_> = screen.lines().collect();
            if rows.len() == 8
                && rows[1].contains("Ask Arany")
                && rows[5].trim() == &edited[56..]
                && rows[1].contains("5/5")
                && rows[6].contains("Ctrl+O")
            {
                eprintln!("16x8 wrapped draft capture:\n{screen}");
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "wrapped draft remains visible at 16x8"
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&[
                "resize-window",
                "-x",
                "80",
                "-y",
                "24",
                "-t",
                "arany-test:0"
            ])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            live_tail_layout(&screen, 80) && String::from_utf8_lossy(&screen).contains(&edited)
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "resize restores the complete unchanged draft"
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-c"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            live_tail_layout(&screen, 80)
                && String::from_utf8_lossy(&screen)
                    .contains("Ask a question, or type / for commands")
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "clear draft without submission");
        thread::yield_now();
    }

    assert!(
        server
            .run(&["resize-window", "-x", "16", "-y", "8", "-t", "arany-test:0"])
            .status
            .success()
    );
    let wait_unicode_frame = |expected: &str, cursor: &str| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let position = server.run(&[
                "display-message",
                "-p",
                "-t",
                "arany-test:0.0",
                "#{cursor_x},#{cursor_y}",
            ]);
            if let Some(screen) = capture_pane(&server) {
                let screen = String::from_utf8_lossy(&screen);
                let rows: Vec<_> = screen.lines().collect();
                if rows.len() == 8
                    && if expected.is_empty() {
                        rows[4] == "> a" && rows[5].is_empty()
                    } else {
                        rows[5].contains(expected)
                    }
                    && rows[6].contains("Ctrl+O")
                    && rows[7].starts_with("ready")
                    && position.status.success()
                    && position.stdout == cursor.as_bytes()
                {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "Unicode frame/caret: {expected:?}"
            );
            thread::yield_now();
        }
    };
    wait_unicode_frame("> Message Arany", "2,5\n");
    assert!(
        server
            .run(&["send-keys", "-l", "-t", "arany-test:0.0", "a"])
            .status
            .success()
    );
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-o"])
            .status
            .success()
    );
    wait_unicode_frame("", "2,5\n");
    assert!(
        server
            .run(&["send-keys", "-l", "-t", "arany-test:0.0", "\u{301}b"])
            .status
            .success()
    );
    wait_unicode_frame("b", "3,5\n");
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Home", "Right", "DC"])
            .status
            .success()
    );
    wait_unicode_frame("> a\u{301}b", "3,5\n");
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "BSpace"])
            .status
            .success()
    );
    wait_unicode_frame("> b", "2,5\n");
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-c"])
            .status
            .success()
    );
    wait_unicode_frame("> Message Arany", "2,5\n");
    assert!(
        server
            .run(&["send-keys", "-l", "-t", "arany-test:0.0", "one e\u{301} 中"])
            .status
            .success()
    );
    wait_unicode_frame("> one e\u{301} 中", "10,5\n");
    for (key, text, caret) in [
        ("C-Left", "> one e\u{301} 中", "8,5\n"),
        ("C-Right", "> one e\u{301} 中", "10,5\n"),
        ("C-w", "> one e\u{301}", "8,5\n"),
        ("C-w", "> one", "6,5\n"),
        ("C-Left", "> one", "2,5\n"),
        ("C-Right", "> one", "6,5\n"),
        ("C-w", "> Message Arany", "2,5\n"),
    ] {
        assert!(
            server
                .run(&["send-keys", "-t", "arany-test:0.0", key])
                .status
                .success()
        );
        wait_unicode_frame(text, caret);
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-c"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let screen = capture_pane(&server).expect("live pane after first empty Ctrl+C");
        let text = String::from_utf8_lossy(&screen);
        if text.contains("Ctrl+Shift+C") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "copy guidance missing after first empty Ctrl+C: {}",
            text.escape_debug()
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&[
                "resize-window",
                "-x",
                "80",
                "-y",
                "24",
                "-t",
                "arany-test:0"
            ])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| live_tail_layout(&screen, 80)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "composer after Unicode correction"
        );
        thread::yield_now();
    }

    let before = read_trimmed(&before_path);
    let pane = server.run(&[
        "display-message",
        "-p",
        "-t",
        "arany-test:0.0",
        "#{pane_pid}",
    ]);
    assert!(pane.status.success(), "tmux pane PID available");
    let shell_pid = std::str::from_utf8(&pane.stdout)
        .expect("pane PID UTF-8")
        .trim()
        .parse::<u32>()
        .expect("pane PID");
    let product_pid = product_child_of_executable(shell_pid, executable);
    assert_ne!(tty_settings(product_pid), before, "raw mode inside tmux");

    let command = "stty rows 24 cols 80; before=$(stty -g); printf 'OUTER_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_TMUX\" -S \"$ARANY_TEST_SOCKET\" attach-session -t arany-test; exit_code=$?; after=$(stty -g); printf 'OUTER_AFTER:%s\\n' \"$after\"; exit \"$exit_code\"";
    let mut attached = Command::new("/usr/bin/script");
    attached
        .env_clear()
        .env("TERM", "xterm")
        .env("SHELL", "/bin/sh")
        .env("PATH", "/usr/bin:/bin")
        .env("ARANY_TEST_TMUX", "/usr/bin/tmux")
        .env("ARANY_TEST_SOCKET", &server.socket)
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut attached = ChildGuard::new(attached.spawn().expect("tmux client PTY"));
    let mut input = attached.child().stdin.take().expect("tmux client input");
    let mut output = attached.child().stdout.take().expect("tmux client output");
    let flags = fcntl_getfl(&output).expect("tmux output flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking tmux output");
    let mut transcript = Vec::new();
    let mut answered = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump_pty(&mut output, &mut input, &mut transcript, &mut answered);
        if transcript
            .windows(b"Ask Arany".len())
            .any(|part| part == b"Ask Arany")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "attached tmux composer not visible: {}",
            redacted_tail(&transcript)
        );
        thread::yield_now();
    }
    input.write_all(b"/quit\r").expect("tmux client /quit");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump_pty(&mut output, &mut input, &mut transcript, &mut answered);
        if let Ok(exit) = std::fs::read_to_string(&exit_path)
            && exit.ends_with('\n')
        {
            assert_eq!(exit, "0\n", "product exit success");
            break;
        }
        assert!(Instant::now() < deadline, "tmux pane did not exit");
        thread::yield_now();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump_pty(&mut output, &mut input, &mut transcript, &mut answered);
        if attached
            .child()
            .try_wait()
            .expect("tmux client status")
            .is_some()
        {
            break;
        }
        assert!(Instant::now() < deadline, "tmux client did not exit");
        thread::yield_now();
    }
    let result = wait_product(attached.take());
    pump_pty(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(result.status.success(), "tmux client exited cleanly");
    assert_eq!(result.stderr, b"");
    let transcript = String::from_utf8(transcript).expect("tmux PTY UTF-8");
    assert_eq!(
        transcript_field(&transcript, "OUTER_BEFORE:"),
        transcript_field(&transcript, "OUTER_AFTER:"),
        "outer tmux client restored terminal settings"
    );
    assert_eq!(read_trimmed(&after_path), before, "tmux termios restored");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("replay runtime");
    let sessions = runtime
        .block_on(list_sessions(
            StateRoot::open_existing(&state).expect("read-only State"),
            workspace.clone(),
        ))
        .expect("committed Sessions");
    assert_eq!(sessions.len(), 1, "one empty Session");
    let view = runtime
        .block_on(resume_session(
            StateRoot::open_existing(&state).expect("read-only State"),
            workspace,
            sessions[0].id,
        ))
        .expect("strict Session replay");
    assert!(view.runs.is_empty(), "no Provider Run from /quit");

    let _ = server.run(&["kill-server"]);
    let remaining = server.run(&["list-sessions"]);
    assert!(
        !remaining.status.success(),
        "test-owned tmux server stopped"
    );
}

#[test]
#[ignore = "native Linux tmux visual gate; requires /usr/bin/tmux"]
fn active_shelf_stays_above_retained_draft_in_tmux() {
    let executable = test_product_executable();
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let exit_path = temp.path().join("exit");
    let before_path = temp.path().join("before");
    let after_path = temp.path().join("after");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback Provider");
    write_profile(
        &state,
        listener.local_addr().expect("Provider address").port(),
    );
    let (request_sender, request_ready) = mpsc::channel();
    let (response_sender, response_ready) = mpsc::channel();
    let provider = thread::spawn(move || {
        listener.set_nonblocking(true).expect("bounded accept");
        for index in 0..5 {
            let deadline = Instant::now() + Duration::from_secs(if index == 4 { 40 } else { 10 });
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "synthetic request missing");
                        thread::yield_now();
                    }
                    Err(error) => panic!("synthetic accept: {error}"),
                }
            };
            let body = read_request(&mut stream);
            let text = match index {
                0 => serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"ok"}}),
                1 => serde_json::json!({"outcome":{"type":"delegate","children":["child"]}}),
                2 => serde_json::json!({"summary":"summary"}),
                3 => {
                    let wire: serde_json::Value =
                        serde_json::from_slice(&body).expect("run request JSON");
                    let input: serde_json::Value =
                        serde_json::from_str(wire["input"].as_str().expect("semantic input text"))
                            .expect("semantic input JSON");
                    assert_eq!(input["objective"], "first\nsecond");
                    request_sender.send(()).expect("active Provider gate");
                    response_ready
                        .recv_timeout(Duration::from_secs(20))
                        .expect("release Provider response");
                    serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"done"}})
                }
                _ => {
                    let wire: serde_json::Value =
                        serde_json::from_slice(&body).expect("summary request");
                    let input: serde_json::Value =
                        serde_json::from_str(wire["input"].as_str().expect("summary input"))
                            .expect("semantic summary input");
                    assert_eq!(wire["max_output_tokens"], 1024);
                    assert_eq!(input["items"].as_array().expect("accepted turns").len(), 1);
                    assert!(input.get("workspace_guidance").is_none());
                    assert!(input.get("includes").is_none());
                    for text in ["maint", "draf!t", "/setup"] {
                        assert!(!body.windows(text.len()).any(|part| part == text.as_bytes()));
                    }
                    request_sender.send(()).expect("maintenance request gate");
                    response_ready
                        .recv_timeout(Duration::from_secs(20))
                        .expect("release summary response");
                    serde_json::json!({"summary":"derived summary"})
                }
            };
            send_response(&mut stream, index, text);
        }
    });
    check_profile_with_binary(&workspace, &state, &executable);

    let wrapper = temp.path().join("tmux-pane.sh");
    std::fs::write(
        &wrapper,
        "#!/bin/sh\nstty -g > \"$ARANY_TEST_BEFORE\"\n\"$ARANY_TEST_EXE\" --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider custom:local --model model-1 --no-color\nresult=$?\nstty -g > \"$ARANY_TEST_AFTER\"\nprintf '%s\\n' \"$result\" > \"$ARANY_TEST_EXIT\"\nexit \"$result\"\n",
    )
    .expect("tmux pane wrapper");
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700))
        .expect("private executable wrapper");
    let server = TmuxServer {
        socket: temp.path().join("tmux.sock"),
    };
    let mut start =
        crate::process::account_isolated_command(temp.path(), "/usr/bin/tmux", &executable);
    start
        .env_clear()
        .env("TERM", "xterm")
        .env("SHELL", "/bin/sh")
        .env("PATH", "/usr/bin:/bin")
        .env("ARANY_TEST_EXE", &executable)
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_EXIT", &exit_path)
        .env("ARANY_TEST_BEFORE", &before_path)
        .env("ARANY_TEST_AFTER", &after_path)
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .arg("-S")
        .arg(&server.socket)
        .args([
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-x",
            "40",
            "-y",
            "24",
            "-s",
            "arany-test",
        ])
        .arg(&wrapper)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let started = wait_product(start.spawn().expect("tmux server start"));
    assert!(started.status.success(), "tmux pane started");
    let mut declined = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(screen) = capture_pane(&server) {
            decline_initial_consent(&server, &screen, &mut declined);
            if live_tail_layout(&screen, 40) {
                break;
            }
        }
        assert!(Instant::now() < deadline, "idle composer missing");
        thread::yield_now();
    }
    assert!(
        server
            .run(&[
                "send-keys",
                "-l",
                "-t",
                "arany-test:0.0",
                "\x1b[200~first\r\nsecond\x1b[201~"
            ])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| idle_multiline_layout(&screen)) {
            break;
        }
        assert!(Instant::now() < deadline, "inline multiline draft missing");
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Enter"])
            .status
            .success()
    );
    request_ready
        .recv_timeout(Duration::from_secs(10))
        .expect("active Provider request");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.contains("You:") && text.contains("│ first") && text.contains("│ second")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "committed user message missing during Run"
        );
        thread::yield_now();
    }
    let first_turn = capture_pane(&server).expect("first committed user frame");
    let first_turn = String::from_utf8(first_turn).expect("first frame UTF-8");
    assert!(
        first_turn
            .lines()
            .next()
            .expect("top screen row")
            .trim()
            .is_empty(),
        "the first speaker must remain visible below a one-row terminal overlay: {}",
        redacted_tail(first_turn.as_bytes())
    );
    assert!(
        first_turn.lines().skip(1).any(|row| row.contains("› You:")),
        "first user heading remains visible below the top edge"
    );
    assert!(
        server
            .run(&["send-keys", "-l", "-t", "arany-test:0.0", "/s"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.contains("> /setup") && text.contains("Tab/Enter")
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "active slash choices missing");
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Down", "Enter"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.contains("> /status") && text.contains("Enter command")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "active Enter did not fill the focused command"
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Enter"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.lines()
                .any(|row| row.contains("Working · request ") && row.contains("% of local limit"))
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "40-column request-size details missing"
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-l", "-t", "arany-test:0.0", "/provder"])
            .status
            .success()
    );
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Enter"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.contains("Unknown command") && text.contains("/provder")
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "active typo draft was lost");
        thread::yield_now();
    }
    assert!(
        server
            .run(&[
                "send-keys",
                "-t",
                "arany-test:0.0",
                "BSpace",
                "BSpace",
                "BSpace",
                "BSpace",
                "BSpace",
                "BSpace",
                "BSpace"
            ])
            .status
            .success()
    );
    assert!(
        server
            .run(&["send-keys", "-l", "-t", "arany-test:0.0", "prov"])
            .status
            .success()
    );
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Tab", "Enter"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            String::from_utf8_lossy(&screen)
                .lines()
                .any(|row| row.contains("Provider: custom:local") && row.contains("locked"))
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "corrected active command missing"
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-l", "-t", "arany-test:0.0", "/help"])
            .status
            .success()
    );
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Enter"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.contains("Commands · 16 local controls")
                && text.contains("> /help")
                && text.contains("Enter/Esc close")
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "active help panel missing");
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Escape"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.contains("Ask Arany") && text.contains("primary · working")
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "active composer after help");
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-l", "-t", "arany-test:0.0", "pending"])
            .status
            .success()
    );
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-o"])
            .status
            .success()
    );
    assert!(
        server
            .run(&["send-keys", "-l", "-t", "arany-test:0.0", "draft"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| active_layout_with_draft(&screen)) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "active shelf/draft layout missing"
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&[
                "send-keys",
                "-l",
                "-t",
                "arany-test:0.0",
                "\x1b[200~ correction\x1b[201~"
            ])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            active_layout_with_draft(&screen)
                && String::from_utf8_lossy(&screen).contains("  draft correction")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "active word-edit fixture missing"
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-w", "BSpace"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            active_layout_with_draft(&screen)
                && String::from_utf8_lossy(&screen)
                    .lines()
                    .any(|row| row == "  draft")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "active word deletion did not retain the draft"
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&[
                "resize-window",
                "-x",
                "80",
                "-y",
                "24",
                "-t",
                "arany-test:0"
            ])
            .status
            .success(),
        "widen active pane"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            let rows: Vec<_> = text.lines().collect();
            let status = rows
                .iter()
                .rposition(|row| row.contains("Ask Arany"))
                .and_then(|composer| rows.get(composer + 4));
            active_layout_with_draft(&screen)
                && status.is_some_and(|row| {
                    row.contains("working · custom:local/model-1")
                        && !row.contains("context ")
                        && !row.contains("% of local limit")
                })
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "quiet active status missing");
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-f"])
            .status
            .success(),
        "open wide history find"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server)
            .is_some_and(|screen| String::from_utf8_lossy(&screen).contains("Find:"))
        {
            break;
        }
        assert!(Instant::now() < deadline, "wide history find missing");
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-l", "-t", "arany-test:0.0", "first"])
            .status
            .success(),
        "type wide history query"
    );
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Enter"])
            .status
            .success(),
        "find wide history"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            String::from_utf8_lossy(&screen)
                .lines()
                .last()
                .is_some_and(|status| {
                    status.contains("History · Ctrl+L live")
                        && status.contains("working · custom:local/model-1")
                        && !status.contains("context ")
                        && !status.contains("% of local limit")
                })
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "wide history lost model or quiet status"
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-l"])
            .status
            .success(),
        "return to live history before resize"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            String::from_utf8_lossy(&screen)
                .lines()
                .last()
                .is_some_and(|status| status.starts_with("working · custom:local/model-1"))
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "wide live status did not return");
        thread::yield_now();
    }
    assert!(
        server
            .run(&["resize-window", "-x", "40", "-y", "8", "-t", "arany-test:0"])
            .status
            .success(),
        "short active history pane"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let pane = server.run(&[
            "display-message",
            "-p",
            "-t",
            "arany-test:0.0",
            "#{pane_height}",
        ]);
        if pane.status.success()
            && pane.stdout == b"8\n"
            && capture_pane(&server).is_some_and(|screen| {
                let text = String::from_utf8_lossy(&screen);
                text.contains("Ask Arany")
                    && text.contains("pending")
                    && text.contains("draft")
                    && text.contains("working")
            })
        {
            break;
        }
        assert!(Instant::now() < deadline, "short active frame missing");
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-f"])
            .status
            .success(),
        "open history find during the Run"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server)
            .is_some_and(|screen| String::from_utf8_lossy(&screen).contains("Find:"))
        {
            break;
        }
        assert!(Instant::now() < deadline, "active history find missing");
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "first", "Enter"])
            .status
            .success(),
        "find earlier content during the Run"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.contains("History")
                && text.contains("first")
                && text.contains("pending")
                && text.contains("draft")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "active history and draft missing: {}",
            redacted_tail(&capture_pane(&server).unwrap_or_default())
        );
        thread::yield_now();
    }
    response_sender.send(()).expect("release Provider response");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.contains("pending")
                && text.contains("draft")
                && text.contains("Draft retained:")
                && text.contains("Enter sends")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "draft not retained after Run: {}",
            redacted_tail(&capture_pane(&server).unwrap_or_default())
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "PageUp"])
            .status
            .success(),
        "continue reading after the Run"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.contains("New text")
                && text.contains("first")
                && text.contains("pending")
                && text.contains("draft")
                && !text.contains("done")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "new answer displaced the historical reading position"
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-l"])
            .status
            .success(),
        "return to live history"
    );
    assert!(
        server
            .run(&[
                "resize-window",
                "-x",
                "80",
                "-y",
                "24",
                "-t",
                "arany-test:0"
            ])
            .status
            .success(),
        "restore full history pane"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            let rows: Vec<_> = text.lines().collect();
            rows.len() == 24
                && rows[23].starts_with("ready")
                && rows[23].trim_end().ends_with("· ok")
                && !rows[23].contains("New Session")
                && text.contains("Arany:")
                && text.contains("pending")
                && text.contains("draft")
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "live answer and draft missing");
        thread::yield_now();
    }
    let pane = capture_pane(&server).expect("visible history pane");
    let pane = String::from_utf8(pane).expect("pane UTF-8");
    assert!(pane.lines().next().expect("top row").trim().is_empty());
    assert!(pane.lines().skip(1).any(|row| row.contains("› You:")));
    assert_eq!(
        pane.matches("You:").count(),
        1,
        "one committed user block in the owned viewport: {}",
        redacted_tail(pane.as_bytes())
    );
    assert!(
        pane.contains("Arany:"),
        "committed answer in the owned viewport"
    );
    let scrollback = capture_scrollback(&server).expect("native tmux scrollback");
    let scrollback = String::from_utf8(scrollback).expect("scrollback UTF-8");
    assert!(
        scrollback.contains("Answer:"),
        "confirmed stdout answer remains available after restoration"
    );
    assert!(
        server
            .run(&["resize-window", "-x", "40", "-y", "8", "-t", "arany-test:0"])
            .status
            .success(),
        "short history pane"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            let rows: Vec<_> = text.lines().collect();
            rows.len() == 8
                && text.contains("done")
                && rows[3].contains("Ask Arany")
                && rows[4].contains("pending")
                && rows[5].contains("draft")
                && rows[7].starts_with("ready")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "short idle history frame missing"
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "PageUp"])
            .status
            .success(),
        "history page up"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.contains("History") && text.contains("pending") && text.contains("draft")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "history did not retain the draft: {}",
            redacted_tail(&capture_pane(&server).unwrap_or_default())
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-f"])
            .status
            .success(),
        "open narrow history find"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server)
            .is_some_and(|screen| String::from_utf8_lossy(&screen).contains("Find:"))
        {
            break;
        }
        assert!(Instant::now() < deadline, "narrow history find missing");
        thread::yield_now();
    }
    assert!(
        server
            .run(&[
                "send-keys",
                "-l",
                "-t",
                "arany-test:0.0",
                "abcdefghijklmnopqrstuvwxyzXYZ"
            ])
            .status
            .success(),
        "type long history query"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.lines()
                .last()
                .is_some_and(|row| row.contains("…") && row.contains("XYZ Enter Esc"))
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "long narrow history query or actions missing"
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Escape"])
            .status
            .success(),
        "close long history find"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.contains("Ask Arany")
                && text.contains("pending")
                && text
                    .lines()
                    .last()
                    .is_some_and(|row| !row.starts_with("Find"))
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "history find did not close before reopening"
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-f", "first", "Enter"])
            .status
            .success(),
        "find committed objective"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.contains("You:") && text.contains("first") && text.contains("pending")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "history find did not preserve the draft"
        );
        thread::yield_now();
    }
    let before_selector = capture_pane(&server).expect("historical reading frame");
    let before_text = String::from_utf8_lossy(&before_selector);
    assert!(
        before_text.contains("first")
            && before_text.contains("pending")
            && before_text.contains("draft")
            && !before_text.contains("done"),
        "older reading frame and draft"
    );
    assert!(
        server
            .run(&["resize-window", "-x", "24", "-y", "8", "-t", "arany-test:0"])
            .status
            .success(),
        "narrow historical reading pane"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            screen != before_selector
                && text.contains("History")
                && text.contains("first")
                && text.contains("pending")
                && text.contains("draft")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "resize displaced historical reading or draft: {}",
            redacted_tail(&capture_pane(&server).unwrap_or_default())
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["resize-window", "-x", "40", "-y", "8", "-t", "arany-test:0"])
            .status
            .success(),
        "restore historical reading pane width"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| screen == before_selector) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "resize changed historical reading frame or draft: {}",
            redacted_tail(&capture_pane(&server).unwrap_or_default())
        );
        thread::yield_now();
    }
    let pane_pid = server.run(&[
        "display-message",
        "-p",
        "-t",
        "arany-test:0.0",
        "#{pane_pid}",
    ]);
    assert!(pane_pid.status.success(), "test-owned pane PID");
    let pane_pid = String::from_utf8(pane_pid.stdout)
        .expect("pane PID UTF-8")
        .trim()
        .parse::<u32>()
        .expect("pane PID");
    let product_pid = product_child_of_executable(pane_pid, &executable);
    let product_guard = ProductGuard::for_executable(product_pid, state.clone(), executable);
    assert!(product_guard.is_owned_and_running(), "private Arany child");
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-k"])
            .status
            .success(),
        "open quick actions while reading history"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.contains("Quick actions") && text.contains("Esc")
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "quick actions frame missing");
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Down"])
            .status
            .success(),
        "focus Agents quick action"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let focused_quick = loop {
        if let Some(screen) = capture_pane(&server)
            && String::from_utf8_lossy(&screen).contains("> Agents")
        {
            break screen;
        }
        assert!(Instant::now() < deadline, "Agents quick action not focused");
        thread::yield_now();
    };
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-z"])
            .status
            .success(),
        "suspend with quick actions open"
    );
    wait_stopped(product_pid);
    assert!(
        capture_pane(&server)
            .is_some_and(|screen| { !String::from_utf8_lossy(&screen).contains("Quick actions") }),
        "quick selector cleared before terminal suspension"
    );
    kill_process(product_pid, Signal::CONT).expect("resume private Arany child in quick actions");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| screen == focused_quick) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "quick-action focus changed after resume: {}",
            redacted_tail(&capture_pane(&server).unwrap_or_default())
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Escape"])
            .status
            .success(),
        "close quick actions without changing history"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| screen == before_selector) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "selector close displaced the reading frame or draft"
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-z"])
            .status
            .success(),
        "suspend while reading older history"
    );
    wait_stopped(product_pid);
    kill_process(product_pid, Signal::CONT).expect("resume private Arany child");
    let reading_rows = before_selector
        .split(|byte| *byte == b'\n')
        .take(7)
        .collect::<Vec<_>>();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            screen
                .split(|byte| *byte == b'\n')
                .take(7)
                .eq(reading_rows.iter().copied())
                && text.contains("Terminal resumed")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "resume displaced the older reading position or draft: {}",
            redacted_tail(&capture_pane(&server).unwrap_or_default())
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-l"])
            .status
            .success(),
        "return to live history"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if capture_pane(&server).is_some_and(|screen| {
            let text = String::from_utf8_lossy(&screen);
            text.contains("done") && text.contains("pending") && !text.contains("History")
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "history did not return to live tail"
        );
        thread::yield_now();
    }
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-c"])
            .status
            .success()
    );
    let wait_maintenance_frame = |label: &str, predicate: &dyn Fn(&[u8]) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(screen) = capture_pane(&server)
                && predicate(&screen)
            {
                break screen;
            }
            assert!(
                Instant::now() < deadline,
                "{label}: {}",
                redacted_tail(&capture_pane(&server).unwrap_or_default())
            );
            thread::yield_now();
        }
    };
    wait_maintenance_frame("explicitly cleared prior draft", &|screen| {
        let text = String::from_utf8_lossy(screen);
        text.contains("Ask Arany") && !text.contains("pending") && !text.contains("draft")
    });
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "/compact", "Enter"])
            .status
            .success()
    );
    request_ready
        .recv_timeout(Duration::from_secs(10))
        .expect("in-flight manual summary");
    wait_maintenance_frame("busy composer", &|screen| {
        let text = String::from_utf8_lossy(screen);
        text.contains("Compacting Session") && text.contains("Enter keeps draft")
    });
    assert!(
        server
            .run(&[
                "send-keys",
                "-l",
                "-t",
                "arany-test:0.0",
                "\x1b[200~maint\r\ndraft correction\x1b[201~"
            ])
            .status
            .success()
    );
    assert!(
        server
            .run(&[
                "send-keys",
                "-t",
                "arany-test:0.0",
                "C-w",
                "BSpace",
                "Left",
                "!"
            ])
            .status
            .success()
    );
    let maintenance_layout = |screen: &[u8]| {
        let text = String::from_utf8_lossy(screen);
        let rows: Vec<_> = text.lines().collect();
        rows.len() == 8
            && rows[3].contains("Ask Arany")
            && rows[4] == "> maint"
            && rows[5] == "  draf!t"
            && rows[6].contains("Enter keeps")
    };
    wait_maintenance_frame("visible edited maintenance draft", &maintenance_layout);
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Enter"])
            .status
            .success()
    );
    wait_maintenance_frame("Enter retains without queue", &|screen| {
        maintenance_layout(screen)
            && String::from_utf8_lossy(screen).contains("Draft retained; press Enter after")
    });
    assert!(
        server
            .run(&["resize-window", "-x", "16", "-y", "8", "-t", "arany-test:0"])
            .status
            .success()
    );
    wait_maintenance_frame("narrow monochrome maintenance", &|screen| {
        let text = String::from_utf8_lossy(screen);
        let rows: Vec<_> = text.lines().collect();
        rows.len() == 8
            && rows[4] == "> maint"
            && rows[5] == "  draf!t"
            && rows[6].contains("Enter keeps")
    });
    let cursor = server.run(&[
        "display-message",
        "-p",
        "-t",
        "arany-test:0.0",
        "#{cursor_x},#{cursor_y}",
    ]);
    assert!(cursor.status.success());
    assert_eq!(cursor.stdout, b"7,5\n", "mid-draft caret after resize");
    assert!(
        server
            .run(&["resize-window", "-x", "40", "-y", "8", "-t", "arany-test:0"])
            .status
            .success()
    );
    wait_maintenance_frame("restored maintenance width", &maintenance_layout);
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-f"])
            .status
            .success()
    );
    wait_maintenance_frame("find while compacting", &|screen| {
        maintenance_layout(screen) && String::from_utf8_lossy(screen).contains("Find:")
    });
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "first"])
            .status
            .success()
    );
    wait_maintenance_frame("visible maintenance query", &|screen| {
        String::from_utf8_lossy(screen).contains("Find: first")
    });
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "Enter"])
            .status
            .success()
    );
    let before_maintenance_suspend =
        wait_maintenance_frame("read history during maintenance", &|screen| {
            maintenance_layout(screen) && String::from_utf8_lossy(screen).contains("first")
        });
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-z"])
            .status
            .success()
    );
    wait_stopped(product_pid);
    assert_eq!(
        tty_settings(product_pid),
        read_trimmed(&before_path),
        "maintenance suspend restores original terminal settings"
    );
    kill_process(product_pid, Signal::CONT).expect("resume in-flight summary");
    wait_maintenance_frame(
        "maintenance draft and reading anchor after resume",
        &|screen| screen == before_maintenance_suspend,
    );
    let cursor = server.run(&[
        "display-message",
        "-p",
        "-t",
        "arany-test:0.0",
        "#{cursor_x},#{cursor_y}",
    ]);
    assert_eq!(cursor.stdout, b"7,5\n", "mid-draft caret after resume");
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "?"])
            .status
            .success()
    );
    wait_maintenance_frame("edit at same retained caret", &|screen| {
        String::from_utf8_lossy(screen).contains("draf!?t")
    });
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-l"])
            .status
            .success()
    );
    response_sender.send(()).expect("complete manual summary");
    provider.join().expect("synthetic Provider server");
    wait_maintenance_frame("idle draft after maintenance", &|screen| {
        let text = String::from_utf8_lossy(screen);
        text.contains("maint") && text.contains("draf!?t") && text.contains("Enter sends")
    });
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "+"])
            .status
            .success()
    );
    wait_maintenance_frame("same editable caret after completion", &|screen| {
        String::from_utf8_lossy(screen).contains("draf!?+t")
    });
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "C-c"])
            .status
            .success()
    );
    wait_maintenance_frame("clear new draft before explicit command", &|screen| {
        !String::from_utf8_lossy(screen).contains("draf!?+t")
    });
    assert!(
        server
            .run(&["send-keys", "-t", "arany-test:0.0", "/quit", "Enter"])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(exit) = std::fs::read_to_string(&exit_path)
            && exit.ends_with('\n')
        {
            assert_eq!(exit, "0\n", "product exit success");
            break;
        }
        assert!(Instant::now() < deadline, "tmux pane did not exit");
        thread::yield_now();
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("replay runtime");
    let sessions = runtime
        .block_on(list_sessions(
            StateRoot::open_existing(&state).expect("read-only State"),
            workspace.clone(),
        ))
        .expect("committed Sessions");
    assert_eq!(sessions.len(), 1);
    let view = runtime
        .block_on(resume_session(
            StateRoot::open_existing(&state).expect("read-only State"),
            workspace,
            sessions[0].id,
        ))
        .expect("strict Session replay");
    assert_eq!(view.runs.len(), 1, "draft was not submitted");
    assert_eq!(
        read_trimmed(&before_path),
        read_trimmed(&after_path),
        "maintenance success restores original terminal settings"
    );
    assert_eq!(view.runs[0].objective, "first\nsecond");
    assert_eq!(view.runs[0].status, arany::RunStatus::Finished);
    assert_eq!(view.compactions.len(), 1);
    assert!(
        matches!(&view.compactions[0].record.status, arany::CompactionStatus::Succeeded { summary, .. } if summary == "derived summary")
    );
    assert_eq!(view.compactions[0].record.covered_run_id, view.runs[0].id);
}

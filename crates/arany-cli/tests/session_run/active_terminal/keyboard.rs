use crate::loopback::ChildGuard;
use crate::session_picker::{PtyResponses, pump};
use arany_cli::terminal::{AttachedTerminal, Composer, TerminalInput, TerminalMode};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use std::{
    io::Write,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const HELPER: &str = "active_terminal::keyboard::keyboard_child";
const DEADLINE: Duration = Duration::from_secs(10);
const ACQUISITIONS: usize = 3;
const MAX_PROBES: usize = 32;

pub(super) fn answer_supported_queries(input: &mut impl Write, bytes: &[u8], answered: &mut usize) {
    let queries = count(bytes, b"\x1b[?u\x1b[c");
    assert!(queries <= MAX_PROBES, "bounded keyboard probes");
    while *answered < queries {
        input.write_all(Reply::Supported.bytes()).unwrap();
        input.flush().unwrap();
        *answered += 1;
    }
}

pub(super) fn assert_keyboard_restored(bytes: &[u8], visual: bool) {
    let pushes = count(bytes, b"\x1b[>1u");
    assert_eq!(
        pushes > 0,
        visual,
        "only visual owners enable keyboard disambiguation"
    );
    assert_eq!(count(bytes, b"\x1b[<u"), pushes, "keyboard modes restored");
}

struct RestoredSettings(nix::sys::termios::Termios);

impl Drop for RestoredSettings {
    fn drop(&mut self) {
        assert_eq!(crate::process::terminal_settings(std::io::stdin()), self.0);
        println!("KEYBOARD_RESTORED\r");
    }
}

#[test]
#[ignore = "supervised keyboard protocol helper"]
fn keyboard_child() {
    let mode = match std::env::var("ARANY_TEST_KEYBOARD_MODE").unwrap().as_str() {
        "classic" => TerminalMode::Classic,
        "fullscreen" => TerminalMode::Fullscreen,
        _ => panic!("test-owned keyboard mode"),
    };
    let _settings = RestoredSettings(crate::process::terminal_settings(std::io::stdin()));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut terminal =
            AttachedTerminal::acquire_with_mode(false, mode, Default::default()).unwrap();
        assert!(!terminal.is_linear());
        println!("KEYBOARD_ACQUIRED\r");
        let mut composer = Composer::default();
        loop {
            let input = terminal.next_chat_input().await.unwrap();
            if input == TerminalInput::Submit {
                if composer.continue_line() {
                    continue;
                }
                break;
            }
            if input == TerminalInput::Paste {
                terminal.paste_into(&mut composer).unwrap();
            } else {
                composer.apply(input);
            }
        }
        assert_eq!(composer.text(), "alpha beta\\\n\ngamma\ndelta\nomega");
        terminal.release_for_output().unwrap();
        println!("KEYBOARD_RELEASED\r");
        terminal.reacquire_after_output().unwrap();
        terminal
            .set_mode(mode.toggled(), composer.input_capacity())
            .unwrap();
        if std::env::var_os("ARANY_TEST_KEYBOARD_PANIC").is_some() {
            panic!("TEST_ONLY_KEYBOARD_UNWIND");
        }
        terminal.restore().unwrap();
        println!("KEYBOARD_DONE\r");
    });
}

#[derive(Clone, Copy, Debug)]
enum Reply {
    Supported,
    Unsupported,
    Malformed,
    Absent,
    Late,
}

impl Reply {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Supported => b"\x1b[?0u\x1b[?1;2c",
            Self::Unsupported | Self::Late => b"\x1b[?1;2c",
            Self::Malformed => b"\x1b[?1;2u\x1b[?1;2c",
            Self::Absent => b"",
        }
    }
}

#[test]
#[cfg_attr(not(arany_ci), ignore = "native keyboard negotiation and restoration")]
fn keyboard_negotiation_retains_input_and_restores_each_screen() {
    for mode in [TerminalMode::Classic, TerminalMode::Fullscreen] {
        for reply in [
            Reply::Supported,
            Reply::Unsupported,
            Reply::Malformed,
            Reply::Absent,
            Reply::Late,
        ] {
            keyboard_case(mode, reply, false);
        }
        keyboard_case(mode, Reply::Supported, true);
    }
}

fn keyboard_command(root: &std::path::Path, mode: TerminalMode, unwind: bool) -> Command {
    let script = "stty rows 24 cols 80; \"$ARANY_TEST_HELPER\" --ignored --exact \"$ARANY_TEST_FILTER\" --nocapture; result=$?; printf 'KEYBOARD_EXIT:%s\n' \"$result\"; exit \"$result\"";
    let mut command = Command::new("/usr/bin/script");
    #[cfg(target_os = "linux")]
    command.args(["-q", "-e", "-c", script, "/dev/null"]);
    #[cfg(target_os = "macos")]
    command.args(["-q", "/dev/null", "/bin/sh", "-c", script]);
    command
        .env_clear()
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .env("ARANY_TEST_HELPER", std::env::current_exe().unwrap())
        .env("ARANY_TEST_FILTER", HELPER)
        .env("ARANY_TEST_KEYBOARD_MODE", mode.label())
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if unwind {
        command.env("ARANY_TEST_KEYBOARD_PANIC", "1");
    }
    command
}

fn keyboard_case(mode: TerminalMode, reply: Reply, unwind: bool) {
    let root = tempfile::tempdir().unwrap();
    let mut process = ChildGuard::new(keyboard_command(root.path(), mode, unwind).spawn().unwrap());
    let mut input = process.child().stdin.take().unwrap();
    let mut output = process.child().stdout.take().unwrap();
    let flags = fcntl_getfl(&output).unwrap();
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).unwrap();
    let mut transcript = Vec::new();
    let mut responses = PtyResponses::manual_consent();
    let mut queries = 0;
    let mut typed = false;
    let deadline = Instant::now() + DEADLINE;
    loop {
        pump(&mut output, &mut input, &mut transcript, &mut responses);
        answer_queries(&mut input, &transcript, &mut queries, reply);
        if !typed && contains(&transcript, b"KEYBOARD_ACQUIRED") {
            if matches!(reply, Reply::Late) {
                input.write_all(b"\x1b[?1u").unwrap();
            }
            input
                .write_all(b"\x1bOMgamma\x1b[13;2udelta\\\romega\r")
                .unwrap();
            typed = true;
        }
        if let Some(status) = process.child().try_wait().unwrap() {
            assert!(status.success() || unwind, "{mode:?} {reply:?}");
            pump(&mut output, &mut input, &mut transcript, &mut responses);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "keyboard helper deadline: {mode:?} {reply:?}"
        );
        thread::yield_now();
    }
    verify_restoration(&transcript, reply, unwind);
    assert_eq!(
        queries, ACQUISITIONS,
        "fresh probe for release and view change"
    );
}

fn answer_queries(input: &mut impl Write, bytes: &[u8], answered: &mut usize, reply: Reply) {
    let queries = count(bytes, b"\x1b[?u\x1b[c");
    assert!(queries <= ACQUISITIONS, "bounded keyboard probes");
    while *answered < queries {
        if *answered == 0 {
            input
                .write_all(b"alpha\x1b[200~ beta\\\n\x1b[201~")
                .unwrap();
        }
        input.write_all(reply.bytes()).unwrap();
        input.flush().unwrap();
        *answered += 1;
    }
}

fn verify_restoration(bytes: &[u8], reply: Reply, unwind: bool) {
    assert!(
        !contains(bytes, b"\x1b="),
        "acquisition never enables application keypad mode"
    );
    assert!(
        contains(bytes, b"KEYBOARD_RESTORED"),
        "native settings restored exactly"
    );
    let pushes = count(bytes, b"\x1b[>1u");
    assert_eq!(
        pushes,
        if matches!(reply, Reply::Supported) {
            ACQUISITIONS
        } else {
            0
        }
    );
    assert_eq!(
        count(bytes, b"\x1b[<u"),
        pushes,
        "all keyboard pushes popped"
    );
    assert!(contains(bytes, b"KEYBOARD_RELEASED"));
    if unwind {
        assert!(contains(bytes, b"TEST_ONLY_KEYBOARD_UNWIND"));
        assert!(contains(bytes, b"1 failed"));
        assert!(contains(bytes, b"KEYBOARD_EXIT:101"));
    } else {
        assert!(contains(bytes, b"KEYBOARD_DONE"));
        assert!(contains(bytes, b"1 passed"));
        assert!(contains(bytes, b"KEYBOARD_EXIT:0"));
    }
}

fn count(bytes: &[u8], needle: &[u8]) -> usize {
    bytes
        .windows(needle.len())
        .filter(|part| *part == needle)
        .count()
}

fn contains(bytes: &[u8], needle: &[u8]) -> bool {
    count(bytes, needle) != 0
}

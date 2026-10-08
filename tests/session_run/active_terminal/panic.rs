use super::{redacted_tail, transcript_field, tty_settings};
use crate::loopback::{ChildGuard, wait_product};
use arany::AttachedTerminal;
use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    process::Pid,
};
use std::{
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn drain_nonblocking(reader: &mut impl Read, bytes: &mut Vec<u8>) {
    loop {
        let mut chunk = [0; 4096];
        match reader.read(&mut chunk) {
            Ok(0) => return,
            Ok(count) => {
                assert!(
                    bytes.len() + count <= 16 * 1024,
                    "bounded panic PTY transcript"
                );
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("panic PTY read failed: {error}"),
        }
    }
}

#[test]
#[ignore = "supervised Linux terminal-owner panic helper"]
fn panic_child() {
    let gate = std::env::var_os("ARANY_TEST_PANIC_GATE").expect("test-owned panic gate");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("terminal helper runtime");
    runtime.block_on(async {
        let terminal = AttachedTerminal::acquire().expect("inline terminal owner");
        assert!(!terminal.is_linear(), "raw-mode owner acquired");
        let mut gate = UnixStream::connect(gate).expect("panic gate connection");
        let mut release = [0];
        gate.read_exact(&mut release).expect("panic gate release");
        assert_eq!(release, [1]);
        panic!("TEST_ONLY_TERMINAL_OWNER_PANIC");
    });
}

#[test]
#[ignore = "native Linux terminal-owner panic unwind release gate"]
fn panic_unwind_restores_the_exact_terminal_settings() {
    let temp = tempfile::tempdir().expect("private test root");
    let gate_path = temp.path().join("panic-gate.sock");
    let gate = UnixListener::bind(&gate_path).expect("test-owned panic gate");
    gate.set_nonblocking(true).expect("bounded gate accept");
    let command = "/usr/bin/stty rows 24 cols 80; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(/usr/bin/stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_HELPER\" --ignored --exact active_terminal::panic::panic_child --nocapture; exit_code=$?; after=$(/usr/bin/stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\"";
    let mut attached = Command::new("/usr/bin/script");
    attached
        .env_clear()
        .env(
            "ARANY_TEST_HELPER",
            std::env::current_exe().expect("absolute test helper"),
        )
        .env("ARANY_TEST_PANIC_GATE", &gate_path)
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(temp.path())
        .args(["-q", "-e", "-c", command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut attached = ChildGuard::new(attached.spawn().expect("supervised panic PTY"));
    let _input = attached
        .child()
        .stdin
        .take()
        .expect("PTY input remains open");
    let mut output = attached.child().stdout.take().expect("PTY transcript");
    let flags = fcntl_getfl(&output).expect("transcript flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking transcript");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut transcript = Vec::new();
    let mut gate_stream = loop {
        drain_nonblocking(&mut output, &mut transcript);
        match gate.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "inline owner did not reach panic gate"
                );
                thread::yield_now();
            }
            Err(error) => panic!("panic gate accept failed: {error}"),
        }
    };
    let visible = String::from_utf8_lossy(&transcript);
    let shell_pid = transcript_field(&visible, "SHELL_PID:")
        .parse::<i32>()
        .expect("shell PID");
    let shell_pid = Pid::from_raw(shell_pid).expect("positive shell PID");
    let before = transcript_field(&visible, "TTY_BEFORE:").to_owned();
    assert_ne!(
        tty_settings(shell_pid),
        before,
        "raw mode active at panic gate"
    );
    gate_stream
        .write_all(&[1])
        .expect("trigger test-only panic");

    loop {
        drain_nonblocking(&mut output, &mut transcript);
        if attached
            .child()
            .try_wait()
            .expect("panic helper status")
            .is_some()
        {
            break;
        }
        assert!(Instant::now() < deadline, "panic helper did not exit");
        thread::yield_now();
    }
    let result = wait_product(attached.take());
    assert_eq!(
        result.status.code(),
        Some(101),
        "expected test-harness failure"
    );
    assert_eq!(result.stderr, b"");
    drain_nonblocking(&mut output, &mut transcript);
    assert!(transcript.len() <= 16 * 1024);
    let transcript = String::from_utf8(transcript).expect("PTY transcript UTF-8");
    let after = transcript_field(&transcript, "TTY_AFTER:");
    assert_eq!(
        before,
        after,
        "terminal settings restored after unwind: {}",
        redacted_tail(transcript.as_bytes())
    );
    assert!(
        transcript.contains("TEST_ONLY_TERMINAL_OWNER_PANIC"),
        "deliberate test panic observed"
    );
}

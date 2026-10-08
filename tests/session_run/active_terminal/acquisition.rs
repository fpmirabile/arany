use super::{kill_process, product_child_of, redacted_tail, transcript_field, tty_settings};
use crate::loopback::{ChildGuard, wait_product};
use crate::session_picker::{pump, tail};
use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    io::{FdFlags, fcntl_getfd},
    process::Signal,
    pty::{OpenptFlags, grantpt, ioctl_tiocgptpeer, openpt, ptsname, unlockpt},
    termios::{Winsize, tcsetwinsize},
};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Stdio,
    thread,
    time::{Duration, Instant},
};

pub(crate) struct ProductGuard {
    pid: rustix::process::Pid,
    state: PathBuf,
    executable: PathBuf,
}

impl ProductGuard {
    pub(crate) fn new(pid: rustix::process::Pid, state: PathBuf) -> Self {
        Self::for_executable(pid, state, PathBuf::from(env!("CARGO_BIN_EXE_arany")))
    }

    pub(crate) fn for_executable(
        pid: rustix::process::Pid,
        state: PathBuf,
        executable: PathBuf,
    ) -> Self {
        assert!(executable.is_absolute(), "absolute guarded executable");
        Self {
            pid,
            state,
            executable,
        }
    }

    pub(crate) fn is_owned_and_running(&self) -> bool {
        let pid = self.pid.as_raw_pid();
        if !std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .as_deref()
            .is_some_and(|path| is_expected_binary(path, &self.executable))
        {
            return false;
        }
        std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|args| {
            args.split(|byte| *byte == 0)
                .any(|arg| arg == self.state.as_os_str().as_bytes())
        })
    }
}

fn is_expected_binary(path: &Path, expected: &Path) -> bool {
    if path == expected {
        return true;
    }
    let mut deleted = expected.as_os_str().to_os_string();
    deleted.push(" (deleted)");
    path == Path::new(&deleted)
}

#[test]
fn product_guard_recognizes_only_its_test_binary_even_after_replacement() {
    let expected = Path::new(env!("CARGO_BIN_EXE_arany"));
    let mut deleted = expected.as_os_str().to_os_string();
    deleted.push(" (deleted)");
    assert!(is_expected_binary(expected, expected));
    assert!(is_expected_binary(Path::new(&deleted), expected));
    assert!(!is_expected_binary(Path::new("/usr/bin/arany"), expected));
    assert!(!is_expected_binary(
        Path::new("/usr/bin/arany (deleted)"),
        expected
    ));
    let candidate = Path::new("/tmp/private-bundle/arany");
    assert!(is_expected_binary(candidate, candidate));
    assert!(is_expected_binary(
        Path::new("/tmp/private-bundle/arany (deleted)"),
        candidate
    ));
    assert!(!is_expected_binary(expected, candidate));
}

impl Drop for ProductGuard {
    fn drop(&mut self) {
        if self.is_owned_and_running() {
            let _ = kill_process(self.pid, Signal::KILL);
        }
    }
}

fn drain_nonblocking(reader: &mut impl Read, bytes: &mut Vec<u8>) {
    loop {
        let mut chunk = [0; 4096];
        match reader.read(&mut chunk) {
            Ok(0) => return,
            Ok(count) => {
                assert!(bytes.len() + count <= 16 * 1024, "bounded PTY transcript");
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => panic!("PTY read failed: {error}"),
        }
    }
}

#[test]
fn cursor_query_timeout_during_acquisition_restores_raw_mode_without_state() {
    for (case, signal) in [
        ("no signal", None),
        ("SIGINT", Some(Signal::INT)),
        ("SIGTERM", Some(Signal::TERM)),
        ("SIGHUP", Some(Signal::HUP)),
    ] {
        acquisition_case(case, signal);
    }
}

fn acquisition_case(case: &str, signal: Option<Signal>) {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let command = "stty rows 24 cols 80; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; \"$ARANY_TEST_EXE\" --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\"";
    let mut attached =
        crate::process::account_isolated_script(state.parent().expect("fixture root"));
    attached
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut attached = ChildGuard::new(attached.spawn().expect("attached PTY process"));
    let _input = attached
        .child()
        .stdin
        .take()
        .expect("PTY input remains open");
    let mut output = attached.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout pipe flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking stdout pipe");
    let mut transcript = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        drain_nonblocking(&mut output, &mut transcript);
        if transcript.windows(4).any(|part| part == b"\x1b[6n") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cursor query was not issued: {case}"
        );
        thread::yield_now();
    }
    let visible = String::from_utf8_lossy(&transcript);
    let shell_pid = transcript_field(&visible, "SHELL_PID:")
        .parse::<u32>()
        .expect("shell PID");
    let before = transcript_field(&visible, "TTY_BEFORE:").to_owned();
    let product_pid = product_child_of(shell_pid);
    assert_ne!(
        tty_settings(product_pid),
        before,
        "raw mode active before cursor query timeout: {case}"
    );
    if let Some(signal) = signal {
        kill_process(product_pid, signal).expect("signal during terminal acquisition");
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        drain_nonblocking(&mut output, &mut transcript);
        if attached
            .child()
            .try_wait()
            .expect("attached status")
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cursor query did not time out: {case}"
        );
        thread::yield_now();
    }
    let result = wait_product(attached.take());
    drain_nonblocking(&mut output, &mut transcript);
    assert!(
        !result.status.success(),
        "failed terminal acquisition exits: {case}"
    );
    assert_eq!(result.stderr, b"");
    let transcript = String::from_utf8(transcript).expect("PTY transcript UTF-8");
    let after = transcript
        .split_once("TTY_AFTER:")
        .expect("restored terminal marker")
        .1
        .lines()
        .next()
        .expect("restored terminal settings")
        .trim_end_matches('\r');
    assert_eq!(
        transcript_field(&transcript, "TTY_BEFORE:"),
        after,
        "raw mode restored after failed acquisition ({case}): {}",
        redacted_tail(transcript.as_bytes())
    );
    assert!(
        transcript.contains("terminal rendering failed"),
        "typed acquisition failure: {case}"
    );
    assert!(!transcript.contains("Status:"), "no Run receipt: {case}");
    assert!(!transcript.contains("Answer:"), "no Run answer: {case}");
    assert!(
        !state.exists(),
        "no Session state before terminal admission: {case}"
    );
}

#[test]
fn broken_stderr_after_cursor_query_restores_raw_mode_without_state() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");

    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)
        .expect("stderr PTY master");
    grantpt(&master).expect("grant stderr PTY");
    unlockpt(&master).expect("unlock stderr PTY");
    let slave = ioctl_tiocgptpeer(
        &master,
        OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC,
    )
    .expect("stderr PTY slave");
    assert!(
        fcntl_getfd(&master)
            .expect("master descriptor flags")
            .contains(FdFlags::CLOEXEC),
        "stderr master must not leak into the child"
    );
    assert!(
        fcntl_getfd(&slave)
            .expect("slave descriptor flags")
            .contains(FdFlags::CLOEXEC),
        "parent stderr slave must not leak into the child"
    );
    tcsetwinsize(
        &slave,
        Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .expect("stderr PTY size");
    let slave_path = ptsname(&master, Vec::new())
        .expect("stderr PTY path")
        .into_string()
        .expect("UTF-8 PTY path");
    let mut stderr_master = File::from(master);
    let flags = fcntl_getfl(&stderr_master).expect("stderr master flags");
    fcntl_setfl(&stderr_master, flags | OFlags::NONBLOCK).expect("nonblocking stderr master");

    let command = "stty rows 24 cols 80; printf 'SHELL_PID:%s\\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\\n' \"$before\"; exec 2>\"$ARANY_TEST_STDERR_PTY\"; \"$ARANY_TEST_EXE\" --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\\n' \"$after\"; exit \"$exit_code\"";
    let mut attached =
        crate::process::account_isolated_script(state.parent().expect("fixture root"));
    attached
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_STDERR_PTY", &slave_path)
        .env("SHELL", "/bin/sh")
        .env("TERM", "xterm")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut attached = ChildGuard::new(attached.spawn().expect("attached PTY process"));
    let mut input = attached.child().stdin.take().expect("PTY input");
    let mut output = attached.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout pipe flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking stdout pipe");
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        drain_nonblocking(&mut output, &mut stdout_bytes);
        drain_nonblocking(&mut stderr_master, &mut stderr_bytes);
        if stdout_bytes
            .windows(4)
            .chain(stderr_bytes.windows(4))
            .any(|part| part == b"\x1b[6n")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cursor query not issued; stdout: {}; stderr: {}",
            redacted_tail(&stdout_bytes),
            redacted_tail(&stderr_bytes)
        );
        thread::yield_now();
    }
    let visible = String::from_utf8_lossy(&stdout_bytes);
    let shell_pid = transcript_field(&visible, "SHELL_PID:")
        .parse::<u32>()
        .expect("shell PID");
    let before = transcript_field(&visible, "TTY_BEFORE:").to_owned();
    let product_pid = product_child_of(shell_pid);
    assert_ne!(tty_settings(product_pid), before, "raw mode active");
    assert_eq!(
        std::fs::read_link(format!("/proc/{}/fd/2", product_pid.as_raw_pid()))
            .expect("product stderr descriptor"),
        std::path::Path::new(&slave_path),
        "product stderr must be the fault-injected PTY"
    );
    let mut fault_probe = File::from(slave);
    let flags = fcntl_getfl(&fault_probe).expect("fault probe flags");
    fcntl_setfl(&fault_probe, flags | OFlags::NONBLOCK).expect("nonblocking fault probe");
    drop(stderr_master);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut probe_successes = 0;
    loop {
        match fault_probe.write(b".") {
            Ok(count) => {
                probe_successes += count;
                assert!(probe_successes <= 1024, "stderr fault did not take effect");
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => {
                assert_eq!(
                    error.raw_os_error(),
                    Some(rustix::io::Errno::IO.raw_os_error()),
                    "stderr PTY disconnect error"
                );
                break;
            }
        }
        assert!(Instant::now() < deadline, "stderr fault probe timed out");
        thread::yield_now();
    }
    drop(fault_probe);
    input.write_all(b"\x1b[24;1R").expect("cursor response");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        drain_nonblocking(&mut output, &mut stdout_bytes);
        if attached
            .child()
            .try_wait()
            .expect("attached status")
            .is_some()
        {
            break;
        }
        assert!(Instant::now() < deadline, "acquisition fault did not exit");
        thread::yield_now();
    }
    let result = wait_product(attached.take());
    drain_nonblocking(&mut output, &mut stdout_bytes);
    assert!(!result.status.success(), "failed acquisition exits");
    assert_eq!(result.stderr, b"");
    let transcript = String::from_utf8(stdout_bytes).expect("PTY transcript UTF-8");
    let after = transcript
        .split_once("TTY_AFTER:")
        .expect("restored terminal marker")
        .1
        .lines()
        .next()
        .expect("restored terminal settings")
        .trim_end_matches('\r');
    assert_eq!(
        transcript_field(&transcript, "TTY_BEFORE:"),
        after,
        "raw mode restored after stderr fault: {}",
        redacted_tail(transcript.as_bytes())
    );
    assert!(!transcript.contains("Status:"), "no Run receipt");
    assert!(!transcript.contains("Answer:"), "no Run answer");
    assert!(
        !state.exists(),
        "no Session state before terminal admission; probe accepted {probe_successes} bytes before disconnect; transcript: {}",
        redacted_tail(transcript.as_bytes())
    );
}

#[test]
fn lost_pty_exits_idle_attached_product_without_spinning() {
    lost_pty_case(false);
}

#[test]
fn lost_pty_exits_screen_reader_setup_without_spinning() {
    lost_pty_case(true);
}

fn lost_pty_case(screen_reader: bool) {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let command = if screen_reader {
        "stty rows 24 cols 80; printf 'PRODUCT_PID:%s\n' \"$$\"; exec \"$ARANY_TEST_EXE\" --screen-reader --setup --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\""
    } else {
        "stty rows 24 cols 80; printf 'PRODUCT_PID:%s\n' \"$$\"; exec \"$ARANY_TEST_EXE\" --setup --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\""
    };
    let mut attached =
        crate::process::account_isolated_script(state.parent().expect("fixture root"));
    attached
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
        .env("SHELL", "/bin/sh")
        .env("TERM", if screen_reader { "dumb" } else { "xterm" })
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", command, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut attached = ChildGuard::new(attached.spawn().expect("attached PTY process"));
    let mut input = attached.child().stdin.take().expect("PTY input");
    let mut output = attached.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout pipe flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
    let mut transcript = Vec::new();
    let mut answered = crate::session_picker::PtyResponses::default();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript
        .windows(b"PRODUCT_PID:".len())
        .position(|part| part == b"PRODUCT_PID:")
        .is_some_and(|start| transcript[start..].contains(&b'\n'))
    {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "product PID marker missing: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    let product_pid = transcript_field(&String::from_utf8_lossy(&transcript), "PRODUCT_PID:")
        .parse::<i32>()
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .expect("product PID");
    let product = ProductGuard::new(product_pid, state);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !transcript
        .windows(b"Choose access method".len())
        .any(|part| part == b"Choose access method")
    {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(
            Instant::now() < deadline,
            "setup prompt missing: {}",
            tail(&transcript)
        );
        thread::yield_now();
    }
    assert!(product.is_owned_and_running(), "test owns attached product");
    attached.child().kill().expect("close PTY master");
    attached.take().wait().expect("reap PTY owner");
    drop(input);
    drop(output);
    let deadline = Instant::now() + Duration::from_secs(5);
    while product.is_owned_and_running() {
        assert!(Instant::now() < deadline, "product survived PTY loss");
        thread::yield_now();
    }
}

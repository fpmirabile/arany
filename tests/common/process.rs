use std::{
    io::{self, Read},
    os::fd::AsFd,
    process::{Child, Command, Output, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

/// Serializes heavy native gates that share one test binary.
#[allow(dead_code)]
pub static NATIVE_GATE: Mutex<()> = Mutex::new(());

pub trait BoundedOutput {
    fn bounded_output(&mut self) -> io::Result<Output>;
    fn bounded_output_for(&mut self, duration: Duration, cap: usize) -> io::Result<Output>;
    fn bounded_output_with_input(&mut self) -> io::Result<Output>;
}

impl BoundedOutput for Command {
    fn bounded_output(&mut self) -> io::Result<Output> {
        self.bounded_output_for(Duration::from_secs(30), 32 * 1024 * 1024)
    }

    fn bounded_output_for(&mut self, duration: Duration, cap: usize) -> io::Result<Output> {
        self.stdin(Stdio::null());
        self.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = OwnedChild(self.spawn()?);
        Ok(capture(&mut child.0, duration, cap))
    }

    fn bounded_output_with_input(&mut self) -> io::Result<Output> {
        self.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = OwnedChild(self.spawn()?);
        Ok(capture(
            &mut child.0,
            Duration::from_secs(30),
            32 * 1024 * 1024,
        ))
    }
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn capture(child: &mut Child, duration: Duration, cap: usize) -> Output {
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    capture_streams(
        child,
        stdout_pipe,
        stderr_pipe,
        duration,
        cap,
        "stderr",
        |_| {},
    )
}

/// Asserts that a self-spawned `--exact` test child completed exactly one passing test.
pub fn assert_single_test_completed(stdout: &[u8], context: &str) {
    assert!(
        String::from_utf8_lossy(stdout)
            .lines()
            .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;")),
        "{context}: child did not complete exactly one test"
    );
}

#[cfg(unix)]
pub fn terminal_settings(terminal: impl AsFd) -> nix::sys::termios::Termios {
    let settings = nix::sys::termios::tcgetattr(terminal).expect("native terminal settings");
    #[cfg(target_os = "macos")]
    {
        let mut settings: nix::libc::termios = settings.into();
        // Darwin sets this input-state bit when canonical input is restored.
        settings.c_lflag &= !nix::sys::termios::LocalFlags::PENDIN.bits();
        settings.into()
    }
    #[cfg(not(target_os = "macos"))]
    settings
}

#[cfg(unix)]
pub fn capture_terminal(
    child: &mut Child,
    terminal: std::fs::File,
    duration: Duration,
    cap: usize,
    observe: impl FnMut(&[u8]),
) -> Output {
    let stdout_pipe = child.stdout.take();
    capture_streams(
        child,
        stdout_pipe,
        Some(terminal),
        duration,
        cap,
        "terminal",
        observe,
    )
}

fn capture_streams(
    child: &mut Child,
    mut stdout_pipe: Option<impl Read + AsFd>,
    mut stderr_pipe: Option<impl Read + AsFd>,
    duration: Duration,
    cap: usize,
    stderr_channel: &'static str,
    mut observe: impl FnMut(&[u8]),
) -> Output {
    let deadline = Instant::now() + duration;
    for pipe in stdout_pipe
        .as_ref()
        .map(AsFd::as_fd)
        .into_iter()
        .chain(stderr_pipe.as_ref().map(AsFd::as_fd))
    {
        let flags = rustix::fs::fcntl_getfl(pipe).expect("subprocess pipe flags");
        rustix::fs::fcntl_setfl(pipe, flags | rustix::fs::OFlags::NONBLOCK)
            .expect("nonblocking subprocess pipe");
    }
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        assert!(
            Instant::now() < deadline,
            "subprocess exit/output EOF deadline exceeded"
        );
        drain(&mut stdout_pipe, &mut stdout, cap, "stdout");
        drain(&mut stderr_pipe, &mut stderr, cap, stderr_channel);
        observe(&stderr);
        if let Some(status) = child.try_wait().expect("subprocess status")
            && stdout_pipe.is_none()
            && stderr_pipe.is_none()
        {
            return Output {
                status,
                stdout,
                stderr,
            };
        }
        std::thread::yield_now();
    }
}

fn drain(pipe: &mut Option<impl Read>, bytes: &mut Vec<u8>, cap: usize, channel: &str) {
    let Some(reader) = pipe.as_mut() else {
        return;
    };
    for _ in 0..16 {
        let mut chunk = [0; 4096];
        match reader.read(&mut chunk) {
            Ok(0) => {
                *pipe = None;
                return;
            }
            Ok(count) => {
                assert!(
                    bytes.len() + count <= cap,
                    "subprocess {channel} capture limit exceeded"
                );
                bytes.extend_from_slice(&chunk[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            #[cfg(target_os = "linux")]
            Err(error)
                if error.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error())
                    && channel == "terminal" =>
            {
                *pipe = None;
                return;
            }
            Err(error) => panic!("subprocess {channel} capture failed: {error}"),
        }
    }
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
pub fn decline_workspace_consent(
    input: &mut impl std::io::Write,
    transcript: &[u8],
    declined: &mut bool,
) {
    if *declined {
        return;
    }
    let prompt = b"Type trust or read only; empty Enter selects read only. Ctrl+C exits:";
    if let Some(start) = transcript
        .windows(prompt.len())
        .rposition(|part| part == prompt)
    {
        let after = &transcript[start + prompt.len()..];
        if (after.starts_with(b"\r\n") || after.starts_with(b"\n"))
            && !after.windows(b"Input:".len()).any(|part| part == b"Input:")
        {
            input
                .write_all(b"read only\r")
                .expect("explicit read-only fixture decision");
            *declined = true;
        }
        return;
    }
    if !transcript.windows(2).any(|part| part == b"\x1b[") {
        return;
    }
    let mut visible = Vec::new();
    let mut bytes = transcript.iter().copied();
    while let Some(byte) = bytes.next() {
        if byte == 0x1b && bytes.next() == Some(b'[') {
            for byte in bytes.by_ref() {
                if (b'@'..=b'~').contains(&byte) {
                    break;
                }
            }
        } else if !byte.is_ascii_whitespace() {
            visible.push(byte);
        }
    }
    if visible
        .windows(b"Doyoutrust".len())
        .any(|part| part == b"Doyoutrust")
        && [b"Ctrl+Cexit".as_slice(), b"^Cexit".as_slice()]
            .iter()
            .any(|needle| visible.windows(needle.len()).any(|part| part == *needle))
    {
        input
            .write_all(b"\x1b")
            .expect("dismiss initial inline consent without authority");
        *declined = true;
    }
}

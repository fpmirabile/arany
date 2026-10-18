use std::{
    io::{self, Read, Write},
    os::fd::AsFd,
    process::{Child, Command, Output, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};

/// Serializes heavy native gates that share one test binary.
pub static NATIVE_GATE: Mutex<()> = Mutex::new(());

pub const READ_ONLY_CONSENT_PROMPT: &str = concat!(
    "Type trust [profile numbers], trust none, or read only; bare trust ",
    "preserves selected profiles. Ctrl+C exits:"
);
pub const CONFIG_CONSENT_PROMPT: &str = concat!(
    "Type trust [profile numbers], trust none, or config; bare trust ",
    "preserves selected profiles. Ctrl+C exits:"
);

pub fn annotate_native_fixture(message: &str) {
    const PREFIX: &str = "::error title=Arany native fixture::";
    const MAX_LINE_BYTES: usize = 2048;
    let mut annotation = String::from(PREFIX);
    for character in message.chars() {
        let escaped = match character {
            '%' => "%25",
            '\r' => "%0D",
            '\n' => "%0A",
            character if character.is_control() => "?",
            character => {
                if annotation.len() + character.len_utf8() + 1 > MAX_LINE_BYTES {
                    break;
                }
                annotation.push(character);
                continue;
            }
        };
        if annotation.len() + escaped.len() + 1 > MAX_LINE_BYTES {
            break;
        }
        annotation.push_str(escaped);
    }
    annotation.push('\n');
    let _ = io::stderr().lock().write_all(annotation.as_bytes());
}

pub fn relay_native_fixture_annotation(stdout: &[u8], stderr: &[u8]) -> bool {
    const PREFIX: &[u8] = b"::error title=Arany native fixture::";
    const MAX_LINE_BYTES: usize = 2048;
    for captured in [stdout, stderr] {
        for line in captured.split(|byte| *byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let Some(message) = line.strip_prefix(PREFIX) else {
                continue;
            };
            if line.len() > MAX_LINE_BYTES
                || !message.is_ascii()
                || !(message.starts_with(b"fixture=shipped_exec;")
                    || message.starts_with(b"fixture=direct_cancellation;"))
                || !message.iter().all(|byte| {
                    byte.is_ascii_alphanumeric()
                        || matches!(
                            byte,
                            b' ' | b'_'
                                | b'='
                                | b';'
                                | b':'
                                | b','
                                | b'.'
                                | b'?'
                                | b'['
                                | b']'
                                | b'"'
                                | b'('
                                | b')'
                                | b'-'
                        )
                })
            {
                continue;
            }
            let mut output = io::stderr().lock();
            let _ = output.write_all(line);
            let _ = output.write_all(b"\n");
            return true;
        }
    }
    false
}

pub fn relay_native_panic_location(stderr: &[u8], expected_case: &str) -> bool {
    const PREFIX: &str = "ARANY_NATIVE_PANIC; case=";
    const MAX_LINE_BYTES: usize = 256;
    if !matches!(
        expected_case,
        "shipped_exec" | "goal_token_cap" | "workspace_lease" | "native_account_test"
    ) {
        return false;
    }
    for line in stderr.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.len() > MAX_LINE_BYTES {
            continue;
        }
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        let Some(fields) = line.strip_prefix(PREFIX) else {
            continue;
        };
        let Some((case, fields)) = fields.split_once("; file=") else {
            continue;
        };
        let Some((file, fields)) = fields.split_once("; line=") else {
            continue;
        };
        let Some((line_number, column)) = fields.split_once("; column=") else {
            continue;
        };
        if case != expected_case
            || file.is_empty()
            || file.len() > 64
            || !file
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            continue;
        }
        let (Ok(line_number), Ok(column)) = (line_number.parse::<u32>(), column.parse::<u32>())
        else {
            continue;
        };
        if line_number == 0 {
            continue;
        }
        annotate_native_fixture(&format!(
            "fixture=isolated_child_panic; case={case}; file={file}; line={line_number}; column={column}"
        ));
        return true;
    }
    false
}

pub trait BoundedOutput {
    fn bounded_output(&mut self) -> io::Result<Output>;
    fn bounded_output_for(&mut self, duration: Duration, cap: usize) -> io::Result<Output>;
    fn bounded_output_with_input(&mut self) -> io::Result<Output>;
}

// `diskimage` can hold a just-used fixture image briefly: retry gracefully, force only last, all within `deadline`.
// Each attempt re-resolves the device from the image path, so a reused disk number is never detached.
#[cfg(target_os = "macos")]
pub fn detach_disk_image(image: &std::path::Path, deadline: Duration) -> Result<(), Output> {
    const FORCE_RESERVE: Duration = Duration::from_secs(8);
    let end = Instant::now() + deadline;
    let wanted = std::fs::canonicalize(image).unwrap_or_else(|_| image.to_owned());
    let hdiutil = |args: &[&std::ffi::OsStr], cap: Duration, until: Instant| {
        let budget = cap.min(until.saturating_duration_since(Instant::now()));
        if budget.is_zero() {
            return None;
        }
        // A command past its bound panics inside the bounded runner; that attempt then just fails.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Command::new("/usr/bin/hdiutil")
                .args(args)
                .env_clear()
                .bounded_output_for(budget, 64 * 1024)
        }))
        .ok()
        .and_then(Result::ok)
    };
    let device = |until: Instant| {
        hdiutil(&["info".as_ref()], Duration::from_secs(3), until)
            .filter(|output| output.status.success())
            .map(|output| attached_device(&String::from_utf8_lossy(&output.stdout), &wanted, image))
    };
    let mut pause = Duration::from_millis(250);
    let graceful_end = end.checked_sub(FORCE_RESERVE).unwrap_or(end);
    while Instant::now() < graceful_end {
        match device(graceful_end) {
            Some(None) => return Ok(()),
            Some(Some(attached)) => {
                let detach = ["detach".as_ref(), "-quiet".as_ref(), attached.as_os_str()];
                if hdiutil(&detach, Duration::from_secs(10), graceful_end)
                    .is_some_and(|output| output.status.success())
                {
                    continue;
                }
            }
            None => {}
        }
        std::thread::sleep(pause.min(graceful_end.saturating_duration_since(Instant::now())));
        pause = (pause * 2).min(Duration::from_secs(4));
    }
    let failed = |stderr: &str| Output {
        status: std::os::unix::process::ExitStatusExt::from_raw(1 << 8),
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    };
    let Some(attached) = device(end).ok_or_else(|| failed("hdiutil info unavailable"))? else {
        return Ok(());
    };
    let detach = [
        "detach".as_ref(),
        "-force".as_ref(),
        "-quiet".as_ref(),
        attached.as_os_str(),
    ];
    match hdiutil(&detach, Duration::from_secs(20), end) {
        Some(output) if output.status.success() => Ok(()),
        Some(output) => Err(output),
        None => Err(failed("forced detach exceeded the deadline")),
    }
}

#[cfg(any(target_os = "macos", test))]
fn attached_device(
    info: &str,
    wanted: &std::path::Path,
    given: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let mut ours = false;
    for line in info.lines() {
        if line.starts_with("====") {
            ours = false;
        } else if let Some(path) = line
            .strip_prefix("image-path")
            .and_then(|rest| rest.trim_start().strip_prefix(':'))
        {
            let path = std::path::Path::new(path.trim());
            ours = path == wanted || path == given;
        } else if ours
            && let Some(device) = line
                .split_whitespace()
                .next()
                .filter(|word| word.starts_with("/dev/disk"))
        {
            return Some(device.into());
        }
    }
    None
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

/// Runs `command` with file-backed stdio, so descendants holding inherited handles cannot
/// delay EOF; enforces a 10-second deadline and 64 KiB per stream.
pub fn file_output(command: &mut Command, input: Option<&[u8]>) -> Output {
    use std::io::{Seek, SeekFrom, Write};

    const STREAM_CAP: u64 = 64 * 1024;
    let root = tempfile::tempdir().expect("private native observation");
    let stdin = if let Some(input) = input {
        assert!(input.len() <= 16 * 1024, "native input bound");
        let mut file = tempfile::tempfile_in(root.path()).expect("private native input");
        file.write_all(input).expect("synthetic native input");
        file.rewind().expect("input rewind");
        Stdio::from(file)
    } else {
        Stdio::null()
    };
    let mut stdout = tempfile::tempfile_in(root.path()).expect("private native stdout");
    let mut stderr = tempfile::tempfile_in(root.path()).expect("private native stderr");
    let mut child = OwnedChild(
        command
            .stdin(stdin)
            .stdout(stdout.try_clone().expect("stdout handle"))
            .stderr(stderr.try_clone().expect("stderr handle"))
            .spawn()
            .expect("native process"),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("native status") {
            break status;
        }
        assert!(Instant::now() < deadline, "native parent deadline");
        assert!(
            stdout.metadata().expect("stdout size").len() <= STREAM_CAP
                && stderr.metadata().expect("stderr size").len() <= STREAM_CAP,
            "native output bound"
        );
        std::thread::yield_now();
    };
    let mut output = Output {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    for (file, bytes) in [
        (&mut stdout, &mut output.stdout),
        (&mut stderr, &mut output.stderr),
    ] {
        file.seek(SeekFrom::Start(0)).expect("output rewind");
        file.take(STREAM_CAP + 1)
            .read_to_end(bytes)
            .expect("bounded native output");
        assert!(bytes.len() as u64 <= STREAM_CAP, "native observation bound");
    }
    output
}

/// Opens a PTY whose master and slave close on exec; returns `(master, slave)`.
#[cfg(unix)]
pub fn cloexec_pty(geometry: Option<&nix::pty::Winsize>) -> (std::fs::File, std::fs::File) {
    let pty = nix::pty::openpty(geometry, None).unwrap();
    let pair = (
        std::fs::File::from(pty.master),
        std::fs::File::from(pty.slave),
    );
    for descriptor in [&pair.0, &pair.1] {
        rustix::io::fcntl_setfd(descriptor, rustix::io::FdFlags::CLOEXEC).unwrap();
    }
    pair
}

/// Accepts one connection from a nonblocking `listener` before `timeout` elapses.
pub fn accept_with_deadline(
    listener: &std::net::TcpListener,
    timeout: Duration,
    context: &str,
) -> std::net::TcpStream {
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                // BSD sockets inherit the listener's non-blocking mode; Linux sockets do not.
                stream
                    .set_nonblocking(false)
                    .expect("blocking accepted stream");
                return stream;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "{context} missing");
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("{context} accept failed: {error}"),
        }
    }
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
    for (prompt, choice) in [
        (
            READ_ONLY_CONSENT_PROMPT.as_bytes(),
            b"read only\r".as_slice(),
        ),
        (CONFIG_CONSENT_PROMPT.as_bytes(), b"config\r".as_slice()),
    ] {
        if let Some(start) = transcript
            .windows(prompt.len())
            .rposition(|part| part == prompt)
        {
            let after = &transcript[start + prompt.len()..];
            if (after.starts_with(b"\r\n") || after.starts_with(b"\n"))
                && !after.windows(b"Input:".len()).any(|part| part == b"Input:")
            {
                input
                    .write_all(choice)
                    .expect("explicit safe folder permission decision");
                *declined = true;
            }
            return;
        }
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
    {
        input
            .write_all(b"\x1b")
            .expect("dismiss initial inline consent without authority");
        *declined = true;
    }
}

#[cfg(test)]
mod tests {
    use super::attached_device;
    use std::path::Path;

    #[test]
    fn a_fixture_image_resolves_only_to_its_own_listed_device() {
        let info = "framework       : 1\n\
================================================\n\
image-path      : /private/tmp/other/storage.dmg\n\
image-type      : read/write\n\
/dev/disk4\tGUID_partition_scheme\t\n\
/dev/disk4s1\tApple_HFS\t/private/tmp/other/mnt\n\
================================================\n\
image-path      : /private/tmp/x/runtime.sparseimage\n\
image-type      : sparse disk image\n\
/dev/disk5\tGUID_partition_scheme\t\n\
/dev/disk5s1\tApple_HFS\t/private/tmp/x/mounted-image\n";
        let wanted = Path::new("/private/tmp/x/runtime.sparseimage");
        assert_eq!(
            attached_device(info, wanted, Path::new("/tmp/x/runtime.sparseimage")),
            Some("/dev/disk5".into())
        );
        assert_eq!(
            attached_device(
                info,
                Path::new("/private/var/a.dmg"),
                Path::new("/var/a.dmg")
            ),
            None,
            "a detached image is no longer listed"
        );
        let reused = "================================================\n\
image-path      : /private/tmp/other/storage.dmg\n\
/dev/disk5\tGUID_partition_scheme\t\n";
        assert_eq!(
            attached_device(reused, wanted, wanted),
            None,
            "a reused disk number backs another image"
        );
    }
}

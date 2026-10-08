use std::{
    io::{self, Read},
    os::fd::AsFd,
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
pub fn isolated_script(root: &std::path::Path) -> Command {
    script(root, true)
}

#[cfg(target_os = "linux")]
pub fn account_isolated_script(root: &std::path::Path) -> Command {
    script(root, false)
}

#[cfg(target_os = "linux")]
fn script(root: &std::path::Path, isolate_network: bool) -> Command {
    let product =
        std::path::Path::new(option_env!("CARGO_BIN_EXE_arany").expect("built product executable"));
    let mut command = account_command(root, isolate_network, product);
    command.arg("/usr/bin/script");
    command
}

#[cfg(target_os = "linux")]
pub fn account_isolated_command(
    root: &std::path::Path,
    executable: &str,
    product: &std::path::Path,
) -> Command {
    let mut command = account_command(root, false, product);
    command.arg(executable);
    command
}

#[cfg(target_os = "linux")]
fn account_command(
    root: &std::path::Path,
    isolate_network: bool,
    product: &std::path::Path,
) -> Command {
    use std::os::unix::fs::PermissionsExt;
    let home = root.join("account-home");
    std::fs::create_dir_all(&home).expect("isolated account home");
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let user = nix::unistd::User::from_uid(nix::unistd::geteuid())
        .unwrap()
        .unwrap();
    let mut command = Command::new("/usr/bin/bwrap");
    command
        .args([
            "--unshare-user",
            "--die-with-parent",
            "--tmpfs",
            "/",
            "--ro-bind",
            "/usr",
            "/usr",
            "--symlink",
            "usr/bin",
            "/bin",
            "--symlink",
            "usr/lib",
            "/lib",
            "--ro-bind",
            "/lib64",
            "/lib64",
        ])
        .arg("--ro-bind")
        .arg(product)
        .arg("/arany")
        .args(["--ro-bind", "/etc/passwd", "/etc/passwd"])
        .arg("--bind")
        .arg(root)
        .arg(root)
        .arg("--bind")
        .arg(&home)
        .arg(user.dir)
        .arg("--ro-bind")
        .arg(product)
        .arg(product)
        .args(["--proc", "/proc", "--dev-bind", "/dev", "/dev"]);
    if isolate_network {
        command.arg("--unshare-net");
    }
    if std::path::Path::new("/etc/ssl").is_dir() {
        command.args(["--ro-bind", "/etc/ssl", "/etc/ssl"]);
    }
    command.args([
        "--", "/usr/bin/sh", "-c",
        "HOME=${HOME:-\"$1\"}; XDG_STATE_HOME=${XDG_STATE_HOME:-\"$1/legacy-state\"}; XDG_DATA_HOME=${XDG_DATA_HOME:-\"$1/legacy-data\"}; export HOME XDG_STATE_HOME XDG_DATA_HOME; shift; exec \"$@\"",
        "arany-account-fixture",
    ]).arg(home);
    command
}

#[cfg(target_os = "linux")]
pub fn remember_workspace_trust(
    account: &std::path::Path,
    workspace: &std::path::Path,
    admitted_path: &std::path::Path,
) {
    use std::{
        io::Write,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    let identity = std::fs::metadata(workspace).expect("fixture directory identity");
    let root = arany::StateRoot::admit(account).expect("private fixture account root");
    let bytes = serde_json::to_vec(&serde_json::json!({
        "version": 1, "workspaces": [{ "path": admitted_path,
            "device": identity.dev(), "inode": identity.ino(), "mode": "auto_edits" }]
    }))
    .unwrap();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.path().join("workspace-permissions.json"))
        .expect("exclusive fixture trust record");
    file.write_all(&bytes).expect("remember fixture consent");
}

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

#[cfg(target_os = "linux")]
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

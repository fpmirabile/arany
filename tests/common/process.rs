use std::{
    io::{self, Read},
    os::fd::AsFd,
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
pub fn isolated_script(root: &std::path::Path) -> Command {
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
            "--unshare-net",
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
            "--symlink",
            "usr/lib",
            "/lib64",
        ])
        .args(["--ro-bind", env!("CARGO_BIN_EXE_arany"), "/arany"])
        .args(["--ro-bind", "/etc/passwd", "/etc/passwd"])
        .arg("--bind")
        .arg(root)
        .arg(root)
        .arg("--bind")
        .arg(home)
        .arg(user.dir)
        .args([
            "--proc",
            "/proc",
            "--dev-bind",
            "/dev",
            "/dev",
            "--",
            "/usr/bin/script",
        ]);
    command
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
    let deadline = Instant::now() + duration;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
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
        drain(&mut stderr_pipe, &mut stderr, cap, "stderr");
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
            Err(error) => panic!("subprocess {channel} capture failed: {error}"),
        }
    }
}

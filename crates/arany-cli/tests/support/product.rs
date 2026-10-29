// The `script` PTY launcher for the `arany` binary: wrapper, isolated environment and guard.
use std::{
    io::{self, Read},
    os::fd::AsFd,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

pub const EXE: &str = env!("CARGO_BIN_EXE_arany");

const MAX_SCANNED_SOURCES: usize = 512;
const MAX_SCANNED_SOURCE_BYTES: u64 = 1024 * 1024;

// The account root a `Isolation::Fixture` launch under `root` uses.
pub fn fixture_account(root: &Path) -> PathBuf {
    root.join("account-root")
}

pub struct ChildGuard(Option<Child>);

impl ChildGuard {
    pub fn new(child: Child) -> Self {
        Self(Some(child))
    }

    pub fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("supervised child")
    }

    pub fn take(&mut self) -> Child {
        self.0.take().expect("supervised child")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub enum Isolation<'a> {
    // Private HOME, XDG and account roots beneath this directory.
    Fixture(&'a Path),
    // The wrapper (Bubblewrap) mounts a private passwd home; `/usr/bin/script` is appended to it.
    Passwd(Command),
    // The caller's fixture isolates the process: a test already inside a private passwd-home
    // fixture, or a PTY that does not run the product itself, such as a tmux client.
    Caller,
}

// A `script` PTY around `shell`, in which `"$ARANY_TEST_EXE"` names the product.
pub struct PtyLaunch {
    command: Command,
}

impl PtyLaunch {
    pub fn new(isolation: Isolation<'_>, shell: &str) -> Self {
        let fixture = match &isolation {
            Isolation::Fixture(root) => Some(*root),
            Isolation::Passwd(_) | Isolation::Caller => None,
        };
        let mut command = match isolation {
            Isolation::Passwd(mut wrapper) => {
                wrapper.arg("/usr/bin/script");
                wrapper
            }
            Isolation::Fixture(_) | Isolation::Caller => Command::new("/usr/bin/script"),
        };
        #[cfg(target_os = "linux")]
        command.args(["-q", "-e", "-c", shell, "/dev/null"]);
        #[cfg(target_os = "macos")]
        command.args(["-q", "/dev/null", "/bin/sh", "-c", shell]);
        command
            .env_clear()
            .env("ARANY_TEST_EXE", EXE)
            .env("SHELL", "/bin/sh")
            .env("TERM", "dumb")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(root) = fixture {
            command
                .current_dir(root)
                .env("HOME", root.join("fixture-home"))
                .env("XDG_DATA_HOME", root.join("fixture-data"))
                .env("XDG_STATE_HOME", root.join("xdg-state"))
                .env("DBUS_SESSION_BUS_ADDRESS", "unixexec:path=/usr/bin/false")
                .envs(arany::test_support::process::account_fixture(
                    fixture_account(root),
                ));
        }
        Self { command }
    }

    // `ARANY_TEST_STATE` and `ARANY_TEST_WORKSPACE`, run from the Workspace.
    pub fn session(mut self, state: &Path, workspace: &Path) -> Self {
        self.command
            .env("ARANY_TEST_STATE", state)
            .env("ARANY_TEST_WORKSPACE", workspace)
            .current_dir(workspace);
        self
    }

    pub fn term(self, term: &str) -> Self {
        self.env("TERM", term)
    }

    pub fn env(mut self, name: &str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        self.command.env(name, value);
        self
    }

    pub fn envs<K, V>(mut self, variables: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
    {
        self.command.envs(variables);
        self
    }

    // A copied or release product image in place of the built binary.
    pub fn executable(self, path: impl AsRef<std::ffi::OsStr>) -> Self {
        self.env("ARANY_TEST_EXE", path)
    }

    pub fn current_dir(mut self, directory: impl AsRef<Path>) -> Self {
        self.command.current_dir(directory);
        self
    }

    pub fn stdin(mut self, stdin: Stdio) -> Self {
        self.command.stdin(stdin);
        self
    }

    pub fn piped_stderr(mut self) -> Self {
        self.command.stderr(Stdio::piped());
        self
    }

    // For a fixture that adjusts the environment itself, such as a credential helper.
    pub fn command(&mut self) -> &mut Command {
        &mut self.command
    }

    // The guarded `script` process; its stdout is nonblocking, and stdin and stdout stay on the child.
    pub fn spawn(mut self) -> ChildGuard {
        let mut process = ChildGuard::new(self.command.spawn().expect("product PTY process"));
        let output = process.child().stdout.as_ref().expect("PTY output");
        let flags = rustix::fs::fcntl_getfl(output).expect("PTY output flags");
        rustix::fs::fcntl_setfl(output, flags | rustix::fs::OFlags::NONBLOCK)
            .expect("nonblocking PTY output");
        process
    }
}

// Blocking reads over a nonblocking PTY stream, for reader threads.
pub struct Blocking<R>(pub R);

impl<R: Read + AsFd> Read for Blocking<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.0.read(buffer) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    arany::test_support::process::wait_readable(&self.0);
                }
                result => return result,
            }
        }
    }
}

// Only this file starts `script`, so a new PTY fixture cannot copy the wrapper and environment.
pub fn assert_launches_use_the_product_helper(sources: &[&str]) {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let helper = tests.join("support/product.rs");
    let mut pending: Vec<PathBuf> = sources.iter().map(|source| tests.join(source)).collect();
    let mut scanned = 0;
    let mut violations = Vec::new();
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            for entry in std::fs::read_dir(&path).expect("test source directory") {
                pending.push(entry.expect("test source entry").path());
            }
            continue;
        }
        if path == helper || path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        scanned += 1;
        assert!(scanned <= MAX_SCANNED_SOURCES, "bounded test source scan");
        let mut text = String::new();
        std::fs::File::open(&path)
            .expect("test source")
            .take(MAX_SCANNED_SOURCE_BYTES + 1)
            .read_to_string(&mut text)
            .expect("UTF-8 test source");
        assert!(
            text.len() as u64 <= MAX_SCANNED_SOURCE_BYTES,
            "bounded test source {}",
            path.display()
        );
        if text.contains(concat!("\"/usr/bin/", "script\"")) {
            violations.push(path.display().to_string());
        }
    }
    assert!(scanned > 0, "the scan found test sources");
    assert!(
        violations.is_empty(),
        "start `script` through PtyLaunch in tests/support/product.rs: {violations:?}"
    );
}

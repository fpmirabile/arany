use arany::ImageAttachment;
use std::sync::{
    Arc, LazyLock, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
const CLEANUP_UNCONFIRMED: &str =
    "clipboard cleanup unconfirmed; restart Arany before another clipboard read";

#[derive(Default)]
struct ClipboardWorker {
    task: Mutex<Option<ClipboardTask>>,
    cleanup_unconfirmed: AtomicBool,
}

struct ClipboardTask {
    thread: JoinHandle<()>,
    cancelled: Arc<AtomicBool>,
    finished: mpsc::Receiver<()>,
}

impl ClipboardWorker {
    fn shutdown(&self, timeout: Duration) -> Result<(), &'static str> {
        let deadline = Instant::now() + timeout;
        let mut current = self.task.try_lock().map_err(|_| {
            self.cleanup_unconfirmed.store(true, Ordering::Release);
            CLEANUP_UNCONFIRMED
        })?;
        let Some(task) = current.take() else {
            return Ok(());
        };
        task.cancelled.store(true, Ordering::Release);
        task.thread.thread().unpark();
        if task
            .finished
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .is_err()
            || self.cleanup_unconfirmed.load(Ordering::Acquire)
        {
            self.cleanup_unconfirmed.store(true, Ordering::Release);
            return Err(CLEANUP_UNCONFIRMED);
        }
        Ok(())
    }
}

pub(super) fn shutdown() -> std::io::Result<()> {
    WORKER.shutdown(CLEANUP_TIMEOUT).map_err(|message| {
        #[cfg(unix)]
        warn_cleanup_unconfirmed();
        std::io::Error::other(message)
    })
}

#[cfg(unix)]
fn warn_cleanup_unconfirmed() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    // A blocked warning must never hold Rust's stderr lock during teardown or exit.
    if std::thread::Builder::new()
        .name("arany-clipboard-warning".into())
        .spawn(|| {
            if let Ok(stderr) = rustix::io::dup(std::io::stderr()) {
                let message = CLEANUP_UNCONFIRMED;
                #[cfg(test)]
                let message = cleanup_tests::WarningMessage(message);
                let _ = rustix::io::write(stderr, format!("{message}\n").as_bytes());
            }
        })
        .is_err()
    {
        WARNED.store(false, Ordering::Relaxed);
    }
}

static WORKER: LazyLock<Arc<ClipboardWorker>> =
    LazyLock::new(|| Arc::new(ClipboardWorker::default()));

struct Cleanup {
    child: Option<std::process::Child>,
    timeout: std::time::Duration,
}

impl Default for Cleanup {
    fn default() -> Self {
        Self {
            child: None,
            timeout: CLEANUP_TIMEOUT,
        }
    }
}

pub(super) enum ClipboardContent {
    Text(String),
    Image(ImageAttachment),
}

pub(super) struct ClipboardRead {
    result: oneshot::Receiver<Result<ClipboardContent, &'static str>>,
    cancelled: Arc<AtomicBool>,
}

impl ClipboardRead {
    pub(super) fn start() -> Result<Self, &'static str> {
        Self::start_with(&WORKER, read)
    }

    fn start_with(
        owner: &Arc<ClipboardWorker>,
        read: impl FnOnce(&AtomicBool, &mut Cleanup) -> Result<ClipboardContent, &'static str>
        + Send
        + 'static,
    ) -> Result<Self, &'static str> {
        let mut task = owner
            .task
            .try_lock()
            .map_err(|_| "clipboard worker unavailable")?;
        if owner.cleanup_unconfirmed.load(Ordering::Acquire) {
            return Err(CLEANUP_UNCONFIRMED);
        }
        if task.as_ref().is_some_and(|task| !task.thread.is_finished()) {
            return Err("clipboard read or cleanup already in progress");
        }
        let (sender, result) = oneshot::channel();
        let (finished, completion) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancelled);
        let owner = Arc::clone(owner);
        *task = Some(ClipboardTask {
            cancelled: Arc::clone(&cancelled),
            finished: completion,
            thread: std::thread::Builder::new()
                .name("arany-clipboard-read".into())
                .spawn(move || {
                    let mut cleanup = Cleanup::default();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        read(&stop, &mut cleanup)
                    }))
                    .unwrap_or(Err("clipboard worker unavailable"));
                    let result = if cleanup.child.is_some() {
                        owner.cleanup_unconfirmed.store(true, Ordering::Release);
                        Err(CLEANUP_UNCONFIRMED)
                    } else {
                        result
                    };
                    let _ = sender.send(result);
                    while let Some(child) = &mut cleanup.child {
                        if matches!(child.try_wait(), Ok(Some(_))) {
                            cleanup.child = None;
                        } else {
                            std::thread::park_timeout(std::time::Duration::from_millis(5));
                        }
                    }
                    let _ = finished.send(());
                })
                .map_err(|_| "clipboard worker unavailable")?,
        });
        Ok(Self { result, cancelled })
    }

    pub(super) async fn recv(&mut self) -> Result<ClipboardContent, &'static str> {
        (&mut self.result)
            .await
            .unwrap_or(Err("clipboard read unavailable"))
    }
}

impl Drop for ClipboardRead {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read(_cancelled: &AtomicBool, _cleanup: &mut Cleanup) -> Result<ClipboardContent, &'static str> {
    Err("explicit clipboard access is unavailable on this platform")
}

#[cfg(target_os = "linux")]
fn read(cancelled: &AtomicBool, cleanup: &mut Cleanup) -> Result<ClipboardContent, &'static str> {
    use super::composer::MAX_DRAFT_BYTES;
    use std::process::Command;
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(2);
    let (mut command, image) = {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        use std::path::Path;
        if let Some(display) = std::env::var_os("WAYLAND_DISPLAY") {
            let display = display
                .to_str()
                .filter(|display| {
                    !display.is_empty()
                        && display.len() <= 128
                        && !matches!(*display, "." | "..")
                        && display
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
                })
                .ok_or("invalid local Wayland display")?;
            let runtime =
                std::env::var_os("XDG_RUNTIME_DIR").ok_or("Wayland runtime unavailable")?;
            let path = Path::new(&runtime);
            let metadata =
                std::fs::symlink_metadata(path).map_err(|_| "Wayland runtime unavailable")?;
            if !path.is_absolute()
                || !metadata.is_dir()
                || metadata.uid() != rustix::process::getuid().as_raw()
                || metadata.mode() & 0o077 != 0
                || std::fs::canonicalize(path).ok().as_deref() != Some(path)
            {
                return Err("unsafe Wayland runtime");
            }
            let socket = std::fs::symlink_metadata(path.join(display))
                .map_err(|_| "Wayland clipboard unavailable")?;
            if !socket.file_type().is_socket() || socket.uid() != metadata.uid() {
                return Err("unsafe Wayland display");
            }
            let mut command = Command::new(installed("wl-paste")?);
            command
                .env_clear()
                .env("XDG_RUNTIME_DIR", runtime)
                .env("WAYLAND_DISPLAY", display)
                .arg("--list-types");
            let types = run_process(&mut command, cancelled, deadline, 16 * 1024, cleanup)?;
            let types = std::str::from_utf8(&types).map_err(|_| "invalid clipboard types")?;
            let kind = selected_type(
                types,
                &["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"],
            )?;
            let mut text = Command::new(command.get_program());
            text.env_clear()
                .envs(
                    command
                        .get_envs()
                        .filter_map(|(name, value)| value.map(|value| (name, value))),
                )
                .args(["--no-newline", "--type", kind]);
            (text, kind == "image/png")
        } else {
            let display = std::env::var("DISPLAY")
                .map_err(|_| "local clipboard unavailable; use terminal paste")?;
            if !local_x11_display(&display) {
                return Err("remote or invalid X11 clipboard display is not allowed");
            }
            let mut command = Command::new(installed("xclip")?);
            command
                .env_clear()
                .env("DISPLAY", display)
                .env("HOME", "/")
                .args(["-selection", "clipboard", "-out", "-target", "TARGETS"]);
            let authority = std::env::var_os("XAUTHORITY").or_else(|| {
                nix::unistd::User::from_uid(nix::unistd::Uid::effective())
                    .ok()
                    .flatten()
                    .map(|user| user.dir.join(".Xauthority").into_os_string())
            });
            if let Some(authority) = authority {
                let path = Path::new(&authority);
                match std::fs::symlink_metadata(path) {
                    Ok(metadata)
                        if path.is_absolute()
                            && metadata.is_file()
                            && metadata.nlink() == 1
                            && metadata.uid() == rustix::process::getuid().as_raw()
                            && metadata.mode() & 0o077 == 0 =>
                    {
                        command.env("XAUTHORITY", authority);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    _ => return Err("unsafe X11 authority file"),
                }
            }
            let types = run_process(&mut command, cancelled, deadline, 16 * 1024, cleanup)?;
            let types = std::str::from_utf8(&types).map_err(|_| "invalid clipboard types")?;
            let kind = selected_type(
                types,
                &["UTF8_STRING", "text/plain;charset=utf-8", "text/plain"],
            )?;
            let mut payload = Command::new(command.get_program());
            payload
                .env_clear()
                .envs(
                    command
                        .get_envs()
                        .filter_map(|(name, value)| value.map(|value| (name, value))),
                )
                .args(["-selection", "clipboard", "-out", "-target", kind]);
            (payload, kind == "image/png")
        }
    };
    let limit = if image {
        arany::MAX_IMAGE_BYTES
    } else {
        MAX_DRAFT_BYTES
    };
    let bytes = run_process(&mut command, cancelled, deadline, limit, cleanup)?;
    if image {
        ImageAttachment::from_png(&bytes).map(ClipboardContent::Image)
    } else {
        String::from_utf8(bytes)
            .map(ClipboardContent::Text)
            .map_err(|_| "clipboard text is not valid UTF-8")
    }
}

#[cfg(target_os = "macos")]
fn read(cancelled: &AtomicBool, cleanup: &mut Cleanup) -> Result<ClipboardContent, &'static str> {
    use std::time::{Duration, Instant};

    let mut command = native_command(None)?;
    let bytes = run_process(
        &mut command,
        cancelled,
        Instant::now() + Duration::from_secs(2),
        super::composer::MAX_DRAFT_BYTES.max(arany::MAX_IMAGE_BYTES) + 2,
        cleanup,
    )?;
    native_content(&bytes)
}

#[cfg(target_os = "macos")]
fn native_command(name: Option<&str>) -> Result<std::process::Command, &'static str> {
    let mut command = std::process::Command::new(installed("osascript")?);
    command
        .env_clear()
        .args(["-l", "JavaScript", "-e", include_str!("clipboard.js")]);
    command
        .arg(name.unwrap_or(""))
        .arg(arany::MAX_IMAGE_BYTES.to_string())
        .arg(super::composer::MAX_DRAFT_BYTES.to_string());
    Ok(command)
}

#[cfg(target_os = "macos")]
fn native_content(bytes: &[u8]) -> Result<ClipboardContent, &'static str> {
    match bytes {
        [b'P', b'\n', payload @ ..] => {
            ImageAttachment::from_png(payload).map(ClipboardContent::Image)
        }
        [b'T', b'\n', payload @ ..] if payload.len() <= super::composer::MAX_DRAFT_BYTES => {
            String::from_utf8(payload.to_vec())
                .map(ClipboardContent::Text)
                .map_err(|_| "clipboard text is not valid UTF-8")
        }
        _ => Err("clipboard contains no supported bounded text or PNG image"),
    }
}

#[cfg(target_os = "linux")]
fn selected_type(types: &str, text_types: &[&'static str]) -> Result<&'static str, &'static str> {
    if types.lines().any(|kind| kind == "image/png") {
        return Ok("image/png");
    }
    if types.lines().any(|kind| kind.starts_with("image/")) {
        return Err("clipboard image format is unsupported; use PNG");
    }
    text_types
        .iter()
        .copied()
        .find(|kind| types.lines().any(|offered| offered == *kind))
        .ok_or("clipboard contains no supported text or PNG image")
}

#[cfg(target_os = "linux")]
fn local_x11_display(display: &str) -> bool {
    display.strip_prefix(':').is_some_and(|number| {
        number.len() <= 11
            && number.split('.').count() <= 2
            && number.split('.').all(|part| {
                !part.is_empty()
                    && part.bytes().all(|byte| byte.is_ascii_digit())
                    && part.parse::<u16>().is_ok()
            })
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn installed(name: &str) -> Result<std::path::PathBuf, &'static str> {
    use std::os::unix::fs::MetadataExt;
    #[cfg(target_os = "linux")]
    const ROOTS: &[&str] = &["/usr/bin", "/usr/local/bin", "/run/current-system/sw/bin"];
    #[cfg(target_os = "macos")]
    const ROOTS: &[&str] = &["/usr/bin"];
    for root in ROOTS {
        let candidate = std::path::Path::new(root).join(name);
        let Ok(path) = std::fs::canonicalize(&candidate) else {
            continue;
        };
        if !candidate.ancestors().chain(path.ancestors()).all(|part| {
            std::fs::metadata(part)
                .is_ok_and(|metadata| metadata.uid() == 0 && metadata.mode() & 0o022 == 0)
        }) {
            continue;
        }
        if std::fs::metadata(&path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.mode() & 0o111 != 0)
        {
            return Ok(path);
        }
    }
    Err("trusted clipboard client unavailable; use terminal paste")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_process(
    command: &mut std::process::Command,
    cancelled: &AtomicBool,
    deadline: std::time::Instant,
    limit: usize,
    cleanup: &mut Cleanup,
) -> Result<Vec<u8>, &'static str> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};
    use std::io::{self, Read};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};

    struct OwnedChild<'a> {
        child: Option<Child>,
        group: Option<Pid>,
        cleanup: &'a mut Cleanup,
    }
    impl OwnedChild<'_> {
        fn kill(&mut self) {
            if let Some(child) = &mut self.child {
                if let Some(group) = self.group {
                    let _ = kill_process_group(group, Signal::KILL);
                }
                let _ = child.kill();
            }
        }

        fn finish(mut self) -> Result<(), &'static str> {
            let deadline = Instant::now() + self.cleanup.timeout;
            self.kill();
            while Instant::now() < deadline {
                match self
                    .child
                    .as_mut()
                    .expect("owned clipboard child")
                    .try_wait()
                {
                    Ok(Some(_)) => {
                        self.child = None;
                        return Ok(());
                    }
                    Ok(None) => std::thread::park_timeout(
                        Duration::from_millis(5)
                            .min(deadline.saturating_duration_since(Instant::now())),
                    ),
                    Err(_) => break,
                }
            }
            Err(CLEANUP_UNCONFIRMED)
        }
    }
    impl Drop for OwnedChild<'_> {
        fn drop(&mut self) {
            self.kill();
            if let Some(child) = self.child.take() {
                self.cleanup.child = Some(child);
            }
        }
    }
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        return Err("clipboard read cancelled or timed out");
    }
    let child = command
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|_| "clipboard client unavailable")?;
    let group = child.id().try_into().ok().and_then(Pid::from_raw);
    let mut owner = OwnedChild {
        child: Some(child),
        group,
        cleanup,
    };
    let result = (|| {
        let group = owner.group.ok_or("clipboard process unavailable")?;
        let child = owner.child.as_mut().expect("owned clipboard child");
        let mut stdout = child.stdout.take().ok_or("clipboard pipe unavailable")?;
        let mut stderr = child.stderr.take().ok_or("clipboard pipe unavailable")?;
        fcntl_setfl(
            &stdout,
            fcntl_getfl(&stdout).map_err(|_| "clipboard pipe unavailable")? | OFlags::NONBLOCK,
        )
        .map_err(|_| "clipboard pipe unavailable")?;
        fcntl_setfl(
            &stderr,
            fcntl_getfl(&stderr).map_err(|_| "clipboard pipe unavailable")? | OFlags::NONBLOCK,
        )
        .map_err(|_| "clipboard pipe unavailable")?;
        let mut output = Vec::new();
        let mut output_bytes = 0;
        let mut error_bytes = 0;
        let mut stdout_closed = false;
        let mut stderr_closed = false;
        loop {
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err("clipboard read cancelled or timed out");
            }
            let mut buffer = [0; 4096];
            for (stream, closed, count, cap, collect) in [
                (
                    &mut stdout as &mut dyn Read,
                    &mut stdout_closed,
                    &mut output_bytes,
                    limit,
                    true,
                ),
                (
                    &mut stderr as &mut dyn Read,
                    &mut stderr_closed,
                    &mut error_bytes,
                    4096,
                    false,
                ),
            ] {
                if *closed {
                    continue;
                }
                match stream.read(&mut buffer) {
                    Ok(0) => *closed = true,
                    Ok(length) => {
                        if length > cap.saturating_sub(*count) {
                            return Err("clipboard output exceeds its limit");
                        }
                        *count += length;
                        if collect {
                            output.extend_from_slice(&buffer[..length]);
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => return Err("clipboard read unavailable"),
                }
            }
            // Keep the leader waitable until group cleanup to prevent process-ID reuse.
            if let Some(status) = waitid(
                WaitId::Pid(group),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            )
            .map_err(|_| "clipboard process unavailable")?
                && stdout_closed
                && stderr_closed
            {
                return if status.exit_status() == Some(0) {
                    Ok(output)
                } else {
                    Err("clipboard read unavailable")
                };
            }
            std::thread::park_timeout(Duration::from_millis(5));
        }
    })();
    owner.finish()?;
    result
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[cfg(unix)]
    struct WarningWriteGate {
        ready: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    }

    #[cfg(unix)]
    static WARNING_WRITE_GATE: Mutex<Option<WarningWriteGate>> = Mutex::new(None);

    #[cfg(unix)]
    pub(super) struct WarningMessage(pub(super) &'static str);

    #[cfg(unix)]
    impl std::fmt::Display for WarningMessage {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(self.0)?;
            if let Some(gate) = WARNING_WRITE_GATE.lock().unwrap().take() {
                // Formatting through Stderr would hold its lock across this gate.
                gate.ready.send(()).unwrap();
                gate.release.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            Ok(())
        }
    }

    #[cfg(unix)]
    #[test]
    fn clipboard_cleanup_warning_never_waits_for_a_lock_or_full_pipe() {
        use arany::test_support::process::capture;
        use nix::unistd::pipe;
        use rustix::{
            fs::{OFlags, fcntl_getfl, fcntl_setfl},
            io::{Errno, write},
        };
        use std::process::{Command, Stdio};

        for case in ["locked", "full", "transitions", "drop"] {
            let (reader, writer) = pipe().unwrap();
            if case == "full" {
                let flags = fcntl_getfl(&writer).unwrap();
                fcntl_setfl(&writer, flags | OFlags::NONBLOCK).unwrap();
                let mut filled = 0;
                let mut chunk: &[u8] = &[0; 4096];
                loop {
                    match write(&writer, chunk) {
                        Ok(length) => filled += length,
                        Err(Errno::AGAIN) if chunk.len() > 1 => chunk = &[0],
                        Err(Errno::AGAIN) => break,
                        Err(error) => panic!("fill warning pipe: {error}"),
                    }
                    assert!(filled <= 1024 * 1024, "warning pipe capacity exceeded");
                }
                fcntl_setfl(&writer, flags).unwrap();
            }
            let mut child = Command::new(std::env::current_exe().unwrap())
                .current_dir("/")
                .env_clear()
                .env("ARANY_CLIPBOARD_WARNING_TEST", case)
                .args([
                    "--exact",
                    "terminal::clipboard::cleanup_tests::clipboard_warning_fixture",
                    "--ignored",
                    "--nocapture",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::from(writer))
                .spawn()
                .unwrap();
            let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                capture(&mut child, Duration::from_secs(5), 16 * 1024)
            }));
            let _ = child.kill();
            child.wait().unwrap();
            drop(reader);
            let output = output.expect(case);
            assert!(output.status.success(), "{case}: {:?}", output.status);
            assert!(
                String::from_utf8(output.stdout)
                    .unwrap()
                    .contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
                "{case}: warning fixture did not complete"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "blocked stderr fixture for the clipboard cleanup owner"]
    fn clipboard_warning_fixture() {
        let case = std::env::var("ARANY_CLIPBOARD_WARNING_TEST").unwrap();
        if matches!(case.as_str(), "transitions" | "drop") {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let released = || super::super::AttachedTerminal {
                        waiting_tool: None,
                        terminal: None,
                        parked_terminal: None,
                        mode: super::super::TerminalMode::Classic,
                        classic_pinned: false,
                        alternate: None,
                        transcript_open: false,
                        native: Default::default(),
                        active_run: None,
                        history: Default::default(),
                        classic_history: Default::default(),
                        input: None,
                        _raw_mode: None,
                        mouse_capture: None,
                        bracketed_paste: None,
                        session_picker_area: None,
                        agent_picker_area: None,
                        linear: None,
                        secret_echo: None,
                        agent_panel: None,
                        help_selected: None,
                        clipboard: None,
                        clipboard_paste: None,
                        clipboard_preview: false,
                        shutdown_clipboard_on_drop: true,
                        requested_screen_reader: false,
                        color_enabled: false,
                        interrupt: tokio::signal::unix::signal(
                            tokio::signal::unix::SignalKind::interrupt(),
                        )
                        .unwrap(),
                        suspend: tokio::signal::unix::signal(
                            tokio::signal::unix::SignalKind::from_raw(nix::libc::SIGTSTP),
                        )
                        .unwrap(),
                        hangup: tokio::signal::unix::signal(
                            tokio::signal::unix::SignalKind::hangup(),
                        )
                        .unwrap(),
                        terminate: tokio::signal::unix::signal(
                            tokio::signal::unix::SignalKind::terminate(),
                        )
                        .unwrap(),
                    };
                    let _worker_lock = WORKER.task.lock().unwrap();
                    let mut terminal = released();
                    terminal.release_for_output().unwrap();
                    terminal.replace_released(released());
                    assert!(!WORKER.cleanup_unconfirmed.load(Ordering::Acquire));
                    if case == "transitions" {
                        assert!(terminal.restore().is_err());
                    } else {
                        drop(terminal);
                    }
                    assert!(WORKER.cleanup_unconfirmed.load(Ordering::Acquire));
                });
            return;
        }
        let (ready, writing) = mpsc::sync_channel(1);
        let (release, held) = mpsc::sync_channel(1);
        if case == "locked" {
            let (ready, locked) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let _lock = std::io::stderr().lock();
                ready.send(()).unwrap();
                loop {
                    std::thread::park();
                }
            });
            locked.recv_timeout(Duration::from_secs(1)).unwrap();
        } else {
            assert_eq!(case, "full");
            *WARNING_WRITE_GATE.lock().unwrap() = Some(WarningWriteGate {
                ready,
                release: held,
            });
        }
        let _worker_lock = WORKER.task.lock().unwrap();
        let start = Instant::now();
        assert_eq!(shutdown().unwrap_err().to_string(), CLEANUP_UNCONFIRMED);
        assert!(
            start.elapsed() < CLEANUP_TIMEOUT,
            "warning blocked teardown"
        );
        if case == "full" {
            use std::io::{Read, Seek};
            use std::os::fd::AsRawFd;

            writing.recv_timeout(CLEANUP_TIMEOUT).unwrap();
            // Keep the warning's pipe full while exercising the normal error reporter.
            let mut sink = tempfile::tempfile().unwrap();
            nix::unistd::dup2(sink.as_raw_fd(), std::io::stderr().as_raw_fd()).unwrap();
            super::super::report_error("clipboard shutdown failed");
            sink.rewind().unwrap();
            let mut reported = String::new();
            sink.read_to_string(&mut reported).unwrap();
            assert_eq!(reported, "error: clipboard shutdown failed\n");
            release.send(()).unwrap();
        }
    }

    #[test]
    fn cancellation_does_not_wait_for_the_clipboard_worker() {
        let (release, held) = mpsc::sync_channel::<()>(1);
        let (progress, progressed) = mpsc::sync_channel(1);
        let (observed, cancellation) = mpsc::sync_channel(1);
        let owner = Arc::new(ClipboardWorker::default());
        let read = ClipboardRead::start_with(&owner, move |stop, _| {
            held.recv_timeout(Duration::from_secs(5)).unwrap();
            observed.send(stop.load(Ordering::Acquire)).unwrap();
            Err("clipboard read cancelled or timed out")
        })
        .unwrap();
        let reactor = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(async {
                    drop(read);
                    tokio::task::yield_now().await;
                    progress.send(()).unwrap();
                });
        });
        let responsive = progressed.recv_timeout(Duration::from_secs(1)).is_ok();
        release.send(()).unwrap();
        reactor.join().unwrap();
        owner
            .task
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .thread
            .join()
            .unwrap();
        assert!(responsive, "clipboard cancellation blocked the reactor");
        assert!(cancellation.recv_timeout(Duration::from_secs(1)).unwrap());
    }

    #[test]
    fn clipboard_shutdown_reports_a_worker_that_misses_its_deadline() {
        let (release, held) = mpsc::sync_channel::<()>(1);
        let (observed, cancellation) = mpsc::sync_channel(1);
        let owner = Arc::new(ClipboardWorker::default());
        let read = ClipboardRead::start_with(&owner, move |stop, _| {
            held.recv_timeout(Duration::from_secs(5)).unwrap();
            observed.send(stop.load(Ordering::Acquire)).unwrap();
            Err("clipboard read cancelled or timed out")
        })
        .unwrap();
        assert_eq!(owner.shutdown(Duration::ZERO), Err(CLEANUP_UNCONFIRMED));
        release.send(()).unwrap();
        assert!(cancellation.recv_timeout(Duration::from_secs(1)).unwrap());
        assert_eq!(
            ClipboardRead::start_with(&owner, |_, _| panic!("retry after shutdown deadline")).err(),
            Some(CLEANUP_UNCONFIRMED)
        );
        drop(read);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn clipboard_process_cleanup_preserves_reap_evidence_and_reports_deadline_faults() {
        use rustix::process::{Pid, WaitId, WaitIdOptions, test_kill_process, waitid};
        use std::{
            os::unix::process::{CommandExt, ExitStatusExt},
            process::{Child, Command, Stdio},
            time::Instant,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        struct GroupPeer(Child);

        impl Drop for GroupPeer {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        for (cancel, cleanup_timeout, expected, shutdown) in [
            (false, Duration::from_secs(1), None, false),
            (
                true,
                Duration::from_secs(1),
                Some("clipboard read cancelled or timed out"),
                false,
            ),
            (true, Duration::ZERO, Some(CLEANUP_UNCONFIRMED), false),
            (false, Duration::ZERO, Some(CLEANUP_UNCONFIRMED), false),
            (
                true,
                Duration::from_secs(1),
                Some("clipboard read cancelled or timed out"),
                true,
            ),
            (true, Duration::ZERO, Some(CLEANUP_UNCONFIRMED), true),
        ] {
            let fixture = tempfile::tempdir().unwrap();
            let socket = fixture.path().join("clipboard.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let owner = Arc::new(ClipboardWorker::default());
            let (retained, child_retained) = oneshot::channel();
            let worker_socket = socket.clone();
            let mut read = ClipboardRead::start_with(&owner, move |cancelled, cleanup| {
                cleanup.timeout = cleanup_timeout;
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .env_clear()
                    .env("ARANY_CLIPBOARD_TEST_SOCKET", worker_socket)
                    .args([
                        "--exact",
                        "terminal::clipboard::cleanup_tests::clipboard_child_fixture",
                        "--ignored",
                        "--nocapture",
                    ]);
                let result = run_process(
                    &mut command,
                    cancelled,
                    Instant::now() + Duration::from_secs(5),
                    16 * 1024,
                    cleanup,
                );
                let _ = retained.send(cleanup.child.is_some());
                result.map(|bytes| ClipboardContent::Text(String::from_utf8(bytes).unwrap()))
            })
            .unwrap();
            let (mut gate, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let pid = tokio::time::timeout(Duration::from_secs(5), gate.read_u32_le())
                .await
                .unwrap()
                .unwrap();
            let pid = Pid::from_raw(pid.try_into().unwrap()).unwrap();
            // A direct child in the same group gives deterministic reaping on both OSes.
            let mut peer = GroupPeer(
                Command::new(std::env::current_exe().unwrap())
                    .current_dir("/")
                    .env_clear()
                    .env("ARANY_CLIPBOARD_TEST_SOCKET", &socket)
                    .args([
                        "--exact",
                        "terminal::clipboard::cleanup_tests::clipboard_child_fixture",
                        "--ignored",
                        "--nocapture",
                    ])
                    .process_group(pid.as_raw_pid())
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
            let (mut peer_gate, _) =
                tokio::time::timeout(Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
            let peer_pid = tokio::time::timeout(Duration::from_secs(5), peer_gate.read_u32_le())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(peer_pid, peer.0.id());
            let peer_pid = Pid::from_raw(peer_pid.try_into().unwrap()).unwrap();
            assert!(
                ClipboardRead::start_with(&owner, |_, _| panic!("overlapping clipboard read"))
                    .is_err()
            );
            if shutdown {
                assert_eq!(
                    owner.shutdown(CLEANUP_TIMEOUT),
                    if cleanup_timeout.is_zero() {
                        Err(CLEANUP_UNCONFIRMED)
                    } else {
                        Ok(())
                    }
                );
            } else if cancel {
                read.cancelled.store(true, Ordering::Release);
            } else {
                gate.write_all(&[1]).await.unwrap();
            }
            let result = tokio::time::timeout(Duration::from_secs(5), read.recv())
                .await
                .unwrap();
            match (result, expected) {
                (Ok(ClipboardContent::Text(output)), None) => {
                    assert!(output.contains("clipboard fixture payload"));
                    assert!(output.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"));
                }
                (Err(actual), Some(expected)) => assert_eq!(actual, expected),
                _ => panic!("unexpected clipboard process result"),
            }
            let status = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(status) = peer.0.try_wait().unwrap() {
                        break status;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("clipboard process-group peer survived cleanup");
            assert_eq!(status.signal(), Some(nix::libc::SIGKILL));
            assert_eq!(test_kill_process(peer_pid), Err(rustix::io::Errno::SRCH));
            assert_eq!(child_retained.await.unwrap(), cleanup_timeout.is_zero());
            tokio::time::timeout(Duration::from_secs(5), async {
                while owner
                    .task
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|task| !task.thread.is_finished())
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            if let Some(task) = owner.task.lock().unwrap().take() {
                task.thread.join().unwrap();
            }
            assert!(
                matches!(
                    waitid(
                        WaitId::Pid(pid),
                        WaitIdOptions::EXITED | WaitIdOptions::NOHANG
                    ),
                    Err(rustix::io::Errno::CHILD)
                ),
                "clipboard child was not reaped"
            );
            if cleanup_timeout.is_zero() {
                assert_eq!(
                    ClipboardRead::start_with(&owner, |_, _| panic!(
                        "retry after unconfirmed cleanup"
                    ))
                    .err(),
                    Some(CLEANUP_UNCONFIRMED)
                );
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[ignore = "held subprocess fixture for the clipboard cleanup owner"]
    fn clipboard_child_fixture() {
        use std::{
            io::{Read, Write},
            os::unix::net::UnixStream,
        };

        let socket = std::env::var_os("ARANY_CLIPBOARD_TEST_SOCKET").unwrap();
        let mut gate = UnixStream::connect(socket).unwrap();
        gate.write_all(&std::process::id().to_le_bytes()).unwrap();
        let mut release = [0];
        gate.read_exact(&mut release).unwrap();
        std::io::stdout()
            .write_all(b"clipboard fixture payload")
            .unwrap();
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use std::{
        process::Command,
        time::{Duration, Instant},
    };

    const FIXTURE: &str = r#"
ObjC.import('AppKit');
ObjC.import('Foundation');
function run(args) {
    const board = $.NSPasteboard.pasteboardWithName(args[0]);
    if (args.length === 1) { board.releaseGlobally; return; }
    board.clearContents;
    const data = $.NSData.dataWithContentsOfFile(args[2]);
    if (!board.setDataForType(data, args[1])) { throw new Error('fixture write failed'); }
    if (args.length === 4) { board.setStringForType('text fallback', 'public.utf8-plain-text'); }
}
"#;

    struct Board(String);

    impl Board {
        fn invoke(&self, args: &[&str]) -> Result<Vec<u8>, &'static str> {
            let mut command = Command::new(installed("osascript")?);
            command
                .env_clear()
                .args(["-l", "JavaScript", "-e", FIXTURE, &self.0])
                .args(args);
            run_process(
                &mut command,
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(5),
                1024,
                &mut Cleanup::default(),
            )
        }
    }

    impl Drop for Board {
        fn drop(&mut self) {
            let _ = self.invoke(&[]);
        }
    }

    #[test]
    fn native_clipboard_frames_keep_image_and_text_admission_closed() {
        for bytes in [b"".as_slice(), b"X\ntext", b"T\n\xff", b"P\ninvalid"] {
            assert!(native_content(bytes).is_err());
        }
        let mut oversized = b"T\n".to_vec();
        oversized.extend(vec![b'x'; super::super::composer::MAX_DRAFT_BYTES + 1]);
        assert!(native_content(&oversized).is_err());
        let image = arany::test_support::test_image();
        let png = STANDARD.decode(image.base64()).unwrap();
        let mut frame = b"P\n".to_vec();
        frame.extend(&png);
        assert!(
            matches!(native_content(&frame), Ok(ClipboardContent::Image(value)) if value == image)
        );
        assert!(
            matches!(native_content(b"T\nline\ntext"), Ok(ClipboardContent::Text(value)) if value == "line\ntext")
        );
    }

    #[test]
    #[cfg_attr(
        not(arany_ci),
        ignore = "native named-pasteboard service gate; never accesses the general clipboard"
    )]
    fn native_named_pasteboard_png_text_and_rejection() {
        let _gate = crate::test_process::NATIVE_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let board = Board(format!("dev.Arany.test.{}", uuid::Uuid::now_v7()));
        let fixture = tempfile::tempdir().unwrap();
        let payload = fixture.path().join("payload");
        let image = arany::test_support::test_image();
        for (kind, bytes, fallback, accepted) in [
            (
                "public.utf8-plain-text",
                b"synthetic\ntext".to_vec(),
                false,
                true,
            ),
            (
                "public.png",
                STANDARD.decode(image.base64()).unwrap(),
                true,
                true,
            ),
            ("public.png", b"invalid PNG".to_vec(), true, false),
            ("public.tiff", b"unsupported image".to_vec(), true, false),
            (
                "public.png",
                vec![b'x'; arany::MAX_IMAGE_BYTES + 1],
                true,
                false,
            ),
            (
                "public.utf8-plain-text",
                vec![b'x'; super::super::composer::MAX_DRAFT_BYTES + 1],
                false,
                false,
            ),
        ] {
            std::fs::write(&payload, &bytes).unwrap();
            let mut args = vec![kind, payload.to_str().unwrap()];
            if fallback {
                args.push("fallback");
            }
            assert!(
                board.invoke(&args).is_ok(),
                "isolated pasteboard fixture failed"
            );
            let mut command = native_command(Some(&board.0)).unwrap();
            let result = run_process(
                &mut command,
                &AtomicBool::new(false),
                Instant::now() + Duration::from_secs(5),
                arany::MAX_IMAGE_BYTES + 2,
                &mut Cleanup::default(),
            )
            .and_then(|bytes| native_content(&bytes));
            assert_eq!(result.is_ok(), accepted, "native clipboard case {kind}");
            if accepted {
                match result.unwrap() {
                    ClipboardContent::Image(value) => {
                        assert!(value == image, "native PNG bytes changed")
                    }
                    ClipboardContent::Text(value) => assert_eq!(value.as_bytes(), bytes),
                }
            }
        }
    }
}

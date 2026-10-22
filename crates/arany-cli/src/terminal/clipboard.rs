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

pub(super) struct ClipboardJob<T> {
    result: oneshot::Receiver<Result<T, &'static str>>,
    cancelled: Arc<AtomicBool>,
}

pub(super) type ClipboardRead = ClipboardJob<ClipboardContent>;

impl ClipboardRead {
    pub(super) fn start() -> Result<Self, &'static str> {
        Self::start_with(&WORKER, read)
    }
}

impl<T: Send + 'static> ClipboardJob<T> {
    fn start_with(
        owner: &Arc<ClipboardWorker>,
        job: impl FnOnce(&AtomicBool, &mut Cleanup) -> Result<T, &'static str> + Send + 'static,
    ) -> Result<Self, &'static str> {
        let mut task = owner
            .task
            .try_lock()
            .map_err(|_| "clipboard worker unavailable")?;
        if owner.cleanup_unconfirmed.load(Ordering::Acquire) {
            return Err(CLEANUP_UNCONFIRMED);
        }
        if task.as_ref().is_some_and(|task| !task.thread.is_finished()) {
            return Err("clipboard read, copy or cleanup already in progress");
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
                .name("arany-clipboard".into())
                .spawn(move || {
                    let mut cleanup = Cleanup::default();
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        job(&stop, &mut cleanup)
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

    pub(super) async fn recv(&mut self) -> Result<T, &'static str> {
        (&mut self.result)
            .await
            .unwrap_or(Err("clipboard worker unavailable"))
    }
}

impl<T> Drop for ClipboardJob<T> {
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
    use super::composer::MAX_PASTE_BYTES;
    use std::process::Command;
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(2);
    let display = LocalDisplay::detect(&|name| std::env::var_os(name))?;
    let (mut command, image) = match display {
        LocalDisplay::Wayland { .. } => {
            let mut command = display.command("wl-paste")?;
            command.arg("--list-types");
            let types = run_process(&mut command, cancelled, deadline, 16 * 1024, cleanup)?;
            let types = std::str::from_utf8(&types).map_err(|_| "invalid clipboard types")?;
            let kind = selected_type(
                types,
                &["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"],
            )?;
            let mut text = Command::new(command.get_program());
            display.configure(&mut text);
            text.args(["--no-newline", "--type", kind]);
            (text, kind == "image/png")
        }
        LocalDisplay::X11 { .. } => {
            let mut command = display.command("xclip")?;
            command.args(["-selection", "clipboard", "-out", "-target", "TARGETS"]);
            let types = run_process(&mut command, cancelled, deadline, 16 * 1024, cleanup)?;
            let types = std::str::from_utf8(&types).map_err(|_| "invalid clipboard types")?;
            let kind = selected_type(
                types,
                &["UTF8_STRING", "text/plain;charset=utf-8", "text/plain"],
            )?;
            let mut payload = Command::new(command.get_program());
            display.configure(&mut payload);
            payload.args(["-selection", "clipboard", "-out", "-target", kind]);
            (payload, kind == "image/png")
        }
    };
    let limit = if image {
        arany::MAX_IMAGE_BYTES
    } else {
        MAX_PASTE_BYTES
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

#[cfg(target_os = "linux")]
enum LocalDisplay {
    Wayland {
        runtime: std::ffi::OsString,
        display: String,
    },
    X11 {
        display: String,
        authority: Option<std::ffi::OsString>,
    },
}

#[cfg(target_os = "linux")]
impl LocalDisplay {
    fn detect(var: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Result<Self, &'static str> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        use std::path::Path;
        if let Some(display) = var("WAYLAND_DISPLAY") {
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
                .ok_or("invalid local Wayland display")?
                .to_owned();
            let runtime = var("XDG_RUNTIME_DIR").ok_or("Wayland runtime unavailable")?;
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
            let socket = std::fs::symlink_metadata(path.join(&display))
                .map_err(|_| "Wayland clipboard unavailable")?;
            if !socket.file_type().is_socket() || socket.uid() != metadata.uid() {
                return Err("unsafe Wayland display");
            }
            return Ok(Self::Wayland { runtime, display });
        }
        let display = var("DISPLAY")
            .and_then(|display| display.into_string().ok())
            .ok_or("local clipboard unavailable; use terminal paste")?;
        if !local_x11_display(&display) {
            return Err("remote or invalid X11 clipboard display is not allowed");
        }
        let authority = var("XAUTHORITY").or_else(|| {
            nix::unistd::User::from_uid(nix::unistd::Uid::effective())
                .ok()
                .flatten()
                .map(|user| user.dir.join(".Xauthority").into_os_string())
        });
        let authority = match authority {
            None => None,
            Some(authority) => {
                let path = Path::new(&authority);
                match std::fs::symlink_metadata(path) {
                    Ok(metadata)
                        if path.is_absolute()
                            && metadata.is_file()
                            && metadata.nlink() == 1
                            && metadata.uid() == rustix::process::getuid().as_raw()
                            && metadata.mode() & 0o077 == 0 =>
                    {
                        Some(authority)
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    _ => return Err("unsafe X11 authority file"),
                }
            }
        };
        Ok(Self::X11 { display, authority })
    }

    fn command(&self, client: &str) -> Result<std::process::Command, &'static str> {
        let mut command = std::process::Command::new(installed(client)?);
        self.configure(&mut command);
        Ok(command)
    }

    fn configure(&self, command: &mut std::process::Command) {
        command.env_clear();
        match self {
            Self::Wayland { runtime, display } => {
                command
                    .env("XDG_RUNTIME_DIR", runtime)
                    .env("WAYLAND_DISPLAY", display);
            }
            Self::X11 { display, authority } => {
                command.env("DISPLAY", display).env("HOME", "/");
                if let Some(authority) = authority {
                    command.env("XAUTHORITY", authority);
                }
            }
        }
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
        super::composer::MAX_PASTE_BYTES.max(arany::MAX_IMAGE_BYTES) + 2,
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
        .arg(super::composer::MAX_PASTE_BYTES.to_string());
    Ok(command)
}

#[cfg(target_os = "macos")]
fn native_content(bytes: &[u8]) -> Result<ClipboardContent, &'static str> {
    match bytes {
        [b'P', b'\n', payload @ ..] => {
            ImageAttachment::from_png(payload).map(ClipboardContent::Image)
        }
        [b'T', b'\n', payload @ ..] if payload.len() <= super::composer::MAX_PASTE_BYTES => {
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
struct OwnedChild<'a> {
    child: Option<std::process::Child>,
    group: Option<rustix::process::Pid>,
    cleanup: &'a mut Cleanup,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl OwnedChild<'_> {
    fn spawn<'a>(
        command: &mut std::process::Command,
        cleanup: &'a mut Cleanup,
    ) -> Result<OwnedChild<'a>, &'static str> {
        use std::os::unix::process::CommandExt;
        let child = command
            .current_dir("/")
            .process_group(0)
            .spawn()
            .map_err(|_| "clipboard client unavailable")?;
        let group = child
            .id()
            .try_into()
            .ok()
            .and_then(rustix::process::Pid::from_raw);
        Ok(OwnedChild {
            child: Some(child),
            group,
            cleanup,
        })
    }

    fn kill(&mut self) {
        use rustix::process::{Signal, kill_process_group};
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

    // A serving client's descendant keeps the selection, so only the exited leader is reaped.
    fn release_server(mut self) -> Result<(), &'static str> {
        match self
            .child
            .as_mut()
            .expect("owned clipboard child")
            .try_wait()
        {
            Ok(Some(_)) => {
                self.child = None;
                Ok(())
            }
            _ => self.finish(),
        }
    }

    fn exited(&self) -> Result<Option<bool>, &'static str> {
        use rustix::process::{WaitId, WaitIdOptions, waitid};
        let group = self.group.ok_or("clipboard process unavailable")?;
        // Keep the leader waitable until group cleanup to prevent process-ID reuse.
        waitid(
            WaitId::Pid(group),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )
        .map(|status| status.map(|status| status.exit_status() == Some(0)))
        .map_err(|_| "clipboard process unavailable")
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for OwnedChild<'_> {
    fn drop(&mut self) {
        self.kill();
        if let Some(child) = self.child.take() {
            self.cleanup.child = Some(child);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn set_nonblocking(pipe: &impl std::os::fd::AsFd) -> Result<(), &'static str> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    fcntl_setfl(
        pipe,
        fcntl_getfl(pipe).map_err(|_| "clipboard pipe unavailable")? | OFlags::NONBLOCK,
    )
    .map_err(|_| "clipboard pipe unavailable")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_process(
    command: &mut std::process::Command,
    cancelled: &AtomicBool,
    deadline: std::time::Instant,
    limit: usize,
    cleanup: &mut Cleanup,
) -> Result<Vec<u8>, &'static str> {
    use std::io::{self, Read};
    use std::process::Stdio;

    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        return Err("clipboard read cancelled or timed out");
    }
    let mut owner = OwnedChild::spawn(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
        cleanup,
    )?;
    let result = (|| {
        let child = owner.child.as_mut().expect("owned clipboard child");
        let mut stdout = child.stdout.take().ok_or("clipboard pipe unavailable")?;
        let mut stderr = child.stderr.take().ok_or("clipboard pipe unavailable")?;
        set_nonblocking(&stdout)?;
        set_nonblocking(&stderr)?;
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
            if let Some(success) = owner.exited()?
                && stdout_closed
                && stderr_closed
            {
                return if success {
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

pub(super) const MAX_COPY_BYTES: usize = 100_000;
pub(super) const COPY_TOO_LARGE: &str = "Copy exceeds 100 KB; use arany show";

// The escaped display text and what it names in the copy cue.
pub(super) struct CopyRequest(String, &'static str);

// Bytes of `display_text(text)`, counted without materializing it.
pub(super) fn display_bytes(text: &str) -> usize {
    use crate::presentation::escaped_terminal_bytes;
    text.split(['\n', '\t'])
        .map(|part| escaped_terminal_bytes(part) + 1)
        .sum::<usize>()
        - 1
}

pub(super) fn display_text(text: &str) -> String {
    use crate::presentation::escape_terminal;
    text.split('\n')
        .map(|line| {
            line.split('\t')
                .map(escape_terminal)
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl CopyRequest {
    pub(super) fn latest_answer(view: &arany::SessionView) -> Result<Self, &'static str> {
        Self::answer(
            view.runs
                .iter()
                .rev()
                .find_map(|run| run.assistant_message.as_deref())
                .ok_or("No answer to copy yet")?,
        )
    }

    fn answer(answer: &str) -> Result<Self, &'static str> {
        // Refuse before materializing an escaped copy larger than the limit.
        if display_bytes(answer) > MAX_COPY_BYTES {
            return Err(COPY_TOO_LARGE);
        }
        Self::from_display(display_text(answer), "answer")
    }

    // Selection text is already escaped and bounded by its History owner.
    pub(super) fn selection(text: String) -> Result<Self, &'static str> {
        Self::from_display(text, "selection")
    }

    fn from_display(text: String, label: &'static str) -> Result<Self, &'static str> {
        let text: String = text
            .chars()
            .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
            .collect();
        if text.len() > MAX_COPY_BYTES {
            return Err(COPY_TOO_LARGE);
        }
        Ok(Self(text, label))
    }

    fn osc52(&self) -> Vec<u8> {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let mut sequence = b"\x1b]52;c;".to_vec();
        sequence.extend_from_slice(STANDARD.encode(&self.0).as_bytes());
        sequence.push(0x07);
        sequence
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Destination {
    Native,
    Tmux,
    Osc52,
}

fn destinations(
    var: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    terminal: bool,
) -> Vec<Destination> {
    let remote = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .into_iter()
        .any(|name| var(name).is_some_and(|value| !value.is_empty()));
    [
        (!remote).then_some(Destination::Native),
        var("TMUX")
            .filter(|value| !value.is_empty())
            .map(|_| Destination::Tmux),
        terminal.then_some(Destination::Osc52),
    ]
    .into_iter()
    .flatten()
    .collect()
}

type EnvVars<'a> = &'a dyn Fn(&str) -> Option<std::ffi::OsString>;

// Names the emulator's own selection gesture; tmux and SSH hide the outer emulator.
pub(super) fn native_selection_gesture(var: EnvVars<'_>) -> &'static str {
    let set = |name: &str| var(name).is_some_and(|value| !value.is_empty());
    let hidden = set("TMUX")
        || ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
            .into_iter()
            .any(set);
    match var("TERM_PROGRAM")
        .as_ref()
        .and_then(|value| value.to_str())
    {
        _ if hidden => "Shift-drag (Option in iTerm2, Fn in Terminal.app)",
        Some("Apple_Terminal") => "Fn-drag",
        Some("iTerm.app") => "Option-drag",
        _ => "Shift-drag",
    }
}

pub(super) fn native_selection_hint(var: EnvVars<'_>) -> String {
    let gesture = native_selection_gesture(var);
    if var("TMUX").is_some_and(|value| !value.is_empty()) {
        format!("{gesture} selects natively · tmux wheel scrolling needs `set -g mouse on`")
    } else {
        format!("{gesture} selects natively")
    }
}

// `terminal` is the owner's non-linear stderr; OSC 52 is never written without it.
pub(super) async fn copy(
    request: CopyRequest,
    terminal: Option<&mut dyn std::io::Write>,
) -> Result<String, String> {
    let label = request.1;
    let plan = destinations(&|name| std::env::var_os(name), terminal.is_some());
    let local: Vec<_> = plan
        .iter()
        .copied()
        .filter(|destination| *destination != Destination::Osc52)
        .collect();
    if !local.is_empty() {
        let text = request.0.clone();
        let outcome = match ClipboardJob::start_with(&WORKER, move |cancelled, cleanup| {
            copy_locally(&local, text.as_bytes(), cancelled, cleanup)
        }) {
            Ok(mut job) => job.recv().await,
            Err(error) => Err(error),
        };
        match outcome {
            Ok(destination) => return Ok(format!("Copied {label} to {destination}")),
            Err(CLEANUP_UNCONFIRMED) => {
                return Err(format!("Copy unavailable: {CLEANUP_UNCONFIRMED}"));
            }
            Err(_) => {}
        }
    }
    let Some(output) = terminal else {
        return Err(
            "Copy unavailable: no native or tmux clipboard worked; OSC 52 unavailable in linear mode"
                .into(),
        );
    };
    output
        .write_all(&request.osc52())
        .and_then(|()| output.flush())
        .map_err(|_| "Copy unavailable: terminal output failed".to_owned())?;
    Ok(format!(
        "Copied {label}: sent to terminal (OSC 52); it may be ignored"
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn copy_locally(
    _destinations: &[Destination],
    _payload: &[u8],
    _cancelled: &AtomicBool,
    _cleanup: &mut Cleanup,
) -> Result<&'static str, &'static str> {
    Err("native clipboard copy is unavailable on this platform")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn copy_locally(
    destinations: &[Destination],
    payload: &[u8],
    cancelled: &AtomicBool,
    cleanup: &mut Cleanup,
) -> Result<&'static str, &'static str> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let var = |name: &str| std::env::var_os(name);
    let mut failure = "no native or tmux clipboard worked";
    for destination in destinations {
        let attempt = match destination {
            Destination::Native => native_writer(&var).and_then(|(mut command, label, serving)| {
                write_process(&mut command, payload, cancelled, deadline, serving, cleanup)
                    .map(|()| label)
            }),
            Destination::Tmux => var("TMUX")
                .ok_or("tmux unavailable")
                .and_then(|value| tmux_writer(&value))
                .and_then(|(mut command, socket)| {
                    socket.unchanged()?;
                    write_process(&mut command, payload, cancelled, deadline, false, cleanup)?;
                    socket.unchanged().map(|()| "tmux buffer")
                }),
            Destination::Osc52 => continue,
        };
        match attempt {
            Ok(label) => return Ok(label),
            Err(CLEANUP_UNCONFIRMED) => return Err(CLEANUP_UNCONFIRMED),
            Err(error) => failure = error,
        }
    }
    Err(failure)
}

#[cfg(target_os = "linux")]
fn native_writer(
    var: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<(std::process::Command, &'static str, bool), &'static str> {
    let display = LocalDisplay::detect(var)?;
    let (client, label, args): (_, _, &[&str]) = match display {
        LocalDisplay::Wayland { .. } => (
            "wl-copy",
            "clipboard (wl-copy)",
            &["--type", "text/plain;charset=utf-8"],
        ),
        // xsel is not admitted: its daemon calls setsid() and leaves the owned process group.
        LocalDisplay::X11 { .. } => (
            "xclip",
            "clipboard (xclip)",
            &["-selection", "clipboard", "-in"],
        ),
    };
    let mut command = display.command(client)?;
    command.args(args);
    Ok((command, label, true))
}

#[cfg(target_os = "macos")]
fn native_writer(
    _var: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<(std::process::Command, &'static str, bool), &'static str> {
    let mut command = std::process::Command::new(installed("pbcopy")?);
    // pbcopy decodes stdin by locale.
    command.env_clear().env("LANG", "en_US.UTF-8");
    Ok((command, "clipboard (pbcopy)", false))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct TmuxSocket {
    path: std::path::PathBuf,
    identity: (u64, u64),
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl TmuxSocket {
    fn admit(value: &std::ffi::OsStr) -> Result<Self, &'static str> {
        use std::os::unix::{
            ffi::OsStrExt,
            fs::{FileTypeExt, MetadataExt},
        };
        const UNSAFE: &str = "unsafe tmux socket";
        let path = std::path::Path::new(std::ffi::OsStr::from_bytes(
            value
                .as_bytes()
                .split(|byte| *byte == b',')
                .next()
                .unwrap_or_default(),
        ));
        let parent = path.parent().filter(|_| path.is_absolute()).ok_or(UNSAFE)?;
        // A canonical parent has no link components, so these checks cover the directories tmux reaches.
        if std::fs::canonicalize(parent).ok().as_deref() != Some(parent) {
            return Err(UNSAFE);
        }
        let uid = rustix::process::getuid().as_raw();
        for (depth, ancestor) in parent.ancestors().enumerate() {
            let metadata = std::fs::symlink_metadata(ancestor).map_err(|_| UNSAFE)?;
            let mode = metadata.mode();
            // Only root or the user may rename entries; a sticky directory protects entries it does not own.
            let replaceable = mode & 0o022 != 0 && mode & 0o1000 == 0;
            let owned = metadata.uid() == uid || metadata.uid() == 0;
            let private = metadata.uid() == uid && mode & 0o077 == 0;
            if !metadata.is_dir() || !owned || replaceable || (depth == 0 && !private) {
                return Err(UNSAFE);
            }
        }
        let socket = std::fs::symlink_metadata(path).map_err(|_| UNSAFE)?;
        if !socket.file_type().is_socket() || socket.uid() != uid {
            return Err(UNSAFE);
        }
        Ok(Self {
            path: path.to_owned(),
            identity: (socket.dev(), socket.ino()),
        })
    }

    fn unchanged(&self) -> Result<(), &'static str> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        std::fs::symlink_metadata(&self.path)
            .is_ok_and(|socket| {
                socket.file_type().is_socket() && (socket.dev(), socket.ino()) == self.identity
            })
            .then_some(())
            .ok_or("tmux socket changed during copy")
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn tmux_writer(
    value: &std::ffi::OsStr,
) -> Result<(std::process::Command, TmuxSocket), &'static str> {
    let socket = TmuxSocket::admit(value)?;
    let mut command = std::process::Command::new(installed("tmux")?);
    command
        .env_clear()
        .arg("-S")
        .arg(&socket.path)
        .args(["load-buffer", "-w", "-"]);
    Ok((command, socket))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_process(
    command: &mut std::process::Command,
    payload: &[u8],
    cancelled: &AtomicBool,
    deadline: Instant,
    serving: bool,
    cleanup: &mut Cleanup,
) -> Result<(), &'static str> {
    use std::io::{ErrorKind, Write};
    use std::process::Stdio;

    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        return Err("clipboard copy cancelled or timed out");
    }
    // A serving descendant inherits no pipe, so its lifetime never holds the copy open.
    let mut owner = OwnedChild::spawn(
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
        cleanup,
    )?;
    let result = (|| {
        let mut stdin = owner
            .child
            .as_mut()
            .expect("owned clipboard child")
            .stdin
            .take();
        set_nonblocking(stdin.as_ref().ok_or("clipboard pipe unavailable")?)?;
        let mut written = 0;
        loop {
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err("clipboard copy cancelled or timed out");
            }
            if written == payload.len() {
                stdin = None;
            }
            if let Some(pipe) = stdin.as_mut() {
                match pipe.write(&payload[written..]) {
                    Ok(length) => written += length,
                    Err(error)
                        if matches!(
                            error.kind(),
                            ErrorKind::WouldBlock | ErrorKind::Interrupted
                        ) => {}
                    Err(_) => return Err("clipboard copy unavailable"),
                }
            }
            if let Some(success) = owner.exited()? {
                return if success && stdin.is_none() {
                    Ok(())
                } else {
                    Err("clipboard copy unavailable")
                };
            }
            std::thread::park_timeout(Duration::from_millis(5));
        }
    })();
    if serving && result.is_ok() {
        owner.release_server()?;
    } else {
        owner.finish()?;
    }
    result
}

#[cfg(test)]
pub(super) mod cleanup_tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[cfg(unix)]
    pub(in crate::terminal) fn released_terminal(
        linear: Option<crate::terminal::linear::LinearState>,
    ) -> super::super::AttachedTerminal {
        super::super::AttachedTerminal {
            waiting_tool: None,
            terminal: None,
            mode: super::super::TerminalMode::Classic,
            classic_pinned: false,
            alternate: None,
            transcript_open: false,
            scrollback_dump: false,
            native: Default::default(),
            active_run: None,
            history: Default::default(),
            classic_history: Default::default(),
            dialog_armed: false,
            history_adjacent: true,
            input: None,
            _raw_mode: None,
            pointer: Default::default(),
            pointer_modes: None,
            history_area: None,
            selection_cue: None,
            bracketed_paste: None,
            session_picker_area: None,
            agent_picker_area: None,
            linear,
            kept_tools: None,
            secret_echo: None,
            agent_panel: None,
            agent_chat: None,
            child_drafts: Vec::new(),
            primary_input_capacity: crate::terminal::MAX_DRAFT_BYTES,
            help_selected: None,
            clipboard: None,
            clipboard_paste: None,
            deferred_input: None,
            clipboard_preview: false,
            shutdown_clipboard_on_drop: true,
            requested_screen_reader: false,
            color_enabled: false,
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .unwrap(),
            suspend: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(
                nix::libc::SIGTSTP,
            ))
            .unwrap(),
            hangup: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).unwrap(),
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .unwrap(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn written_tool_rows_survive_release_and_reacquisition() {
        use crate::terminal::linear::LinearState;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let written = Some((arany::RunId::new(), 3));
                let mut linear = LinearState::new("screen-reader");
                linear.keep_written_tools(written);
                let mut terminal = released_terminal(Some(linear));
                terminal.release_for_output().unwrap();
                terminal
                    .replace_released(released_terminal(Some(LinearState::new("screen-reader"))));
                assert_eq!(terminal.linear.as_ref().unwrap().written_tools(), written);
            });
    }

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
                    let released = || released_terminal(None);
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

            writing.recv_timeout(CLEANUP_TIMEOUT).unwrap();
            // Keep the warning's pipe full while exercising the normal error reporter.
            let mut sink = tempfile::tempfile().unwrap();
            nix::unistd::dup2_stderr(&sink).unwrap();
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

#[cfg(test)]
mod copy_tests {
    use super::*;
    use std::ffi::OsString;

    type Vars<'a> = &'a [(&'a str, &'a str)];

    fn env<'a>(vars: Vars<'a>) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.into())
        }
    }

    #[test]
    fn copy_destinations_try_native_then_tmux_then_the_terminal_once_each() {
        use Destination::{Native, Osc52, Tmux};
        let cases: [(Vars, bool, &[Destination]); 8] = [
            (&[], true, &[Native, Osc52]),
            (&[("TMUX", "/t/default,1,0")], true, &[Native, Tmux, Osc52]),
            (
                &[("SSH_CONNECTION", "10.0.0.1 1 10.0.0.2 22")],
                true,
                &[Osc52],
            ),
            (
                &[("SSH_TTY", "/dev/pts/1"), ("TMUX", "/t/default,1,0")],
                true,
                &[Tmux, Osc52],
            ),
            (&[("SSH_CLIENT", "10.0.0.1 1 22")], false, &[]),
            (&[("SSH_CONNECTION", ""), ("TMUX", "")], false, &[Native]),
            (
                &[("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")],
                false,
                &[Native],
            ),
            (&[("TMUX", "/t/default,1,0")], false, &[Native, Tmux]),
        ];
        for (vars, terminal, expected) in cases {
            assert_eq!(destinations(&env(vars), terminal), expected, "{vars:?}");
        }
    }

    #[test]
    fn copy_payload_is_sanitized_display_text_within_the_limit() {
        assert_eq!(
            CopyRequest::answer("tab\there\n\u{1b}]52;c;aGk=\u{7}\r\u{202e}x")
                .unwrap()
                .0,
            "tab\there\n\\u{001b}]52;c;aGk=\\u{0007}\\u{000d}\\u{202e}x"
        );
        for answer in ["", "\t", "\n\t\t\n", "\tfirst\t\n\tlast\t"] {
            assert_eq!(CopyRequest::answer(answer).unwrap().0, answer);
        }
        assert_eq!(
            CopyRequest::selection("a\u{1b}b\u{9b}c\u{7}\r\n\td\u{7f}".into())
                .unwrap()
                .0,
            "abc\n\td"
        );
        assert_eq!(
            CopyRequest("hé\n".into(), "answer").osc52(),
            b"\x1b]52;c;aMOpCg==\x07"
        );
        let x = |count| "x".repeat(count);
        for (answer, accepted) in [
            (x(MAX_COPY_BYTES), true),
            (x(MAX_COPY_BYTES + 1), false),
            (format!("{}\u{1b}", x(MAX_COPY_BYTES - 8)), true),
            (format!("{}\u{1b}", x(MAX_COPY_BYTES - 7)), false),
            (format!("{}\n{}", x(50_000), x(49_999)), true),
            (format!("{}\n{}", x(50_000), x(50_000)), false),
            (format!("{}\t{}", x(50_000), x(49_999)), true),
            (format!("{}\t{}", x(50_000), x(50_000)), false),
        ] {
            let request = CopyRequest::answer(&answer);
            assert_eq!(request.is_ok(), accepted, "{} bytes", answer.len());
            if !accepted {
                assert_eq!(request.err(), Some(COPY_TOO_LARGE));
            }
        }
        assert_eq!(
            CopyRequest::selection(x(MAX_COPY_BYTES + 1)).err(),
            Some(COPY_TOO_LARGE)
        );
    }

    #[test]
    fn native_selection_hint_follows_the_emulator_and_names_tmux_mouse() {
        let cases: [(Vars, &str); 6] = [
            (
                &[("TERM_PROGRAM", "Apple_Terminal")],
                "Fn-drag selects natively",
            ),
            (
                &[("TERM_PROGRAM", "iTerm.app")],
                "Option-drag selects natively",
            ),
            (&[("TERM_PROGRAM", "vscode")], "Shift-drag selects natively"),
            (&[], "Shift-drag selects natively"),
            (
                &[
                    ("TERM_PROGRAM", "iTerm.app"),
                    ("SSH_CONNECTION", "10.0.0.1 1 10.0.0.2 22"),
                ],
                "Shift-drag (Option in iTerm2, Fn in Terminal.app) selects natively",
            ),
            (
                &[("TERM_PROGRAM", "tmux"), ("TMUX", "/t/default,1,0")],
                "Shift-drag (Option in iTerm2, Fn in Terminal.app) selects natively · tmux wheel scrolling needs `set -g mouse on`",
            ),
        ];
        for (vars, expected) in cases {
            assert_eq!(native_selection_hint(&env(vars)), expected, "{vars:?}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_copy_clients_receive_only_admitted_display_variables() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        std::fs::create_dir(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = std::fs::canonicalize(runtime).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(runtime.join("wayland-1")).unwrap();
        let authority = root.path().join("authority");
        std::fs::write(&authority, b"").unwrap();
        std::fs::set_permissions(&authority, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (runtime, authority) = (runtime.to_str().unwrap(), authority.to_str().unwrap());
        let missing = root.path().join("missing");
        let missing = missing.to_str().unwrap();
        let unrelated = [
            ("HOME", "/home/user"),
            ("SSH_AUTH_SOCK", "/agent"),
            ("PATH", "/bin"),
        ];
        let wayland = format!("WAYLAND_DISPLAY=wayland-1\nXDG_RUNTIME_DIR={runtime}");
        let x11 = format!("DISPLAY=:0\nHOME=/\nXAUTHORITY={authority}");
        let cases: [(Vec<_>, Option<&str>); 6] = [
            (
                vec![
                    ("WAYLAND_DISPLAY", "wayland-1"),
                    ("XDG_RUNTIME_DIR", runtime),
                    ("DISPLAY", ":0"),
                ],
                Some(&wayland),
            ),
            (
                vec![("DISPLAY", ":0"), ("XAUTHORITY", authority)],
                Some(&x11),
            ),
            (
                vec![("DISPLAY", ":0"), ("XAUTHORITY", missing)],
                Some("DISPLAY=:0\nHOME=/"),
            ),
            (vec![("DISPLAY", "remote:0"), ("XAUTHORITY", missing)], None),
            (
                vec![("WAYLAND_DISPLAY", "../x"), ("XDG_RUNTIME_DIR", runtime)],
                None,
            ),
            (vec![], None),
        ];
        for (mut vars, expected) in cases {
            vars.extend(unrelated);
            let detected = LocalDisplay::detect(&env(&vars));
            assert_eq!(detected.is_ok(), expected.is_some(), "{vars:?}");
            let (Ok(display), Some(expected)) = (detected, expected) else {
                continue;
            };
            let mut command = std::process::Command::new("/usr/bin/env");
            display.configure(&mut command);
            let output = command.output().unwrap();
            let mut seen: Vec<_> = std::str::from_utf8(&output.stdout)
                .unwrap()
                .lines()
                .collect();
            seen.sort_unstable();
            assert_eq!(seen.join("\n"), expected, "{vars:?}");
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn tmux_copy_requires_an_absolute_private_socket_under_unreplaceable_ancestors() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let fixture = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(fixture.path()).unwrap();
        let private = root.join("private");
        let shared = root.join("shared");
        let open = root.join("open");
        for directory in [&private, &shared, &open, &open.join("private")] {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(directory)
                .unwrap();
        }
        let bind = |path: std::path::PathBuf| std::os::unix::net::UnixListener::bind(path).unwrap();
        let _listeners = [
            bind(private.join("default")),
            bind(shared.join("default")),
            bind(open.join("private/default")),
            // Bound while the original exists, so the substitute cannot reuse its inode.
            bind(private.join("substitute")),
        ];
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(private.join("file"), b"").unwrap();
        let value = |path: &std::path::Path| OsString::from(format!("{},4242,0", path.display()));
        let admitted =
            |path: &std::path::Path| TmuxSocket::admit(&value(path)).map(|socket| socket.path);
        let socket = private.join("default");
        assert_eq!(admitted(&socket), Ok(socket.clone()));
        for rejected in [
            OsString::from("default,4242,0"),
            OsString::new(),
            value(std::path::Path::new("/")),
            value(&shared.join("default")),
            value(&private.join("file")),
            value(&private.join("missing")),
            value(&private.join("../private/default")),
        ] {
            assert_eq!(
                TmuxSocket::admit(&rejected).map(|socket| socket.path),
                Err("unsafe tmux socket"),
                "{rejected:?}"
            );
        }

        // Another user could rename `open/private` away and substitute their own socket.
        let nested = open.join("private/default");
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(admitted(&nested), Err("unsafe tmux socket"));
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o1777)).unwrap();
        assert_eq!(admitted(&nested), Ok(nested.clone()));

        let admitted = TmuxSocket::admit(&value(&socket)).unwrap();
        assert_eq!(admitted.unchanged(), Ok(()));
        std::fs::rename(private.join("substitute"), &socket).unwrap();
        assert_eq!(admitted.unchanged(), Err("tmux socket changed during copy"));

        if let Ok((command, _)) = tmux_writer(&value(&nested)) {
            let args: Vec<_> = command.get_args().collect();
            assert_eq!(
                args,
                [
                    std::ffi::OsStr::new("-S"),
                    nested.as_os_str(),
                    "load-buffer".as_ref(),
                    "-w".as_ref(),
                    "-".as_ref(),
                ]
            );
            assert_eq!(command.get_envs().count(), 0);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn copy_client_receives_the_whole_payload_and_a_serving_descendant_survives() {
        use rustix::process::{Pid, Signal, kill_process, test_kill_process};
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("payload");
        let payload: Vec<u8> = (0..MAX_COPY_BYTES)
            .map(|index| b'a' + (index % 26) as u8)
            .collect();
        let deadline = || Instant::now() + Duration::from_secs(5);
        let mut command = std::process::Command::new("/bin/sh");
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .args(["-c", "cat > \"$0\"; sleep 30 & echo $! > \"$0.pid\""])
            .arg(&output);
        let mut cleanup = Cleanup::default();
        let result = write_process(
            &mut command,
            &payload,
            &AtomicBool::new(false),
            deadline(),
            true,
            &mut cleanup,
        );
        let server = std::fs::read_to_string(root.path().join("payload.pid"))
            .ok()
            .and_then(|pid| pid.trim().parse().ok())
            .and_then(Pid::from_raw);
        let alive = server.is_some_and(|pid| test_kill_process(pid).is_ok());
        if let Some(pid) = server {
            let _ = kill_process(pid, Signal::KILL);
        }
        assert_eq!(result, Ok(()));
        assert!(cleanup.child.is_none(), "exited leader was not reaped");
        assert!(alive, "serving descendant did not survive the copy");
        assert!(
            std::fs::read(&output).unwrap() == payload,
            "payload changed"
        );

        let mut failing = std::process::Command::new("/bin/sh");
        failing
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .args(["-c", "cat > /dev/null; exit 3"]);
        assert_eq!(
            write_process(
                &mut failing,
                b"text",
                &AtomicBool::new(false),
                deadline(),
                true,
                &mut cleanup,
            ),
            Err("clipboard copy unavailable")
        );
        assert!(cleanup.child.is_none());
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
    fn pbcopy_receives_only_a_utf8_locale() {
        let (command, label, serving) = native_writer(&|_| Some("ignored".into())).unwrap();
        assert_eq!(command.get_program(), "/usr/bin/pbcopy");
        assert_eq!(
            command.get_envs().collect::<Vec<_>>(),
            [(
                std::ffi::OsStr::new("LANG"),
                Some(std::ffi::OsStr::new("en_US.UTF-8"))
            )]
        );
        assert_eq!((label, serving), ("clipboard (pbcopy)", false));
    }

    #[test]
    fn native_clipboard_frames_keep_image_and_text_admission_closed() {
        for bytes in [b"".as_slice(), b"X\ntext", b"T\n\xff", b"P\ninvalid"] {
            assert!(native_content(bytes).is_err());
        }
        let mut oversized = b"T\n".to_vec();
        oversized.extend(vec![b'x'; super::super::composer::MAX_PASTE_BYTES + 1]);
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
                vec![b'x'; super::super::composer::MAX_PASTE_BYTES + 1],
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

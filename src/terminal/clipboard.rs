use crate::provider::ImageAttachment;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use tokio::sync::oneshot;

pub(super) enum ClipboardContent {
    Text(String),
    Image(ImageAttachment),
}

pub(super) struct ClipboardRead {
    result: oneshot::Receiver<Result<ClipboardContent, &'static str>>,
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ClipboardRead {
    pub(super) fn start() -> Result<Self, &'static str> {
        let (sender, result) = oneshot::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancelled);
        let worker = std::thread::Builder::new()
            .name("arany-clipboard-read".into())
            .spawn(move || {
                let _ = sender.send(read(&stop));
            })
            .map_err(|_| "clipboard worker unavailable")?;
        Ok(Self {
            result,
            cancelled,
            worker: Some(worker),
        })
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
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read(_cancelled: &AtomicBool) -> Result<ClipboardContent, &'static str> {
    Err("explicit clipboard access is unavailable on this platform")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read(cancelled: &AtomicBool) -> Result<ClipboardContent, &'static str> {
    use super::composer::MAX_DRAFT_BYTES;
    use std::process::Command;
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(2);
    #[cfg(target_os = "linux")]
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
            let types = run_process(&mut command, cancelled, deadline, 16 * 1024)?;
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
            let types = run_process(&mut command, cancelled, deadline, 16 * 1024)?;
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
    #[cfg(target_os = "macos")]
    let (mut command, image) = {
        let mut command = Command::new(installed("pbpaste")?);
        command.env_clear().env("LANG", "en_US.UTF-8");
        (command, false)
    };
    let limit = if image {
        crate::provider::MAX_IMAGE_BYTES
    } else {
        MAX_DRAFT_BYTES
    };
    let bytes = run_process(&mut command, cancelled, deadline, limit)?;
    if image {
        ImageAttachment::from_png(&bytes).map(ClipboardContent::Image)
    } else {
        String::from_utf8(bytes)
            .map(ClipboardContent::Text)
            .map_err(|_| "clipboard text is not valid UTF-8")
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
    for root in ["/usr/bin", "/usr/local/bin", "/run/current-system/sw/bin"] {
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
) -> Result<Vec<u8>, &'static str> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};
    use std::io::{self, Read};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};

    struct OwnedChild {
        child: Child,
        group: Option<Pid>,
    }
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if let Some(group) = self.group {
                let _ = kill_process_group(group, Signal::KILL);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
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
    let mut owner = OwnedChild { child, group };
    let group = owner.group.ok_or("clipboard process unavailable")?;
    let mut stdout = owner
        .child
        .stdout
        .take()
        .ok_or("clipboard pipe unavailable")?;
    let mut stderr = owner
        .child
        .stderr
        .take()
        .ok_or("clipboard pipe unavailable")?;
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
}

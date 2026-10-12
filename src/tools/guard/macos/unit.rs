use super::super::{BootstrapFailure, GuardStage, Task, read_line_capped};
use super::storage::{cleanup_storage, prepare_command_storage, storage_mounted};
use super::{
    CPU_LIMIT, PREPARATION_DEADLINE, SUPERVISION_INTERVAL, bootstrap, ffi, system_program,
};
use crate::engine::RunCancellation;
use crate::private_fs::{PrivateDirectory, open_directory};
use crate::tools::types::{MAX_PROCESSES_OR_THREADS, MEMORY_BYTES};
use crate::tools::{EffectIntent, ToolError, ToolObservation, ToolRuntime};
use core_foundation::array::CFArray;
use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::propertylist::{create_data, kCFPropertyListBinaryFormat_v1_0};
use core_foundation::string::CFString;
use nix::libc;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::Command;

const HOST_PROCESS_CAP: usize = 65_536;
const JOB_BYTES_CAP: usize = 128 * 1024;
const AVAILABILITY_DEADLINE: Duration = Duration::from_secs(3);
const CONNECT_DEADLINE: Duration = Duration::from_secs(5);
const UNIT_ROOT_PARENT: &str = "/private/tmp";
const UNIT_ROOT_PREFIX: &str = "arany-u-";
// A live unit holds an exclusive lock here; the kernel releases it when the owner exits.
const UNIT_LIVENESS_LOCK: &str = "unit.lock";
// Each retained root holds two 64 MiB images; eight bounds retained storage to 1 GiB.
const RETAINED_ROOT_LIMIT: usize = 8;
// Bounds the directory scan itself; an unverifiable listing fails closed.
const UNIT_ROOT_SCAN_LIMIT: usize = 4096;

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    pid: u32,
    version: u32,
    unique_id: u64,
    coalition: u64,
}

fn identity(pid: u32) -> Result<Identity, ToolError> {
    let process = ffi::process_info(pid)?;
    Ok(Identity {
        pid,
        version: process.version as u32,
        unique_id: process.unique_id,
        coalition: ffi::coalition(pid)?,
    })
}

pub(super) fn check_available() -> Result<(), ToolError> {
    Scope::new(identity(std::process::id())?.coalition)?;
    // Admission can run inside Tokio; this bounded probe owns a separate reactor.
    std::thread::Builder::new()
        .spawn(|| {
            let reactor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| ToolError::ProtectionUnavailable)?;
            reactor.block_on(async {
                let mut child = Command::new("/bin/launchctl")
                    .args([
                        "print",
                        &format!("user/{}", rustix::process::geteuid().as_raw()),
                    ])
                    .env_clear()
                    .current_dir("/")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|_| ToolError::ProtectionUnavailable)?;
                let result = tokio::time::timeout(
                    AVAILABILITY_DEADLINE,
                    super::super::reap_owned_child(&mut child),
                )
                .await;
                if matches!(result, Ok(Ok(status)) if status.success()) {
                    return Ok(());
                }
                let _ = child.start_kill();
                let _ = tokio::time::timeout(
                    Duration::from_secs(1),
                    super::super::reap_owned_child(&mut child),
                )
                .await;
                Err(ToolError::ProtectionUnavailable)
            })
        })
        .map_err(|_| ToolError::ProtectionUnavailable)?
        .join()
        .unwrap_or(Err(ToolError::ProtectionUnavailable))
}

impl Identity {
    fn same_process(self, other: Self) -> bool {
        self.pid == other.pid
            && self.unique_id == other.unique_id
            && self.coalition == other.coalition
    }

    fn signal(self, signal: i32) -> Result<(), ToolError> {
        ffi::signal(self.pid, self.version, signal)
    }
}

pub(super) struct Scope {
    coalition: u64,
    pids: Vec<i32>,
    timebase: ffi::Timebase,
}

impl Scope {
    fn new(coalition: u64) -> Result<Self, ToolError> {
        if coalition <= 1 {
            return Err(ToolError::ProtectionUnavailable);
        }
        let timebase = ffi::timebase()?;
        if timebase.numer == 0 || timebase.denom == 0 {
            return Err(ToolError::ProtectionUnavailable);
        }
        let maximum = ffi::max_processes()?;
        if maximum < 1 || maximum as usize >= HOST_PROCESS_CAP {
            return Err(ToolError::ProtectionUnavailable);
        }
        Ok(Self {
            coalition,
            pids: vec![0; maximum as usize + 1],
            timebase,
        })
    }

    fn members(&mut self) -> Result<Vec<Identity>, ToolError> {
        let mut members = Vec::new();
        for pid in ffi::list_pids(&mut self.pids)? {
            if *pid <= 1 {
                continue;
            }
            match ffi::coalition(*pid as u32) {
                Ok(coalition) if coalition != self.coalition => continue,
                Err(ToolError::ChangedInput) => continue,
                Err(error) => return Err(error),
                Ok(_) => {}
            }
            let first = match identity(*pid as u32) {
                Ok(first) => first,
                Err(ToolError::ChangedInput) => continue,
                Err(error) => return Err(error),
            };
            if first.coalition == self.coalition {
                match identity(first.pid) {
                    Ok(second) if first.same_process(second) => members.push(second),
                    Ok(second) if first.unique_id != second.unique_id => {}
                    Err(ToolError::ChangedInput) => {}
                    _ => return Err(ToolError::ProtectionUnavailable),
                }
            }
        }
        Ok(members)
    }

    pub(super) fn check_resources(&mut self) -> Result<(), ToolError> {
        let mut memory = 0_u64;
        let mut threads = 0_u64;
        for member in self.members()? {
            let sample = (|| {
                let task = match ffi::task_info(member.pid) {
                    Ok(task) => task,
                    Err(_) if !super::check_process_resources(member.pid)? => return Ok(None),
                    Err(error) => return Err(error),
                };
                Ok(Some((task, ffi::rusage(member.pid)?)))
            })();
            // Revalidate even failed samples: a reaped member has no remaining task metadata.
            match identity(member.pid) {
                Ok(current) if current.same_process(member) => {}
                Ok(current) if current.unique_id != member.unique_id => continue,
                Err(ToolError::ChangedInput) => continue,
                _ => return Err(ToolError::ProtectionUnavailable),
            }
            let Some((task, usage)) = sample? else {
                continue;
            };
            if task.pti_threadnum < 1 {
                return Err(ToolError::ProtectionUnavailable);
            }
            memory = memory
                .checked_add(task.pti_resident_size)
                .ok_or(ToolError::Limit)?;
            threads = threads
                .checked_add(task.pti_threadnum as u64)
                .ok_or(ToolError::Limit)?;
            if memory > MEMORY_BYTES
                || threads > u64::from(MAX_PROCESSES_OR_THREADS)
                || (u128::from(usage.ri_user_time) + u128::from(usage.ri_system_time))
                    * u128::from(self.timebase.numer)
                    / u128::from(self.timebase.denom)
                    > CPU_LIMIT.as_nanos()
            {
                return Err(ToolError::Limit);
            }
        }
        Ok(())
    }

    pub(super) async fn stop(&mut self, except: Option<u32>) -> Result<(), ToolError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let members = self.members()?;
            let members: Vec<_> = members
                .into_iter()
                .filter(|member| Some(member.pid) != except)
                .collect();
            if members.is_empty() {
                return Ok(());
            }
            for member in &members {
                member.signal(libc::SIGSTOP)?;
            }
            for member in &members {
                member.signal(libc::SIGKILL)?;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ToolError::ProtectionUnavailable);
            }
            tokio::time::sleep(SUPERVISION_INTERVAL).await;
        }
    }
}

fn peer(stream: &UnixStream) -> Result<u32, ToolError> {
    let (pid, credentials) = ffi::peer_credentials(stream)?;
    if pid <= 1
        || credentials.cr_version != 0
        || credentials.cr_uid != rustix::process::geteuid().as_raw()
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(pid as u32)
}

struct Unit {
    root: PathBuf,
    _directory: PrivateDirectory,
    _liveness: cap_std::fs::File,
    domain: String,
    label: String,
    scope: Option<Scope>,
    stopped: bool,
}

enum ManagerAction<'a> {
    Bootstrap(&'a Path),
    Kickstart,
    Bootout,
}

// Retained roots are never reclaimed automatically; counting is the only bound until
// positive ownership recovery exists.
fn check_retained_roots(parent: &Path, owner: u32) -> Result<(), ToolError> {
    let mut retained = 0;
    for (scanned, entry) in std::fs::read_dir(parent)
        .map_err(|_| ToolError::ProtectionUnavailable)?
        .enumerate()
    {
        if scanned == UNIT_ROOT_SCAN_LIMIT {
            return Err(ToolError::ProtectionUnavailable);
        }
        let entry = entry.map_err(|_| ToolError::ProtectionUnavailable)?;
        if !entry
            .file_name()
            .as_encoded_bytes()
            .starts_with(UNIT_ROOT_PREFIX.as_bytes())
        {
            continue;
        }
        match std::fs::symlink_metadata(entry.path()) {
            Ok(metadata) if metadata.uid() == owner && is_retained(&entry.path())? => {
                retained += 1;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(ToolError::ProtectionUnavailable),
        }
    }
    if retained >= RETAINED_ROOT_LIMIT {
        return Err(ToolError::RetainedRoots);
    }
    Ok(())
}

// An unlocked liveness lock was left by a finished or crashed owner. A root without the
// lock is still being created or was abandoned before it could hold storage.
fn is_retained(root: &Path) -> Result<bool, ToolError> {
    use std::os::unix::fs::OpenOptionsExt;
    let lock = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(root.join(UNIT_LIVENESS_LOCK))
    {
        Ok(lock) => lock,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(false);
        }
        Err(_) => return Err(ToolError::ProtectionUnavailable),
    };
    match rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockShared) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(false),
        Err(_) => Err(ToolError::ProtectionUnavailable),
    }
}

fn hold_liveness_lock(directory: &PrivateDirectory) -> Result<cap_std::fs::File, ToolError> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
    use cap_std::fs::OpenOptionsExt;
    let mut options = cap_std::fs::OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No)
        // Creating and locking atomically keeps admission from seeing an unlocked live root.
        .custom_flags(libc::O_EXLOCK | libc::O_NONBLOCK)
        .mode(0o600);
    directory
        .dir
        .open_with(UNIT_LIVENESS_LOCK, &options)
        .map_err(|_| ToolError::Operation)
}

impl Unit {
    fn new() -> Result<Self, ToolError> {
        check_retained_roots(
            Path::new(UNIT_ROOT_PARENT),
            rustix::process::geteuid().as_raw(),
        )?;
        let name = format!("{UNIT_ROOT_PREFIX}{}", uuid::Uuid::now_v7());
        let root = Path::new(UNIT_ROOT_PARENT).join(&name);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .map_err(|_| ToolError::Operation)?;
        let directory = match open_directory(&root, false) {
            Ok(directory) => directory,
            Err(_) => {
                let _ = std::fs::remove_dir(&root);
                return Err(ToolError::Path);
            }
        };
        let liveness = match hold_liveness_lock(&directory) {
            Ok(liveness) => liveness,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&root);
                return Err(error);
            }
        };
        Ok(Self {
            root,
            _directory: directory,
            _liveness: liveness,
            domain: format!("user/{}", rustix::process::geteuid().as_raw()),
            label: format!("dev.arany.tool.{name}"),
            scope: None,
            stopped: false,
        })
    }

    fn target(&self) -> String {
        format!("{}/{}", self.domain, self.label)
    }

    fn write_job(&self, arguments: &[&str], stderr: &str) -> Result<PathBuf, ToolError> {
        if arguments.len() > 16
            || arguments
                .iter()
                .copied()
                .chain(std::iter::once(stderr))
                .any(|value| value.len() > 4096 || value.contains('\0'))
        {
            return Err(ToolError::Limit);
        }
        let arguments: Vec<_> = arguments.iter().map(|value| CFString::new(value)).collect();
        let entries = [
            ("Label", CFString::new(&self.label).as_CFType()),
            (
                "ProgramArguments",
                CFArray::from_CFTypes(&arguments).as_CFType(),
            ),
            (
                "LimitLoadToSessionType",
                CFString::new("Background").as_CFType(),
            ),
            ("RunAtLoad", CFBoolean::true_value().as_CFType()),
            ("LaunchOnlyOnce", CFBoolean::true_value().as_CFType()),
            ("KeepAlive", CFBoolean::false_value().as_CFType()),
            ("AbandonProcessGroup", CFBoolean::false_value().as_CFType()),
            ("ExitTimeOut", CFNumber::from(2_i64).as_CFType()),
            ("WorkingDirectory", CFString::new("/").as_CFType()),
            ("StandardOutPath", CFString::new(stderr).as_CFType()),
            ("StandardErrorPath", CFString::new(stderr).as_CFType()),
        ]
        .map(|(key, value)| (CFString::new(key), value));
        let dictionary = CFDictionary::from_CFType_pairs(&entries);
        let data = create_data(dictionary.as_CFTypeRef(), kCFPropertyListBinaryFormat_v1_0)
            .map_err(|_| ToolError::ProtectionUnavailable)?;
        if data.bytes().len() > JOB_BYTES_CAP {
            return Err(ToolError::Limit);
        }
        let path = self.root.join("unit.plist");
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .map_err(|_| ToolError::Operation)?;
        file.write_all(data.bytes())
            .map_err(|_| ToolError::Operation)?;
        Ok(path)
    }

    async fn manager(&self, action: ManagerAction<'_>) -> Result<Vec<u8>, ToolError> {
        if !system_program("/bin/launchctl")? {
            return Err(ToolError::ProtectionUnavailable);
        }
        let bootout = matches!(action, ManagerAction::Bootout);
        let mut command = Command::new("/bin/launchctl");
        command.env_clear().current_dir("/");
        match action {
            ManagerAction::Bootstrap(path) => {
                command.arg("bootstrap").arg(&self.domain).arg(path);
            }
            ManagerAction::Kickstart => {
                command.args(["kickstart", "-p", &self.target()]);
            }
            ManagerAction::Bootout => {
                command.args(["bootout", &self.target()]);
            }
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let (status, stdout, _) = super::super::capture(
            command
                .spawn()
                .map_err(|_| ToolError::ProtectionUnavailable)?,
            4096,
            Duration::from_secs(3),
        )
        .await?;
        if status.success() || (bootout && status.code() == Some(libc::ESRCH)) {
            Ok(stdout)
        } else {
            Err(ToolError::ProtectionUnavailable)
        }
    }

    async fn connect(
        &mut self,
        runtime: &ToolRuntime,
    ) -> Result<tokio::net::UnixStream, ToolError> {
        let socket = self.root.join("s");
        let listener = UnixListener::bind(&socket).map_err(|_| ToolError::Operation)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .map_err(|_| ToolError::Operation)?;
        listener
            .set_nonblocking(true)
            .map_err(|_| ToolError::Operation)?;
        let listener =
            tokio::net::UnixListener::from_std(listener).map_err(|_| ToolError::Operation)?;
        if !system_program("/usr/bin/env")? {
            return Err(ToolError::ProtectionUnavailable);
        }
        let path = self.write_job(
            &[
                "/usr/bin/env",
                "-i",
                runtime.guard_executable.to_str().ok_or(ToolError::Path)?,
                "--internal-native-tool-unit",
                socket.to_str().ok_or(ToolError::Path)?,
            ],
            "/dev/null",
        )?;
        self.manager(ManagerAction::Bootstrap(&path)).await?;
        let launched = self.manager(ManagerAction::Kickstart).await?;
        let launched = std::str::from_utf8(&launched)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .filter(|pid| *pid > 1 && *pid <= i32::MAX as u32)
            .ok_or(ToolError::ProtectionUnavailable)?;
        let launched = identity(launched)?;
        let own = identity(std::process::id())?;
        if launched.coalition == own.coalition {
            return Err(ToolError::ProtectionUnavailable);
        }
        self.scope = Some(Scope::new(launched.coalition)?);
        let (stream, _) = listener.accept().await.map_err(|_| ToolError::Operation)?;
        let stream = stream.into_std().map_err(|_| ToolError::Operation)?;
        let pid = peer(&stream)?;
        let child = identity(pid)?;
        if child.pid != launched.pid
            || child.unique_id != launched.unique_id
            || child.coalition != launched.coalition
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let stream = tokio::net::UnixStream::from_std(stream).map_err(|_| ToolError::Operation)?;
        let mut stream = BufReader::new(stream);
        let received: Identity = serde_json::from_slice(&read_line_capped(&mut stream, 512).await?)
            .map_err(|_| ToolError::ProtectionUnavailable)?;
        if received != child {
            return Err(ToolError::ProtectionUnavailable);
        }
        Ok(stream.into_inner())
    }

    async fn stop(&mut self) -> Result<(), ToolError> {
        if self.stopped {
            return Ok(());
        }
        let cleanup = if let Some(scope) = &mut self.scope {
            scope.stop(None).await
        } else {
            Ok(())
        };
        let removal = self.manager(ManagerAction::Bootout).await;
        cleanup?;
        removal?;
        cleanup_storage(&self.root).await?;
        self.stopped = true;
        Ok(())
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        if !self.stopped {
            // A scoped reactor also completes teardown when the caller's reactor is unwinding.
            std::thread::scope(|threads| {
                if let Ok(thread) = std::thread::Builder::new().spawn_scoped(threads, || {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|_| ToolError::Operation)?
                        .block_on(self.stop())
                }) {
                    let _ = thread.join();
                }
            });
        }
        if self.stopped
            && !storage_mounted(&self.root).unwrap_or(true)
            && let (Ok(tmp), Some(name)) = (
                crate::tools::fs::open_directory(Path::new("/private/tmp")),
                self.root.file_name(),
            )
        {
            let _ = tmp.remove_dir_all(name);
        }
    }
}

pub(super) async fn execute(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    mut cancellation: RunCancellation,
    dispatched: &AtomicBool,
) -> Result<ToolObservation, ToolError> {
    let mut unit = Unit::new()?;
    crate::tools::fs::reject_overlap(&unit.root, &runtime.workspace)?;
    let execution = async {
        let mut stream = BufReader::new(
            tokio::time::timeout(CONNECT_DEADLINE, unit.connect(runtime))
                .await
                .map_err(|_| ToolError::Limit)??,
        );
        let task = super::super::encode_task(&Task::new(runtime, intent))?;
        stream
            .get_mut()
            .write_all(&task)
            .await
            .map_err(|_| ToolError::Operation)?;
        let bytes = read_line_capped(&mut stream, 2048).await?;
        if let Ok(failure) = serde_json::from_slice::<BootstrapFailure>(&bytes) {
            return Err(ToolError::GuardBootstrap(failure));
        }
        let ready = super::super::parse_ready(&bytes, intent)?;
        if identity(ready.pid)?.coalition
            != unit
                .scope
                .as_ref()
                .ok_or(ToolError::ProtectionUnavailable)?
                .coalition
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        unit.scope
            .as_mut()
            .ok_or(ToolError::ProtectionUnavailable)?
            .check_resources()
            .map_err(|_| ToolError::GuardRejected(GuardStage::ResourceAttestation))?;
        dispatched.store(true, Ordering::Release);
        stream
            .get_mut()
            .write_all(b"GO\n")
            .await
            .map_err(|_| ToolError::Operation)?;
        super::super::read_observation(&mut stream, intent).await
    };
    let deadline = CONNECT_DEADLINE
        + PREPARATION_DEADLINE
        + Duration::from_millis(u64::from(intent.limits.runtime_ms))
        + Duration::from_secs(5);
    let result = tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(ToolError::Operation),
        result = tokio::time::timeout(deadline, execution) => result.unwrap_or(Err(ToolError::Limit)),
    };
    unit.stop()
        .await
        .map_err(|_| ToolError::GuardRejected(GuardStage::ProcessCleanup))?;
    result
}

fn connect_control(path: &Path) -> Result<(PrivateDirectory, UnixStream), ToolError> {
    use std::os::unix::fs::FileTypeExt;
    let root = open_directory(path.parent().ok_or(ToolError::Path)?, false)
        .map_err(|_| ToolError::Path)?;
    let metadata = std::fs::symlink_metadata(path).map_err(|_| ToolError::Path)?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(ToolError::Path);
    }
    let stream = UnixStream::connect(path).map_err(|_| ToolError::Operation)?;
    Ok((root, stream))
}

pub(super) fn helper_main() -> std::process::ExitCode {
    let mut diagnostic = None;
    let mut stage = super::super::BootstrapStage::Admission;
    let result = (|| -> Result<(), ToolError> {
        bootstrap()?;
        let path = std::env::args_os().nth(2).ok_or(ToolError::Configuration)?;
        let path = Path::new(&path);
        let (_root, mut stream) = connect_control(path)?;
        diagnostic = Some(stream.try_clone().map_err(|_| ToolError::Operation)?);
        stage = super::super::BootstrapStage::Capabilities;
        let parent = peer(&stream)?;
        let own = identity(std::process::id())?;
        if identity(parent)?.coalition == own.coalition {
            return Err(ToolError::ProtectionUnavailable);
        }
        let mut scope = Scope::new(own.coalition)?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|_| ToolError::Operation)?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|_| ToolError::Operation)?;
        serde_json::to_writer(&mut stream, &own).map_err(|_| ToolError::Operation)?;
        stream.write_all(b"\n").map_err(|_| ToolError::Operation)?;
        stage = super::super::BootstrapStage::Admission;
        let task = {
            let mut reader = std::io::BufReader::new(&mut stream);
            super::super::read_task(&mut reader)?
        };
        stage = super::super::BootstrapStage::Capabilities;
        if task.guard_executable
            != std::env::current_exe().map_err(|_| ToolError::ProtectionUnavailable)?
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let (runtime, intent) = task.into_runtime();
        stream
            .set_read_timeout(None)
            .map_err(|_| ToolError::Operation)?;
        stream
            .set_write_timeout(None)
            .map_err(|_| ToolError::Operation)?;
        stream
            .set_nonblocking(true)
            .map_err(|_| ToolError::Operation)?;
        let reactor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| ToolError::Operation)?;
        diagnostic = None;
        reactor.block_on(async {
            let stream =
                tokio::net::UnixStream::from_std(stream).map_err(|_| ToolError::Operation)?;
            let (mut input, mut output) = stream.into_split();
            let cancellation = RunCancellation::new();
            let watched = cancellation.clone();
            let (go, receive_go) = tokio::sync::oneshot::channel();
            let reader = tokio::spawn(async move {
                let mut bytes = [0; 3];
                if input.read_exact(&mut bytes).await.is_ok() && bytes == *b"GO\n" {
                    let _ = go.send(());
                    let _ = input.read(&mut [0; 1]).await;
                }
                watched.cancel();
            });
            let result = async {
                stage = super::super::BootstrapStage::Workspace;
                let _storage = if intent.call.is_process() {
                    Some(
                        prepare_command_storage(path.parent().ok_or(ToolError::Path)?, &runtime)
                            .await?,
                    )
                } else {
                    None
                };
                stage = super::super::BootstrapStage::Capabilities;
                super::execute_worker(
                    &runtime,
                    &intent,
                    cancellation,
                    &mut scope,
                    &mut output,
                    receive_go,
                    path.parent().ok_or(ToolError::Path)?,
                )
                .await
            }
            .await;
            let cleanup = scope.stop(Some(std::process::id())).await;
            let storage_cleanup = if cleanup.is_ok() {
                cleanup_storage(path.parent().ok_or(ToolError::Path)?).await
            } else {
                Err(ToolError::ProtectionUnavailable)
            };
            reader.abort();
            let _ = reader.await;
            let result = cleanup.and(storage_cleanup).and(result);
            let bytes = match &result {
                Ok(observation) => serde_json::to_vec(observation),
                Err(ToolError::GuardBootstrap(failure)) => serde_json::to_vec(failure),
                Err(_) => serde_json::to_vec(&BootstrapFailure {
                    stage: stage.clone(),
                }),
            }
            .map_err(|_| ToolError::Operation)?;
            output
                .write_all(&bytes)
                .await
                .map_err(|_| ToolError::Operation)?;
            output
                .write_all(b"\n")
                .await
                .map_err(|_| ToolError::Operation)?;
            output.shutdown().await.map_err(|_| ToolError::Operation)?;
            result.map(|_| ())
        })
    })();
    if let Err(error) = &result
        && let Some(stream) = &mut diagnostic
    {
        let failure = match error {
            ToolError::GuardBootstrap(failure) => serde_json::to_vec(failure),
            _ => serde_json::to_vec(&BootstrapFailure { stage }),
        };
        if let Ok(mut bytes) = failure {
            bytes.push(b'\n');
            let _ = stream.write_all(&bytes);
        }
    }
    if result.is_ok() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::super::storage::{STORAGE_BYTES_CAP, STORAGE_PENDING, prepare_storage};
    use super::super::{COMMAND_DENIED_SYSCALLS, COMMAND_PROFILE, SYS_COALITION_INFO, SYS_PERSONA};
    use super::*;
    use crate::tools::ToolCall;
    use crate::tools::guard::read_capped;
    use std::io::Read;

    #[test]
    fn retained_root_bound_counts_only_this_users_unit_roots() {
        use crate::test_process::BoundedOutput;

        let root = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::guard::macos::unit::tests::retained_root_bound_helper",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("TMPDIR", root.path())
            .env("ARANY_TEST_RETAINED_ROOT", root.path())
            .bounded_output_for(Duration::from_secs(10), 16 * 1024)
            .unwrap();
        assert!(output.status.success(), "retained-root corpus: {output:?}");
        crate::test_process::assert_single_test_completed(&output.stdout, "retained-root corpus");
        assert!(output.stderr.is_empty());
    }

    #[test]
    #[ignore = "isolated liveness-lock corpus owned by retained-root test"]
    fn retained_root_bound_helper() {
        std::env::var_os("ARANY_TEST_RETAINED_ROOT").expect("private retained-root fixture");
        let parent = tempfile::tempdir().unwrap();
        let owner = rustix::process::geteuid().as_raw();
        let abandoned = |name: &str| {
            let root = parent.path().join(format!("{UNIT_ROOT_PREFIX}{name}"));
            std::fs::create_dir(&root).unwrap();
            std::fs::File::create(root.join(UNIT_LIVENESS_LOCK)).unwrap();
        };
        std::fs::create_dir(parent.path().join("unrelated")).unwrap();
        for index in 0..RETAINED_ROOT_LIMIT - 1 {
            abandoned(&index.to_string());
        }
        std::fs::create_dir(parent.path().join(format!("{UNIT_ROOT_PREFIX}creating"))).unwrap();
        check_retained_roots(parent.path(), owner)
            .expect("a root without its liveness lock holds no storage");
        abandoned("last");
        assert!(matches!(
            check_retained_roots(parent.path(), owner),
            Err(ToolError::RetainedRoots)
        ));
        assert!(
            check_retained_roots(parent.path(), owner.wrapping_add(1)).is_ok(),
            "roots owned by another uid are not counted"
        );
        let live = parent.path().join(format!("{UNIT_ROOT_PREFIX}last"));
        std::fs::set_permissions(&live, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_file(live.join(UNIT_LIVENESS_LOCK)).unwrap();
        let held = hold_liveness_lock(&open_directory(&live, false).unwrap()).unwrap();
        check_retained_roots(parent.path(), owner).expect("a live unit root is not retained");
        drop(held);
        assert!(
            matches!(
                check_retained_roots(parent.path(), owner),
                Err(ToolError::RetainedRoots)
            ),
            "a released lock marks its root retained"
        );
        let scanned = tempfile::tempdir().unwrap();
        for index in 0..=UNIT_ROOT_SCAN_LIMIT {
            std::fs::File::create(scanned.path().join(format!("entry-{index}"))).unwrap();
        }
        assert!(matches!(
            check_retained_roots(scanned.path(), owner),
            Err(ToolError::ProtectionUnavailable)
        ));
    }

    const HELPER: &str = "tools::guard::macos::unit::tests::native_unit_helper";
    const MCP_SCRIPT: &str = r#"set -eu
while IFS= read -r request; do
    case "$request" in
        *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"native-fixture","version":"1"}}}' ;;
        *'"method":"notifications/initialized"'*) ;;
        *'"method":"tools/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}]}}' ;;
        *'"method":"tools/call"'*) /usr/bin/true; printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"native MCP success"}]}}' ;;
        *) exit 1 ;;
    esac
done
"#;

    fn verify_coalition_spawn_denied(coalition: u64) {
        assert_eq!(
            ffi::spawn_in_coalition(c"/usr/bin/true", coalition),
            Err(libc::EPERM),
            "spawn must not enter the outside coalition"
        );
    }

    fn send(stream: &mut UnixStream, identities: &[Identity]) {
        serde_json::to_writer(&mut *stream, identities).unwrap();
        stream.write_all(b"\n").unwrap();
    }

    fn receive(stream: &mut UnixStream) -> Vec<Identity> {
        let mut bytes = Vec::new();
        loop {
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).unwrap(), 1);
            if byte[0] == b'\n' {
                return serde_json::from_slice(&bytes).unwrap();
            }
            assert!(bytes.len() < 2048);
            bytes.push(byte[0]);
        }
    }

    fn diagnostic(root: &Path) -> String {
        let mut bytes = Vec::new();
        std::fs::File::open(root.join("errors"))
            .unwrap()
            .take(16 * 1024 + 1)
            .read_to_end(&mut bytes)
            .unwrap();
        assert!(bytes.len() <= 16 * 1024);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn child(root: &Path, phase: &str) -> std::process::Child {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", HELPER, "--ignored", "--nocapture"])
            .env_clear()
            .env("ARANY_TEST_NATIVE_UNIT_ROOT", root)
            .env("ARANY_TEST_NATIVE_UNIT_PHASE", phase)
            .current_dir("/")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(root.join("errors"))
                    .unwrap(),
            )
            .spawn()
            .unwrap()
    }

    fn snapshot_runtime(workspace: &Path) -> ToolRuntime {
        let directory = crate::tools::fs::open_directory(workspace).unwrap();
        let shell = "/bin/sh";
        let system_programs = super::super::SystemPrograms::admit().unwrap();
        let process_enforcement_digest =
            super::super::macos_enforcement_digest(Some(&system_programs)).unwrap();
        ToolRuntime {
            config: crate::tools::config::Config {
                version: 1,
                workspace_paths: vec!["selected".into(), "empty".into(), "script".into()],
                write: true,
                commands: vec![
                    crate::tools::config::Program {
                        name: "shell".into(),
                        executable: shell.into(),
                        sha256: crate::tools::types::hex_digest(&std::fs::read(shell).unwrap()),
                        interpreter: true,
                        inputs: vec![crate::tools::config::ProgramInput {
                            path: workspace.join("runtime-input").to_str().unwrap().into(),
                            destination: "fixture.txt".into(),
                            sha256: crate::tools::types::hex_digest(b"pinned synthetic input"),
                        }],
                    },
                    crate::tools::config::Program {
                        name: "native".into(),
                        executable: workspace.join("script").to_str().unwrap().into(),
                        sha256: crate::tools::types::hex_digest(
                            &std::fs::read(workspace.join("script")).unwrap(),
                        ),
                        interpreter: false,
                        inputs: Vec::new(),
                    },
                ],
                skills: Vec::new(),
                mcp: vec![crate::tools::config::Server {
                    name: "local".into(),
                    program: crate::tools::config::Program {
                        name: "mcp-shell".into(),
                        executable: shell.into(),
                        sha256: crate::tools::types::hex_digest(&std::fs::read(shell).unwrap()),
                        interpreter: true,
                        inputs: vec![crate::tools::config::ProgramInput {
                            path: workspace.join("server.sh").to_str().unwrap().into(),
                            destination: "server.sh".into(),
                            sha256: crate::tools::types::hex_digest(MCP_SCRIPT.as_bytes()),
                        }],
                    },
                    args: vec!["/inputs/mcp-shell/server.sh".into()],
                    tools: vec!["echo".into()],
                }],
            },
            workspace: workspace.to_path_buf(),
            workspace_identity: crate::tools::fs::identity(&directory).unwrap(),
            guard_executable: std::env::current_exe().unwrap(),
            enforcement_digest: super::super::enforcement_digest().unwrap(),
            system_programs: Some(std::sync::Arc::new(system_programs)),
            process_enforcement_digest,
        }
    }

    /// Compiles the native actor and returns its entitlement-signed and plain copies.
    fn native_actors(directory: &Path) -> (PathBuf, PathBuf) {
        use crate::test_process::BoundedOutput;

        let actor = directory.join("actor");
        let plain = directory.join("plain");
        let entitlements = directory.join("entitlements.plist");
        let compiled = std::process::Command::new("/usr/bin/xcrun")
            .args([
                "--sdk", "macosx", "clang", "-Wall", "-Wextra", "-Werror", "-O1",
            ])
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/macos-native-actor.c"
            ))
            .arg("-o")
            .arg(&actor)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", directory)
            .env("TMPDIR", directory)
            .current_dir(directory)
            .bounded_output_for(Duration::from_secs(20), 16 * 1024)
            .unwrap();
        assert!(
            compiled.status.success(),
            "native actor compilation: {compiled:?}"
        );
        assert!(compiled.stdout.is_empty() && compiled.stderr.is_empty());
        std::fs::copy(&actor, &plain).unwrap();
        std::fs::write(
            &entitlements,
            br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>com.apple.security.get-task-allow</key><true/></dict></plist>
"#,
        )
        .unwrap();
        let signed = std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-", "--entitlements"])
            .arg(&entitlements)
            .arg(&actor)
            .env_clear()
            .env("HOME", directory)
            .env("TMPDIR", directory)
            .current_dir(directory)
            .bounded_output_for(Duration::from_secs(10), 16 * 1024)
            .unwrap();
        assert!(
            signed.status.success(),
            "native actor ad-hoc signing: {signed:?}"
        );
        assert!(signed.stdout.is_empty());
        for executable in [&actor, &plain] {
            assert!(std::fs::metadata(executable).unwrap().len() < 1024 * 1024);
        }
        (actor, plain)
    }

    async fn verify_uncertain_storage_is_retained() {
        let mut uncertain = Unit::new().unwrap();
        std::fs::write(uncertain.root.join(STORAGE_PENDING), []).unwrap();
        assert!(uncertain.stop().await.is_err());
        let uncertain_root = uncertain.root.clone();
        drop(uncertain);
        assert!(uncertain_root.exists());
        crate::tools::fs::open_directory(&uncertain_root)
            .unwrap()
            .remove_file(STORAGE_PENDING)
            .unwrap();
        assert!(!storage_mounted(&uncertain_root).unwrap());
        crate::tools::fs::open_directory(Path::new("/private/tmp"))
            .unwrap()
            .remove_dir_all(uncertain_root.file_name().unwrap())
            .unwrap();
    }

    async fn verify_partial_storage_is_cleaned() {
        let mut partial = Unit::new().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), partial.root.join("scratch")).unwrap();
        let preparation = prepare_storage(&partial.root).await;
        assert!(
            matches!(preparation, Err(ToolError::ChangedInput)),
            "partial native storage: {:?}",
            preparation.err()
        );
        assert!(storage_mounted(&partial.root).unwrap());
        assert!(partial.stop().await.is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        assert!(!partial.root.join("scratch.dmg").exists());
        std::fs::remove_file(partial.root.join("scratch")).unwrap();
        partial.stop().await.unwrap();
        assert!(!storage_mounted(&partial.root).unwrap());
        let partial_root = partial.root.clone();
        drop(partial);
        assert!(!partial_root.exists());
    }

    fn phase_workspace(script: &Path) -> tempfile::TempDir {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("selected"), b"before edit").unwrap();
        std::fs::write(source.path().join("omitted"), b"omitted synthetic data").unwrap();
        std::fs::write(
            source.path().join("runtime-input"),
            b"pinned synthetic input",
        )
        .unwrap();
        std::fs::create_dir(source.path().join("empty")).unwrap();
        std::fs::write(source.path().join("server.sh"), MCP_SCRIPT).unwrap();
        std::fs::copy(script, source.path().join("script")).unwrap();
        std::fs::set_permissions(
            source.path().join("script"),
            std::fs::Permissions::from_mode(0o6755),
        )
        .unwrap();
        source
    }

    /// Starts the helper as a launchd job in its own coalition and accepts its attestation.
    async fn launch_helper(
        unit: &mut Unit,
        phase: &str,
        workspace: &Path,
    ) -> (Identity, UnixStream, tokio::net::UnixListener) {
        let listener = UnixListener::bind(unit.root.join("s")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
        let executable = std::env::current_exe().unwrap();
        let root = format!(
            "ARANY_TEST_NATIVE_UNIT_ROOT={}",
            unit.root.to_str().unwrap()
        );
        let phase_argument = format!("ARANY_TEST_NATIVE_UNIT_PHASE={phase}");
        let source_argument = format!(
            "ARANY_TEST_NATIVE_UNIT_WORKSPACE={}",
            workspace.to_str().unwrap()
        );
        let outside = identity(std::process::id()).unwrap();
        let outside_pid = format!("ARANY_TEST_NATIVE_UNIT_OUTSIDE_PID={}", outside.pid);
        let outside_coalition = format!(
            "ARANY_TEST_NATIVE_UNIT_OUTSIDE_COALITION={}",
            outside.coalition
        );
        std::fs::write(unit.root.join("errors"), []).unwrap();
        let path = unit
            .write_job(
                &[
                    "/usr/bin/env",
                    "-i",
                    &root,
                    &phase_argument,
                    &source_argument,
                    &outside_pid,
                    &outside_coalition,
                    "ARANY_TEST_NATIVE_UNIT_ARGUMENT=&<>\"'é😀\u{1}",
                    executable.to_str().unwrap(),
                    "--exact",
                    HELPER,
                    "--ignored",
                    "--nocapture",
                ],
                unit.root.join("errors").to_str().unwrap(),
            )
            .unwrap();
        unit.manager(ManagerAction::Bootstrap(&path)).await.unwrap();
        let pid: u32 = std::str::from_utf8(&unit.manager(ManagerAction::Kickstart).await.unwrap())
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let root = identity(pid).unwrap();
        assert_ne!(root.coalition, outside.coalition);
        unit.scope = Some(Scope::new(root.coalition).unwrap());
        let accepted = tokio::time::timeout(Duration::from_secs(10), listener.accept()).await;
        assert!(
            accepted.is_ok(),
            "native {phase} readiness: {}",
            diagnostic(&unit.root)
        );
        let attestation = accepted.unwrap().unwrap().0.into_std().unwrap();
        assert_eq!(peer(&attestation).unwrap(), pid);
        (root, attestation, listener)
    }

    #[tokio::test]
    #[cfg_attr(
        not(arany_ci),
        ignore = "native aggregate-resource and escaped-descendant gate; finite synthetic jobs"
    )]
    #[allow(clippy::await_holding_lock)]
    async fn native_unit_owns_descendants_and_supervises_resources() {
        let _gate = crate::test_process::NATIVE_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let generated = tempfile::tempdir().unwrap();
        let (actor, plain) = native_actors(generated.path());
        verify_uncertain_storage_is_retained().await;
        verify_partial_storage_is_cleaned().await;

        for phase in [
            "descendants",
            "threads",
            "memory",
            "cpu",
            "storage",
            "storage-drop",
            "command-profile",
            "entitlement-profile",
        ] {
            let mut unit = Unit::new().unwrap();
            assert_eq!(std::fs::read_dir(&unit.root).unwrap().count(), 1);
            if phase == "entitlement-profile" {
                for (name, executable) in [("actor", &actor), ("plain", &plain)] {
                    std::fs::copy(executable, unit.root.join(name)).unwrap();
                    std::fs::set_permissions(
                        unit.root.join(name),
                        std::fs::Permissions::from_mode(0o700),
                    )
                    .unwrap();
                }
            }
            let source = phase_workspace(&plain);
            let (root, mut attestation, listener) =
                launch_helper(&mut unit, phase, source.path()).await;
            if matches!(phase, "storage" | "storage-drop" | "command-profile") {
                crate::tools::fs::native(
                    source.path(),
                    &ToolCall::Write {
                        path: "selected".into(),
                        expected_digest: Some(crate::tools::types::hex_digest(b"before edit")),
                        content: "after typed edit".into(),
                    },
                    &snapshot_runtime(source.path()).config,
                )
                .unwrap();
            }
            attestation.write_all(b"G").unwrap();
            let accepted = tokio::time::timeout(Duration::from_secs(10), listener.accept()).await;
            assert!(
                accepted.is_ok(),
                "native {phase} descendant readiness: {}",
                diagnostic(&unit.root)
            );
            drop(attestation);
            let stream = accepted.unwrap().unwrap().0;
            let mut stream = BufReader::new(stream);
            let members: Vec<Identity> = serde_json::from_slice(
                &tokio::time::timeout(Duration::from_secs(10), read_line_capped(&mut stream, 2048))
                    .await
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            assert!(members.len() == 1 || members.len() == 2);
            for member in &members {
                assert_eq!(member.coalition, root.coalition);
                assert!(identity(member.pid).unwrap() == *member);
            }
            stream.get_mut().write_all(b"G").await.unwrap();
            let scope = unit.scope.as_mut().unwrap();
            if phase == "descendants" {
                assert_ne!(
                    rustix::process::getpgid(rustix::process::Pid::from_raw(members[0].pid as i32))
                        .unwrap(),
                    rustix::process::getpgid(rustix::process::Pid::from_raw(root.pid as i32))
                        .unwrap()
                );
                let mut stale = members[0];
                stale.version = stale.version.wrapping_sub(1);
                stale.signal(libc::SIGKILL).unwrap();
                assert!(identity(members[0].pid).unwrap() == members[0]);
                assert!(scope.members().unwrap().contains(&members[0]));
                scope.check_resources().unwrap();
            } else if matches!(
                phase,
                "storage" | "storage-drop" | "command-profile" | "entitlement-profile"
            ) {
                assert_eq!(
                    storage_mounted(&unit.root).unwrap(),
                    phase != "entitlement-profile"
                );
                if matches!(phase, "command-profile" | "entitlement-profile") {
                    assert!(
                        tokio::time::timeout(Duration::from_secs(20), read_capped(&mut stream, 1))
                            .await
                            .unwrap_or_else(|error| {
                                panic!(
                                    "native {phase} EOF: {error}; helper={}",
                                    diagnostic(&unit.root)
                                )
                            })
                            .unwrap()
                            .is_empty()
                    );
                    assert_eq!(
                        std::fs::read(unit.root.join(if phase == "command-profile" {
                            "workspace/profile-receipt"
                        } else {
                            "entitlement-receipt"
                        }))
                        .unwrap_or_else(|error| {
                            panic!(
                                "native {phase} receipt: {error}; helper={}",
                                diagnostic(&unit.root)
                            )
                        }),
                        b"PROFILE\n"
                    );
                    tokio::time::timeout(Duration::from_secs(5), async {
                        let mut samples = tokio::time::interval(SUPERVISION_INTERVAL);
                        loop {
                            samples.tick().await;
                            if scope.members().unwrap().is_empty() {
                                break;
                            }
                        }
                    })
                    .await
                    .expect("native profile helper must complete and retire its coalition");
                    let output = diagnostic(&unit.root);
                    crate::test_process::assert_single_test_completed(
                        output.as_bytes(),
                        "native profile helper completion",
                    );
                } else {
                    assert_eq!(
                        tokio::time::timeout(
                            Duration::from_secs(20),
                            read_line_capped(&mut stream, 8)
                        )
                        .await
                        .unwrap()
                        .unwrap_or_else(|error| panic!(
                            "native {phase} payload: {error}; {}",
                            diagnostic(&unit.root)
                        )),
                        b"FULL\n"
                    );
                }
                scope.check_resources().unwrap();
                assert_eq!(
                    scope.members().unwrap().len(),
                    usize::from(!matches!(phase, "command-profile" | "entitlement-profile"))
                );
                if phase != "storage-drop" {
                    scope.stop(None).await.unwrap();
                    assert_eq!(
                        storage_mounted(&unit.root).unwrap(),
                        phase != "entitlement-profile"
                    );
                }
            } else {
                let mut samples = tokio::time::interval(SUPERVISION_INTERVAL);
                let observed = tokio::time::timeout(Duration::from_secs(60), async {
                    loop {
                        samples.tick().await;
                        match scope.check_resources() {
                            Ok(()) => {}
                            Err(ToolError::Limit) => break,
                            Err(error) => panic!("{phase} observer failed: {error}"),
                        }
                    }
                })
                .await;
                assert!(
                    observed.is_ok(),
                    "native {phase} threshold was not observed"
                );
            }
            if phase == "storage-drop" {
                let path = unit.root.clone();
                let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _unit = unit;
                    panic!("synthetic native unit cleanup unwind");
                }));
                assert!(unwound.is_err());
                assert!(!path.exists());
                assert!(
                    Scope::new(root.coalition)
                        .unwrap()
                        .members()
                        .unwrap()
                        .is_empty()
                );
            } else {
                unit.stop().await.unwrap();
                assert!(!storage_mounted(&unit.root).unwrap());
                assert!(unit.scope.as_mut().unwrap().members().unwrap().is_empty());
            }
            for member in std::iter::once(&root).chain(&members) {
                assert!(match identity(member.pid) {
                    Err(ToolError::ChangedInput) => true,
                    Ok(current) => current.unique_id != member.unique_id,
                    Err(_) => false,
                });
            }
            if matches!(phase, "storage" | "storage-drop" | "command-profile") {
                assert_eq!(
                    std::fs::read(source.path().join("selected")).unwrap(),
                    b"after typed edit"
                );
                assert_eq!(
                    std::fs::read(source.path().join("omitted")).unwrap(),
                    b"omitted synthetic data"
                );
            }
        }
    }

    #[test]
    #[ignore = "isolated helper owned by the native unit gate"]
    fn native_unit_helper() {
        // The fixture owns a finite process and intentionally ignores the inherited CPU signal.
        assert!(ffi::ignore_cpu_signal_with_watchdog(75));
        let root = std::env::var_os("ARANY_TEST_NATIVE_UNIT_ROOT").unwrap();
        let root = Path::new(&root);
        let phase = std::env::var("ARANY_TEST_NATIVE_UNIT_PHASE").unwrap();
        if phase == "detach" {
            rustix::process::setsid().unwrap();
            // The fixture deliberately orphans this child; the coalition owner verifies its removal.
            #[allow(clippy::zombie_processes)]
            let _child = child(root, "hold");
            return;
        }
        if ["hold", "load-threads", "load-memory", "load-cpu"].contains(&phase.as_str()) {
            let mut stream = UnixStream::connect(root.join("c")).unwrap();
            send(&mut stream, &[identity(std::process::id()).unwrap()]);
            let mut go = [0];
            stream.read_exact(&mut go).unwrap();
            assert_eq!(&go, b"G");
            if phase == "load-memory" {
                let mut memory = vec![0_u8; 280 * 1024 * 1024];
                let mut value = 3_u64;
                for word in memory.as_chunks_mut::<8>().0 {
                    value ^= value << 13;
                    value ^= value >> 7;
                    value ^= value << 17;
                    word.copy_from_slice(&value.to_le_bytes());
                }
                std::thread::spawn(move || {
                    loop {
                        for page in memory.chunks_mut(4096) {
                            page[0] = page[0].wrapping_add(1);
                        }
                        std::hint::black_box(&memory);
                    }
                });
            }
            if phase == "load-threads" {
                let barrier = std::sync::Arc::new(std::sync::Barrier::new(65));
                for _ in 0..64 {
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        loop {
                            std::thread::park();
                        }
                    });
                }
                barrier.wait();
            }
            if phase == "load-cpu" {
                rustix::process::setrlimit(
                    rustix::process::Resource::Cpu,
                    rustix::process::Rlimit {
                        current: Some(CPU_LIMIT.as_secs()),
                        maximum: Some(CPU_LIMIT.as_secs()),
                    },
                )
                .unwrap();
                loop {
                    std::hint::black_box(1_u64.wrapping_add(std::hint::black_box(2)));
                }
            }
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
            return;
        }
        assert_eq!(
            std::env::var("ARANY_TEST_NATIVE_UNIT_ARGUMENT").unwrap(),
            "&<>\"'é😀\u{1}"
        );
        let listener = UnixListener::bind(root.join("c")).unwrap();
        let mut attestation = UnixStream::connect(root.join("s")).unwrap();
        attestation
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        assert_eq!(attestation.read(&mut [0]).unwrap(), 1);
        drop(attestation);
        if phase == "entitlement-profile" {
            use crate::test_process::BoundedOutput;

            let verify_actor = |name: &str, mode: &str| {
                let output = std::process::Command::new(root.join(name))
                    .arg(mode)
                    .env_clear()
                    .current_dir(root)
                    .bounded_output_for(Duration::from_secs(5), 1024)
                    .unwrap();
                assert!(
                    output.status.success(),
                    "native {name} {mode} actor: {output:?}"
                );
                assert_eq!(output.stdout, b"native actor success\n");
                assert!(output.stderr.is_empty());
            };
            verify_actor("plain", "allow");
            verify_actor("actor", "allow");
            let mut stream = UnixStream::connect(root.join("s")).unwrap();
            send(&mut stream, &[identity(std::process::id()).unwrap()]);
            let mut go = [0];
            stream.read_exact(&mut go).unwrap();
            assert_eq!(&go, b"G");
            // The runnable ad-hoc key proves the same dynamic predicate, not an Apple entitlement grant.
            let mut profile = super::super::profile(COMMAND_PROFILE, COMMAND_DENIED_SYSCALLS);
            for key in [
                "com.apple.private.coalition-spawn",
                "com.apple.private.persona-mgmt",
            ] {
                assert!(profile.contains(&format!("(require-entitlement \"{key}\")")));
                profile = profile.replace(key, "com.apple.security.get-task-allow");
            }
            for path in [
                root.join("plain"),
                root.join("actor"),
                PathBuf::from("/usr/bin/true"),
            ] {
                super::super::ancestors(&mut profile, &path).unwrap();
                profile.push_str(&format!(
                    "(allow file-read* file-map-executable process-exec (literal {}))\n",
                    super::super::quoted(&path).unwrap(),
                ));
            }
            profile.push_str(&format!(
                "(allow file-read* file-write* (literal {}))\n",
                super::super::quoted(&root.join("entitlement-receipt")).unwrap(),
            ));
            super::super::configure_resources().unwrap();
            super::super::install_profile(profile).unwrap();
            verify_actor("plain", "allow");
            verify_actor("actor", "deny");
            std::fs::write(root.join("entitlement-receipt"), b"PROFILE\n").unwrap();
            return;
        }
        if matches!(
            phase.as_str(),
            "storage" | "storage-drop" | "command-profile"
        ) {
            let reactor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let source = std::env::var_os("ARANY_TEST_NATIVE_UNIT_WORKSPACE").unwrap();
            let runtime = snapshot_runtime(Path::new(&source));
            let volumes = reactor
                .block_on(prepare_command_storage(root, &runtime))
                .unwrap();
            assert_eq!(volumes[0].read("selected").unwrap(), b"after typed edit");
            assert!(!volumes[0].exists("omitted"));
            assert!(volumes[0].is_dir("empty"));
            use cap_std::fs::MetadataExt;
            assert_eq!(
                volumes[0].metadata("script").unwrap().mode() & 0o7777,
                0o700
            );
            volumes[0].write("selected", b"disposable").unwrap();
            let mut stream = UnixStream::connect(root.join("s")).unwrap();
            send(&mut stream, &[identity(std::process::id()).unwrap()]);
            let mut go = [0];
            stream.read_exact(&mut go).unwrap();
            assert_eq!(&go, b"G");
            if phase == "command-profile" {
                use crate::test_process::BoundedOutput;

                let outside_pid = std::env::var("ARANY_TEST_NATIVE_UNIT_OUTSIDE_PID")
                    .unwrap()
                    .parse::<i32>()
                    .unwrap();
                let outside_coalition = std::env::var("ARANY_TEST_NATIVE_UNIT_OUTSIDE_COALITION")
                    .unwrap()
                    .parse::<u64>()
                    .unwrap();
                std::env::set_current_dir(root).unwrap();
                let task = Task {
                    config: runtime.config.clone(),
                    intent: EffectIntent {
                        id: uuid::Uuid::now_v7(),
                        run_id: crate::session::RunId::default(),
                        agent_run_id: crate::session::AgentRunId::default(),
                        policy_digest: runtime.config.digest(),
                        enforcement_digest: runtime.process_enforcement_digest,
                        workspace_device: runtime.workspace_identity.0,
                        workspace_inode: runtime.workspace_identity.1,
                        call: ToolCall::Command {
                            program: "shell".into(),
                            args: Vec::new(),
                            cwd: ".".into(),
                        },
                        limits: crate::tools::ToolLimits::default(),
                        expires_at_ms: crate::tools::now_ms() + 60_000,
                        use_count: 1,
                    },
                    workspace: runtime.workspace.clone(),
                    guard_executable: runtime.guard_executable.clone(),
                    system_programs: runtime.system_programs.as_deref().cloned(),
                };
                let (mut wake_sender, mut wake_receiver) = UnixStream::pair().unwrap();
                wake_sender.write_all(&[1]).unwrap();
                wake_receiver.read_exact(&mut [0]).unwrap();
                assert!(Path::new("/bin/launchctl").is_file());
                super::super::configure_resources().unwrap();
                super::super::install(&task).unwrap();
                let cpu = rustix::process::getrlimit(rustix::process::Resource::Cpu);
                assert_eq!(cpu.current, None);
                assert_eq!(cpu.maximum, None);
                assert_eq!(
                    wake_sender.write(&[1]).unwrap_err().kind(),
                    std::io::ErrorKind::PermissionDenied
                );
                // macOS 15 reports the denied exec as ENOENT; later releases report EPERM.
                assert!(matches!(
                    std::process::Command::new("/bin/launchctl")
                        .arg("help")
                        .status()
                        .unwrap_err()
                        .kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
                ));
                // Signal 0 observes permission without risking a signal to the fixture's outside owner.
                assert_eq!(
                    rustix::process::test_kill_process(
                        rustix::process::Pid::from_raw(outside_pid).unwrap()
                    )
                    .unwrap_err(),
                    rustix::io::Errno::PERM
                );
                assert_eq!(
                    std::fs::read(runtime.workspace.join("selected"))
                        .unwrap_err()
                        .kind(),
                    std::io::ErrorKind::PermissionDenied
                );
                assert_eq!(
                    std::fs::write(runtime.workspace.join("selected"), b"forbidden")
                        .unwrap_err()
                        .kind(),
                    std::io::ErrorKind::PermissionDenied
                );
                assert_eq!(
                    std::fs::read(root.join("errors")).unwrap_err().kind(),
                    std::io::ErrorKind::PermissionDenied
                );
                assert_eq!(
                    std::fs::metadata(runtime.workspace.join("selected"))
                        .unwrap_err()
                        .kind(),
                    std::io::ErrorKind::PermissionDenied
                );
                assert_eq!(
                    std::fs::read_dir(root.parent().unwrap())
                        .unwrap_err()
                        .kind(),
                    std::io::ErrorKind::PermissionDenied
                );
                assert_eq!(
                    std::fs::read_dir(root).unwrap_err().kind(),
                    std::io::ErrorKind::PermissionDenied
                );
                assert_eq!(
                    std::fs::write(root.join("runtime/inputs/shell/fixture.txt"), b"forbidden")
                        .unwrap_err()
                        .kind(),
                    std::io::ErrorKind::PermissionDenied
                );
                assert_eq!(
                    std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                        .unwrap_err()
                        .kind(),
                    std::io::ErrorKind::PermissionDenied
                );
                let output = std::process::Command::new("/usr/bin/true")
                    .current_dir(root.join("workspace"))
                    .bounded_output_for(Duration::from_secs(5), 1024)
                    .unwrap();
                assert!(
                    output.status.success(),
                    "ordinary Rust child launch: {output:?}"
                );
                assert!(output.stdout.is_empty() && output.stderr.is_empty());
                verify_coalition_spawn_denied(outside_coalition);
                for syscall in [SYS_COALITION_INFO, SYS_PERSONA] {
                    // Invalid operations and null arguments cannot read or mutate host state.
                    assert_eq!(
                        ffi::invalid_argument_syscall_errno(syscall),
                        Some(libc::EPERM)
                    );
                }
                for definition in [&runtime.config.commands[1], &runtime.config.commands[0]] {
                    let args = if definition.name == "native" {
                        vec!["workspace".into()]
                    } else {
                        vec!["-c".into(), "exec ./script workspace".into()]
                    };
                    let mut command =
                        super::super::super::payload(definition, &args, ".", root).unwrap();
                    let (status, stdout, stderr) = reactor
                        .block_on(async {
                            super::super::super::capture(
                                command.spawn().unwrap(),
                                1024,
                                Duration::from_secs(5),
                            )
                            .await
                        })
                        .unwrap();
                    assert!(
                        status.success(),
                        "{} native child: {status}; stderr={stderr:?}",
                        definition.name
                    );
                    assert_eq!(stdout, b"native actor success\n");
                    assert!(
                        stderr.is_empty(),
                        "{} native child stderr: {stderr:?}",
                        definition.name
                    );
                }
                let mut command = super::super::super::payload(
                    &runtime.config.commands[0],
                    &["-c".into(), "set -eu; /usr/bin/true; test \"$(/bin/cat \"$ARANY_TOOL_INPUTS/shell/fixture.txt\")\" = 'pinned synthetic input'; printf 'command change' > \"$1\"; printf 'native command success'".into(), "--".into(), "/workspace/selected".into()],
                    ".",
                    root,
                ).unwrap();
                let (status, stdout, stderr) = reactor
                    .block_on(async {
                        super::super::super::capture(
                            command.spawn().unwrap(),
                            1024,
                            Duration::from_secs(5),
                        )
                        .await
                    })
                    .unwrap();
                assert!(
                    status.success(),
                    "native command status: {status}; stderr={stderr:?}"
                );
                assert_eq!(stdout, b"native command success");
                assert!(stderr.is_empty());
                assert_eq!(volumes[0].read("selected").unwrap(), b"command change");
                let catalog = reactor
                    .block_on(crate::tools::mcp::execute(
                        &ToolCall::McpList {
                            server: "local".into(),
                        },
                        &runtime.config,
                        root,
                    ))
                    .unwrap();
                let catalog: serde_json::Value = serde_json::from_str(&catalog).unwrap();
                assert_eq!(catalog["tools"][0]["name"], "echo");
                let call = ToolCall::McpCall {
                    server: "local".into(),
                    tool: "echo".into(),
                    schema_digest: catalog["tools"][0]["sha256"].as_str().unwrap().into(),
                    arguments: "{}".into(),
                };
                let result = reactor
                    .block_on(crate::tools::mcp::execute(&call, &runtime.config, root))
                    .unwrap();
                let result: serde_json::Value = serde_json::from_str(&result).unwrap();
                assert_eq!(result["content"][0]["text"], "native MCP success");
                assert_eq!(result["is_error"], false);
                volumes[0].write("profile-receipt", b"PROFILE\n").unwrap();
                return;
            }
            let block = vec![0; 1024 * 1024];
            for volume in &volumes {
                let mut output = volume.create("filled").unwrap();
                let full = (0..65).any(|_| {
                    match output.write_all(&block).and_then(|()| output.sync_all()) {
                        Ok(()) => false,
                        Err(error) => {
                            assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
                            true
                        }
                    }
                });
                assert!(full, "native filesystem must enforce its aggregate ceiling");
                assert!(output.metadata().unwrap().len() <= STORAGE_BYTES_CAP);
            }
            stream.write_all(b"FULL\n").unwrap();
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
            return;
        }
        let count = if phase == "memory" { 2 } else { 1 };
        let mut children = Vec::new();
        for _ in 0..count {
            children.push(child(
                root,
                match phase.as_str() {
                    "descendants" => "detach",
                    "threads" => "load-threads",
                    "memory" => "load-memory",
                    "cpu" => "load-cpu",
                    _ => panic!("invalid unit fixture"),
                },
            ));
        }
        let mut identities = Vec::new();
        let mut controls = Vec::new();
        for _ in 0..count {
            let mut stream = listener.accept().unwrap().0;
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            identities.extend(receive(&mut stream));
            controls.push(stream);
        }
        if phase == "descendants" {
            assert!(children[0].wait().unwrap().success());
        }
        let mut stream = UnixStream::connect(root.join("s")).unwrap();
        send(&mut stream, &identities);
        let mut go = [0];
        stream.read_exact(&mut go).unwrap();
        assert_eq!(&go, b"G");
        for control in &mut controls {
            control.write_all(b"G").unwrap();
        }
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    }
}

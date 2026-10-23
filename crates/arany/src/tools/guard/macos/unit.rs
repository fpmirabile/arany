use super::super::selection_phase::{primary_command, selection_limits};
use super::super::{BootstrapFailure, GuardStage, Task, read_line_capped};
use super::fixture::{Span as FixtureSpan, Stage as FixtureStage};
use super::storage::{
    cleanup_storage, cleanup_storage_until, prepare_command_storage, read_record, storage_mounted,
    write_record,
};
use super::{
    CPU_LIMIT, PREPARATION_DEADLINE, SUPERVISION_INTERVAL, TEARDOWN_WAIT, bootstrap, ffi,
    system_program,
};
use crate::private_fs::{PrivateDirectory, open_directory};
use crate::tools::Cancellation;
use crate::tools::process::MAX_ARGUMENT_BYTES;
use crate::tools::types::{CommandResult, ToolDisposition, ToolLimits, ToolResourceClass};
use crate::tools::worker::WORKER_PREPARATION_MS;
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
use std::sync::Mutex;
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

pub(super) fn private_recovered(parent: &Path, id: uuid::Uuid) -> Result<(), ToolError> {
    match std::fs::symlink_metadata(parent.join(format!("{UNIT_ROOT_PREFIX}{id}"))) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(ToolError::ProtectionUnavailable),
    }
}
// A live unit holds an exclusive lock here; the kernel releases it when the owner exits.
const UNIT_LIVENESS_LOCK: &str = "unit.lock";
#[cfg(test)]
const RECOVERY_ATTEMPT_LIMIT: usize = super::super::admission::RECOVERY_ATTEMPTS;
// Bounds the directory scan itself; an unverifiable listing fails closed.
const UNIT_ROOT_SCAN_LIMIT: usize = 4096;
const RECOVERY_RECORD: &str = "recovery.json";
const WORKER_OUTPUT: &str = "worker.output";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recovery {
    version: u32,
    root: (u64, u64),
    boot: uuid::Uuid,
    supervisor: Option<Identity>,
}

fn recovery(directory: &PrivateDirectory) -> Result<Recovery, ToolError> {
    let record: Recovery = read_record(&directory.dir, RECOVERY_RECORD)?;
    if record.version != 1
        || record.boot.is_nil()
        || record.root != crate::tools::fs::identity(&directory.dir)?
        || record.supervisor.is_some_and(|identity| {
            identity.pid <= 1
                || identity.pid > i32::MAX as u32
                || identity.unique_id == 0
                || identity.coalition <= 1
        })
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(record)
}

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
    capture_cpu: Option<(Identity, u128)>,
    limits: ToolLimits,
    pub(super) teardown: Option<tokio::time::Instant>,
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
            capture_cpu: None,
            limits: ToolLimits::default(),
            teardown: None,
        })
    }

    fn apply_limits(&mut self, limits: ToolLimits) {
        self.limits = limits;
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

    fn begin_capture(&mut self, limits: &ToolLimits) -> Result<(), ToolError> {
        let members = self.members()?;
        let [supervisor] = members.as_slice() else {
            return Err(ToolError::ProtectionUnavailable);
        };
        let usage = ffi::rusage(supervisor.pid)?;
        if !identity(supervisor.pid)?.same_process(*supervisor) {
            return Err(ToolError::ProtectionUnavailable);
        }
        self.capture_cpu = Some((
            *supervisor,
            u128::from(usage.ri_user_time) + u128::from(usage.ri_system_time),
        ));
        self.apply_limits(selection_limits(limits));
        self.check_resources()
    }

    fn cpu_elapsed(&self, member: Identity, ticks: u128) -> Result<u128, ToolError> {
        let baseline = self
            .capture_cpu
            .filter(|(supervisor, _)| supervisor.same_process(member))
            .map_or(0, |(_, baseline)| baseline);
        ticks
            .checked_sub(baseline)
            .map(|ticks| ticks * u128::from(self.timebase.numer) / u128::from(self.timebase.denom))
            .ok_or(ToolError::ProtectionUnavailable)
    }

    pub(super) fn check_resources(&mut self) -> Result<(), ToolError> {
        let mut memory = 0_u64;
        let mut threads = 0_u64;
        for member in self.members()? {
            let sample = (|| match ffi::task_info(member.pid)
                .and_then(|task| Ok((task, ffi::rusage(member.pid)?)))
            {
                Ok(sample) => Ok(Some(sample)),
                Err(_) if !super::check_process_resources(member.pid)? => Ok(None),
                Err(error) => Err(error),
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
                .checked_add(usage.ri_phys_footprint)
                .ok_or(ToolError::Limit)?;
            threads = threads
                .checked_add(task.pti_threadnum as u64)
                .ok_or(ToolError::Limit)?;
            let cpu_limit = if self.limits.resource_class == ToolResourceClass::Build {
                Duration::from_millis(u64::from(self.limits.runtime_ms))
            } else {
                CPU_LIMIT
            };
            if memory > self.limits.memory_bytes
                || threads > u64::from(self.limits.max_processes)
                || self.cpu_elapsed(
                    member,
                    u128::from(usage.ri_user_time) + u128::from(usage.ri_system_time),
                )? > cpu_limit.as_nanos()
            {
                return Err(ToolError::Limit);
            }
        }
        Ok(())
    }

    pub(super) async fn stop(&mut self, except: Option<u32>) -> Result<(), ToolError> {
        self.stop_until(except, tokio::time::Instant::now() + TEARDOWN_WAIT)
            .await
    }

    pub(super) async fn stop_until(
        &mut self,
        except: Option<u32>,
        deadline: tokio::time::Instant,
    ) -> Result<(), ToolError> {
        loop {
            match self.members() {
                Ok(members) => {
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
                }
                // A member caught mid-exit can fail one enumeration; only an unobserved deadline fails.
                Err(error) if tokio::time::Instant::now() >= deadline => return Err(error),
                Err(_) => {}
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
    limits: Option<ToolLimits>,
    stopped: bool,
    _permit: Option<super::super::admission::Permit>,
}

enum ManagerAction<'a> {
    Bootstrap(&'a Path),
    Kickstart,
    Bootout,
}

#[cfg(test)]
async fn recover_retained_roots(parent: &Path, owner: u32) -> Result<(), ToolError> {
    let mut attempts = RECOVERY_ATTEMPT_LIMIT;
    recover_retained_roots_with_budget(parent, owner, &mut attempts).await
}

pub(super) async fn recover_retained_roots_with_budget(
    parent: &Path,
    owner: u32,
    attempts: &mut usize,
) -> Result<(), ToolError> {
    let mut candidates = Vec::new();
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
        let metadata = match std::fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(ToolError::ProtectionUnavailable),
        };
        if metadata.uid() == owner
            && matches!(is_retained(&entry.path()), Ok(true))
            && open_directory(&entry.path(), false)
                .ok()
                .and_then(|directory| recovery(&directory).ok())
                .is_some()
        {
            candidates.push(entry.path());
        }
    }
    for root in candidates {
        if *attempts == 0 {
            break;
        }
        *attempts -= 1;
        // A failed recovery retains the root and contributes to the ordinary admission bound.
        let _ = recover_root(&root).await;
    }
    Ok(())
}

async fn recover_root(root: &Path) -> Result<(), ToolError> {
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(ToolError::Path)?;
    let uuid = name.strip_prefix(UNIT_ROOT_PREFIX).ok_or(ToolError::Path)?;
    if uuid::Uuid::parse_str(uuid).is_err() {
        return Err(ToolError::Path);
    }
    let directory = open_directory(root, false).map_err(|_| ToolError::Path)?;
    let lock = super::super::admission::open_file(&directory.dir, UNIT_LIVENESS_LOCK, false)?
        .ok_or(ToolError::Path)?;
    if lock.metadata().map_err(|_| ToolError::Path)?.len() != 0 {
        return Err(ToolError::Path);
    }
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    let initial = recovery(&directory)?;
    let same_boot = initial.boot == ffi::boot_session()?;
    if same_boot {
        manager(
            &format!("user/{}", rustix::process::geteuid().as_raw()),
            &format!("dev.arany.tool.{name}"),
            ManagerAction::Bootout,
        )
        .await?;
        // A bootstrapping supervisor may publish its identity while bootout is running.
        let current = recovery(&directory)?;
        if current.boot != initial.boot
            || current.root != initial.root
            || (initial.supervisor.is_some() && initial.supervisor != current.supervisor)
        {
            return Err(ToolError::ChangedInput);
        }
        if let Some(supervisor) = current.supervisor {
            if supervisor.coalition == identity(std::process::id())?.coalition {
                return Err(ToolError::ProtectionUnavailable);
            }
            Scope::new(supervisor.coalition)?.stop(None).await?;
        }
    }
    cleanup_storage(root).await?;
    remove_root(root, &directory)?;
    if root.exists() {
        return Err(ToolError::Operation);
    }
    Ok(())
}

// Only an unlocked liveness lock proves its owner finished or crashed; a missing lock may be unpublished.
fn is_retained(root: &Path) -> Result<bool, ToolError> {
    let directory = match open_directory(root, false) {
        Ok(directory) => directory,
        Err(crate::private_fs::Error::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(false);
        }
        Err(_) => return Err(ToolError::Path),
    };
    let Some(lock) = super::super::admission::open_file(&directory.dir, UNIT_LIVENESS_LOCK, false)?
    else {
        return Ok(false);
    };
    if lock.metadata().map_err(|_| ToolError::Path)?.len() != 0 {
        return Err(ToolError::Path);
    }
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

async fn manager(
    domain: &str,
    label: &str,
    action: ManagerAction<'_>,
) -> Result<Vec<u8>, ToolError> {
    if !system_program("/bin/launchctl")? {
        return Err(ToolError::ProtectionUnavailable);
    }
    let bootout = matches!(action, ManagerAction::Bootout);
    let mut command = Command::new("/bin/launchctl");
    command.env_clear().current_dir("/");
    match action {
        ManagerAction::Bootstrap(path) => {
            command.arg("bootstrap").arg(domain).arg(path);
        }
        ManagerAction::Kickstart => {
            command.args(["kickstart", "-p", &format!("{domain}/{label}")]);
        }
        ManagerAction::Bootout => {
            command.args(["bootout", &format!("{domain}/{label}")]);
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
            .map_err(|_| ToolError::GuardRejected(GuardStage::NativeManager))?,
        4096,
        Duration::from_secs(3),
    )
    .await
    .map_err(|_| ToolError::GuardRejected(GuardStage::NativeManager))?;
    if status.success() || (bootout && status.code() == Some(libc::ESRCH)) {
        Ok(stdout)
    } else {
        Err(ToolError::GuardRejected(GuardStage::NativeManager))
    }
}

impl Unit {
    #[cfg(test)]
    async fn new(fixture: &Path) -> Result<Self, ToolError> {
        let directory =
            open_directory(&fixture.join("guard"), true).map_err(|_| ToolError::Path)?;
        let permit = super::super::admission::acquire_in(
            &directory,
            fixture,
            uuid::Uuid::now_v7(),
            u64::MAX,
            |_| {},
            crate::tools::now_ms,
        )
        .await?;
        Self::create(permit, fixture)
    }

    async fn admit(
        runtime: &ToolRuntime,
        intent: &EffectIntent,
    ) -> Result<(Self, Option<crate::tools::ActiveToolWindow>), ToolError> {
        let capture = FixtureSpan::begin(FixtureStage::Admission);
        let permit =
            super::super::admission::acquire(intent.id, intent.limits.runtime_ms, |queued| {
                runtime.queue_changed(intent.id, queued)
            })
            .await?;
        let admitted_at = tokio::time::Instant::now();
        let active_window = runtime.begin_active_action(admitted_at).await?;
        let mut unit = Self::create(permit, Path::new(UNIT_ROOT_PARENT))?;
        unit.limits = Some(intent.limits.clone());
        capture.complete();
        Ok((unit, active_window))
    }

    fn create(permit: super::super::admission::Permit, parent: &Path) -> Result<Self, ToolError> {
        let name = format!("{UNIT_ROOT_PREFIX}{}", permit.id);
        let root = parent.join(&name);
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
        let unit = Self {
            root,
            _directory: directory,
            _liveness: liveness,
            domain: format!("user/{}", rustix::process::geteuid().as_raw()),
            label: format!("dev.arany.tool.{name}"),
            scope: None,
            limits: None,
            stopped: false,
            _permit: Some(permit),
        };
        write_record(
            &unit._directory.dir,
            RECOVERY_RECORD,
            &Recovery {
                version: 1,
                root: crate::tools::fs::identity(&unit._directory.dir)?,
                boot: ffi::boot_session()?,
                supervisor: None,
            },
        )?;
        Ok(unit)
    }

    fn write_job(&self, arguments: &[&str], stderr: &str) -> Result<PathBuf, ToolError> {
        if arguments.len() > 16
            || arguments
                .iter()
                .copied()
                .chain(std::iter::once(stderr))
                .any(|value| value.len() > MAX_ARGUMENT_BYTES || value.contains('\0'))
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
        manager(&self.domain, &self.label, action).await
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
        let mut arguments = vec!["/usr/bin/env", "-i"];
        #[cfg(debug_assertions)]
        let fixture_environment = super::fixture::directory().and_then(|directory| {
            let path = directory.path.to_str()?;
            let account = directory.path.parent()?.to_str()?;
            Some([
                format!("ARANY_TEST_ACCOUNT_ROOT={account}"),
                format!("ARANY_TEST_GUARD_TRACE={path}"),
            ])
        });
        #[cfg(debug_assertions)]
        if let Some(environment) = &fixture_environment {
            arguments.extend(environment.iter().map(String::as_str));
        }
        arguments.extend([
            runtime.guard_executable.to_str().ok_or(ToolError::Path)?,
            "--internal-native-tool-unit",
            socket.to_str().ok_or(ToolError::Path)?,
        ]);
        // The supervisor's stdout and stderr carry only its bounded panic text; a tail names a silent exit.
        super::super::admission::open_file(&self._directory.dir, WORKER_OUTPUT, true)?
            .ok_or(ToolError::Path)?;
        let output = self.root.join(WORKER_OUTPUT);
        let path = self.write_job(&arguments, output.to_str().ok_or(ToolError::Path)?)?;
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
        let limits = self
            .limits
            .clone()
            .ok_or(ToolError::ProtectionUnavailable)?;
        let mut scope = Scope::new(launched.coalition)?;
        scope.apply_limits(limits);
        self.scope = Some(scope);
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
        let record = recovery(&self._directory)?;
        if record.boot != ffi::boot_session()? || record.supervisor != Some(child) {
            return Err(ToolError::ProtectionUnavailable);
        }
        Ok(stream.into_inner())
    }

    fn worker_output(&self) -> Vec<u8> {
        use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        self._directory
            .dir
            .open_with(WORKER_OUTPUT, &options)
            .ok()
            .filter(|file| file.metadata().is_ok_and(|metadata| metadata.is_file()))
            .and_then(|mut file| super::super::read_tail(&mut file).ok())
            .unwrap_or_default()
    }

    async fn stop(&mut self) -> Result<(), ToolError> {
        if self.stopped {
            return Ok(());
        }
        let capture = FixtureSpan::begin(FixtureStage::ControllerCoalition);
        let cleanup = if let Some(scope) = &mut self.scope {
            scope.stop(None).await
        } else {
            Ok(())
        };
        capture.finish(cleanup.is_ok());
        let capture = FixtureSpan::begin(FixtureStage::ControllerBootout);
        let removal = self.manager(ManagerAction::Bootout).await;
        capture.finish(removal.is_ok());
        cleanup?;
        removal?;
        let capture = FixtureSpan::begin(FixtureStage::ControllerStorage);
        cleanup_storage(&self.root).await?;
        capture.complete();
        self.stopped = true;
        Ok(())
    }
}

fn remove_root(root: &Path, admitted: &PrivateDirectory) -> Result<(), ToolError> {
    if storage_mounted(root)?
        || ffi::mounts()?
            .iter()
            .any(|(_, target)| target.starts_with(root))
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    let current = open_directory(root, false).map_err(|_| ToolError::Path)?;
    if crate::tools::fs::identity(&current.dir)? != crate::tools::fs::identity(&admitted.dir)? {
        return Err(ToolError::ChangedInput);
    }
    let parent = crate::tools::fs::open_directory(root.parent().ok_or(ToolError::Path)?)?;
    parent
        .remove_dir_all(root.file_name().ok_or(ToolError::Path)?)
        .map_err(|_| ToolError::Operation)
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
        if self.stopped {
            let _ = remove_root(&self.root, &self._directory);
        }
    }
}

fn action_deadlines(
    active_window: Option<crate::tools::ActiveToolWindow>,
    intent: &EffectIntent,
    fallback_started: tokio::time::Instant,
) -> (
    tokio::time::Instant,
    tokio::time::Instant,
    tokio::time::Instant,
) {
    let selection_time = if primary_command(intent) {
        crate::tools::selection::PHASE_TIME
    } else {
        Duration::ZERO
    };
    let preparation_deadline = active_window.map_or(
        fallback_started + CONNECT_DEADLINE + PREPARATION_DEADLINE + selection_time,
        |window| window.preparation_deadline,
    );
    let go_deadline = active_window.map_or(
        preparation_deadline
            + Duration::from_millis(u64::from(intent.limits.cache_transfer_ms))
            + super::super::super::worker::transfer_time(intent),
        |window| window.go_deadline,
    );
    let go_deadline = if super::super::super::worker::transfer_time(intent).is_zero() {
        go_deadline
    } else {
        go_deadline.min(fallback_started + Duration::from_millis(u64::from(WORKER_PREPARATION_MS)))
    };
    let completion_deadline = active_window.map_or_else(
        || {
            fallback_started
                + CONNECT_DEADLINE
                + PREPARATION_DEADLINE
                + selection_time
                + Duration::from_millis(
                    u64::from(intent.limits.cache_transfer_ms).saturating_mul(2),
                )
                + Duration::from_millis(u64::from(intent.limits.runtime_ms))
                + super::super::super::worker::transfer_overhead(intent)
                + Duration::from_secs(10)
        },
        |window| window.completion_deadline,
    );
    (preparation_deadline, go_deadline, completion_deadline)
}

#[expect(clippy::too_many_lines, reason = "frozen until C2")]
pub(super) async fn execute(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    mut cancellation: Cancellation,
    dispatched: &AtomicBool,
) -> Result<ToolObservation, ToolError> {
    let (mut unit, active_window) = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(ToolError::Operation),
        unit = Unit::admit(runtime, intent) => unit?,
    };
    crate::tools::fs::reject_overlap(&unit.root, &runtime.workspace)?;
    let mut cache = super::super::build_cache_session(runtime, intent)?;
    let cache_transfer = cache.as_ref().map(|session| session.transfer.clone());
    if let Some(private) = &runtime.worker {
        private.contained.store(false, Ordering::Release);
    }
    let (preparation_deadline, go_deadline, completion_deadline) =
        action_deadlines(active_window, intent, tokio::time::Instant::now());
    let go_cancellation = cancellation.clone();
    let execution = async {
        let capture = FixtureSpan::begin(FixtureStage::Connect);
        let mut stream = BufReader::new(
            tokio::time::timeout(CONNECT_DEADLINE, unit.connect(runtime))
                .await
                .map_err(|_| ToolError::GuardRejected(GuardStage::ConnectionDeadline))??,
        );
        capture.complete();
        let capture = FixtureSpan::begin(FixtureStage::ControllerReady);
        let primary_command = primary_command(intent);
        let payload_phase = AtomicBool::new(false);
        let selecting = AtomicBool::new(false);
        let watcher = Mutex::new(Scope::new(
            unit.scope
                .as_ref()
                .ok_or(ToolError::ProtectionUnavailable)?
                .coalition,
        )?);
        let phases = async {
            let task = super::super::encode_task(&Task::new(runtime, intent, cache_transfer)?)?;
            let mut bytes = tokio::time::timeout_at(preparation_deadline, async {
                stream
                    .get_mut()
                    .write_all(&task)
                    .await
                    .map_err(|_| ToolError::Operation)?;
                read_line_capped(&mut stream, 2048).await
            })
            .await
            .map_err(|_| ToolError::GuardRejected(GuardStage::PreparationDeadline))??;
            if bytes == b"SELECT_READY\n" {
                selecting.store(true, Ordering::Release);
                stream
                    .get_mut()
                    .write_all(b"SELECT\n")
                    .await
                    .map_err(|_| ToolError::Operation)?;
                let deadline = preparation_deadline
                    .min(tokio::time::Instant::now() + crate::tools::selection::PHASE_TIME);
                stream
                    .get_mut()
                    .write_all(&super::super::selection_phase::encode_deadline(deadline)?)
                    .await
                    .map_err(|_| ToolError::Operation)?;
                let mut selection_cancel = go_cancellation.clone();
                super::super::selection_phase::wait(
                    deadline,
                    &mut selection_cancel,
                    super::super::selection_phase::receive(&mut stream, deadline),
                )
                .await?;
                selecting.store(false, Ordering::Release);
                bytes = tokio::time::timeout_at(go_deadline, read_line_capped(&mut stream, 2048))
                    .await
                    .map_err(|_| ToolError::Limit)??;
            }
            let mut cache_sent = false;
            let mut image_sent = false;
            for _ in 0..2 {
                if bytes == b"CACHE_READY\n" && !cache_sent {
                    cache_sent = true;
                    let session = cache.as_ref().ok_or(ToolError::Operation)?;
                    let transfer_deadline =
                        super::super::cache_transfer_deadline(intent, Some(go_deadline));
                    tokio::time::timeout_at(
                        transfer_deadline,
                        super::super::send_cache_archive(stream.get_mut(), &session.lease),
                    )
                    .await
                    .map_err(|_| ToolError::Limit)??;
                    bytes =
                        tokio::time::timeout_at(go_deadline, read_line_capped(&mut stream, 2048))
                            .await
                            .map_err(|_| {
                                ToolError::GuardRejected(GuardStage::PreparationDeadline)
                            })??;
                } else if bytes == b"WORKER_READY\n" && !image_sent {
                    image_sent = true;
                    let input = super::super::super::worker::bulk_input(runtime)
                        .ok_or(ToolError::Configuration)?;
                    tokio::time::timeout_at(
                        go_deadline,
                        super::super::super::worker::send_frame(stream.get_mut(), input),
                    )
                    .await
                    .map_err(|_| ToolError::Limit)??;
                    bytes =
                        tokio::time::timeout_at(go_deadline, read_line_capped(&mut stream, 2048))
                            .await
                            .map_err(|_| ToolError::Limit)??;
                } else {
                    break;
                }
            }
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
            capture.complete();
            let capture = FixtureSpan::begin(FixtureStage::ControllerReceipt);
            runtime
                .before_dispatch(active_window, intent.limits.runtime_ms, &go_cancellation)
                .await?;
            payload_phase.store(true, Ordering::Release);
            dispatched.store(true, Ordering::Release);
            let observation = async {
                stream
                    .get_mut()
                    .write_all(b"GO\n")
                    .await
                    .map_err(|_| ToolError::Operation)?;
                if primary_command {
                    use tokio::io::AsyncBufReadExt;
                    if stream
                        .fill_buf()
                        .await
                        .map_err(|_| ToolError::Operation)?
                        .first()
                        == Some(&b'C')
                    {
                        let marker = read_line_capped(&mut stream, 64).await?;
                        if marker != b"CAPTURE_READY\n" {
                            return Err(ToolError::Operation);
                        }
                        watcher
                            .lock()
                            .map_err(|_| ToolError::ProtectionUnavailable)?
                            .begin_capture(&intent.limits)?;
                        selecting.store(true, Ordering::Release);
                        payload_phase.store(false, Ordering::Release);
                        let (deadline, report_deadline) =
                            super::super::selection_phase::capture_deadlines(completion_deadline)?;
                        stream
                            .get_mut()
                            .write_all(b"CAPTURE\n")
                            .await
                            .map_err(|_| ToolError::Operation)?;
                        stream
                            .get_mut()
                            .write_all(&super::super::selection_phase::encode_deadline(deadline)?)
                            .await
                            .map_err(|_| ToolError::Operation)?;
                        let done = tokio::time::timeout_at(
                            report_deadline,
                            read_line_capped(&mut stream, 64),
                        )
                        .await
                        .map_err(|_| ToolError::Timeout(String::new()))??;
                        if done != b"CAPTURE_DONE\n" {
                            return Err(ToolError::Operation);
                        }
                        selecting.store(false, Ordering::Release);
                        payload_phase.store(true, Ordering::Release);
                    }
                }
                let capture_result = async {
                    if let Some(session) = cache.as_ref().filter(|_| runtime.worker.is_none()) {
                        let observation =
                            super::super::read_observation_line(&mut stream, intent).await?;
                        let update_deadline = super::super::cache_transfer_deadline(
                            intent,
                            active_window.map(|window| window.completion_deadline),
                        );
                        let candidate = match tokio::time::timeout_at(
                            update_deadline,
                            super::super::receive_cache_candidate(&mut stream, &session.lease),
                        )
                        .await
                        {
                            Ok(Ok(candidate)) => candidate,
                            Ok(Err(_)) | Err(_) => super::super::CacheCandidate::Uncertain,
                        };
                        if candidate == super::super::CacheCandidate::Uncertain {
                            return Ok((observation, candidate));
                        }
                        let cache_status = tokio::time::timeout_at(
                            completion_deadline,
                            read_line_capped(&mut stream, 32),
                        )
                        .await
                        .map_err(|_| ToolError::Limit)??;
                        match cache_status.as_slice() {
                            b"CACHE_DONE\n" => {}
                            b"CACHE_FAILED\n" => {
                                let failure = tokio::time::timeout_at(
                                    completion_deadline,
                                    read_line_capped(&mut stream, 256),
                                )
                                .await
                                .map_err(|_| ToolError::GuardRejected(GuardStage::StorageCleanup))?
                                .ok()
                                .and_then(|bytes| {
                                    serde_json::from_slice::<BootstrapFailure>(&bytes).ok()
                                });
                                return Err(failure.map_or(
                                    ToolError::GuardRejected(GuardStage::StorageCleanup),
                                    ToolError::GuardBootstrap,
                                ));
                            }
                            _ => return Err(ToolError::Operation),
                        }
                        Ok((observation, candidate))
                    } else if runtime.worker.is_some() {
                        super::super::read_worker_observation(
                            &mut stream,
                            runtime,
                            intent,
                            Duration::from_millis(u64::from(intent.limits.runtime_ms))
                                + super::super::super::worker::transfer_time(intent)
                                + TEARDOWN_WAIT,
                        )
                        .await
                        .map(|observation| (observation, super::super::CacheCandidate::Received))
                    } else {
                        super::super::read_worker_report(&mut stream, intent)
                            .await
                            .map(|observation| {
                                (observation, super::super::CacheCandidate::Received)
                            })
                    }
                };
                capture_result.await
            }
            .await;
            capture.complete();
            observation
        };
        tokio::pin!(phases);
        let mut samples = tokio::time::interval(super::SUPERVISION_INTERVAL);
        loop {
            tokio::select! {
                biased;
                result = &mut phases => break result,
                _ = samples.tick() => {
                    if !payload_phase.load(Ordering::Acquire) {
                        let checked = watcher
                            .lock()
                            .map_err(|_| ToolError::ProtectionUnavailable)
                            .and_then(|mut watcher| {
                                let limits = if selecting.load(Ordering::Acquire) {
                                    selection_limits(&intent.limits)
                                } else {
                                    intent.limits.clone()
                                };
                                watcher.apply_limits(limits);
                                watcher.check_resources()
                            });
                        if let Err(error) = checked { break Err(error); }
                    }
                }
            }
        }
    };
    let result = if let Some(window) = active_window {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(super::super::execution_failure(super::super::GuardCheck::Cancelled)),
            result = tokio::time::timeout_at(window.completion_deadline, execution) => result.unwrap_or(Err(ToolError::Limit)),
        }
    } else {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(super::super::execution_failure(super::super::GuardCheck::Cancelled)),
            result = tokio::time::timeout_at(completion_deadline, execution) => result.unwrap_or(Err(ToolError::Limit)),
        }
    };
    tokio::time::timeout_at(completion_deadline, unit.stop())
        .await
        .map_err(|_| ToolError::GuardRejected(GuardStage::ProcessCleanup))?
        .map_err(|_| ToolError::GuardRejected(GuardStage::ProcessCleanup))?;
    let result =
        result.map_err(|error| super::super::with_worker_exit(error, &unit.worker_output()));
    if let Some(private) = &runtime.worker {
        private.contained.store(true, Ordering::Release);
    }
    match result {
        Ok((mut observation, candidate)) => {
            super::super::publish_cache_candidate(
                &mut observation,
                candidate,
                cache.take(),
                completion_deadline,
            )
            .await;
            Ok(observation)
        }
        Err(error) => Err(error),
    }
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

#[expect(clippy::too_many_lines, reason = "frozen until C2")]
pub(super) fn helper_main() -> std::process::ExitCode {
    super::super::bound_worker_output();
    let mut diagnostic = None;
    let mut stage = super::super::BootstrapStage::Admission;
    let result = (|| -> Result<(), ToolError> {
        bootstrap()?;
        let path = std::env::args_os().nth(2).ok_or(ToolError::Configuration)?;
        let path = Path::new(&path);
        let (root, mut stream) = connect_control(path)?;
        diagnostic = Some(stream.try_clone().map_err(|_| ToolError::Operation)?);
        stage = super::super::BootstrapStage::Capabilities;
        let parent = peer(&stream)?;
        let own = identity(std::process::id())?;
        if identity(parent)?.coalition == own.coalition {
            return Err(ToolError::ProtectionUnavailable);
        }
        let mut scope = Scope::new(own.coalition)?;
        let mut record = recovery(&root)?;
        if record.boot != ffi::boot_session()? || record.supervisor.is_some() {
            return Err(ToolError::ProtectionUnavailable);
        }
        record.supervisor = Some(own);
        write_record(&root.dir, RECOVERY_RECORD, &record)?;
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
        if let Some(cache) = &task.cache {
            cache.validate(&task.intent, &task.config)?;
        }
        stage = super::super::BootstrapStage::Capabilities;
        if task.guard_executable
            != std::env::current_exe().map_err(|_| ToolError::ProtectionUnavailable)?
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let worker_input = if task.worker.is_some() || task.handover.is_some() {
            stream
                .write_all(b"WORKER_READY\n")
                .map_err(|_| ToolError::Operation)?;
            let bytes = super::super::super::worker::read_frame(&mut stream)?;
            if let Some(transfer) = &task.worker {
                transfer.decode(&bytes)?;
            } else {
                super::super::super::worker::decode_handover(&task.intent, &bytes, &task.config)?;
            }
            Some(bytes)
        } else {
            None
        };
        let (runtime, intent, cache) = task.into_runtime(worker_input);
        scope.apply_limits(intent.limits.clone());
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
            let cancellation = Cancellation::new();
            let mut reader = None;
            let mut storage: Option<Vec<cap_std::fs::Dir>> = None;
            let mut command_before = None;
            let (capture_go, mut receive_capture) = tokio::sync::oneshot::channel();
            let result = async {
                stage = super::super::BootstrapStage::Workspace;
                let capture = FixtureSpan::begin(FixtureStage::SupervisorStorage);
                storage = if runtime.worker.is_some() && !intent.call.is_process() {
                    Some(
                        super::storage::prepare_worker_storage(
                            path.parent().ok_or(ToolError::Path)?,
                            &intent.limits,
                        )
                        .await?,
                    )
                } else if intent.call.is_process() {
                    Some(
                        prepare_command_storage(
                            path.parent().ok_or(ToolError::Path)?,
                            &runtime,
                            &intent.limits,
                            primary_command(&intent),
                        )
                        .await?,
                    )
                } else {
                    None
                };
                if primary_command(&intent) {
                    output
                        .write_all(b"SELECT_READY\n")
                        .await
                        .map_err(|_| ToolError::Operation)?;
                    let mut select = [0; 7];
                    input
                        .read_exact(&mut select)
                        .await
                        .map_err(|_| ToolError::Operation)?;
                    if select != *b"SELECT\n" {
                        return Err(ToolError::Configuration);
                    }
                    let mut frame = [0; 8];
                    input
                        .read_exact(&mut frame)
                        .await
                        .map_err(|_| ToolError::Operation)?;
                    let deadline = super::super::selection_phase::decode_deadline(frame)?;
                    let source = crate::tools::fs::open_directory(&runtime.workspace)?;
                    let resources =
                        crate::tools::fs::open_directory(path.parent().ok_or(ToolError::Path)?)?;
                    let target = storage
                        .as_ref()
                        .and_then(|volumes| volumes.first())
                        .ok_or(ToolError::Operation)?;
                    let selected = crate::tools::fs::prepare_command(
                        &source,
                        target,
                        &resources,
                        &runtime.config,
                        deadline,
                    );
                    let message = super::super::selection_phase::response(&selected);
                    output
                        .write_all(message)
                        .await
                        .map_err(|_| ToolError::Operation)?;
                    selected?;
                }
                if let Some(cache) = &cache {
                    output
                        .write_all(b"CACHE_READY\n")
                        .await
                        .map_err(|_| ToolError::Operation)?;
                    let mut length = [0; 8];
                    input
                        .read_exact(&mut length)
                        .await
                        .map_err(|_| ToolError::Operation)?;
                    let length = u64::from_le_bytes(length);
                    if length > cache.view_bytes {
                        return Err(ToolError::Limit);
                    }
                    if length > 0 {
                        let volumes = storage.as_ref().ok_or(ToolError::Operation)?;
                        let mut archive = (&mut input).take(length);
                        crate::store::restore_archive_async_with_limit(
                            &mut archive,
                            volumes.get(2).ok_or(ToolError::Operation)?,
                            &cache.identity,
                            &cache.exclusions,
                            cache.view_bytes,
                        )
                        .await
                        .map_err(super::super::map_cache_error)?;
                        drop(archive);
                    }
                }
                let (go, receive_go) = tokio::sync::oneshot::channel();
                let watched = cancellation.clone();
                reader = Some(tokio::spawn(async move {
                    let mut bytes = [0; 3];
                    if input.read_exact(&mut bytes).await.is_ok() && bytes == *b"GO\n" {
                        let _ = go.send(());
                        let mut capture = [0; 8];
                        if input.read_exact(&mut capture).await.is_ok() && capture == *b"CAPTURE\n"
                        {
                            let mut frame = [0; 8];
                            if input.read_exact(&mut frame).await.is_ok()
                                && let Ok(deadline) =
                                    super::super::selection_phase::decode_deadline(frame)
                            {
                                let _ = capture_go.send(deadline);
                            }
                            let _ = input.read(&mut [0; 1]).await;
                        }
                    }
                    watched.cancel();
                }));
                if primary_command(&intent) {
                    let root = path.parent().ok_or(ToolError::Path)?;
                    command_before = Some((
                        crate::tools::fs::command_snapshot(&root.join("workspace"))?,
                        crate::tools::fs::CommandSelection::read(root)?,
                    ));
                }
                capture.complete();
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
            let capture = FixtureSpan::begin(FixtureStage::SupervisorCoalition);
            let teardown = scope
                .teardown
                .unwrap_or_else(|| tokio::time::Instant::now() + TEARDOWN_WAIT);
            let cleanup = scope.stop_until(Some(std::process::id()), teardown).await;
            capture.finish(cleanup.is_ok());
            let capture_started = cleanup.is_ok() && command_before.is_some() && result.is_ok();
            let mut capture_deadline = None;
            let capture_handshake = async {
                if capture_started {
                    output
                        .write_all(b"CAPTURE_READY\n")
                        .await
                        .map_err(|_| ToolError::Operation)?;
                    capture_deadline = Some(
                        tokio::time::timeout(
                            crate::tools::selection::PHASE_TIME,
                            &mut receive_capture,
                        )
                        .await
                        .map_err(|_| ToolError::Timeout(String::new()))?
                        .map_err(|_| ToolError::Operation)?,
                    );
                    scope.begin_capture(&intent.limits)?;
                }
                Ok(())
            }
            .await;
            let result = result.map(|mut worker| {
                if let Err(error) = capture_handshake {
                    let (disposition, output, cause, limit_cause) =
                        crate::tools::dispatch::outcome(&intent, Err(error));
                    worker.observation.disposition = disposition;
                    worker.observation.output = output;
                    worker.observation.enforcement_cause = cause;
                    worker.observation.limit_cause = limit_cause;
                    command_before = None;
                }
                worker
            });
            let mut result = result.and_then(
                |super::WorkerOutput {
                     mut observation,
                     windows,
                 }| {
                    observation.cache_update =
                        if runtime.worker.is_some() && intent.call.is_process() {
                            crate::tools::CacheUpdate::Discarded
                        } else if cache.is_some() {
                            crate::tools::CacheUpdate::NotAttempted
                        } else {
                            crate::tools::CacheUpdate::NotApplicable
                        };
                    if cleanup.is_ok()
                        && let Some((before, selection)) = &command_before
                        && let Ok(command) =
                            serde_json::from_str::<CommandResult>(&observation.output)
                    {
                        let captured = if observation.disposition == ToolDisposition::Limit {
                            Ok((crate::tools::CommandChangeset::default(), selection.summary))
                        } else {
                            crate::tools::fs::selected_command_changeset(
                                &path.parent().ok_or(ToolError::Path)?.join("workspace"),
                                before,
                                selection,
                                capture_deadline.ok_or(ToolError::Operation)?,
                            )
                        }
                        .and_then(|(changeset, summary)| {
                            let windows = windows.ok_or(ToolError::Operation)?;
                            crate::tools::process::fit_selected_command(
                                command.exit_code,
                                &windows.stdout,
                                &windows.stderr,
                                changeset,
                                intent.limits.result_bytes as usize,
                                if runtime.hook_stdin.is_some() {
                                    crate::tools::process::Truncation::Refuse
                                } else {
                                    crate::tools::process::Truncation::Allow
                                },
                                summary,
                            )
                        });
                        match captured {
                            Ok(output) => {
                                observation.output = output;
                                if !observation.valid() {
                                    observation.disposition = ToolDisposition::Limit;
                                    observation.limit_cause =
                                        Some(crate::tools::LimitCause::Resource);
                                    observation.enforcement_cause = None;
                                    observation.output =
                                        "Command changeset exceeded its reserved capacity".into();
                                }
                            }
                            Err(error) => {
                                let (disposition, _, cause, limit_cause) =
                                    crate::tools::dispatch::outcome(
                                        &observation.intent,
                                        Err(error),
                                    );
                                observation.disposition = disposition;
                                observation.enforcement_cause = cause;
                                observation.limit_cause = limit_cause;
                                observation.output =
                                    "Command changeset could not be captured".into();
                            }
                        }
                    }
                    if cleanup.is_ok()
                        && intent.call.is_process()
                        && let Some(action) = &runtime.worker
                    {
                        let before = action.transfer.decode(&action.input)?;
                        let (image, capture) = if observation.disposition
                            == ToolDisposition::Succeeded
                        {
                            crate::tools::worker::command_capture(
                                &before,
                                &crate::tools::fs::open_directory(
                                    &path.parent().ok_or(ToolError::Path)?.join("workspace/tree"),
                                )?,
                                &action.transfer.grant,
                            )
                        } else {
                            (before, crate::session::worker::PrivateCapture::None {})
                        };
                        let mut command: crate::session::worker::WorkerCommandResult =
                            serde_json::from_str(&observation.output)
                                .map_err(|_| ToolError::Operation)?;
                        command.private_capture = capture;
                        observation.output =
                            serde_json::to_string(&command).map_err(|_| ToolError::Operation)?;
                        *action.output.lock().map_err(|_| ToolError::Operation)? =
                            Some(image.encode()?);
                    }
                    Ok(observation)
                },
            );
            if capture_started && output.write_all(b"CAPTURE_DONE\n").await.is_err() {
                result = Err(ToolError::Operation);
            }
            let mut response_sent = false;
            let mut cache_transfer_failed = false;
            if let (Some(cache), Some(volumes), Ok(observation)) = (&cache, &storage, &result)
                && runtime.worker.is_none()
            {
                let transfer = async {
                    let bytes =
                        serde_json::to_vec(observation).map_err(|_| ToolError::Operation)?;
                    output
                        .write_all(&bytes)
                        .await
                        .map_err(|_| ToolError::Operation)?;
                    output
                        .write_all(b"\n")
                        .await
                        .map_err(|_| ToolError::Operation)?;
                    response_sent = true;
                    if cleanup.is_ok() && observation.disposition == ToolDisposition::Succeeded {
                        if cache.candidate_bytes == 0 {
                            output
                                .write_all(b"CACHE_REJECTED\n")
                                .await
                                .map_err(|_| ToolError::Operation)?;
                        } else {
                            match crate::store::archive_size_with_limit(
                                volumes.get(2).ok_or(ToolError::Operation)?,
                                &cache.identity,
                                &cache.exclusions,
                                cache.candidate_bytes,
                            ) {
                                Ok(size) => {
                                    output
                                        .write_all(b"CACHE_CANDIDATE\n")
                                        .await
                                        .map_err(|_| ToolError::Operation)?;
                                    output
                                        .write_all(&size.to_le_bytes())
                                        .await
                                        .map_err(|_| ToolError::Operation)?;
                                    let captured = crate::store::capture_archive_async_with_limit(
                                        volumes.get(2).ok_or(ToolError::Operation)?,
                                        &cache.identity,
                                        &cache.exclusions,
                                        &mut output,
                                        cache.candidate_bytes,
                                    )
                                    .await
                                    .map_err(super::super::map_cache_error)?;
                                    if captured != size {
                                        return Err(ToolError::Operation);
                                    }
                                }
                                Err(crate::store::StoreError::StorageFull) => {
                                    output
                                        .write_all(b"CACHE_REJECTED\n")
                                        .await
                                        .map_err(|_| ToolError::Operation)?;
                                }
                                Err(_) => {
                                    output
                                        .write_all(b"CACHE_FAILED\n")
                                        .await
                                        .map_err(|_| ToolError::Operation)?;
                                }
                            }
                        }
                    } else {
                        output
                            .write_all(b"CACHE_REJECTED\n")
                            .await
                            .map_err(|_| ToolError::Operation)?;
                    }
                    Ok::<(), ToolError>(())
                }
                .await;
                cache_transfer_failed = transfer.is_err();
            }
            drop(storage);
            let mut storage_check = None;
            let storage_cleanup = if cleanup.is_ok() {
                let capture = FixtureSpan::begin(FixtureStage::SupervisorDetach);
                let result =
                    cleanup_storage_until(path.parent().ok_or(ToolError::Path)?, teardown).await;
                capture.finish(result.is_ok());
                result.map_err(|failed| {
                    storage_check = Some(failed.check);
                    failed.error
                })
            } else {
                Err(ToolError::ProtectionUnavailable)
            };
            if let Some(reader) = reader {
                reader.abort();
                let _ = reader.await;
            }
            if cache_transfer_failed {
                output.shutdown().await.map_err(|_| ToolError::Operation)?;
                return Err(ToolError::Operation);
            }
            let teardown_stage = if cleanup.is_err() {
                Some(super::super::BootstrapStage::ProcessStop)
            } else if storage_cleanup.is_err() {
                Some(super::super::BootstrapStage::StorageCleanup)
            } else {
                match &result {
                    Err(ToolError::GuardRejected(GuardStage::LauncherReap)) => {
                        Some(super::super::BootstrapStage::LauncherReap)
                    }
                    Err(ToolError::GuardRejected(GuardStage::OutputEof)) => {
                        Some(super::super::BootstrapStage::StderrDrain)
                    }
                    _ => None,
                }
            };
            let result = match (cleanup.and(storage_cleanup).and(result), teardown_stage) {
                (Err(_), Some(stage)) => {
                    let check = storage_check
                        .filter(|_| matches!(stage, super::super::BootstrapStage::StorageCleanup));
                    Err(ToolError::GuardBootstrap(BootstrapFailure {
                        stage,
                        check,
                        detail: None,
                    }))
                }
                (result, _) => result,
            };
            if response_sent {
                let bytes = match &result {
                    Ok(_) => b"CACHE_DONE\n".to_vec(),
                    Err(error) => {
                        let failure = match error {
                            ToolError::GuardBootstrap(failure) => serde_json::to_vec(failure),
                            _ => serde_json::to_vec(&BootstrapFailure::at(
                                super::super::BootstrapStage::StorageCleanup,
                            )),
                        }
                        .map_err(|_| ToolError::Operation)?;
                        [&b"CACHE_FAILED\n"[..], failure.as_slice(), &b"\n"[..]].concat()
                    }
                };
                output
                    .write_all(&bytes)
                    .await
                    .map_err(|_| ToolError::Operation)?;
            } else {
                let bytes = match &result {
                    Ok(observation) => serde_json::to_vec(observation),
                    Err(ToolError::GuardBootstrap(failure)) => serde_json::to_vec(failure),
                    Err(_) => serde_json::to_vec(&BootstrapFailure::at(stage.clone())),
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
            }
            if result.is_ok()
                && let Some(action) = &runtime.worker
            {
                let bytes = action
                    .output
                    .lock()
                    .map_err(|_| ToolError::Operation)?
                    .take()
                    .ok_or(ToolError::Operation)?;
                super::super::super::worker::send_frame(&mut output, &bytes).await?;
            }
            output.shutdown().await.map_err(|_| ToolError::Operation)?;
            result.map(|_| ())
        })
    })();
    if let Err(error) = &result
        && let Some(stream) = &mut diagnostic
    {
        let failure = match error {
            ToolError::GuardBootstrap(failure) => serde_json::to_vec(failure),
            _ => serde_json::to_vec(&BootstrapFailure::at(stage)),
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
    #[tokio::test]
    async fn selection_protocol_preserves_outcomes_and_deadline() {
        crate::tools::guard::selection_phase::tests::protocol_outcomes().await;
    }

    use super::super::storage::{STORAGE_BYTES_CAP, STORAGE_PENDING, prepare_storage};
    use super::super::{ProfileKind, SYS_COALITION_INFO, SYS_PERSONA, SeatbeltProfile};
    use super::*;
    use crate::tools::ToolCall;
    use crate::tools::guard::read_capped;
    use std::io::Read;

    #[test]
    fn capture_cpu_excludes_prior_work_only_for_the_same_supervisor() {
        let supervisor = Identity {
            pid: 42,
            version: 1,
            unique_id: 7,
            coalition: 3,
        };
        let prior = CPU_LIMIT.as_nanos() * 4;
        let mut scope = Scope {
            coalition: supervisor.coalition,
            pids: Vec::new(),
            timebase: ffi::Timebase { numer: 3, denom: 2 },
            capture_cpu: None,
            limits: ToolLimits::default(),
            teardown: None,
        };
        assert!(scope.cpu_elapsed(supervisor, prior).unwrap() > CPU_LIMIT.as_nanos());
        scope.capture_cpu = Some((supervisor, prior));
        let ceiling_ticks = CPU_LIMIT.as_nanos() * 2 / 3;
        for (ticks, nanos, exceeds) in [
            (prior, 0, false),
            (prior + ceiling_ticks, CPU_LIMIT.as_nanos(), false),
            (prior + ceiling_ticks + 1, CPU_LIMIT.as_nanos() + 1, true),
        ] {
            let elapsed = scope.cpu_elapsed(supervisor, ticks).unwrap();
            assert_eq!(elapsed, nanos);
            assert_eq!(elapsed > CPU_LIMIT.as_nanos(), exceeds);
        }
        assert!(matches!(
            scope.cpu_elapsed(supervisor, prior - 1),
            Err(ToolError::ProtectionUnavailable)
        ));
        let mut executed = supervisor;
        executed.version += 1;
        assert_eq!(scope.cpu_elapsed(executed, prior).unwrap(), 0);
        for replacement in [
            Identity {
                pid: 43,
                ..supervisor
            },
            Identity {
                unique_id: 8,
                ..supervisor
            },
            Identity {
                coalition: 4,
                ..supervisor
            },
        ] {
            assert_eq!(
                scope.cpu_elapsed(replacement, prior).unwrap(),
                prior * 3 / 2
            );
        }
    }

    #[test]
    fn fallback_command_deadlines_reserve_selection_without_widening_active_windows() {
        let mut intent = EffectIntent {
            host: None,
            host_command: None,
            target: crate::session::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: crate::session::RunId::default(),
            agent_run_id: crate::session::AgentRunId::default(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: ToolCall::Command {
                program: "shell".into(),
                args: Vec::new(),
                cwd: ".".into(),
            },
            limits: ToolLimits {
                cache_transfer_ms: 1_000,
                ..ToolLimits::default()
            },
            use_count: 1,
        };
        let start = tokio::time::Instant::now();
        let (preparation, go, completion) = action_deadlines(None, &intent, start);
        let old_preparation = start + CONNECT_DEADLINE + PREPARATION_DEADLINE;
        assert_eq!(
            preparation - old_preparation,
            crate::tools::selection::PHASE_TIME
        );
        assert_eq!(go - preparation, Duration::from_secs(1));
        let window = crate::tools::ActiveToolWindow {
            preparation_deadline: start + Duration::from_secs(1),
            go_deadline: start + Duration::from_secs(2),
            payload_deadline: start + Duration::from_secs(3),
            completion_deadline: start + Duration::from_secs(4),
        };
        assert_eq!(
            action_deadlines(Some(window), &intent, start),
            (
                window.preparation_deadline,
                window.go_deadline,
                window.completion_deadline
            )
        );
        intent.call = ToolCall::Read {
            path: "src/main.rs".into(),
            offset: None,
            limit: None,
        };
        let (other_preparation, other_go, other_completion) =
            action_deadlines(None, &intent, start);
        for (command, other) in [
            (preparation, other_preparation),
            (go, other_go),
            (completion, other_completion),
        ] {
            assert_eq!(command - other, crate::tools::selection::PHASE_TIME);
        }
    }

    #[tokio::test]
    async fn native_manager_failures_keep_their_boundary() {
        let root = tempfile::tempdir().unwrap();
        let domain = format!("user/{}", rustix::process::geteuid().as_raw());
        let label = format!("dev.arany.test.{}", uuid::Uuid::now_v7());
        let missing = root.path().join("missing.plist");
        for action in [ManagerAction::Bootstrap(&missing), ManagerAction::Kickstart] {
            assert!(matches!(
                manager(&domain, &label, action).await,
                Err(ToolError::GuardRejected(GuardStage::NativeManager))
            ));
        }
    }

    #[test]
    fn recovery_refuses_unowned_records_and_live_roots() {
        use crate::test_process::BoundedOutput;

        let root = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::guard::macos::unit::tests::recovery_refusal_helper",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("TMPDIR", root.path())
            .env("ARANY_TEST_RECOVERY_ROOT", root.path())
            .bounded_output_for(Duration::from_secs(10), 16 * 1024)
            .unwrap();
        assert!(
            output.status.success(),
            "recovery refusal corpus: {output:?}"
        );
        crate::test_process::assert_single_test_completed(
            &output.stdout,
            "recovery refusal corpus",
        );
        assert!(output.stderr.is_empty());
    }

    #[tokio::test]
    #[ignore = "isolated liveness-lock corpus owned by recovery refusal test"]
    async fn recovery_refusal_helper() {
        std::env::var_os("ARANY_TEST_RECOVERY_ROOT").expect("private recovery fixture");
        #[derive(Debug)]
        enum Case {
            Missing,
            Malformed,
            Version,
            Identity,
            Boot,
            Oversized,
            Link,
            Hardlink,
            Live,
        }
        let parent = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keep"), b"outside").unwrap();
        for case in [
            Case::Missing,
            Case::Malformed,
            Case::Version,
            Case::Identity,
            Case::Boot,
            Case::Oversized,
            Case::Link,
            Case::Hardlink,
            Case::Live,
        ] {
            let root = parent
                .path()
                .join(format!("{UNIT_ROOT_PREFIX}{}", uuid::Uuid::now_v7()));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&root)
                .unwrap();
            let directory = open_directory(&root, false).unwrap();
            let lock = hold_liveness_lock(&directory).unwrap();
            let mut record = Recovery {
                version: 1,
                root: crate::tools::fs::identity(&directory.dir).unwrap(),
                boot: uuid::Uuid::now_v7(),
                supervisor: None,
            };
            match case {
                Case::Version => record.version += 1,
                Case::Identity => record.root.1 += 1,
                Case::Boot => record.boot = uuid::Uuid::nil(),
                Case::Missing
                | Case::Malformed
                | Case::Oversized
                | Case::Link
                | Case::Hardlink
                | Case::Live => {}
            }
            write_record(&directory.dir, RECOVERY_RECORD, &record).unwrap();
            match case {
                Case::Missing => std::fs::remove_file(root.join(RECOVERY_RECORD)).unwrap(),
                Case::Malformed => {
                    std::fs::write(root.join(RECOVERY_RECORD), b"{\"version\":").unwrap()
                }
                Case::Oversized => std::fs::write(
                    root.join(RECOVERY_RECORD),
                    vec![b' '; super::super::super::admission::RECORD_BYTES_CAP + 1],
                )
                .unwrap(),
                Case::Link => {
                    std::fs::remove_file(root.join(RECOVERY_RECORD)).unwrap();
                    std::os::unix::fs::symlink(
                        outside.path().join("keep"),
                        root.join(RECOVERY_RECORD),
                    )
                    .unwrap();
                }
                Case::Hardlink => {
                    std::fs::hard_link(root.join(RECOVERY_RECORD), root.join("alias")).unwrap()
                }
                Case::Version | Case::Identity | Case::Boot | Case::Live => {}
            }
            let held = if matches!(case, Case::Live) {
                Some(lock)
            } else {
                drop(lock);
                None
            };
            assert!(
                recover_root(&root).await.is_err(),
                "recovery must reject {case:?}"
            );
            assert!(root.exists(), "rejected {case:?} root must remain intact");
            assert_eq!(
                std::fs::read(outside.path().join("keep")).unwrap(),
                b"outside"
            );
            drop(held);
        }
        let alias = parent
            .path()
            .join(format!("{UNIT_ROOT_PREFIX}{}", uuid::Uuid::now_v7()));
        std::os::unix::fs::symlink(outside.path(), &alias).unwrap();
        assert!(recover_root(&alias).await.is_err());
        assert_eq!(
            std::fs::read(outside.path().join("keep")).unwrap(),
            b"outside"
        );
        let priority = tempfile::tempdir().unwrap();
        for _ in 0..=RECOVERY_ATTEMPT_LIMIT {
            let root = priority
                .path()
                .join(format!("{UNIT_ROOT_PREFIX}{}", uuid::Uuid::now_v7()));
            let directory = open_directory(&root, true).unwrap();
            drop(hold_liveness_lock(&directory).unwrap());
        }
        let roots: Vec<_> = std::fs::read_dir(priority.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        let valid = roots.last().unwrap();
        let directory = open_directory(valid, false).unwrap();
        write_record(
            &directory.dir,
            RECOVERY_RECORD,
            &Recovery {
                version: 1,
                root: crate::tools::fs::identity(&directory.dir).unwrap(),
                boot: uuid::Uuid::now_v7(),
                supervisor: None,
            },
        )
        .unwrap();
        recover_retained_roots(priority.path(), rustix::process::geteuid().as_raw())
            .await
            .unwrap();
        assert!(
            !valid.exists(),
            "invalid records cannot consume the valid-candidate recovery budget"
        );
        assert!(
            roots
                .iter()
                .take(RECOVERY_ATTEMPT_LIMIT)
                .all(|root| root.exists())
        );
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

    fn native_storage_snapshot(root: &Path) -> String {
        let parent_device = std::fs::metadata(root).ok().map(|metadata| metadata.dev());
        let volumes: Vec<_> = ["workspace", "scratch", "cache"]
            .into_iter()
            .map(|name| {
                let mounted = std::fs::metadata(root.join(name))
                    .ok()
                    .zip(parent_device)
                    .is_some_and(|(metadata, parent)| metadata.dev() != parent);
                let image_bytes = std::fs::metadata(root.join(format!("{name}.dmg")))
                    .ok()
                    .map(|metadata| metadata.len());
                format!("{name}={{mounted:{mounted},image_bytes:{image_bytes:?}}}")
            })
            .collect();
        format!("volumes={volumes:?}")
    }

    fn native_storage_stage(phase: &str, stage: &str) {
        if matches!(phase, "storage" | "storage-drop" | "command-profile") {
            eprintln!("native {phase} stage: {stage}");
        }
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

    #[test]
    fn drifted_runtime_grant_is_rejected_before_command_storage() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("script"), b"#!/bin/sh\n").unwrap();
        let runtime = snapshot_runtime(&workspace);
        let node_path = temp.path().join("profile-home/.nvm/versions/node/v22.3.0");
        // Keep the previous directory alive so the recreated root cannot reuse its inode.
        std::fs::rename(&node_path, temp.path().join("previous-node")).unwrap();
        std::fs::create_dir_all(node_path.join("bin")).unwrap();
        let root = temp.path().join("command-root");
        std::fs::create_dir(&root).unwrap();
        let reactor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let storage = reactor.block_on(prepare_command_storage(
            &root,
            &runtime,
            &ToolLimits::default(),
            true,
        ));
        assert!(matches!(storage, Err(ToolError::ChangedInput)));
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn runtime_compat_payload_rejects_missing_or_changed_library() {
        let directory = tempfile::tempdir().unwrap();
        let resources = directory.path().join("runtime");
        std::fs::create_dir(&resources).unwrap();
        let library = resources.join(super::super::RUNTIME_COMPAT_FILE);
        std::fs::write(&library, super::super::RUNTIME_COMPAT_BYTES).unwrap();
        let program = crate::tools::config::Program {
            name: "probe".into(),
            executable: "/usr/bin/true".into(),
            sha256: "0".repeat(64),
            interpreter: false,
            inputs: Vec::new(),
        };
        let mut command = tokio::process::Command::new("/usr/bin/true");
        super::super::configure_runtime_compat(&mut command, &program, &mut [], &resources)
            .unwrap();

        let mut changed = super::super::RUNTIME_COMPAT_BYTES.to_vec();
        changed[0] ^= 1;
        std::fs::write(&library, changed).unwrap();
        assert!(matches!(
            super::super::configure_runtime_compat(&mut command, &program, &mut [], &resources),
            Err(ToolError::ChangedInput)
        ));

        std::fs::remove_file(library).unwrap();
        assert!(matches!(
            super::super::configure_runtime_compat(&mut command, &program, &mut [], &resources),
            Err(ToolError::Path)
        ));
    }

    fn snapshot_runtime(workspace: &Path) -> ToolRuntime {
        let directory = crate::tools::fs::open_directory(workspace).unwrap();
        let shell = "/bin/sh";
        let system_programs = super::super::SystemPrograms::admit().unwrap();
        let home = workspace.parent().unwrap().join("profile-home");
        let node_path = home.join(".nvm/versions/node/v22.3.0");
        std::fs::create_dir_all(node_path.join("bin")).unwrap();
        let node = node_path.join("bin/node");
        std::fs::write(&node, b"#!/bin/sh\nprintf runtime-profile-selected\n").unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Like nvm/fnm/Volta shims: a `bin` symlink whose resolved target lives outside `bin`.
        std::fs::create_dir_all(node_path.join("lib")).unwrap();
        let tool = node_path.join("lib/tool.sh");
        std::fs::write(&tool, b"#!/bin/sh\nprintf runtime-tool-ran\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
        let tool_link = node_path.join("bin/tool");
        let _ = std::fs::remove_file(&tool_link);
        std::os::unix::fs::symlink("../lib/tool.sh", &tool_link).unwrap();
        let cache_path = home.join(".cargo/registry/cache");
        std::fs::create_dir_all(&cache_path).unwrap();
        std::fs::write(
            cache_path.join("index.marker"),
            b"private registry metadata",
        )
        .unwrap();
        std::fs::create_dir_all(home.join(".cargo")).unwrap();
        std::fs::write(home.join(".cargo/credentials.toml"), b"synthetic secret").unwrap();
        let npm_cache_path = home.join(".npm/_cacache");
        std::fs::create_dir_all(&npm_cache_path).unwrap();
        std::fs::write(npm_cache_path.join("index.marker"), b"private npm metadata").unwrap();
        let pip_cache_path = home.join("Library/Caches/pip/http-v2");
        std::fs::create_dir_all(&pip_cache_path).unwrap();
        std::fs::write(pip_cache_path.join("index.marker"), b"private pip metadata").unwrap();
        let identity = |path: &Path| {
            crate::tools::fs::identity(&crate::tools::fs::open_directory(path).unwrap()).unwrap()
        };
        let (node_device, node_inode) = identity(&node_path);
        let (cache_device, cache_inode) = identity(&cache_path);
        let (npm_cache_device, npm_cache_inode) = identity(&npm_cache_path);
        let (pip_cache_device, pip_cache_inode) = identity(&pip_cache_path);
        let runtime_grants = vec![
            crate::tools::toolchains::RuntimeGrant {
                kind: crate::tools::toolchains::RuntimeKind::Node,
                manager: crate::tools::toolchains::RuntimeManager::Nvm,
                version: Some("v22.3.0".into()),
                path: node_path,
                device: node_device,
                inode: node_inode,
            },
            crate::tools::toolchains::RuntimeGrant {
                kind: crate::tools::toolchains::RuntimeKind::CargoCache,
                manager: crate::tools::toolchains::RuntimeManager::HostCache,
                version: None,
                path: cache_path,
                device: cache_device,
                inode: cache_inode,
            },
            crate::tools::toolchains::RuntimeGrant {
                kind: crate::tools::toolchains::RuntimeKind::NpmCache,
                manager: crate::tools::toolchains::RuntimeManager::HostCache,
                version: None,
                path: npm_cache_path,
                device: npm_cache_device,
                inode: npm_cache_inode,
            },
            crate::tools::toolchains::RuntimeGrant {
                kind: crate::tools::toolchains::RuntimeKind::PipHttpCache,
                manager: crate::tools::toolchains::RuntimeManager::HostCache,
                version: None,
                path: pip_cache_path,
                device: pip_cache_device,
                inode: pip_cache_inode,
            },
        ];
        let process_enforcement_digest = super::super::super::macos_runtime_enforcement_digest(
            Some(&system_programs),
            &runtime_grants,
        )
        .unwrap();
        let mut commands = vec![
            crate::tools::config::Program {
                name: "shell".into(),
                executable: shell.into(),
                sha256: crate::validate::hex_digest(&std::fs::read(shell).unwrap()),
                interpreter: true,
                inputs: vec![crate::tools::config::ProgramInput {
                    path: workspace.join("runtime-input").to_str().unwrap().into(),
                    destination: "fixture.txt".into(),
                    sha256: crate::validate::hex_digest(b"pinned synthetic input"),
                }],
            },
            crate::tools::config::Program {
                name: "native".into(),
                executable: workspace.join("script").to_str().unwrap().into(),
                sha256: crate::validate::hex_digest(
                    &std::fs::read(workspace.join("script")).unwrap(),
                ),
                interpreter: false,
                inputs: Vec::new(),
            },
        ];
        let sysctl_probe = workspace.join("sysctl-probe");
        let mut workspace_paths = vec!["selected".into(), "empty".into(), "script".into()];
        if sysctl_probe.is_file() {
            workspace_paths.push("sysctl-probe".into());
            commands.push(crate::tools::config::Program {
                name: "sysctl-probe".into(),
                executable: sysctl_probe.to_str().unwrap().into(),
                sha256: crate::validate::hex_digest(&std::fs::read(&sysctl_probe).unwrap()),
                interpreter: false,
                inputs: Vec::new(),
            });
            let rust_probe = workspace.join("rust-startup-probe");
            commands.push(crate::tools::config::Program {
                name: "rust-startup-probe".into(),
                executable: rust_probe.to_str().unwrap().into(),
                sha256: crate::validate::hex_digest(&std::fs::read(rust_probe).unwrap()),
                interpreter: false,
                inputs: Vec::new(),
            });
        }
        ToolRuntime {
            host_settings_admitted: true,
            guard_available: true,
            git_writes_available: false,
            host_authentication: Default::default(),
            worker: None,
            handover: None,
            definitions: Default::default(),
            permissions: None,
            result_filter: None,
            unguarded_rules: None,
            context_limit_bytes: crate::tools::types::MAX_TOOL_CONTEXT_BYTES,
            queue_progress: None,
            queue_wait: None,
            active_action_gate: None,
            checkpoint_gate: None,
            hook_stdin: None,
            cache_state: None,
            config: crate::tools::config::Config {
                version: 1,
                workspace_paths,
                write: true,
                child_unguarded: crate::tools::UnguardedGrant::Denied,
                commands,
                skills: Vec::new(),
                mcp: vec![crate::tools::config::Server {
                    name: "local".into(),
                    program: crate::tools::config::Program {
                        name: "mcp-shell".into(),
                        executable: shell.into(),
                        sha256: crate::validate::hex_digest(&std::fs::read(shell).unwrap()),
                        interpreter: true,
                        inputs: vec![crate::tools::config::ProgramInput {
                            path: workspace.join("server.sh").to_str().unwrap().into(),
                            destination: "server.sh".into(),
                            sha256: crate::validate::hex_digest(MCP_SCRIPT.as_bytes()),
                        }],
                    },
                    args: vec!["/inputs/mcp-shell/server.sh".into()],
                    tools: vec!["echo".into()],
                }],
                runtime_grants: runtime_grants.clone(),
                git: None,
                git_identity: None,
            },
            workspace: workspace.to_path_buf(),
            reads: None,
            workspace_identity: crate::tools::fs::workspace_identity(&directory).unwrap(),
            guard_executable: std::env::current_exe().unwrap(),
            enforcement_digest: super::super::super::enforcement_digest(&[]).unwrap(),
            system_programs: Some(std::sync::Arc::new(system_programs)),
            process_enforcement_digest,
        }
    }

    #[test]
    #[cfg_attr(
        not(arany_ci),
        ignore = "native macOS mounted-image runtime topology check; requires hdiutil"
    )]
    fn runtime_mount_attestation_rejects_nested_mounted_image() {
        use crate::test_process::BoundedOutput;
        use crate::tools::toolchains::{RuntimeGrant, RuntimeKind, RuntimeManager};
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        struct MountedImage(std::path::PathBuf);

        impl Drop for MountedImage {
            fn drop(&mut self) {
                let _ = crate::test_process::detach_disk_image(&self.0, Duration::from_secs(30));
            }
        }

        let fixture = tempfile::tempdir().unwrap();
        let runtime = fixture.path().join("home/.nvm/versions/node/v22.3.0");
        let mountpoint = runtime.join("mounted-image");
        std::fs::create_dir_all(runtime.join("bin")).unwrap();
        std::fs::create_dir(&mountpoint).unwrap();
        let node = runtime.join("bin/node");
        std::fs::write(&node, b"synthetic node executable").unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o700)).unwrap();
        let image = fixture.path().join("runtime.sparseimage");
        let created = std::process::Command::new("/usr/bin/hdiutil")
            .args([
                "create",
                "-quiet",
                "-size",
                "8m",
                "-fs",
                "HFS+",
                "-volname",
                "AranyRuntimeFixture",
                "-type",
                "SPARSE",
                "-ov",
            ])
            .arg(&image)
            .env_clear()
            .env("HOME", fixture.path())
            .env("TMPDIR", fixture.path())
            .current_dir(fixture.path())
            .bounded_output_for(Duration::from_secs(30), 4096)
            .unwrap();
        assert!(created.status.success(), "hdiutil create: {created:?}");
        let attached = std::process::Command::new("/usr/bin/hdiutil")
            .args(["attach", "-quiet", "-nobrowse", "-mountpoint"])
            .arg(&mountpoint)
            .arg(&image)
            .env_clear()
            .env("HOME", fixture.path())
            .env("TMPDIR", fixture.path())
            .current_dir(fixture.path())
            .bounded_output_for(Duration::from_secs(30), 4096)
            .unwrap();
        assert!(attached.status.success(), "hdiutil attach: {attached:?}");
        let _mounted = MountedImage(image.clone());

        let metadata = std::fs::metadata(&runtime).unwrap();
        let grant = RuntimeGrant {
            kind: RuntimeKind::Node,
            manager: RuntimeManager::Nvm,
            version: Some("v22.3.0".into()),
            path: runtime,
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        let target = std::fs::canonicalize(&mountpoint).unwrap();
        assert!(
            ffi::mounts()
                .unwrap()
                .iter()
                .any(|(_, path)| path == &target)
        );
        let call = ToolCall::Command {
            program: "node".into(),
            args: Vec::new(),
            cwd: String::new(),
        };
        let mut stage = super::super::super::BootstrapStage::Workspace;
        assert!(matches!(
            super::super::super::attest_runtime_mounts(
                &call,
                std::slice::from_ref(&grant),
                false,
                &mut stage,
            ),
            Err(ToolError::ProtectionUnavailable)
        ));
        assert!(matches!(
            stage,
            super::super::super::BootstrapStage::RuntimeMounts
        ));
    }

    fn native_actors(directory: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
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
        let sysctl_source = directory.join("sysctl-probe.c");
        let sysctl_probe = directory.join("sysctl-probe");
        std::fs::write(
            &sysctl_source,
            br#"#include <errno.h>
#include <stdint.h>
#include <pthread.h>
#include <signal.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <sys/sysctl.h>
#include <mach/vm_page_size.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc != 3) return 1;
    sigset_t full_mask, previous_mask;
    if (sigfillset(&full_mask) != 0) return 3;
    int mask_result = pthread_sigmask(SIG_SETMASK, &full_mask, &previous_mask);
    if (mask_result != 0) {
        fprintf(stderr, "pthread_sigmask block: %d\n", mask_result);
        return 3;
    }
    mask_result = pthread_sigmask(SIG_SETMASK, &previous_mask, NULL);
    if (mask_result != 0) {
        fprintf(stderr, "pthread_sigmask restore: %d\n", mask_result);
        return 3;
    }
    int guarded = strcmp(argv[1], "shimmed") == 0;
    int outside_pid = atoi(argv[2]);
    if (outside_pid <= 0 || outside_pid == getpid()) return 1;
    int page_mib[] = { CTL_HW, HW_PAGESIZE };
    int page_size = 0;
    size_t page_size_len = sizeof(page_size);
    errno = 0;
    int page_result = sysctl(page_mib, 2, &page_size, &page_size_len, NULL, 0);
    int page_errno = errno;
    long runtime_page_size = sysconf(_SC_PAGESIZE);
    long clock_ticks = sysconf(_SC_CLK_TCK);
    int named_compat_page = 0;
    size_t named_compat_page_len = sizeof(named_compat_page);
    errno = 0;
    int named_compat_result = sysctlbyname("hw.pagesize_compat", &named_compat_page, &named_compat_page_len, NULL, 0);
    int named_compat_errno = errno;
    uint64_t named_page_size = 0;
    size_t named_page_size_len = sizeof(named_page_size);
    errno = 0;
    int named_page_result = sysctlbyname("hw.pagesize", &named_page_size, &named_page_size_len, NULL, 0);
    int named_page_errno = errno;
    int kernel_mib[] = { CTL_KERN, KERN_OSTYPE };
    char kernel_type[128];
    size_t kernel_type_len = sizeof(kernel_type);
    errno = 0;
    int kernel_result = sysctl(kernel_mib, 2, kernel_type, &kernel_type_len, NULL, 0);
    int kernel_errno = errno;
    char named_kernel_type[128];
    size_t named_kernel_type_len = sizeof(named_kernel_type);
    errno = 0;
    int named_kernel_result = sysctlbyname("kern.ostype", named_kernel_type, &named_kernel_type_len, NULL, 0);
    int named_kernel_errno = errno;
    int process_mib[] = { CTL_KERN, KERN_PROC, KERN_PROC_PID, getpid() };
    char process_info[4096];
    size_t process_info_len = sizeof(process_info);
    errno = 0;
    int process_result = sysctl(process_mib, 4, process_info, &process_info_len, NULL, 0);
    int process_errno = errno;
    int outside_process_mib[] = { CTL_KERN, KERN_PROC, KERN_PROC_PID, outside_pid };
    char outside_process_info[4096];
    size_t outside_process_info_len = sizeof(outside_process_info);
    errno = 0;
    int outside_process_result = sysctl(outside_process_mib, 4, outside_process_info, &outside_process_info_len, NULL, 0);
    int outside_process_errno = errno;
    int policy_ok = guarded
        ? vm_page_size > 0 && runtime_page_size == (long)vm_page_size
            && clock_ticks > 0
            && getpagesize() == (int)vm_page_size
            && page_result == -1 && page_errno == EPERM
            && named_compat_result == -1 && named_compat_errno == EPERM
            && named_page_result == -1 && named_page_errno == EPERM
            && kernel_result == -1 && kernel_errno == EPERM
            && named_kernel_result == -1 && named_kernel_errno == EPERM
            && process_result == -1 && process_errno == EPERM
            && outside_process_result == -1 && outside_process_errno == EPERM
        : runtime_page_size > 0 && clock_ticks > 0 && getpagesize() == page_size
            && page_result == 0 && page_size > 0
            && runtime_page_size == page_size && page_size_len == sizeof(page_size)
            && named_compat_result == 0 && named_compat_page == page_size
            && named_compat_page_len == sizeof(named_compat_page)
            && named_page_result == 0 && named_page_size_len == sizeof(named_page_size)
            && kernel_result == 0 && kernel_type_len > 0
            && named_kernel_result == 0 && named_kernel_type_len > 0
            && process_result == 0 && process_info_len > 0
            && outside_process_result == 0 && outside_process_info_len > 0;
    if (!policy_ok) {
        fprintf(stderr, "sysctl probe: guarded=%d sysconf=%ld clock_ticks=%ld page_result=%d page_errno=%d page_len=%zu compat_result=%d compat_errno=%d compat_len=%zu quad_result=%d quad_errno=%d quad_len=%zu kernel_result=%d kernel_errno=%d named_kernel_result=%d named_kernel_errno=%d process_result=%d process_errno=%d process_len=%zu outside_process_result=%d outside_process_errno=%d outside_process_len=%zu\n",
            guarded, runtime_page_size, clock_ticks, page_result, page_errno, page_size_len,
            named_compat_result, named_compat_errno, named_compat_page_len,
            named_page_result, named_page_errno, named_page_size_len,
            kernel_result, kernel_errno, named_kernel_result, named_kernel_errno,
            process_result, process_errno, process_info_len,
            outside_process_result, outside_process_errno, outside_process_info_len);
        return 2;
    }
    puts("sysctl isolation policy ok");
    return 0;
}
"#,
        )
        .unwrap();
        let probe_compiled = std::process::Command::new("/usr/bin/xcrun")
            .args([
                "--sdk", "macosx", "clang", "-Wall", "-Wextra", "-Werror", "-O1",
            ])
            .arg(&sysctl_source)
            .arg("-o")
            .arg(&sysctl_probe)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", directory)
            .env("TMPDIR", directory)
            .current_dir(directory)
            .bounded_output_for(Duration::from_secs(20), 16 * 1024)
            .unwrap();
        assert!(
            probe_compiled.status.success(),
            "native sysctl probe compilation: {probe_compiled:?}"
        );
        assert!(probe_compiled.stdout.is_empty() && probe_compiled.stderr.is_empty());
        let rust_source = directory.join("rust-startup-probe.rs");
        let rust_probe = directory.join("rust-startup-probe");
        std::fs::write(
            &rust_source,
            "fn main() { println!(\"rust startup reached main\"); }\n",
        )
        .unwrap();
        let mut sysroot_command = std::process::Command::new("rustc");
        sysroot_command
            .args(["--print", "sysroot"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env_clear();
        for name in ["PATH", "HOME", "RUSTUP_HOME"] {
            if let Some(value) = std::env::var_os(name) {
                sysroot_command.env(name, value);
            }
        }
        let sysroot = sysroot_command
            .bounded_output_for(Duration::from_secs(10), 4096)
            .unwrap();
        assert!(
            sysroot.status.success(),
            "Rust sysroot discovery: {sysroot:?}"
        );
        let sysroot = String::from_utf8(sysroot.stdout).unwrap().trim().to_owned();
        let rustc = Path::new(&sysroot).join("bin/rustc");
        let sdk = std::process::Command::new("/usr/bin/xcrun")
            .args(["--sdk", "macosx", "--show-sdk-path"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", directory)
            .env("TMPDIR", directory)
            .current_dir(directory)
            .bounded_output_for(Duration::from_secs(10), 4096)
            .unwrap();
        assert!(sdk.status.success(), "native SDK discovery: {sdk:?}");
        let sdk = String::from_utf8(sdk.stdout).unwrap().trim().to_owned();
        let rust_compiled = std::process::Command::new(rustc)
            .args(["--edition=2021"])
            .arg(&rust_source)
            .arg("-o")
            .arg(&rust_probe)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", directory)
            .env("TMPDIR", directory)
            .env("SDKROOT", sdk)
            .current_dir(directory)
            .bounded_output_for(Duration::from_secs(30), 16 * 1024)
            .unwrap();
        assert!(
            rust_compiled.status.success(),
            "native Rust startup actor compilation: {rust_compiled:?}"
        );
        assert!(rust_compiled.stdout.is_empty() && rust_compiled.stderr.is_empty());
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
        (actor, plain, sysctl_probe, rust_probe)
    }

    async fn verify_uncertain_storage_is_retained(fixture: &Path) {
        let mut uncertain = Unit::new(fixture).await.unwrap();
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
        crate::tools::fs::open_directory(fixture)
            .unwrap()
            .remove_dir_all(uncertain_root.file_name().unwrap())
            .unwrap();
    }

    async fn verify_partial_storage_is_cleaned(fixture: &Path) {
        let mut partial = Unit::new(fixture).await.unwrap();
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

    async fn verify_storage_ownership(fixture: &Path) {
        use super::super::storage::ImageIdentity;
        let mut unit = Unit::new(fixture).await.unwrap();
        let _volumes = prepare_storage(&unit.root).await.unwrap();
        let mut image: ImageIdentity =
            read_record(&unit._directory.dir, "scratch.image.json").unwrap();
        image.inode = image.inode.wrapping_add(1);
        write_record(&unit._directory.dir, "scratch.image.json", &image).unwrap();
        let mounted = ffi::mounts().unwrap();
        assert!(
            unit.stop().await.is_err(),
            "a mismatched backing inode must reject before any detach"
        );
        assert_eq!(
            ffi::mounts()
                .unwrap()
                .iter()
                .filter(|(_, target)| target.starts_with(&unit.root))
                .count(),
            2
        );
        assert!(
            mounted
                .iter()
                .filter(|(_, target)| target.starts_with(&unit.root))
                .all(|mount| ffi::mounts().unwrap().contains(mount))
        );
        image.inode = image.inode.wrapping_sub(1);
        write_record(&unit._directory.dir, "scratch.image.json", &image).unwrap();
        use crate::test_process::BoundedOutput;
        let unmounted = std::process::Command::new("/usr/bin/hdiutil")
            .args(["unmount", "-force", "-quiet"])
            .arg(unit.root.join("workspace"))
            .env_clear()
            .current_dir(&unit.root)
            .bounded_output_for(Duration::from_secs(20), 4096)
            .unwrap();
        assert!(unmounted.status.success(), "owned image unmount fixture");
        assert!(
            !ffi::mounts()
                .unwrap()
                .iter()
                .any(|(_, target)| target == &unit.root.join("workspace"))
        );
        unit.stop().await.unwrap();
    }

    async fn verify_storage_free_cleanup(fixture: &Path) {
        use crate::test_process::BoundedOutput;
        let mut unit = Unit::new(fixture).await.unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", HELPER, "--ignored", "--nocapture"])
            .env_clear()
            .env("ARANY_TEST_NATIVE_UNIT_ROOT", &unit.root)
            .env("ARANY_TEST_NATIVE_UNIT_PHASE", "storage-free-cleanup")
            .current_dir("/")
            .bounded_output_for(Duration::from_secs(10), 16 * 1024)
            .unwrap();
        assert!(
            output.status.success(),
            "storage-free cleanup with hdiutil exec denied: {output:?}"
        );
        crate::test_process::assert_single_test_completed(
            &output.stdout,
            "storage-free cleanup helper",
        );
        unit.stop().await.unwrap();
    }

    async fn verify_previous_boot_never_signals() {
        let parent = tempfile::tempdir().unwrap();
        let own = identity(std::process::id()).unwrap();
        let mut roots = Vec::new();
        for _ in 0..RECOVERY_ATTEMPT_LIMIT {
            let root = parent
                .path()
                .join(format!("{UNIT_ROOT_PREFIX}{}", uuid::Uuid::now_v7()));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&root)
                .unwrap();
            let directory = open_directory(&root, false).unwrap();
            let lock = hold_liveness_lock(&directory).unwrap();
            write_record(
                &directory.dir,
                RECOVERY_RECORD,
                &Recovery {
                    version: 1,
                    root: crate::tools::fs::identity(&directory.dir).unwrap(),
                    boot: uuid::Uuid::now_v7(),
                    supervisor: Some(own),
                },
            )
            .unwrap();
            drop(lock);
            roots.push(root);
        }
        recover_retained_roots(parent.path(), rustix::process::geteuid().as_raw())
            .await
            .unwrap();
        assert!(roots.iter().all(|root| !root.exists()));
        assert!(identity(own.pid).unwrap().same_process(own));
    }

    struct DiskGate {
        child: tokio::process::Child,
        output: BufReader<tokio::process::ChildStdout>,
    }

    impl DiskGate {
        async fn start(executable: &Path, action: &str, root: &Path, image: &str) -> Self {
            let name = format!("{}-{image}", root.file_name().unwrap().to_str().unwrap());
            let mut child = Command::new(executable)
                .args([action, &name])
                .env_clear()
                .current_dir("/")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let output = BufReader::new(child.stdout.take().unwrap());
            let mut gate = Self { child, output };
            assert_eq!(gate.message().await, b"READY\n");
            gate
        }

        async fn message(&mut self) -> Vec<u8> {
            tokio::time::timeout(
                Duration::from_secs(8),
                read_line_capped(&mut self.output, 128),
            )
            .await
            .unwrap()
            .unwrap()
        }

        async fn blocked(&mut self) -> PathBuf {
            let message = self.message().await;
            let device = std::str::from_utf8(&message)
                .unwrap()
                .trim()
                .strip_prefix("BLOCKED ")
                .unwrap();
            assert!(device.starts_with("/dev/disk"));
            device.into()
        }

        async fn release(mut self) {
            self.child
                .stdin
                .take()
                .unwrap()
                .write_all(b"G")
                .await
                .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_secs(5), self.child.wait())
                    .await
                    .unwrap()
                    .unwrap()
                    .success()
            );
        }
    }

    fn disk_gate_actor(directory: &Path) -> PathBuf {
        use crate::test_process::BoundedOutput;
        let executable = directory.join("disk-gate");
        let compiled = std::process::Command::new("/usr/bin/xcrun")
            .args([
                "--sdk",
                "macosx",
                "clang",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-O1",
                "-framework",
                "DiskArbitration",
                "-framework",
                "CoreFoundation",
            ])
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/macos-disk-approval-gate.c"
            ))
            .arg("-o")
            .arg(&executable)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", directory)
            .env("TMPDIR", directory)
            .current_dir(directory)
            .bounded_output_for(Duration::from_secs(20), 16 * 1024)
            .unwrap();
        assert!(
            compiled.status.success(),
            "disk gate compilation: {compiled:?}"
        );
        assert!(compiled.stdout.is_empty() && compiled.stderr.is_empty());
        executable
    }

    async fn verify_recovery_cancellation(gate: &Path, fixture: &Path) {
        let mut unit = Unit::new(fixture).await.unwrap();
        let _volumes = prepare_storage(&unit.root).await.unwrap();
        let record = unit._directory.dir.read(RECOVERY_RECORD).unwrap();
        write_record(
            &unit._directory.dir,
            STORAGE_PENDING,
            &super::super::storage::PendingAttachment {
                version: 1,
                image: "workspace".into(),
                boot: ffi::boot_session().unwrap(),
                state: super::super::storage::AttachmentState::Attaching,
            },
        )
        .unwrap();
        let mut observer = DiskGate::start(gate, "unmount", &unit.root, "workspace").await;
        rustix::fs::flock(&unit._liveness, rustix::fs::FlockOperation::Unlock).unwrap();
        let mut recovering = Box::pin(recover_root(&unit.root));
        let device = tokio::select! {
            result = &mut recovering => panic!("recovery finished before native unmount gate: {result:?}"),
            device = observer.blocked() => device,
        };
        assert!(
            super::super::storage::owned_devices(&unit.root)
                .await
                .unwrap()
                .contains(&device)
        );
        drop(recovering);
        assert!(unit.root.exists());
        assert_eq!(unit._directory.dir.read(RECOVERY_RECORD).unwrap(), record);
        assert!(is_retained(&unit.root).unwrap());
        let pending: super::super::storage::PendingAttachment =
            read_record(&unit._directory.dir, STORAGE_PENDING).unwrap();
        assert!(pending.state == super::super::storage::AttachmentState::Observed);
        observer.release().await;
        super::super::storage::detach_owned_image(&unit.root, "workspace")
            .await
            .unwrap();
        assert!(
            !super::super::storage::owned_devices(&unit.root)
                .await
                .unwrap()
                .contains(&device)
        );
        assert!(
            unit._directory
                .dir
                .symlink_metadata(STORAGE_PENDING)
                .is_ok()
        );
        recover_root(&unit.root).await.unwrap();
        assert!(!unit.root.exists());
        unit.stopped = true;
    }

    async fn verify_controller_crash_recovery(script: &Path, gate: &Path) {
        struct Controller(std::process::Child);
        impl Drop for Controller {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut fixture = tempfile::tempdir().unwrap();
        fixture.disable_cleanup(true);
        let source = phase_workspace(script, None);
        std::fs::write(fixture.path().join("errors"), []).unwrap();
        let listener = UnixListener::bind(fixture.path().join("crash")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
        // The controller drops before this fallback, even when an assertion unwinds.
        let mut fallback: Option<Unit> = None;
        let mut controller = Controller(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", HELPER, "--ignored", "--nocapture"])
                .env_clear()
                .env("ARANY_TEST_NATIVE_UNIT_ROOT", fixture.path())
                .env("ARANY_TEST_NATIVE_UNIT_PHASE", "recovery-controller")
                .env("ARANY_TEST_NATIVE_UNIT_WORKSPACE", source.path())
                .current_dir("/")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(
                    std::fs::OpenOptions::new()
                        .append(true)
                        .open(fixture.path().join("errors"))
                        .unwrap(),
                )
                .spawn()
                .unwrap(),
        );
        let stream = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .unwrap()
            .unwrap()
            .0;
        let mut stream = BufReader::new(stream);
        let root: PathBuf =
            serde_json::from_slice(&read_line_capped(&mut stream, 1024).await.unwrap()).unwrap();
        let directory = open_directory(&root, false).unwrap();
        let record = recovery(&directory).unwrap();
        fallback.replace(Unit {
            _permit: None,
            root: root.clone(),
            _liveness: directory.dir.open(UNIT_LIVENESS_LOCK).unwrap(),
            _directory: directory,
            domain: format!("user/{}", rustix::process::geteuid().as_raw()),
            label: format!(
                "dev.arany.tool.{}",
                root.file_name().unwrap().to_str().unwrap()
            ),
            scope: record
                .supervisor
                .map(|supervisor| Scope::new(supervisor.coalition).unwrap()),
            limits: None,
            stopped: false,
        });
        let mut observer = DiskGate::start(gate, "mount", &root, "scratch").await;
        stream.get_mut().write_all(b"G").await.unwrap();
        let members: Vec<Identity> = serde_json::from_slice(
            &tokio::time::timeout(Duration::from_secs(30), read_line_capped(&mut stream, 2048))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let record = recovery(&fallback.as_ref().unwrap()._directory).unwrap();
        let coalition = record.supervisor.unwrap().coalition;
        fallback.as_mut().unwrap().scope = Some(Scope::new(coalition).unwrap());
        let device = observer.blocked().await;
        assert!(storage_mounted(&root).unwrap());
        assert!(
            super::super::storage::owned_devices(&root)
                .await
                .unwrap()
                .contains(&device)
        );
        assert!(
            !ffi::mounts()
                .unwrap()
                .iter()
                .any(|(_, target)| target == &root.join("scratch"))
        );
        let pending: super::super::storage::PendingAttachment =
            read_record(&fallback.as_ref().unwrap()._directory.dir, STORAGE_PENDING).unwrap();
        assert_eq!(pending.image, "scratch");
        assert!(Scope::new(coalition).unwrap().members().unwrap().len() > members.len() + 1);
        assert!(!is_retained(&root).unwrap());
        assert!(
            recover_root(&root).await.is_err(),
            "a live controller's lock must exclude recovery"
        );
        controller.0.kill().unwrap();
        let status =
            crate::test_process::capture(&mut controller.0, Duration::from_secs(5), 16 * 1024)
                .status;
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(
            is_retained(&root).unwrap(),
            "process death releases the liveness lock"
        );
        // The restarted admission path must recover storage and escaped descendants together.
        let mut recovering = Box::pin(recover_retained_roots(
            fixture.path(),
            rustix::process::geteuid().as_raw(),
        ));
        let emptied = async {
            let mut samples = tokio::time::interval(SUPERVISION_INTERVAL);
            loop {
                if Scope::new(coalition).unwrap().members().unwrap().is_empty() {
                    break;
                }
                samples.tick().await;
            }
        };
        tokio::select! {
            result = &mut recovering => panic!("recovery ended before attachment gate release: {result:?}"),
            result = tokio::time::timeout(Duration::from_secs(5), emptied) => result.unwrap(),
        }
        observer.release().await;
        recovering.await.unwrap();
        assert!(!root.exists());
        assert!(Scope::new(coalition).unwrap().members().unwrap().is_empty());
        for member in members {
            assert!(
                matches!(identity(member.pid), Err(ToolError::ChangedInput))
                    || identity(member.pid).is_ok_and(|current| !current.same_process(member))
            );
        }
        assert!(
            !ffi::mounts()
                .unwrap()
                .iter()
                .any(|(_, target)| target.starts_with(&root))
        );
        fallback.as_mut().unwrap().stopped = true;
        let mut next = Unit::new(fixture.path()).await.unwrap();
        next.stop().await.unwrap();
        drop(next);
        drop(fallback);
        close_unit_fixture(fixture);
    }

    fn close_unit_fixture(fixture: tempfile::TempDir) {
        assert!(
            !ffi::mounts()
                .unwrap()
                .iter()
                .any(|(_, target)| target.starts_with(fixture.path()))
        );
        assert!(std::fs::read_dir(fixture.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_str()
                .unwrap()
                .starts_with(UNIT_ROOT_PREFIX)
        }));
        fixture.close().unwrap();
    }

    fn phase_workspace(script: &Path, sysctl_probe: Option<(&Path, &Path)>) -> tempfile::TempDir {
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
        if let Some((sysctl_probe, rust_probe)) = sysctl_probe {
            std::fs::copy(sysctl_probe, source.path().join("sysctl-probe")).unwrap();
            std::fs::copy(rust_probe, source.path().join("rust-startup-probe")).unwrap();
            std::fs::set_permissions(
                source.path().join("sysctl-probe"),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        std::fs::set_permissions(
            source.path().join("script"),
            std::fs::Permissions::from_mode(0o6755),
        )
        .unwrap();
        source
    }

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
        let mut record = recovery(&unit._directory).unwrap();
        record.supervisor = Some(root);
        write_record(&unit._directory.dir, RECOVERY_RECORD, &record).unwrap();
        assert_ne!(root.coalition, outside.coalition);
        unit.scope = Some(Scope::new(root.coalition).unwrap());
        let accepted = tokio::time::timeout(Duration::from_secs(10), listener.accept()).await;
        assert!(
            accepted.is_ok(),
            "native {phase} readiness: {}; {}",
            diagnostic(&unit.root),
            native_storage_snapshot(&unit.root)
        );
        let attestation = accepted.unwrap().unwrap().0.into_std().unwrap();
        assert_eq!(peer(&attestation).unwrap(), pid);
        (root, attestation, listener)
    }

    #[tokio::test]
    // Skipped in the Guard: local test IPC capability is missing.
    #[cfg_attr(
        any(arany_guard, not(arany_ci)),
        ignore = "native aggregate-resource and escaped-descendant gate; finite synthetic jobs"
    )]
    #[allow(clippy::await_holding_lock)]
    async fn native_unit_owns_descendants_and_supervises_resources() {
        use crate::test_process::BoundedOutput;

        let _gate = crate::test_process::NATIVE_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut generated = tempfile::tempdir().unwrap();
        generated.disable_cleanup(true);
        #[cfg(target_arch = "aarch64")]
        {
            let library = generated.path().join(super::super::RUNTIME_COMPAT_FILE);
            std::fs::write(&library, super::super::RUNTIME_COMPAT_BYTES).unwrap();
            let architectures = std::process::Command::new("/usr/bin/xcrun")
                .args(["--sdk", "macosx", "lipo"])
                .arg(&library)
                .args(["-verify_arch", "arm64", "arm64e"])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", generated.path())
                .env("TMPDIR", generated.path())
                .current_dir(generated.path())
                .bounded_output_for(Duration::from_secs(20), 16 * 1024)
                .unwrap();
            assert!(
                architectures.status.success(),
                "runtime compatibility library architectures: {architectures:?}"
            );
            assert!(architectures.stdout.is_empty() && architectures.stderr.is_empty());
        }
        let (actor, plain, sysctl_probe, rust_probe) = native_actors(generated.path());
        let disk_gate = disk_gate_actor(generated.path());
        verify_uncertain_storage_is_retained(generated.path()).await;
        verify_partial_storage_is_cleaned(generated.path()).await;
        verify_storage_ownership(generated.path()).await;
        verify_storage_free_cleanup(generated.path()).await;
        verify_previous_boot_never_signals().await;
        verify_controller_crash_recovery(&plain, &disk_gate).await;
        verify_recovery_cancellation(&disk_gate, generated.path()).await;

        let baseline = std::process::Command::new(&sysctl_probe)
            .arg("baseline")
            .arg(std::process::id().to_string())
            .env_clear()
            .current_dir(generated.path())
            .bounded_output_for(Duration::from_secs(5), 1024)
            .unwrap();
        assert!(
            baseline.status.success(),
            "unsandboxed sysctl baseline: {baseline:?}"
        );
        assert_eq!(baseline.stdout, b"sysctl isolation policy ok\n");
        assert!(baseline.stderr.is_empty());

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
            let mut unit = Unit::new(generated.path()).await.unwrap();
            assert_eq!(std::fs::read_dir(&unit.root).unwrap().count(), 2);
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
            let source = phase_workspace(
                &plain,
                (phase == "command-profile").then_some((&sysctl_probe, &rust_probe)),
            );
            let (root, mut attestation, listener) =
                launch_helper(&mut unit, phase, source.path()).await;
            if matches!(phase, "storage" | "storage-drop" | "command-profile") {
                crate::tools::fs::native(
                    source.path(),
                    &ToolCall::Write {
                        path: "selected".into(),
                        expected_digest: Some(crate::validate::hex_digest(b"before edit")),
                        content: "after typed edit".into(),
                    },
                    &snapshot_runtime(source.path()).config,
                )
                .unwrap();
            }
            attestation.write_all(b"G").unwrap();
            // Storage phases use the full product preparation window before the fixture times out.
            let readiness_bound = if matches!(phase, "storage" | "storage-drop" | "command-profile")
            {
                PREPARATION_DEADLINE + Duration::from_secs(5)
            } else {
                Duration::from_secs(10)
            };
            let accepted = tokio::time::timeout(readiness_bound, listener.accept()).await;
            assert!(
                accepted.is_ok(),
                "native {phase} descendant readiness after {}s: {}; {}",
                readiness_bound.as_secs(),
                diagnostic(&unit.root),
                native_storage_snapshot(&unit.root)
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
        close_unit_fixture(generated);
    }

    #[test]
    #[ignore = "isolated helper owned by the native unit gate"]
    fn native_unit_helper() {
        // The finite fixture ignores and blocks SIGXCPU to require external CPU supervision.
        assert!(ffi::ignore_cpu_signal_with_watchdog(75));
        let root = std::env::var_os("ARANY_TEST_NATIVE_UNIT_ROOT").unwrap();
        let root = Path::new(&root);
        let phase = std::env::var("ARANY_TEST_NATIVE_UNIT_PHASE").unwrap();
        native_storage_stage(&phase, "helper-entered");
        if phase == "storage-free-cleanup" {
            let reactor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            super::super::install_profile("(version 1)\n(allow default)\n(deny process-exec (literal \"/usr/bin/hdiutil\"))\n".into()).unwrap();
            reactor.block_on(cleanup_storage(root)).unwrap();
            return;
        }
        if phase == "recovery-controller" {
            let source = std::env::var_os("ARANY_TEST_NATIVE_UNIT_WORKSPACE").unwrap();
            let reactor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            reactor.block_on(async {
                let mut unit = Unit::new(root).await.unwrap();
                let stream = UnixStream::connect(root.join("crash")).unwrap();
                stream.set_nonblocking(true).unwrap();
                let mut stream = tokio::net::UnixStream::from_std(stream).unwrap();
                stream
                    .write_all(&serde_json::to_vec(&unit.root).unwrap())
                    .await
                    .unwrap();
                stream.write_all(b"\n").await.unwrap();
                stream.read_exact(&mut [0]).await.unwrap();
                let (_, mut attestation, listener) =
                    launch_helper(&mut unit, "attachment", Path::new(&source)).await;
                attestation.write_all(b"G").unwrap();
                let child = tokio::time::timeout(Duration::from_secs(10), listener.accept())
                    .await
                    .unwrap()
                    .unwrap()
                    .0;
                let mut child = BufReader::new(child);
                let members = read_line_capped(&mut child, 2048).await.unwrap();
                child.get_mut().write_all(b"G").await.unwrap();
                stream.write_all(&members).await.unwrap();
                assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
            });
            return;
        }
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
                        maximum: Some(75),
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
            let mut profile = SeatbeltProfile::new(ProfileKind::Command);
            for key in [
                "com.apple.private.coalition-spawn",
                "com.apple.private.persona-mgmt",
            ] {
                assert!(
                    profile
                        .text
                        .contains(&format!("(require-entitlement \"{key}\")"))
                );
                profile.text = profile
                    .text
                    .replace(key, "com.apple.security.get-task-allow");
            }
            for path in [
                root.join("plain"),
                root.join("actor"),
                PathBuf::from("/usr/bin/true"),
            ] {
                profile
                    .grant(
                        "allow file-read* file-map-executable process-exec",
                        "literal",
                        &path,
                    )
                    .unwrap();
            }
            profile
                .rule(
                    "allow file-read* file-write*",
                    "literal",
                    &root.join("entitlement-receipt"),
                )
                .unwrap();
            super::super::configure_resources().unwrap();
            super::super::install_profile(profile.text).unwrap();
            verify_actor("plain", "allow");
            verify_actor("actor", "deny");
            std::fs::write(root.join("entitlement-receipt"), b"PROFILE\n").unwrap();
            return;
        }
        if matches!(
            phase.as_str(),
            "storage" | "storage-drop" | "command-profile"
        ) {
            native_storage_stage(&phase, "runtime-building");
            let reactor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            native_storage_stage(&phase, "snapshot-runtime-building");
            let source = std::env::var_os("ARANY_TEST_NATIVE_UNIT_WORKSPACE").unwrap();
            let runtime = snapshot_runtime(Path::new(&source));
            native_storage_stage(&phase, "command-storage-preparation-started");
            let limits = crate::tools::types::ToolLimits {
                cache_requested_bytes: 0,
                cache_bytes: 0,
                cache_candidate_bytes: 0,
                cache_transfer_ms: 0,
                ..crate::tools::types::ToolLimits::default()
            };
            let volumes = reactor
                .block_on(prepare_command_storage(root, &runtime, &limits, false))
                .unwrap();
            native_storage_stage(&phase, "command-storage-preparation-finished");
            assert_eq!(volumes[0].read("selected").unwrap(), b"after typed edit");
            assert!(!volumes[0].exists("omitted"));
            assert!(volumes[0].is_dir("empty"));
            use cap_std::fs::MetadataExt;
            assert_eq!(
                volumes[0].metadata("script").unwrap().mode() & 0o7777,
                0o700
            );
            volumes[0].write("selected", b"disposable").unwrap();
            native_storage_stage(&phase, "volume-verification-finished");
            let mut stream = UnixStream::connect(root.join("s")).unwrap();
            send(&mut stream, &[identity(std::process::id()).unwrap()]);
            native_storage_stage(&phase, "second-readiness-published");
            let mut go = [0];
            stream.read_exact(&mut go).unwrap();
            native_storage_stage(&phase, "second-readiness-released");
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
                    preparation: false,
                    definition: None,
                    worker: None,
                    handover: None,
                    config: runtime.config.clone(),
                    cache: None,
                    hook_stdin: None,
                    hidden: Default::default(),
                    intent: EffectIntent {
                        host: None,
                        host_command: None,
                        target: crate::session::worker::EffectTarget::Workspace,
                        id: uuid::Uuid::now_v7(),
                        run_id: crate::session::RunId::default(),
                        agent_run_id: crate::session::AgentRunId::default(),
                        policy_digest: runtime.config.digest(),
                        enforcement_digest: runtime.process_enforcement_digest,
                        workspace_device: runtime.workspace_identity.device,
                        workspace_inode: runtime.workspace_identity.inode,
                        call: ToolCall::Command {
                            program: "shell".into(),
                            args: Vec::new(),
                            cwd: ".".into(),
                        },
                        limits: limits.clone(),
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
                let mut stage = super::super::super::BootstrapStage::Workspace;
                super::super::install(&task, &mut stage).unwrap();
                let probe = runtime
                    .config
                    .commands
                    .iter()
                    .find(|definition| definition.name == "sysctl-probe")
                    .expect("command-profile fixture includes its sysctl actor");
                let mut probe = super::super::super::payload(
                    probe,
                    &["shimmed".into(), outside_pid.to_string()],
                    ".",
                    root,
                    &runtime.config.runtime_grants,
                    false,
                    ToolResourceClass::Default,
                )
                .unwrap();
                let (probe_status, probe_stdout, probe_stderr) = reactor
                    .block_on(async {
                        super::super::super::capture(
                            probe.spawn().unwrap(),
                            1024,
                            Duration::from_secs(5),
                        )
                        .await
                    })
                    .unwrap();
                assert!(
                    probe_status.success(),
                    "native sysctl command actor: {probe_status}; stderr={probe_stderr:?}"
                );
                assert_eq!(probe_stdout, b"sysctl isolation policy ok\n");
                assert!(probe_stderr.is_empty());
                let rust_probe = runtime
                    .config
                    .commands
                    .iter()
                    .find(|definition| definition.name == "rust-startup-probe")
                    .expect("command-profile fixture includes its Rust startup actor");
                let mut rust_probe = super::super::super::payload(
                    rust_probe,
                    &[],
                    ".",
                    root,
                    &runtime.config.runtime_grants,
                    false,
                    ToolResourceClass::Default,
                )
                .unwrap();
                let (rust_status, rust_stdout, rust_stderr) = reactor
                    .block_on(async {
                        super::super::super::capture(
                            rust_probe.spawn().unwrap(),
                            1024,
                            Duration::from_secs(5),
                        )
                        .await
                    })
                    .unwrap();
                assert!(
                    rust_status.success(),
                    "native Rust startup actor: {rust_status}; stdout={rust_stdout:?}; stderr={rust_stderr:?}"
                );
                assert_eq!(rust_stdout, b"rust startup reached main\n");
                assert!(rust_stderr.is_empty());
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
                    let mut command = super::super::super::payload(
                        definition,
                        &args,
                        ".",
                        root,
                        &runtime.config.runtime_grants,
                        false,
                        ToolResourceClass::Default,
                    )
                    .unwrap();
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
                let script = r#"
set -eu
/usr/bin/true
test "$(/bin/cat "$ARANY_TOOL_INPUTS/shell/fixture.txt")" = 'pinned synthetic input'
test "$(node)" = runtime-profile-selected
test "$(tool)" = runtime-tool-ran
test -r "$CARGO_HOME/registry/cache/index.marker"
test "$npm_config_cache" = "$TMPDIR/.npm"
test -r "$npm_config_cache/_cacache/index.marker"
test "$PIP_CACHE_DIR" = "$TMPDIR/.cache/pip"
test -r "$PIP_CACHE_DIR/http-v2/index.marker"
if printf changed 2>/dev/null > "$CARGO_HOME/registry/cache/write-marker"; then exit 97; fi
test ! -e "$CARGO_HOME/registry/cache/write-marker"
test ! -e "$CARGO_HOME/credentials.toml"
printf 'command change' > "$1"
printf 'native command success'
"#;
                let mut command = super::super::super::payload(
                    &runtime.config.commands[0],
                    &[
                        "-c".into(),
                        script.into(),
                        "--".into(),
                        "/workspace/selected".into(),
                    ],
                    ".",
                    root,
                    &runtime.config.runtime_grants,
                    false,
                    ToolResourceClass::Default,
                )
                .unwrap();
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
                        crate::tools::ToolLimits::default().cache_bytes > 0,
                        crate::tools::ToolLimits::default().result_bytes as usize,
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
                    .block_on(crate::tools::mcp::execute(
                        &call,
                        &runtime.config,
                        root,
                        crate::tools::ToolLimits::default().cache_bytes > 0,
                        crate::tools::ToolLimits::default().result_bytes as usize,
                    ))
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
                    "descendants" | "attachment" => "detach",
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
        if matches!(phase.as_str(), "descendants" | "attachment") {
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
        if phase == "attachment" {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(prepare_storage(root))
                .unwrap();
        }
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    }
}

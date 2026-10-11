use super::*;
use crate::store::StateRoot;
use core_foundation::array::CFArray;
use core_foundation::base::TCFType;
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::propertylist::{create_data, kCFPropertyListBinaryFormat_v1_0};
use core_foundation::string::CFString;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use tokio::io::AsyncReadExt;

const HOST_PROCESS_CAP: usize = 65_536;
const JOB_BYTES_CAP: usize = 128 * 1024;
const STORAGE_BYTES_CAP: u64 = 64 * 1024 * 1024;
const STORAGE_NAMES: [&str; 2] = ["workspace", "scratch"];
const STORAGE_PENDING: &str = "storage.pending";

#[repr(C)]
struct CoalitionInfo {
    ids: [u64; 2],
    reserved: [u64; 3],
}

#[repr(C)]
struct ProcessInfo {
    executable_uuid: [u8; 16],
    unique_id: u64,
    parent_unique_id: u64,
    version: i32,
    parent_version: i32,
    reserved: [u64; 2],
}

#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}

const _: () = assert!(
    size_of::<CoalitionInfo>() == 40
        && size_of::<ProcessInfo>() == 56
        && size_of::<Timebase>() == 8
);

unsafe extern "C" {
    fn proc_signal_with_audittoken(token: *mut [u32; 8], signal: i32) -> i32;
    fn mach_timebase_info(timebase: *mut Timebase) -> i32;
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
    let mut process = MaybeUninit::<ProcessInfo>::uninit();
    let mut coalition = MaybeUninit::<CoalitionInfo>::uninit();
    let failure = |bytes| {
        if bytes <= 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            ToolError::ChangedInput
        } else {
            ToolError::ProtectionUnavailable
        }
    };
    // Reviewed Darwin POD layouts are read only after exact-size kernel writes.
    let bytes = unsafe { libc::proc_pidinfo(pid as i32, 17, 0, process.as_mut_ptr().cast(), 56) };
    if bytes != 56 {
        return Err(failure(bytes));
    }
    // The coalition output has its own size check; a short positive write is never absence.
    let bytes = unsafe { libc::proc_pidinfo(pid as i32, 20, 0, coalition.as_mut_ptr().cast(), 40) };
    if bytes != 40 {
        return Err(failure(bytes));
    }
    // Every field was initialized by the two exact-size writes above.
    let (process, coalition) = unsafe { (process.assume_init(), coalition.assume_init()) };
    Ok(Identity {
        pid,
        version: process.version as u32,
        unique_id: process.unique_id,
        coalition: coalition.ids[0],
    })
}

impl Identity {
    fn same_process(self, other: Self) -> bool {
        self.pid == other.pid
            && self.unique_id == other.unique_id
            && self.coalition == other.coalition
    }

    fn signal(self, signal: i32) -> Result<(), ToolError> {
        let mut token = [0_u32; 8];
        token[5] = self.pid;
        token[7] = self.version;
        // Darwin resolves the PID/version atomically; the other token fields are not selectors.
        match unsafe { proc_signal_with_audittoken(&mut token, signal) } {
            0 | libc::ESRCH => Ok(()),
            _ => Err(ToolError::ProtectionUnavailable),
        }
    }
}

pub(super) struct Scope {
    coalition: u64,
    pids: Vec<i32>,
    timebase: Timebase,
}

impl Scope {
    fn new(coalition: u64) -> Result<Self, ToolError> {
        if coalition <= 1 {
            return Err(ToolError::ProtectionUnavailable);
        }
        let mut timebase = MaybeUninit::<Timebase>::uninit();
        // Success initializes the SDK's two integer clock-conversion fields.
        if unsafe { mach_timebase_info(timebase.as_mut_ptr()) } != 0 {
            return Err(ToolError::ProtectionUnavailable);
        }
        // Only the successful native call permits reading this output.
        let timebase = unsafe { timebase.assume_init() };
        if timebase.numer == 0 || timebase.denom == 0 {
            return Err(ToolError::ProtectionUnavailable);
        }
        let mut maximum = 0_i32;
        let mut length = size_of::<i32>();
        let mut query = [libc::CTL_KERN, libc::KERN_MAXPROC];
        // This reads one bounded kernel capacity, never process arguments or environments.
        if unsafe {
            libc::sysctl(
                query.as_mut_ptr(),
                2,
                (&mut maximum as *mut i32).cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        } != 0
            || length != size_of::<i32>()
            || maximum < 1
            || maximum as usize >= HOST_PROCESS_CAP
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        Ok(Self {
            coalition,
            pids: vec![0; maximum as usize + 1],
            timebase,
        })
    }

    fn members(&mut self) -> Result<Vec<Identity>, ToolError> {
        // The fixed buffer exceeds the admitted kernel-wide process ceiling.
        let bytes = unsafe {
            libc::proc_listpids(
                1,
                0,
                self.pids.as_mut_ptr().cast(),
                size_of_val(self.pids.as_slice()) as i32,
            )
        };
        if bytes <= 0
            || bytes as usize >= size_of_val(self.pids.as_slice())
            || !(bytes as usize).is_multiple_of(size_of::<i32>())
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let mut members = Vec::new();
        for pid in &self.pids[..bytes as usize / size_of::<i32>()] {
            if *pid <= 1 {
                continue;
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

    pub(super) fn observe(&mut self) -> Result<(), ToolError> {
        let mut memory = 0_u64;
        let mut threads = 0_u64;
        for member in self.members()? {
            let task = match task_info(member.pid) {
                Ok(task) => task,
                Err(_) if !super::observe(member.pid)? => continue,
                Err(error) => return Err(error),
            };
            let mut usage = MaybeUninit::<libc::rusage_info_v0>::uninit();
            // The selected flavor writes the SDK's fixed rusage_info_v0 layout.
            if unsafe {
                libc::proc_pid_rusage(
                    member.pid as i32,
                    libc::RUSAGE_INFO_V0,
                    usage.as_mut_ptr().cast(),
                )
            } != 0
            {
                if matches!(identity(member.pid), Err(ToolError::ChangedInput)) {
                    continue;
                }
                return Err(ToolError::ProtectionUnavailable);
            }
            // The successful flavor-specific write initializes every integer/byte-array field.
            let usage = unsafe { usage.assume_init() };
            match identity(member.pid) {
                Ok(current) if current.same_process(member) => {}
                Ok(current) if current.unique_id != member.unique_id => continue,
                Err(ToolError::ChangedInput) => continue,
                _ => return Err(ToolError::ProtectionUnavailable),
            }
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
                || threads > u64::from(MAX_PROCESSES)
                || (u128::from(usage.ri_user_time) + u128::from(usage.ri_system_time))
                    * u128::from(self.timebase.numer)
                    / u128::from(self.timebase.denom)
                    > 15_000_000_000
            {
                return Err(ToolError::Limit);
            }
        }
        Ok(())
    }

    pub(super) async fn stop(&mut self, except: u32) -> Result<(), ToolError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let members = self.members()?;
            let members: Vec<_> = members
                .into_iter()
                .filter(|member| member.pid != except)
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
            tokio::task::yield_now().await;
        }
    }
}

fn peer(stream: &UnixStream) -> Result<u32, ToolError> {
    let mut pid = 0_i32;
    let mut credentials = MaybeUninit::<libc::xucred>::uninit();
    let mut pid_size = size_of::<i32>() as libc::socklen_t;
    let mut credential_size = size_of::<libc::xucred>() as libc::socklen_t;
    // Both socket options write exactly their installed SDK layouts into live buffers.
    let complete = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut i32).cast(),
            &mut pid_size,
        ) == 0
            && libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERCRED,
                credentials.as_mut_ptr().cast(),
                &mut credential_size,
            ) == 0
    };
    if !complete
        || pid_size as usize != size_of::<i32>()
        || credential_size as usize != size_of::<libc::xucred>()
        || pid <= 1
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    // The exact credential write above initializes the integer-only xucred structure.
    let credentials = unsafe { credentials.assume_init() };
    if credentials.cr_version != 0 || credentials.cr_uid != rustix::process::geteuid().as_raw() {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(pid as u32)
}

async fn disk_command(root: &Path, arguments: &[&str]) -> Result<(), ToolError> {
    let metadata = std::fs::symlink_metadata("/usr/bin/hdiutil")
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o6022 != 0 {
        return Err(ToolError::ProtectionUnavailable);
    }
    let mut command = Command::new("/usr/bin/hdiutil");
    command
        .args(arguments)
        .env_clear()
        .env("HOME", root)
        .env("TMPDIR", root)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let (status, _, _) = super::super::capture(
        command
            .spawn()
            .map_err(|_| ToolError::ProtectionUnavailable)?,
        CAPTURE_BYTES,
        Duration::from_secs(20),
    )
    .await?;
    if status.success() {
        Ok(())
    } else {
        Err(ToolError::ProtectionUnavailable)
    }
}

async fn prepare_storage(root: &Path) -> Result<[cap_std::fs::Dir; 2], ToolError> {
    use cap_fs_ext::{DirExt, OpenOptionsFollowExt};
    use cap_std::fs::{OpenOptionsExt, PermissionsExt};

    StateRoot::open_existing(root).map_err(|_| ToolError::Path)?;
    let directory = crate::tools::fs::open_directory(root)?;
    let parent = crate::tools::fs::identity(&directory)?;
    let mut volumes = Vec::with_capacity(2);
    for name in STORAGE_NAMES {
        let image_name = format!("{name}.dmg");
        for entry in [name, image_name.as_str()] {
            match directory.symlink_metadata(entry) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err(ToolError::ChangedInput),
            }
        }
        let mut builder = cap_std::fs::DirBuilder::new();
        cap_std::fs::DirBuilderExt::mode(&mut builder, 0o700);
        directory
            .create_dir_with(name, &builder)
            .map_err(|_| ToolError::Operation)?;
        let image = root.join(&image_name);
        let mount = root.join(name);
        let image_path = image.to_str().ok_or(ToolError::Path)?;
        let mount_path = mount.to_str().ok_or(ToolError::Path)?;
        directory
            .open_with(
                STORAGE_PENDING,
                cap_std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .follow(cap_fs_ext::FollowSymlinks::No),
            )
            .map_err(|_| ToolError::Operation)?;
        disk_command(
            root,
            &[
                "create",
                "-quiet",
                "-size",
                "64m",
                "-layout",
                "NONE",
                "-type",
                "UDIF",
                "-fs",
                "Case-sensitive HFS+",
                "-nospotlight",
                "-volname",
                name,
                image_path,
            ],
        )
        .await?;
        let image = directory
            .open_with(
                &image_name,
                cap_std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .follow(cap_fs_ext::FollowSymlinks::No),
            )
            .map_err(|_| ToolError::Path)?;
        use cap_std::fs::MetadataExt;
        let metadata = image.metadata().map_err(|_| ToolError::Path)?;
        if !metadata.is_file()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.nlink() != 1
            || metadata.len() != STORAGE_BYTES_CAP
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        image
            .set_permissions(cap_std::fs::Permissions::from_mode(0o600))
            .map_err(|_| ToolError::Path)?;
        disk_command(
            root,
            &[
                "attach",
                "-quiet",
                "-kernel",
                image_path,
                "-mountpoint",
                mount_path,
                "-mount",
                "required",
                "-owners",
                "on",
                "-nobrowse",
                "-noautoopen",
                "-noautofsck",
                "-verify",
            ],
        )
        .await?;
        directory
            .remove_file(STORAGE_PENDING)
            .map_err(|_| ToolError::Operation)?;
        let volume = directory
            .open_dir_nofollow(name)
            .map_err(|_| ToolError::Path)?;
        verify_volume(&volume, parent.0)?;
        volume
            .set_permissions(".", cap_std::fs::Permissions::from_mode(0o700))
            .map_err(|_| ToolError::Path)?;
        volumes.push(volume);
    }
    volumes
        .try_into()
        .map_err(|_| ToolError::ProtectionUnavailable)
}

fn verify_volume(volume: &cap_std::fs::Dir, parent_device: u64) -> Result<(), ToolError> {
    use cap_std::fs::MetadataExt;

    let metadata = volume.dir_metadata().map_err(|_| ToolError::Path)?;
    let capacity = rustix::fs::fstatvfs(volume).map_err(|_| ToolError::ProtectionUnavailable)?;
    if metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.dev() == parent_device
        || capacity.f_blocks.checked_mul(capacity.f_frsize) != Some(STORAGE_BYTES_CAP)
        || !capacity
            .f_flag
            .contains(rustix::fs::StatVfsMountFlags::NOSUID)
        || capacity
            .f_flag
            .contains(rustix::fs::StatVfsMountFlags::RDONLY)
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

pub(super) fn verify_storage(root: &Path) -> Result<(), ToolError> {
    use cap_fs_ext::DirExt;
    use cap_std::fs::MetadataExt;

    let directory = crate::tools::fs::open_directory(root)?;
    let parent = crate::tools::fs::identity(&directory)?;
    for name in STORAGE_NAMES {
        let volume = directory
            .open_dir_nofollow(name)
            .map_err(|_| ToolError::Path)?;
        verify_volume(&volume, parent.0)?;
        if volume.dir_metadata().map_err(|_| ToolError::Path)?.mode() & 0o7777 != 0o700 {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    Ok(())
}

async fn prepare_command_storage(
    root: &Path,
    runtime: &ToolRuntime,
) -> Result<[cap_std::fs::Dir; 2], ToolError> {
    use cap_fs_ext::DirExt;

    runtime.config.validate()?;
    let workspace = crate::tools::fs::open_directory(&runtime.workspace)?;
    if crate::tools::fs::identity(&workspace)? != runtime.workspace_identity {
        return Err(ToolError::ChangedInput);
    }
    crate::tools::fs::reject_overlap(root, &runtime.workspace)?;
    let volumes = prepare_storage(root).await?;
    let directory = crate::tools::fs::open_directory(root)?;
    let mut builder = cap_std::fs::DirBuilder::new();
    cap_std::fs::DirBuilderExt::mode(&mut builder, 0o700);
    directory
        .create_dir_with("runtime", &builder)
        .map_err(|_| ToolError::Operation)?;
    let resources = directory
        .open_dir_nofollow("runtime")
        .map_err(|_| ToolError::Path)?;
    crate::tools::fs::snapshot_into(&workspace, &runtime.config, Some(&volumes[0]), &resources)?;
    Ok(volumes)
}

fn storage_mounted(root: &Path) -> Result<bool, ToolError> {
    let directory = crate::tools::fs::open_directory(root)?;
    let parent = crate::tools::fs::identity(&directory)?;
    for name in STORAGE_NAMES.into_iter().chain([STORAGE_PENDING]) {
        let metadata = match directory.symlink_metadata(name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(ToolError::Path),
        };
        use cap_std::fs::MetadataExt;
        if name == STORAGE_PENDING || !metadata.is_dir() || metadata.dev() != parent.0 {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn cleanup_storage(root: &Path) -> Result<(), ToolError> {
    let directory = crate::tools::fs::open_directory(root)?;
    let parent = crate::tools::fs::identity(&directory)?;
    for name in STORAGE_NAMES {
        let metadata = match directory.symlink_metadata(name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(ToolError::Path),
        };
        use cap_std::fs::MetadataExt;
        if !metadata.is_dir() || metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(ToolError::Path);
        }
        if metadata.dev() != parent.0 {
            let mount = root.join(name);
            disk_command(
                root,
                &[
                    "detach",
                    "-force",
                    "-quiet",
                    mount.to_str().ok_or(ToolError::Path)?,
                ],
            )
            .await?;
        }
    }
    if storage_mounted(root)? {
        Err(ToolError::ProtectionUnavailable)
    } else {
        Ok(())
    }
}

struct Unit {
    root: PathBuf,
    _state: StateRoot,
    target: String,
    scope: Option<Scope>,
    stopped: bool,
}

impl Unit {
    fn new() -> Result<Self, ToolError> {
        let name = format!("arany-u-{}", uuid::Uuid::now_v7());
        let root = Path::new("/private/tmp").join(&name);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .map_err(|_| ToolError::Operation)?;
        let state = match StateRoot::admit(&root) {
            Ok(state) => state,
            Err(_) => {
                let _ = std::fs::remove_dir(&root);
                return Err(ToolError::Path);
            }
        };
        Ok(Self {
            root,
            _state: state,
            target: format!(
                "user/{}/dev.arany.tool.{name}",
                rustix::process::geteuid().as_raw()
            ),
            scope: None,
            stopped: false,
        })
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
        let label = self
            .target
            .rsplit_once('/')
            .ok_or(ToolError::Configuration)?
            .1;
        let arguments: Vec<_> = arguments.iter().map(|value| CFString::new(value)).collect();
        let entries = [
            ("Label", CFString::new(label).as_CFType()),
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

    async fn manager(&self, action: &str, path: Option<&Path>) -> Result<Vec<u8>, ToolError> {
        let mut command = Command::new("/bin/launchctl");
        command.env_clear().current_dir("/").arg(action);
        if action == "kickstart" {
            command.arg("-p");
        }
        if let Some(path) = path {
            command
                .arg(
                    self.target
                        .rsplit_once('/')
                        .ok_or(ToolError::Configuration)?
                        .0,
                )
                .arg(path);
        } else {
            command.arg(&self.target);
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
        if status.success() || (action == "bootout" && status.code() == Some(libc::ESRCH)) {
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
        self.manager("bootstrap", Some(&path)).await?;
        let launched = self.manager("kickstart", None).await?;
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
            scope.stop(0).await
        } else {
            Ok(())
        };
        let removal = self.manager("bootout", None).await;
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
    dispatched: &mut bool,
) -> Result<ToolObservation, ToolError> {
    let mut unit = Unit::new()?;
    crate::tools::fs::reject_overlap(&unit.root, &runtime.workspace)?;
    let execution = async {
        let mut stream = BufReader::new(
            tokio::time::timeout(Duration::from_secs(5), unit.connect(runtime))
                .await
                .map_err(|_| ToolError::Limit)??,
        );
        let task = serde_json::to_vec(&Task {
            config: runtime.config.clone(),
            intent: intent.clone(),
            workspace: runtime.workspace.clone(),
            guard_executable: runtime.guard_executable.clone(),
        })
        .map_err(|_| ToolError::Configuration)?;
        if task.len() > 96 * 1024 {
            return Err(ToolError::Limit);
        }
        stream
            .get_mut()
            .write_all(&task)
            .await
            .map_err(|_| ToolError::Operation)?;
        stream
            .get_mut()
            .write_all(b"\n")
            .await
            .map_err(|_| ToolError::Operation)?;
        let bytes = read_line_capped(&mut stream, 2048).await?;
        if let Ok(failure) = serde_json::from_slice::<BootstrapFailure>(&bytes) {
            return Err(ToolError::GuardBootstrap(failure));
        }
        let ready: Ready =
            serde_json::from_slice(&bytes).map_err(|_| ToolError::ProtectionUnavailable)?;
        if ready.digest != intent.digest()
            || ready.profile_digest != intent.enforcement_digest
            || identity(ready.pid)?.coalition
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
            .observe()
            .map_err(|_| ToolError::GuardRejected("resource attestation"))?;
        *dispatched = true;
        stream
            .get_mut()
            .write_all(b"GO\n")
            .await
            .map_err(|_| ToolError::Operation)?;
        let observation: ToolObservation =
            serde_json::from_slice(&read_line_capped(&mut stream, CAPTURE_BYTES).await?)
                .map_err(|_| ToolError::Operation)?;
        if observation.intent != *intent
            || !observation.valid()
            || observation
                .guard
                .as_ref()
                .is_none_or(|receipt| !receipt.valid_for(intent))
            || !read_capped(stream, 1).await?.is_empty()
        {
            return Err(ToolError::Operation);
        }
        Ok(observation)
    };
    let deadline =
        Duration::from_millis(u64::from(intent.limits.runtime_ms)) + Duration::from_secs(5);
    let result = tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(ToolError::Operation),
        result = tokio::time::timeout(deadline, execution) => result.unwrap_or(Err(ToolError::Limit)),
    };
    unit.stop()
        .await
        .map_err(|_| ToolError::GuardRejected("native unit cleanup"))?;
    result
}

pub(super) fn helper_main() -> std::process::ExitCode {
    let mut diagnostic = None;
    let mut stage = super::super::BootstrapStage::Admission;
    let result = (|| -> Result<(), ToolError> {
        bootstrap()?;
        let path = std::env::args_os().nth(2).ok_or(ToolError::Configuration)?;
        let path = Path::new(&path);
        let _root =
            StateRoot::admit(path.parent().ok_or(ToolError::Path)?).map_err(|_| ToolError::Path)?;
        let metadata = std::fs::symlink_metadata(path).map_err(|_| ToolError::Path)?;
        use std::os::unix::fs::FileTypeExt;
        if !metadata.file_type().is_socket()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o777 != 0o600
            || metadata.nlink() != 1
        {
            return Err(ToolError::Path);
        }
        let mut stream = UnixStream::connect(path).map_err(|_| ToolError::Operation)?;
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
        let mut task = Vec::new();
        loop {
            let mut byte = [0];
            if stream.read(&mut byte).map_err(|_| ToolError::Operation)? != 1 {
                return Err(ToolError::Operation);
            }
            if byte[0] == b'\n' {
                break;
            }
            if task.len() == 96 * 1024 {
                return Err(ToolError::Limit);
            }
            task.push(byte[0]);
        }
        let task: Task = serde_json::from_slice(&task).map_err(|_| ToolError::Configuration)?;
        stage = super::super::BootstrapStage::Capabilities;
        if task.guard_executable
            != std::env::current_exe().map_err(|_| ToolError::ProtectionUnavailable)?
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let runtime = ToolRuntime {
            config: task.config,
            workspace: task.workspace,
            workspace_identity: (task.intent.workspace_device, task.intent.workspace_inode),
            guard_executable: task.guard_executable,
            enforcement_digest: task.intent.enforcement_digest,
        };
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
                let _storage = if matches!(
                    task.intent.call,
                    ToolCall::Command { .. } | ToolCall::McpList { .. } | ToolCall::McpCall { .. }
                ) {
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
                    &task.intent,
                    cancellation,
                    &mut scope,
                    &mut output,
                    receive_go,
                    path.parent().ok_or(ToolError::Path)?,
                )
                .await
            }
            .await;
            let cleanup = scope.stop(std::process::id()).await;
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
    use super::*;

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
        unsafe extern "C" {
            fn posix_spawnattr_setcoalition_np(
                attributes: *const libc::posix_spawnattr_t,
                coalition: u64,
                kind: libc::c_int,
                role: libc::c_int,
            ) -> libc::c_int;
        }
        let mut attributes = MaybeUninit::<libc::posix_spawnattr_t>::uninit();
        let mut child = 0;
        let mut args = [c"/usr/bin/true".as_ptr().cast_mut(), std::ptr::null_mut()];
        let mut environment = [std::ptr::null_mut()];
        // Initialized spawn attributes and NUL-terminated arrays live through the native calls.
        let status = unsafe {
            assert_eq!(libc::posix_spawnattr_init(attributes.as_mut_ptr()), 0);
            let mut attributes = attributes.assume_init();
            assert_eq!(
                posix_spawnattr_setcoalition_np(&attributes, coalition, 0, 0),
                0
            );
            let status = libc::posix_spawn(
                &mut child,
                args[0],
                std::ptr::null(),
                &attributes,
                args.as_mut_ptr(),
                environment.as_mut_ptr(),
            );
            assert_eq!(libc::posix_spawnattr_destroy(&mut attributes), 0);
            if status == 0 {
                assert_eq!(libc::waitpid(child, std::ptr::null_mut(), 0), child);
            }
            status
        };
        assert_eq!(
            status,
            libc::EPERM,
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
        }
    }

    #[tokio::test]
    #[ignore = "native aggregate-resource and escaped-descendant gate; finite synthetic jobs"]
    async fn native_unit_owns_descendants_and_supervises_resources() {
        use crate::test_process::BoundedOutput;

        let generated = tempfile::tempdir().unwrap();
        let source = generated.path().join("child.c");
        let executable = generated.path().join("child");
        std::fs::write(&source, br#"#include <spawn.h>
#include <stdio.h>
#include <sys/wait.h>
int main(void) {
    char *arguments[] = {"/usr/bin/true", NULL};
    char *environment[] = {NULL};
    pid_t child = 0;
    int status = 0;
    if (posix_spawn(&child, arguments[0], NULL, NULL, arguments, environment) != 0) return 1;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) return 2;
    puts("native child success");
    return 0;
}
"#).unwrap();
        let compiled = std::process::Command::new("/usr/bin/xcrun")
            .args([
                "--sdk", "macosx", "clang", "-Wall", "-Wextra", "-Werror", "-O1",
            ])
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", generated.path())
            .env("TMPDIR", generated.path())
            .current_dir(generated.path())
            .bounded_output_for(Duration::from_secs(20), 16 * 1024)
            .unwrap();
        assert!(
            compiled.status.success(),
            "native fixture compilation: {compiled:?}"
        );
        assert!(compiled.stdout.is_empty() && compiled.stderr.is_empty());
        let generated_bytes = std::fs::read(&executable).unwrap();
        assert!(generated_bytes.len() < 1024 * 1024);
        let actor_source = generated.path().join("actor.c");
        let actor = generated.path().join("actor");
        let plain = generated.path().join("plain");
        let entitlements = generated.path().join("entitlements.plist");
        std::fs::write(
            &actor_source,
            br#"#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
#include <errno.h>
int main(int argc, char **argv) {
    if (argc != 2) return 1;
    int denied = strcmp(argv[1], "deny") == 0;
    char *arguments[] = {"/usr/bin/true", NULL};
    char *environment[] = {NULL};
    pid_t child = 0;
    int status = 0;
    int result = posix_spawn(&child, arguments[0], NULL, NULL, arguments, environment);
    if (denied ? result != EPERM : result != 0) return 2;
    if (!result && (waitpid(child, &status, 0) != child || status != 0)) return 3;
    child = fork();
    if (child == 0) _exit(0);
    if (denied ? child != -1 || errno != EPERM : child == -1) return 4;
    if (child > 0 && (waitpid(child, &status, 0) != child || status != 0)) return 5;
    puts("native actor success");
    return 0;
}
"#,
        )
        .unwrap();
        let compiled = std::process::Command::new("/usr/bin/xcrun")
            .args([
                "--sdk", "macosx", "clang", "-Wall", "-Wextra", "-Werror", "-O1",
            ])
            .arg(&actor_source)
            .arg("-o")
            .arg(&actor)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", generated.path())
            .env("TMPDIR", generated.path())
            .current_dir(generated.path())
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
            .env("HOME", generated.path())
            .env("TMPDIR", generated.path())
            .current_dir(generated.path())
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
            std::fs::write(source.path().join("script"), &generated_bytes).unwrap();
            std::fs::set_permissions(
                source.path().join("script"),
                std::fs::Permissions::from_mode(0o6755),
            )
            .unwrap();
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
                source.path().to_str().unwrap()
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
            unit.manager("bootstrap", Some(&path)).await.unwrap();
            let pid: u32 = std::str::from_utf8(&unit.manager("kickstart", None).await.unwrap())
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let root = identity(pid).unwrap();
            assert_ne!(
                root.coalition,
                identity(std::process::id()).unwrap().coalition
            );
            unit.scope = Some(Scope::new(root.coalition).unwrap());
            let accepted = tokio::time::timeout(Duration::from_secs(10), listener.accept()).await;
            assert!(
                accepted.is_ok(),
                "native {phase} readiness: {}",
                diagnostic(&unit.root)
            );
            let stream = accepted.unwrap().unwrap().0;
            let mut attestation = stream.into_std().unwrap();
            assert_eq!(peer(&attestation).unwrap(), pid);
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
                scope.observe().unwrap();
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
                        .unwrap(),
                        b"PROFILE\n"
                    );
                    tokio::time::timeout(Duration::from_secs(5), async {
                        let mut samples = tokio::time::interval(Duration::from_millis(10));
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
                    assert!(
                        output.contains("1 passed; 0 failed"),
                        "native profile helper completion: {output}"
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
                scope.observe().unwrap();
                assert_eq!(
                    scope.members().unwrap().len(),
                    usize::from(!matches!(phase, "command-profile" | "entitlement-profile"))
                );
                if phase != "storage-drop" {
                    scope.stop(0).await.unwrap();
                    assert_eq!(
                        storage_mounted(&unit.root).unwrap(),
                        phase != "entitlement-profile"
                    );
                }
            } else {
                let mut samples = tokio::time::interval(Duration::from_millis(10));
                let observed = tokio::time::timeout(Duration::from_secs(60), async {
                    loop {
                        samples.tick().await;
                        match scope.observe() {
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
        unsafe {
            libc::alarm(75);
            assert_ne!(libc::signal(libc::SIGXCPU, libc::SIG_IGN), libc::SIG_ERR);
        }
        rustix::process::setrlimit(
            rustix::process::Resource::Cpu,
            rustix::process::Rlimit {
                current: Some(15),
                maximum: Some(15),
            },
        )
        .unwrap();
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
            let mut profile = super::super::COMMAND_PROFILE.to_owned();
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
                        enforcement_digest: runtime.enforcement_digest,
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
                };
                let (mut wake_sender, mut wake_receiver) = UnixStream::pair().unwrap();
                wake_sender.write_all(&[1]).unwrap();
                wake_receiver.read_exact(&mut [0]).unwrap();
                super::super::configure_resources().unwrap();
                super::super::install(&task).unwrap();
                assert_eq!(
                    wake_sender.write(&[1]).unwrap_err().kind(),
                    std::io::ErrorKind::PermissionDenied
                );
                assert_eq!(
                    std::process::Command::new("/bin/launchctl")
                        .arg("help")
                        .status()
                        .unwrap_err()
                        .kind(),
                    std::io::ErrorKind::PermissionDenied
                );
                // Signal 0 observes permission without risking a signal to the fixture's outside owner.
                assert_eq!(unsafe { libc::kill(outside_pid, 0) }, -1);
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EPERM)
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
                    .bounded_output_for(Duration::from_secs(5), 1024)
                    .unwrap();
                assert!(
                    output.status.success(),
                    "ordinary Rust child launch: {output:?}"
                );
                assert!(output.stdout.is_empty() && output.stderr.is_empty());
                verify_coalition_spawn_denied(outside_coalition);
                for syscall in [459, 494] {
                    // Invalid operations and null arguments cannot read or mutate host state.
                    assert_eq!(
                        unsafe { libc::syscall(syscall, -1_i32, 0_usize, 0_usize, 0_usize) },
                        -1
                    );
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::EPERM)
                    );
                }
                for definition in [&runtime.config.commands[1], &runtime.config.commands[0]] {
                    let args = if definition.name == "native" {
                        Vec::new()
                    } else {
                        vec!["-c".into(), "exec ./script".into()]
                    };
                    let mut command = super::super::super::payload(definition, &args, ".").unwrap();
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
                    assert_eq!(stdout, b"native child success\n");
                    assert!(stderr.is_empty());
                }
                let mut command = super::super::super::payload(
                    &runtime.config.commands[0],
                    &["-c".into(), "set -eu; /usr/bin/true; test \"$(/bin/cat \"$ARANY_TOOL_INPUTS/shell/fixture.txt\")\" = 'pinned synthetic input'; printf 'command change' > \"$1\"; printf 'native command success'".into(), "--".into(), "/workspace/selected".into()],
                    ".",
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
                    .block_on(crate::tools::mcp::execute(&call, &runtime.config))
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

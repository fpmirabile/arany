use super::*;
use crate::tools::types::BYTES_PER_MEBIBYTE;
use nix::sched::{CloneFlags, unshare};
use rustix::mount::{MountFlags, MountPropagationFlags};
use rustix::thread::{self, CapabilitySet, CapabilitySets};
use serde::{Deserialize, Serialize};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::MetadataExt;

#[cfg(test)]
mod tests;

const MOUNT_DIRECTORY: &str = "namespace";
const PROBE_NAME: &str = "user.arany_probe";
const PROBE_VALUE: &[u8] = b"arany";
const MAX_SUPERVISOR_ARGUMENTS: usize = 4096;
const MAX_PIN_BYTES: usize = 4096;
pub(super) const MAX_SUPERVISOR_EXECUTABLE_BYTES: u64 = crate::tools::types::MEMORY_BYTES;
pub(super) const TMPFS_INODES: u64 = (crate::tools::types::COMMAND_SCAN_ENTRIES
    + crate::tools::config::MAX_WORKSPACE_PATHS * 2
    + 2) as u64;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pin {
    device: u64,
    inode: u64,
    sha256: [u8; 32],
}

impl Pin {
    fn read(path: &Path) -> Result<Self, ToolError> {
        let mut file = if path == Path::new("/proc/self/exe") {
            std::fs::File::open(path).map_err(|_| ToolError::ChangedInput)?
        } else {
            crate::tools::fs::open_absolute(path.to_str().ok_or(ToolError::Path)?)?
        };
        Self::from_file(&mut file)
    }

    fn from_file(file: &mut std::fs::File) -> Result<Self, ToolError> {
        use sha2::{Digest, Sha256};
        let before = file.metadata().map_err(|_| ToolError::ChangedInput)?;
        if !before.is_file()
            || before.mode() & 0o6022 != 0
            || before.len() > MAX_SUPERVISOR_EXECUTABLE_BYTES
        {
            return Err(ToolError::ChangedInput);
        }
        let mut hash = Sha256::new();
        let mut buffer = [0; 64 * 1024];
        let mut reader = (&mut *file).take(MAX_SUPERVISOR_EXECUTABLE_BYTES + 1);
        let mut total = 0;
        loop {
            let count = reader
                .read(&mut buffer)
                .map_err(|_| ToolError::ChangedInput)?;
            if count == 0 {
                break;
            }
            total += count as u64;
            if total > MAX_SUPERVISOR_EXECUTABLE_BYTES {
                return Err(ToolError::Limit);
            }
            hash.update(&buffer[..count]);
        }
        let after = file.metadata().map_err(|_| ToolError::ChangedInput)?;
        if before.len() != total
            || before.mtime_nsec() != after.mtime_nsec()
            || before.mtime() != after.mtime()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
        {
            return Err(ToolError::ChangedInput);
        }
        Ok(Self {
            device: before.dev(),
            inode: before.ino(),
            sha256: hash.finalize().into(),
        })
    }

    fn verify(&self, path: &Path) -> Result<(), ToolError> {
        let current = Self::read(path)?;
        if (self.device, self.inode, self.sha256) != (current.device, current.inode, current.sha256)
        {
            return Err(ToolError::ChangedInput);
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Launch {
    pin: Pin,
    probe: Option<ProjectProbe>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectProbe {
    root: PathBuf,
    bytes: u64,
}

impl Launch {
    pub(super) fn new(
        guard: &Path,
        seed: &Path,
        intent: &EffectIntent,
    ) -> Result<(Self, std::fs::File), ToolError> {
        if std::fs::canonicalize(guard).map_err(|_| ToolError::ChangedInput)? != guard {
            return Err(ToolError::ChangedInput);
        }
        let mut executable =
            crate::tools::fs::open_absolute(guard.to_str().ok_or(ToolError::Path)?)?;
        let pin = Pin::from_file(&mut executable)?;
        let probe = if primary_command(intent) {
            Some(ProjectProbe::new(seed, intent.limits.project_bytes)?)
        } else {
            None
        };
        Ok((Self { pin, probe }, executable))
    }

    pub(super) fn append(&self, command: &mut Command, guard: &Path) -> Result<(), ToolError> {
        command
            .args(["/usr/bin/env", "-i", "PATH=/usr/bin:/bin"])
            .arg(guard)
            .args(["--internal-tool-guard", "--supervisor"])
            .arg(serde_json::to_string(self).map_err(|_| ToolError::Configuration)?)
            .arg("--");
        Ok(())
    }

    pub(super) fn helper_pin(&self) -> Result<String, ToolError> {
        serde_json::to_string(&self.pin.sha256).map_err(|_| ToolError::Configuration)
    }
}

pub(super) fn executable_path(executable: &std::fs::File) -> PathBuf {
    PathBuf::from(format!(
        "/proc/{}/fd/{}",
        std::process::id(),
        executable.as_raw_fd()
    ))
}

impl ProjectProbe {
    fn new(seed: &Path, bytes: u64) -> Result<Self, ToolError> {
        let directory = crate::tools::fs::open_directory(seed)?;
        match directory.create_dir(MOUNT_DIRECTORY) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(ToolError::ProtectionUnavailable),
        }
        let root = seed.join(MOUNT_DIRECTORY);
        let pinned = crate::tools::fs::open_directory(&root)?;
        if pinned
            .entries()
            .map_err(|_| ToolError::ProtectionUnavailable)?
            .next()
            .is_some()
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        Ok(Self { root, bytes })
    }
}

pub(in crate::tools::guard) fn verify_helper() -> Result<(), ToolError> {
    let encoded = std::env::args_os().nth(2).ok_or(ToolError::Configuration)?;
    if encoded.len() > MAX_PIN_BYTES {
        return Err(ToolError::Limit);
    }
    let sha256: [u8; 32] = serde_json::from_str(encoded.to_str().ok_or(ToolError::Configuration)?)
        .map_err(|_| ToolError::Configuration)?;
    if Pin::read(Path::new("/proc/self/exe"))?.sha256 != sha256 {
        return Err(ToolError::ChangedInput);
    }
    verify_helper_mount()
}

pub(in crate::tools::guard) fn main() -> std::process::ExitCode {
    let mut stage = super::super::BootstrapStage::Admission;
    let result = run(&mut stage);
    if result.is_ok() {
        std::process::ExitCode::SUCCESS
    } else if !matches!(stage, super::super::BootstrapStage::LauncherReap) {
        let _ = serde_json::to_writer(
            std::io::stderr().lock(),
            &super::super::BootstrapFailure::at(stage),
        );
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::FAILURE
    }
}

fn run(stage: &mut super::super::BootstrapStage) -> Result<(), ToolError> {
    let mut environment = std::env::vars_os();
    if environment.next() != Some(("PATH".into(), "/usr/bin:/bin".into()))
        || environment.next().is_some()
    {
        return Err(ToolError::Configuration);
    }
    let mut arguments = Vec::new();
    let mut bytes = 0usize;
    for argument in std::env::args_os().skip(3) {
        bytes = bytes.checked_add(argument.len()).ok_or(ToolError::Limit)?;
        if bytes > super::super::TASK_BYTES_CAP || arguments.len() >= MAX_SUPERVISOR_ARGUMENTS {
            return Err(ToolError::Limit);
        }
        arguments.push(argument);
    }
    if arguments.len() < 3 || arguments[1] != "--" {
        return Err(ToolError::Configuration);
    }
    let launch: Launch =
        serde_json::from_str(arguments[0].to_str().ok_or(ToolError::Configuration)?)
            .map_err(|_| ToolError::Configuration)?;
    launch.pin.verify(Path::new("/proc/self/exe"))?;
    let executable = std::fs::File::open("/proc/self/exe").map_err(|_| ToolError::ChangedInput)?;
    if launch.probe.as_ref().is_some_and(|probe| {
        !probe.root.is_absolute()
            || probe.bytes == 0
            || probe.bytes > crate::tools::types::MAX_BUILD_PROJECT_BYTES
            || !probe.bytes.is_multiple_of(BYTES_PER_MEBIBYTE)
    }) {
        return Err(ToolError::Configuration);
    }
    let parent = profile::Namespaces::read()?;
    *stage = super::super::BootstrapStage::NamespaceEntry;
    enter()?;
    if !profile::Namespaces::read()?.supervisor_isolated_from(&parent) {
        return Err(ToolError::ProtectionUnavailable);
    }
    if let Some(probe) = &launch.probe {
        *stage = super::super::BootstrapStage::RuntimeMounts;
        mount_probe(probe)?;
    }
    *stage = super::super::BootstrapStage::Capabilities;
    clear_capabilities()?;
    verify_capabilities(
        &thread_status(Path::new("/proc/self/status"))?.ok_or(ToolError::ProtectionUnavailable)?,
    )?;
    *stage = super::super::BootstrapStage::LauncherReap;
    wait_bwrap(&arguments[2..], &executable)
}

fn verify_helper_mount() -> Result<(), ToolError> {
    let target = PathBuf::from("/arany");
    let mounts = read_mountinfo()?;
    verify_mount_entries(&mounts, std::slice::from_ref(&target))?;
    let entry = mounts
        .iter()
        .find(|entry| entry.path == target)
        .ok_or(ToolError::ProtectionUnavailable)?;
    if entry.filesystem != b"tmpfs"
        || entry.source != b"tmpfs"
        || ![b"ro".as_slice(), b"nosuid", b"nodev"]
            .iter()
            .all(|expected| {
                entry
                    .options
                    .split(|byte| *byte == b',')
                    .any(|option| option == *expected)
            })
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

fn enter() -> Result<(), ToolError> {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS)
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    for (path, value) in [
        ("/proc/self/setgroups", "deny".to_owned()),
        ("/proc/self/uid_map", format!("{uid} {uid} 1\n")),
        ("/proc/self/gid_map", format!("{gid} {gid} 1\n")),
    ] {
        std::fs::write(path, value).map_err(|_| ToolError::ProtectionUnavailable)?;
    }
    rustix::mount::mount_change(
        "/",
        MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
    )
    .map_err(|_| ToolError::ProtectionUnavailable)
}

fn mount_probe(probe: &ProjectProbe) -> Result<(), ToolError> {
    let options = std::ffi::CString::new(format!(
        "size={},nr_inodes={TMPFS_INODES},mode=0700",
        probe.bytes
    ))
    .map_err(|_| ToolError::Configuration)?;
    rustix::mount::mount(
        "tmpfs",
        &probe.root,
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV,
        options.as_c_str(),
    )
    .map_err(|_| ToolError::ProtectionUnavailable)?;
    let mounts = read_mountinfo()?;
    verify_mount_entries(&mounts, std::slice::from_ref(&probe.root))?;
    let entry = mounts
        .iter()
        .find(|entry| entry.path == probe.root)
        .ok_or(ToolError::ProtectionUnavailable)?;
    if entry.filesystem != b"tmpfs"
        || entry.source != b"tmpfs"
        || entry.root != Path::new("/")
        || !entry
            .options
            .split(|byte| *byte == b',')
            .any(|option| option == b"nosuid")
        || !entry
            .options
            .split(|byte| *byte == b',')
            .any(|option| option == b"nodev")
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    let inode_option = format!("nr_inodes={TMPFS_INODES}");
    let size_option = format!("size={}k", probe.bytes.div_ceil(1024));
    if ![inode_option.as_bytes(), size_option.as_bytes()]
        .iter()
        .all(|expected| {
            entry
                .super_options
                .split(|byte| *byte == b',')
                .filter(|option| option == expected)
                .count()
                == 1
        })
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    let path = probe.root.join("probe");
    let file = std::fs::File::create_new(&path).map_err(|_| ToolError::ProtectionUnavailable)?;
    probe_xattr(&file)?;
    drop(file);
    std::fs::remove_file(path).map_err(|_| ToolError::ProtectionUnavailable)
}

fn probe_xattr(file: &std::fs::File) -> Result<(), ToolError> {
    use rustix::fs::{XattrFlags, fgetxattr, fremovexattr, fsetxattr};
    fsetxattr(file, PROBE_NAME, PROBE_VALUE, XattrFlags::CREATE)
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    let mut value = [0; PROBE_VALUE.len()];
    let count =
        fgetxattr(file, PROBE_NAME, &mut value).map_err(|_| ToolError::ProtectionUnavailable)?;
    if count != PROBE_VALUE.len() || value != PROBE_VALUE {
        return Err(ToolError::ProtectionUnavailable);
    }
    fremovexattr(file, PROBE_NAME).map_err(|_| ToolError::ProtectionUnavailable)?;
    if fgetxattr(file, PROBE_NAME, &mut value) != Err(rustix::io::Errno::NODATA) {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

fn clear_capabilities() -> Result<(), ToolError> {
    let last = thread_status(Path::new("/proc/sys/kernel/cap_last_cap"))?
        .ok_or(ToolError::ProtectionUnavailable)?
        .trim()
        .parse::<u32>()
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    if last >= u64::BITS {
        return Err(ToolError::ProtectionUnavailable);
    }
    for bit in 0..=last {
        thread::remove_capability_from_bounding_set(CapabilitySet::from_bits_retain(1 << bit))
            .map_err(|_| ToolError::ProtectionUnavailable)?;
    }
    thread::clear_ambient_capability_set().map_err(|_| ToolError::ProtectionUnavailable)?;
    thread::set_capabilities(
        None,
        CapabilitySets {
            effective: CapabilitySet::empty(),
            permitted: CapabilitySet::empty(),
            inheritable: CapabilitySet::empty(),
        },
    )
    .map_err(|_| ToolError::ProtectionUnavailable)?;
    thread::set_no_new_privs(true).map_err(|_| ToolError::ProtectionUnavailable)
}

fn verify_capabilities(status: &str) -> Result<(), ToolError> {
    for (name, expected) in [
        ("NoNewPrivs:", "1"),
        ("CapEff:", "0000000000000000"),
        ("CapPrm:", "0000000000000000"),
        ("CapInh:", "0000000000000000"),
        ("CapAmb:", "0000000000000000"),
        ("CapBnd:", "0000000000000000"),
    ] {
        let values: Vec<_> = status
            .lines()
            .filter_map(|line| line.strip_prefix(name))
            .collect();
        if values.len() != 1 || values[0].trim() != expected {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    Ok(())
}

fn wait_bwrap(
    arguments: &[std::ffi::OsString],
    executable: &std::fs::File,
) -> Result<(), ToolError> {
    use rustix::process::{Pid, PidfdFlags, WaitId, WaitIdOptions, pidfd_open, waitid};
    rustix::io::fcntl_setfd(executable, rustix::io::FdFlags::empty())
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    let mut child = std::process::Command::new("/usr/bin/bwrap")
        .args(["--perms", "0500", "--ro-bind-data"])
        .arg(executable.as_raw_fd().to_string())
        .arg("/arany")
        .args(arguments)
        .env_clear()
        .current_dir("/")
        .spawn()
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    let result = (|| {
        let pid = i32::try_from(child.id())
            .ok()
            .and_then(Pid::from_raw)
            .ok_or(ToolError::ProtectionUnavailable)?;
        let pidfd =
            pidfd_open(pid, PidfdFlags::empty()).map_err(|_| ToolError::ProtectionUnavailable)?;
        loop {
            match waitid(
                WaitId::PidFd(pidfd.as_fd()),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
            ) {
                Ok(Some(_)) => break,
                Err(rustix::io::Errno::INTR) => continue,
                _ => return Err(ToolError::ProtectionUnavailable),
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let reaped = child.wait().map_err(|_| ToolError::ProtectionUnavailable)?;
    result?;
    if !reaped.success() {
        return Err(ToolError::Operation);
    }
    Ok(())
}

pub(in crate::tools::guard) fn namespace_failure(mut error: ToolError, guard: &Path) -> ToolError {
    if let ToolError::GuardBootstrap(failure) = &mut error
        && matches!(failure.stage, super::super::BootstrapStage::NamespaceEntry)
    {
        failure.namespace_remedy = namespace_remedy(guard);
    }
    error
}

fn namespace_remedy(guard: &Path) -> Option<String> {
    if !guard.is_absolute() || guard.as_os_str().len() > MAX_PATH_BYTES {
        return None;
    }
    let mut path = String::from("\"");
    for byte in guard.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || b"/._-".contains(byte) {
            path.push(char::from(*byte));
        } else {
            path.push_str(&format!("\\x{byte:02x}"));
        }
    }
    path.push('"');
    Some(format!(
        "Namespace authorization failed; this alone does not identify AppArmor. On restricted Ubuntu, review existing executable attachments with sudo aa-status. Keep an existing bwrap-userns-restrict attachment; otherwise include the arany_bwrap block below. Run the following by hand after reviewing local policy; Arany never changes it:\nsudo tee /etc/apparmor.d/arany-userns >/dev/null <<'PROFILE'\nabi <abi/4.0>,\nprofile arany_bwrap /usr/bin/bwrap flags=(unconfined) {{ userns, }}\nprofile arany_supervisor {path} flags=(unconfined) {{ userns, }}\nPROFILE\nsudo chmod 0644 /etc/apparmor.d/arany-userns\nsudo apparmor_parser -r /etc/apparmor.d/arany-userns\nOmit arany_bwrap when an attachment already exists. Keep kernel.apparmor_restrict_unprivileged_userns=1. Stop and restart the exact profiled Arany executable, then explicitly retry. Missing kernel or tmpfs user-xattr enforcement requires a separate repair."
    ))
}

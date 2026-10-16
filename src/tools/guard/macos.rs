use super::{
    BootstrapFailure, BootstrapStage, CAPTURE_BYTES, GuardStage, TASK_BYTES_CAP, Task,
    bootstrap_failure, drain_stderr, parse_ready, read_capped, read_line_capped, read_observation,
};
use crate::engine::RunCancellation;
use crate::tools::types::{MAX_PROCESSES_OR_THREADS, MEMORY_BYTES, digest};
use crate::tools::{EffectIntent, ToolCall, ToolError, ToolObservation, ToolRuntime};

pub(super) fn boot_session() -> Result<uuid::Uuid, ToolError> {
    ffi::boot_session()
}

pub(super) async fn recover_abandoned(
    parent: &Path,
    attempts: &mut usize,
) -> Result<(), ToolError> {
    unit::recover_retained_roots_with_budget(parent, rustix::process::geteuid().as_raw(), attempts)
        .await
}
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::Command;

#[allow(unsafe_code)]
mod ffi;
pub(crate) mod fixture;
mod storage;
mod unit;

pub(super) fn unit_main() -> std::process::ExitCode {
    unit::helper_main()
}

const CPU_LIMIT: Duration = Duration::from_secs(15);
const SYSTEM_PROGRAMS_CAP: usize = 2048;
const PROFILE_BYTES_CAP: usize = 96 * 1024;
pub(super) const SUPERVISION_INTERVAL: Duration = Duration::from_millis(10);
pub(super) const PREPARATION_DEADLINE: Duration = Duration::from_secs(30);
/// The preparation deadline plus slack for the handshake that follows it.
pub(crate) const PREPARATION_OVERHEAD: Duration =
    Duration::from_secs(PREPARATION_DEADLINE.as_secs() + 5);
const PROCESS_LIMITS: &[(rustix::process::Resource, u64)] = &[
    (rustix::process::Resource::Nofile, 64),
    (rustix::process::Resource::Core, 0),
];
const SYS_FORK: i32 = 2;
const SYS_SETUID: i32 = 23;
const SYS_PTRACE: i32 = 26;
const SYS_SENDMSG: i32 = 28;
const SYS_SIGACTION: i32 = 46;
const SYS_SIGPROCMASK: i32 = 48;
const SYS_EXECVE: i32 = 59;
const SYS_VFORK: i32 = 66;
const SYS_SOCKET: i32 = 97;
const SYS_CONNECT: i32 = 98;
const SYS_BIND: i32 = 104;
const SYS_LISTEN: i32 = 106;
const SYS_SIGSUSPEND: i32 = 111;
const SYS_SETREUID: i32 = 126;
const SYS_SETREGID: i32 = 127;
const SYS_SENDTO: i32 = 133;
const SYS_SETGID: i32 = 181;
const SYS_SETEGID: i32 = 182;
const SYS_SETEUID: i32 = 183;
const SYS_SIGRETURN: i32 = 184;
const SYS_SETRLIMIT: i32 = 195;
const SYS_SYSCTL: i32 = 202;
const SYS_POSIX_SPAWN: i32 = 244;
const SYS_SYSCTLBYNAME: i32 = 274;
const SYS___PTHREAD_SIGMASK: i32 = 329;
const SYS___SIGWAIT: i32 = 330;
const SYS___DISABLE_THREADSIGNAL: i32 = 331;
const SYS___MAC_EXECVE: i32 = 380;
const SYS_SENDMSG_NOCANCEL: i32 = 402;
const SYS_CONNECT_NOCANCEL: i32 = 409;
const SYS_SIGSUSPEND_NOCANCEL: i32 = 410;
const SYS_SENDTO_NOCANCEL: i32 = 413;
const SYS___SIGWAIT_NOCANCEL: i32 = 422;
const SYS_CONNECTX: i32 = 447;
const SYS_SOCKET_DELEGATE: i32 = 450;
const SYS_COALITION: i32 = 458;
const SYS_COALITION_INFO: i32 = 459;
const SYS_SENDMSG_X: i32 = 481;
const SYS_PERSONA: i32 = 494;
const SYS_COALITION_POLICY_SET: i32 = 556;
const FILE_DENIED_SYSCALLS: &[i32] = &[
    SYS_FORK,
    SYS_SENDMSG,
    SYS_SIGACTION,
    SYS_SIGPROCMASK,
    SYS_EXECVE,
    SYS_VFORK,
    SYS_SOCKET,
    SYS_CONNECT,
    SYS_BIND,
    SYS_LISTEN,
    SYS_SIGSUSPEND,
    SYS_SENDTO,
    SYS_SIGRETURN,
    SYS_SETRLIMIT,
    SYS_SYSCTL,
    SYS_POSIX_SPAWN,
    SYS_SYSCTLBYNAME,
    SYS___PTHREAD_SIGMASK,
    SYS___SIGWAIT,
    SYS___DISABLE_THREADSIGNAL,
    SYS___MAC_EXECVE,
    SYS_SENDMSG_NOCANCEL,
    SYS_CONNECT_NOCANCEL,
    SYS_SIGSUSPEND_NOCANCEL,
    SYS_SENDTO_NOCANCEL,
    SYS___SIGWAIT_NOCANCEL,
    SYS_CONNECTX,
    SYS_SOCKET_DELEGATE,
    SYS_SENDMSG_X,
];
const COMMAND_DENIED_SYSCALLS: &[i32] = &[
    SYS_SETUID,
    SYS_PTRACE,
    SYS_SENDMSG,
    SYS_SOCKET,
    SYS_CONNECT,
    SYS_BIND,
    SYS_LISTEN,
    SYS_SETREUID,
    SYS_SETREGID,
    SYS_SENDTO,
    SYS_SETGID,
    SYS_SETEGID,
    SYS_SETEUID,
    SYS_SETRLIMIT,
    SYS_SYSCTL,
    SYS_SYSCTLBYNAME,
    SYS___PTHREAD_SIGMASK,
    SYS___SIGWAIT,
    SYS___DISABLE_THREADSIGNAL,
    SYS___MAC_EXECVE,
    SYS_SENDMSG_NOCANCEL,
    SYS_CONNECT_NOCANCEL,
    SYS_SIGSUSPEND_NOCANCEL,
    SYS_SENDTO_NOCANCEL,
    SYS_CONNECTX,
    SYS_SOCKET_DELEGATE,
    SYS_COALITION,
    SYS_COALITION_INFO,
    SYS_SENDMSG_X,
    SYS_PERSONA,
    SYS_COALITION_POLICY_SET,
];

const PROFILE: &str = "(allow file-read-metadata (literal \"/\") (literal \"/System\") (literal \"/usr\") (literal \"/dev\"))
(allow file-read* file-map-executable (subpath \"/System/Library\") (subpath \"/usr/lib\"))
(allow file-read* (literal \"/dev/random\") (literal \"/dev/urandom\"))
";

const COMMAND_PROFILE: &str = "(allow file-read-metadata (literal \"/\") (literal \"/System\") (literal \"/usr\") (literal \"/dev\"))
(allow file-read-metadata (literal \"/System/Cryptexes\") (literal \"/System/Cryptexes/OS\") (literal \"/var\") (literal \"/etc\") (literal \"/private/var\") (literal \"/private/var/select\") (literal \"/private/var/select/sh\"))
(allow file-read* file-map-executable (subpath \"/System/Library\") (subpath \"/usr/lib\"))
(allow file-read* (literal \"/\") (literal \"/bin\") (literal \"/usr/bin\") (literal \"/dev/random\") (literal \"/dev/urandom\"))
(allow file-read* file-write* (literal \"/dev/null\"))
(allow process-fork)
(deny process-fork (require-entitlement \"com.apple.private.coalition-spawn\") (require-entitlement \"com.apple.private.persona-mgmt\"))
(deny process-exec-update-label)
(allow signal (target self) (target children))
";

const GIT_PROFILE: &str = "(allow file-read-metadata (literal \"/\") (literal \"/System\") (literal \"/usr\") (literal \"/dev\"))
(allow file-read-metadata (literal \"/System/Cryptexes\") (literal \"/System/Cryptexes/OS\"))
(allow file-read* file-map-executable (subpath \"/System/Library\") (subpath \"/usr/lib\"))
(allow file-read* (literal \"/\") (literal \"/dev/random\") (literal \"/dev/urandom\"))
(allow file-read* file-write* (literal \"/dev/null\"))
(allow process-fork)
(deny process-fork (require-entitlement \"com.apple.private.coalition-spawn\") (require-entitlement \"com.apple.private.persona-mgmt\"))
(deny process-exec-update-label)
(allow signal (target self) (target children))
";

fn profile(rules: &str, denied_syscalls: &[i32]) -> String {
    let mut profile =
        String::from("(version 1)\n(deny default)\n(allow syscall-unix)\n(deny syscall-unix");
    for syscall in denied_syscalls {
        profile.push_str(&format!(" (syscall-number {syscall})"));
    }
    profile.push_str(")\n");
    profile.push_str(rules);
    profile
}

pub(super) fn check_available() -> Result<(), ToolError> {
    if !cfg!(any(target_arch = "aarch64", target_arch = "x86_64")) {
        return Err(ToolError::ProtectionUnavailable);
    }
    for program in ["/bin/launchctl", "/usr/bin/env", "/usr/bin/hdiutil"] {
        if !system_program(program)? {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    unit::check_available()
}

pub(super) fn enforcement_digest() -> Result<[u8; 32], ToolError> {
    if !cfg!(any(target_arch = "aarch64", target_arch = "x86_64")) {
        return Err(ToolError::ProtectionUnavailable);
    }
    let mut bytes = b"arany-macos-guard-v15:shared-fifo-capacity;durable-observed-attachment;boot-bound-locked-root-recovery;persisted-supervisor-before-task;image-inode-device-mount-ownership;no-buffered-go-before-ready;typed-files-pinned-skills;file-no-fork-exec;closed-inherited-fds;live-no-follow-handles;metadata-only-directory-ancestors;workspace-call-write;guard-read-only;file-cpu-default-unblocked-immutable;supervised-resources;one-use-launchd-coalition;authenticated-private-control;atomic-audit-token-signals;owned-unit-cleanup;exact-child-kill-reap;strict-bounded-no-follow-command-changeset-after-coalition-emptiness-before-detach;bounded-go-receipt;versioned-exact-ro-runtime-and-cache-grants;admitted-cache-volume-grant;mount-table-no-descendant-under-selected-physical-runtime-before-seatbelt-and-before-go".to_vec();
    bytes.extend_from_slice(profile(PROFILE, FILE_DENIED_SYSCALLS).as_bytes());
    bytes.extend_from_slice(profile(COMMAND_PROFILE, COMMAND_DENIED_SYSCALLS).as_bytes());
    bytes.extend_from_slice(profile(GIT_PROFILE, COMMAND_DENIED_SYSCALLS).as_bytes());
    bytes.extend_from_slice(b";typed-git-read-pinned-clt-git-after-go-read-only-workspace-unit-root-view-no-repository-config-closed-env-coalition-supervised");
    bytes.extend_from_slice(b"native-commands-mcp:checked-private-cwd;fixed-capacity-nosuid-project-scratch;fresh-selected-snapshot;read-only-pinned-runtime;bounded-positive-root-owned-non-setid-unentitled-system-exec;stamp-bound-process-signature-verdicts;admission-catalogue-bound-to-enforcement;per-worker-pinned-system-directories-no-follow-metadata-revalidation;exact-literal-union-profile;dynamic-coalition-persona-entitlement-fork-denial;null-bootstrap-registered-ports;child-only-signals;supervised-cpu;physical-footprint-memory");
    bytes.extend_from_slice(&CPU_LIMIT.as_nanos().to_le_bytes());
    bytes.extend_from_slice(&SUPERVISION_INTERVAL.as_nanos().to_le_bytes());
    bytes.extend_from_slice(&MEMORY_BYTES.to_le_bytes());
    bytes.extend_from_slice(&MAX_PROCESSES_OR_THREADS.to_le_bytes());
    bytes.extend_from_slice(&storage::STORAGE_BYTES_CAP.to_le_bytes());
    bytes.extend_from_slice(&(SYSTEM_PROGRAMS_CAP as u64).to_le_bytes());
    bytes.extend_from_slice(&(PROFILE_BYTES_CAP as u64).to_le_bytes());
    bytes.extend_from_slice(&(TASK_BYTES_CAP as u64).to_le_bytes());
    bytes.extend_from_slice(&(crate::tools::types::MAX_CHANGESET_FILES as u64).to_le_bytes());
    bytes.extend_from_slice(&(crate::tools::types::MAX_CHANGESET_BYTES as u64).to_le_bytes());
    for &(resource, limit) in PROCESS_LIMITS {
        bytes.extend_from_slice(&(resource as u32).to_le_bytes());
        bytes.extend_from_slice(&limit.to_le_bytes());
    }
    Ok(digest(&bytes))
}

pub(super) fn bootstrap() -> Result<(), ToolError> {
    if ffi::task_info(std::process::id())?.pti_threadnum != 1 {
        return Err(ToolError::ProtectionUnavailable);
    }
    // This single-threaded helper owns inherited descriptors before Rust creates any handles.
    for descriptor in ffi::open_descriptors()? {
        if descriptor >= 3 {
            nix::unistd::close(descriptor).map_err(|_| ToolError::ProtectionUnavailable)?;
        }
    }
    configure_resources()
}

fn configure_resources() -> Result<(), ToolError> {
    use rustix::process::{Rlimit, setrlimit};
    for &(resource, limit) in PROCESS_LIMITS {
        setrlimit(
            resource,
            Rlimit {
                current: Some(limit),
                maximum: Some(limit),
            },
        )
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    }
    Ok(())
}

fn configure_file_cpu() -> Result<(), ToolError> {
    if !ffi::reset_cpu_signal() {
        return Err(ToolError::ProtectionUnavailable);
    }
    use rustix::process::{Rlimit, setrlimit};
    setrlimit(
        rustix::process::Resource::Cpu,
        Rlimit {
            current: Some(CPU_LIMIT.as_secs()),
            maximum: Some(CPU_LIMIT.as_secs()),
        },
    )
    .map_err(|_| ToolError::ProtectionUnavailable)?;
    Ok(())
}

fn quoted(path: &Path) -> Result<String, ToolError> {
    let path = path.to_str().ok_or(ToolError::Path)?;
    if !path.starts_with('/') || path.len() > 4096 || path.chars().any(char::is_control) {
        return Err(ToolError::Path);
    }
    serde_json::to_string(path).map_err(|_| ToolError::Path)
}

fn ancestors(profile: &mut String, path: &Path) -> Result<(), ToolError> {
    for directory in path.ancestors().skip(1) {
        profile.push_str(&format!(
            "(allow file-read-metadata (literal {}))\n",
            quoted(directory)?
        ));
    }
    Ok(())
}

pub(super) fn install(
    task: &Task,
    stage: &mut super::BootstrapStage,
) -> Result<PathBuf, ToolError> {
    if task.intent.call.is_process() {
        return install_command(task, stage);
    }
    if task.intent.call.is_git_read() {
        return install_git(task, stage);
    }
    configure_file_cpu()?;
    let mut profile = profile(PROFILE, FILE_DENIED_SYSCALLS);
    ancestors(&mut profile, &task.workspace)?;
    profile.push_str(&format!(
        "(allow file-read* (subpath {}))\n",
        quoted(&task.workspace)?
    ));
    if task.intent.call.mutates() {
        profile.push_str(&format!(
            "(allow file-write* (subpath {}))\n",
            quoted(&task.workspace)?
        ));
    }
    if let ToolCall::Skill { name, resource } = &task.intent.call {
        let skill = task
            .config
            .skills
            .iter()
            .find(|skill| &skill.name == name)
            .ok_or(ToolError::Configuration)?;
        let path = Path::new(&skill.directory).join(resource.as_deref().unwrap_or("SKILL.md"));
        ancestors(&mut profile, &path)?;
        profile.push_str(&format!(
            "(allow file-read* (literal {}))\n",
            quoted(&path)?
        ));
    }
    profile.push_str(&format!(
        "(deny file-write* (literal {}))\n",
        quoted(&task.guard_executable)?
    ));
    install_profile(profile)?;
    attest_policy(
        &[(c"process-fork", 1), (c"network-outbound", 1)],
        FILE_DENIED_SYSCALLS,
    )?;
    verify_cpu_controls()?;
    Ok(PathBuf::from("/"))
}

fn install_profile(profile: String) -> Result<(), ToolError> {
    if profile.len() > PROFILE_BYTES_CAP {
        return Err(ToolError::Limit);
    }
    let profile = CString::new(profile).map_err(|_| ToolError::Path)?;
    if !ffi::install_sandbox(&profile) {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

fn unit_root(task: &Task) -> Result<PathBuf, ToolError> {
    let root = std::env::current_dir().map_err(|_| ToolError::Path)?;
    crate::private_fs::open_directory(&root, false).map_err(|_| ToolError::Path)?;
    crate::tools::fs::reject_overlap(&root, &task.workspace)?;
    if crate::tools::fs::identity(&crate::tools::fs::open_directory(&task.workspace)?)?
        != (task.intent.workspace_device, task.intent.workspace_inode)
    {
        return Err(ToolError::ChangedInput);
    }
    Ok(root)
}

fn command_root(task: &Task) -> Result<PathBuf, ToolError> {
    let root = unit_root(task)?;
    storage::verify_storage(&root, &task.intent.limits)?;
    Ok(root)
}

pub(super) fn system_program(path: &str) -> Result<bool, ToolError> {
    Ok(system_program_metadata(path)?.is_some())
}

fn system_program_metadata(path: &str) -> Result<Option<std::fs::Metadata>, ToolError> {
    if !crate::tools::config::valid_absolute(path) {
        return Err(ToolError::Path);
    }
    let path = Path::new(path);
    if !matches!(path.parent(), Some(parent) if [Path::new("/bin"), Path::new("/usr/bin")].contains(&parent))
    {
        return Ok(None);
    }
    let parent = system_program_directory(path.parent().ok_or(ToolError::Path)?)?;
    Ok(Some(system_program_metadata_in(&parent, path)?))
}

fn system_program_directory(path: &Path) -> Result<cap_std::fs::Dir, ToolError> {
    use cap_std::fs::MetadataExt;
    let parent = crate::tools::fs::open_directory(path)?;
    let metadata = parent.dir_metadata().map_err(|_| ToolError::Path)?;
    if !metadata.is_dir() || !system_directory(metadata.uid(), metadata.mode()) {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(parent)
}

fn system_program_metadata_in(
    parent: &cap_std::fs::Dir,
    path: &Path,
) -> Result<std::fs::Metadata, ToolError> {
    use std::os::unix::fs::MetadataExt;
    let file = crate::tools::fs::open_file(
        parent,
        path.file_name()
            .and_then(|name| name.to_str())
            .ok_or(ToolError::Path)?,
    )?;
    let metadata = file.metadata().map_err(|_| ToolError::Path)?;
    if !system_file(
        metadata.is_file(),
        metadata.uid(),
        metadata.nlink(),
        metadata.mode(),
    ) {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(metadata)
}

fn system_directory(uid: u32, mode: u32) -> bool {
    uid == 0 && mode & 0o022 == 0
}

fn system_file(is_file: bool, uid: u32, nlink: u64, mode: u32) -> bool {
    is_file && uid == 0 && nlink == 1 && mode & 0o6022 == 0
}

fn unprivileged_helper() -> Result<(), ToolError> {
    if rustix::process::geteuid().as_raw() == 0
        || rustix::process::getuid() != rustix::process::geteuid()
        || rustix::process::getgid() != rustix::process::getegid()
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

fn install_command(task: &Task, stage: &mut super::BootstrapStage) -> Result<PathBuf, ToolError> {
    use std::os::unix::fs::MetadataExt;

    unprivileged_helper()?;
    let root = command_root(task)?;
    let mut required_system_programs = Vec::new();
    for program in task
        .config
        .commands
        .iter()
        .chain(task.config.mcp.iter().map(|server| &server.program))
    {
        program.verify()?;
        if system_program(&program.executable)? {
            required_system_programs.push(PathBuf::from(&program.executable));
        }
    }
    let mut profile = profile(COMMAND_PROFILE, COMMAND_DENIED_SYSCALLS);
    let grants = if task.intent.limits.cache_bytes > 0 {
        &["workspace", "scratch", "cache"][..]
    } else {
        &["workspace", "scratch"][..]
    };
    for name in grants {
        let path = root.join(name);
        ancestors(&mut profile, &path)?;
        profile.push_str(&format!(
            "(allow file-read* file-write* file-map-executable process-exec (subpath {}))\n",
            quoted(&path)?
        ));
    }
    for grant in &task.config.runtime_grants {
        grant.verify_host()?;
        ancestors(&mut profile, &grant.path)?;
        if grant.kind.is_toolchain() {
            profile.push_str(&format!(
                "(allow file-read* file-map-executable process-exec (subpath {}))\n",
                quoted(&grant.path)?
            ));
        } else {
            profile.push_str(&format!(
                "(allow file-read* (subpath {}))\n",
                quoted(&grant.path)?
            ));
        }
    }
    let resources = root.join("runtime");
    let metadata = std::fs::symlink_metadata(&resources).map_err(|_| ToolError::Path)?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o7777 != 0o700
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    profile.push_str(&format!(
        "(allow file-read* file-map-executable (subpath {}))\n(allow process-exec (subpath {}))\n",
        quoted(&resources)?,
        quoted(&resources.join("programs"))?
    ));
    task.system_programs
        .as_ref()
        .ok_or(ToolError::ProtectionUnavailable)?
        .allow(&mut profile, &mut required_system_programs)?;
    if !required_system_programs.is_empty() {
        return Err(ToolError::ProtectionUnavailable);
    }
    *stage = super::BootstrapStage::RuntimeMounts;
    verify_runtime_mounts(&task.config.runtime_grants)?;
    *stage = super::BootstrapStage::Capabilities;
    profile.push_str(&format!(
        "(deny file-write* (literal {}))\n",
        quoted(&task.guard_executable)?
    ));
    if !ffi::detach_bootstrap_ports() {
        return Err(ToolError::ProtectionUnavailable);
    }
    install_profile(profile)?;
    attest_policy(
        &[
            (c"process-fork", 0),
            (c"network-outbound", 1),
            (c"mach-lookup", 1),
            (c"iokit-open-user-client", 1),
            (c"process-exec-update-label", 1),
        ],
        COMMAND_DENIED_SYSCALLS,
    )?;
    Ok(root)
}

fn install_git(task: &Task, stage: &mut super::BootstrapStage) -> Result<PathBuf, ToolError> {
    unprivileged_helper()?;
    let root = unit_root(task)?;
    let git = task.config.git.as_ref().ok_or(ToolError::Configuration)?;
    crate::tools::git::verify_host_executable(git)?;
    let git_path = Path::new(&git.executable);
    let mut profile = profile(GIT_PROFILE, COMMAND_DENIED_SYSCALLS);
    ancestors(&mut profile, git_path)?;
    profile.push_str(&format!(
        "(allow file-read* file-map-executable process-exec (literal {}))\n",
        quoted(git_path)?
    ));
    ancestors(&mut profile, &task.workspace)?;
    profile.push_str(&format!(
        "(allow file-read* (subpath {}))\n",
        quoted(&task.workspace)?
    ));
    let view = root.join(crate::tools::git::VIEW);
    ancestors(&mut profile, &view)?;
    profile.push_str(&format!(
        "(allow file-read* file-write* (subpath {}))\n",
        quoted(&view)?
    ));
    profile.push_str(&format!(
        "(allow file-read* (subpath {}))\n",
        quoted(&root.join(crate::tools::git::HOME))?
    ));
    profile.push_str(&format!(
        "(deny file-write* (literal {}))\n",
        quoted(&task.guard_executable)?
    ));
    *stage = super::BootstrapStage::Capabilities;
    if !ffi::detach_bootstrap_ports() {
        return Err(ToolError::ProtectionUnavailable);
    }
    install_profile(profile)?;
    attest_policy(
        &[
            (c"process-fork", 0),
            (c"network-outbound", 1),
            (c"mach-lookup", 1),
            (c"iokit-open-user-client", 1),
            (c"process-exec-update-label", 1),
        ],
        COMMAND_DENIED_SYSCALLS,
    )?;
    Ok(root)
}

pub(super) fn verify_runtime_mounts(
    grants: &[crate::tools::toolchains::RuntimeGrant],
) -> Result<(), ToolError> {
    let roots = grants
        .iter()
        .map(|grant| {
            grant.verify_host()?;
            std::fs::canonicalize(&grant.path).map_err(|_| ToolError::ProtectionUnavailable)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mounts = ffi::mounts()?;
    if mounts.iter().any(|(_, target)| {
        !target.is_absolute()
            || target.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::CurDir | std::path::Component::ParentDir
                )
            })
            || roots
                .iter()
                .any(|root| target != root && target.starts_with(root))
    }) {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SystemPrograms {
    // Compact stamps keep the catalogue inside the shared Task byte cap.
    programs: Vec<(String, [u64; 7])>,
}

fn program_stamp(metadata: &std::fs::Metadata) -> [u64; 7] {
    use std::os::unix::fs::MetadataExt;
    [
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime() as u64,
        metadata.mtime_nsec() as u64,
        metadata.ctime() as u64,
        metadata.ctime_nsec() as u64,
    ]
}

pub(crate) fn macos_enforcement_digest(
    programs: Option<&SystemPrograms>,
) -> Result<[u8; 32], ToolError> {
    let policy = enforcement_digest()?;
    let Some(programs) = programs else {
        return Ok(policy);
    };
    if programs.programs.len() > SYSTEM_PROGRAMS_CAP {
        return Err(ToolError::Limit);
    }
    let mut bytes = policy.to_vec();
    bytes.extend_from_slice(&serde_json::to_vec(programs).map_err(|_| ToolError::Configuration)?);
    if bytes.len() > TASK_BYTES_CAP {
        return Err(ToolError::Limit);
    }
    Ok(digest(&bytes))
}

impl SystemPrograms {
    pub(crate) fn admit() -> Result<Self, ToolError> {
        use cap_std::fs::MetadataExt;

        let capture = fixture::Span::begin(fixture::Stage::Catalogue);
        let mut count = 0;
        let mut programs = Vec::new();
        for directory in ["/bin", "/usr/bin"] {
            let dir = crate::tools::fs::open_directory(Path::new(directory))?;
            let metadata = dir
                .dir_metadata()
                .map_err(|_| ToolError::ProtectionUnavailable)?;
            if !system_directory(metadata.uid(), metadata.mode()) {
                return Err(ToolError::ProtectionUnavailable);
            }
            for entry in dir
                .entries()
                .map_err(|_| ToolError::ProtectionUnavailable)?
            {
                count += 1;
                if count > SYSTEM_PROGRAMS_CAP {
                    return Err(ToolError::Limit);
                }
                let entry = entry.map_err(|_| ToolError::ProtectionUnavailable)?;
                if !entry
                    .file_type()
                    .map_err(|_| ToolError::ProtectionUnavailable)?
                    .is_file()
                {
                    continue;
                }
                let metadata = entry
                    .metadata()
                    .map_err(|_| ToolError::ProtectionUnavailable)?;
                if !system_file(
                    metadata.is_file(),
                    metadata.uid(),
                    metadata.nlink(),
                    metadata.mode(),
                ) {
                    continue;
                }
                let path = Path::new(directory).join(entry.file_name());
                let path = path.to_str().ok_or(ToolError::Path)?;
                let stamp = program_stamp(
                    &system_program_metadata(path)?.ok_or(ToolError::ProtectionUnavailable)?,
                );
                if !unentitled_program(path, stamp)? {
                    continue;
                }
                programs.push((path.to_owned(), stamp));
            }
        }
        programs.sort_unstable();
        let programs = Self { programs };
        macos_enforcement_digest(Some(&programs))?;
        capture.complete();
        Ok(programs)
    }

    fn allow(&self, profile: &mut String, required: &mut Vec<PathBuf>) -> Result<(), ToolError> {
        if self.programs.is_empty() || self.programs.len() > SYSTEM_PROGRAMS_CAP {
            return Err(ToolError::ProtectionUnavailable);
        }
        let directories = [
            (
                Path::new("/bin"),
                system_program_directory(Path::new("/bin"))?,
            ),
            (
                Path::new("/usr/bin"),
                system_program_directory(Path::new("/usr/bin"))?,
            ),
        ];
        profile.push_str("(allow file-read* file-map-executable process-exec (require-any\n");
        for (path, expected) in &self.programs {
            if !crate::tools::config::valid_absolute(path) {
                return Err(ToolError::Path);
            }
            let path = Path::new(path);
            let directory = directories
                .iter()
                .find(|(directory, _)| Some(*directory) == path.parent())
                .ok_or(ToolError::ProtectionUnavailable)?;
            let metadata = system_program_metadata_in(&directory.1, path)?;
            if program_stamp(&metadata) != *expected {
                return Err(ToolError::ChangedInput);
            }
            required.retain(|required| required != path);
            profile.push_str(&format!("(literal {})\n", quoted(path)?));
            if profile.len() > PROFILE_BYTES_CAP {
                return Err(ToolError::Limit);
            }
        }
        profile.push_str("))\n");
        if profile.len() > PROFILE_BYTES_CAP {
            return Err(ToolError::Limit);
        }
        Ok(())
    }
}

// Signature evaluation dominates admission; reuse only determined verdicts for an identical stamp.
fn unentitled_program(path: &str, stamp: [u64; 7]) -> Result<bool, ToolError> {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    type Verdicts = HashMap<String, ([u64; 7], bool)>;
    static VERDICTS: LazyLock<Mutex<Verdicts>> = LazyLock::new(Mutex::default);
    let cached = VERDICTS
        .lock()
        .map_err(|_| ToolError::ProtectionUnavailable)?
        .get(path)
        .and_then(|&(cached, verdict)| (cached == stamp).then_some(verdict));
    if let Some(verdict) = cached {
        return Ok(verdict);
    }
    let verdict = ffi::unentitled_code(Path::new(path))?;
    let after = system_program_metadata(path)?.ok_or(ToolError::ProtectionUnavailable)?;
    if program_stamp(&after) != stamp {
        return Err(ToolError::ChangedInput);
    }
    let Some(verdict) = verdict else {
        return Ok(false);
    };
    let mut verdicts = VERDICTS
        .lock()
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    if verdicts.len() >= SYSTEM_PROGRAMS_CAP && !verdicts.contains_key(path) {
        verdicts.clear();
    }
    verdicts.insert(path.to_owned(), (stamp, verdict));
    Ok(verdict)
}

fn attest_policy(operations: &[(&CStr, i32)], denied_syscalls: &[i32]) -> Result<(), ToolError> {
    if operations
        .iter()
        .any(|&(operation, expected)| ffi::sandbox_verdict(operation) != expected)
        || !denied_syscalls
            .iter()
            .all(|&syscall| ffi::syscall_denied(syscall))
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

fn verify_cpu_controls() -> Result<(), ToolError> {
    use rustix::process::getrlimit;
    for &(resource, limit) in PROCESS_LIMITS {
        let actual = getrlimit(resource);
        if actual.current != Some(limit) || actual.maximum != Some(limit) {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    let cpu = getrlimit(rustix::process::Resource::Cpu);
    if cpu.current != Some(CPU_LIMIT.as_secs()) || cpu.maximum != Some(CPU_LIMIT.as_secs()) {
        return Err(ToolError::ProtectionUnavailable);
    }
    if !ffi::cpu_signal_changes_denied() {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

fn check_process_resources(pid: u32) -> Result<bool, ToolError> {
    let (info, usage) = match ffi::task_info(pid).and_then(|info| Ok((info, ffi::rusage(pid)?))) {
        Ok(sample) => sample,
        Err(_) if ffi::exited(pid) => return Ok(false),
        Err(_) => return Err(ToolError::ProtectionUnavailable),
    };
    // Resident size omits compressed and swapped pages; the footprint does not.
    if info.pti_threadnum < 1
        || usage.ri_phys_footprint > MEMORY_BYTES
        || info.pti_threadnum as u32 > MAX_PROCESSES_OR_THREADS
    {
        return Err(ToolError::Limit);
    }
    Ok(true)
}

pub(super) async fn execute_inner(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    mut cancellation: RunCancellation,
    dispatched: &mut bool,
) -> Result<ToolObservation, ToolError> {
    // Dropping the awaiting future stops only this worker, never the caller's Run.
    struct CancelOnDrop(RunCancellation);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let worker = CancelOnDrop(RunCancellation::new());
    let worker_cancellation = worker.0.clone();
    let runtime = runtime.clone();
    let intent = intent.clone();
    let started = Arc::new(AtomicBool::new(false));
    let worker_started = started.clone();
    let mut handle = tokio::task::spawn_blocking(move || {
        crate::tools::fs::validate_workspace(&runtime)?;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| ToolError::Operation)?
            .block_on(unit::execute(
                &runtime,
                &intent,
                worker_cancellation,
                &worker_started,
            ))
    });
    let result = tokio::select! {
        result = &mut handle => result,
        () = cancellation.cancelled() => {
            worker.0.cancel();
            handle.await
        }
    }
    .unwrap_or(Err(ToolError::Operation));
    *dispatched = started.load(Ordering::Acquire);
    result
}

impl Task {
    fn new(
        runtime: &ToolRuntime,
        intent: &EffectIntent,
        cache: Option<super::BuildCacheTransfer>,
    ) -> Self {
        Self {
            config: runtime.config.clone(),
            intent: intent.clone(),
            cache,
            workspace: runtime.workspace.clone(),
            guard_executable: runtime.guard_executable.clone(),
            system_programs: if intent.call.is_process() {
                runtime.system_programs.as_deref().cloned()
            } else {
                None
            },
        }
    }

    fn into_runtime(self) -> (ToolRuntime, EffectIntent, Option<super::BuildCacheTransfer>) {
        let cache = self.cache;
        let runtime = ToolRuntime {
            context_limit_bytes: crate::tools::types::MAX_TOOL_CONTEXT_BYTES,
            queue_progress: None,
            active_action_gate: None,
            cache_state: None,
            config: self.config,
            workspace: self.workspace,
            workspace_identity: (self.intent.workspace_device, self.intent.workspace_inode),
            guard_executable: self.guard_executable,
            enforcement_digest: self.intent.enforcement_digest,
            process_enforcement_digest: self.intent.enforcement_digest,
            system_programs: self.system_programs.map(Arc::new),
        };
        (runtime, self.intent, cache)
    }
}

async fn execute_worker(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    mut cancellation: RunCancellation,
    scope: &mut unit::Scope,
    control: &mut tokio::net::unix::OwnedWriteHalf,
    receive_go: tokio::sync::oneshot::Receiver<()>,
    root: &Path,
) -> Result<ToolObservation, ToolError> {
    crate::tools::fs::validate_workspace(runtime)?;
    let task = super::encode_task(&Task::new(runtime, intent, None))?;
    let mut child = Command::new(&runtime.guard_executable)
        .arg("--internal-tool-guard")
        .env_clear()
        .current_dir(if intent.call.is_process() || intent.call.is_git_read() {
            root
        } else {
            Path::new("/")
        })
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    let pid = child.id().ok_or(ToolError::ProtectionUnavailable)?;
    let mut stdin = child.stdin.take().ok_or(ToolError::Operation)?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or(ToolError::Operation)?);
    let stderr = child.stderr.take().ok_or(ToolError::Operation)?;
    let mut stderr_task = tokio::spawn(read_capped(stderr, CAPTURE_BYTES));
    let (result, status) = {
        let execution = async {
            let capture = fixture::Span::begin(fixture::Stage::WorkerReady);
            let ready = tokio::time::timeout(PREPARATION_DEADLINE, async {
                stdin
                    .write_all(&task)
                    .await
                    .map_err(|_| ToolError::Operation)?;
                parse_ready(&read_line_capped(&mut stdout, 2048).await?, intent)
            })
            .await
            .map_err(|_| {
                ToolError::GuardBootstrap(BootstrapFailure {
                    stage: BootstrapStage::Handshake,
                })
            })??;
            if ready.pid != pid {
                return Err(ToolError::ProtectionUnavailable);
            }
            if !check_process_resources(pid)
                .map_err(|_| ToolError::GuardRejected(GuardStage::ResourceAttestation))?
            {
                return Err(ToolError::GuardRejected(GuardStage::ResourceAttestation));
            }
            let ready = serde_json::to_vec(&ready).map_err(|_| ToolError::Operation)?;
            control
                .write_all(&ready)
                .await
                .map_err(|_| ToolError::Operation)?;
            control
                .write_all(b"\n")
                .await
                .map_err(|_| ToolError::Operation)?;
            capture.complete();
            let capture = fixture::Span::begin(fixture::Stage::WorkerGo);
            receive_go.await.map_err(|_| ToolError::Operation)?;
            capture.complete();
            let payload_deadline = tokio::time::Instant::now()
                + Duration::from_millis(u64::from(intent.limits.runtime_ms));
            tokio::time::timeout_at(payload_deadline, async {
                let capture = fixture::Span::begin(fixture::Stage::WorkerReceipt);
                stdin
                    .write_all(b"GO\n")
                    .await
                    .map_err(|_| ToolError::Operation)?;
                stdin.shutdown().await.map_err(|_| ToolError::Operation)?;
                let observation = read_observation(&mut stdout, intent).await?;
                capture.complete();
                Ok(observation)
            })
            .await
            .map_err(|_| ToolError::Limit)?
        };
        let deadline = tokio::time::Instant::now()
            + PREPARATION_DEADLINE
            + Duration::from_millis(u64::from(intent.limits.runtime_ms))
            + Duration::from_secs(5);
        let execution = tokio::time::timeout_at(deadline, execution);
        tokio::pin!(execution);
        let mut samples = tokio::time::interval(SUPERVISION_INTERVAL);
        let mut status = None;
        let result = loop {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => break Err(ToolError::Operation),
                result = &mut execution => break result.unwrap_or(Err(ToolError::Limit)),
                exited = child.wait(), if status.is_none() => {
                    status = Some(exited.map_err(|_| ToolError::Operation));
                }
                _ = samples.tick() => {
                    if let Err(error) = scope.check_resources() { break Err(error); }
                }
            }
        };
        (result, status)
    };
    if result.is_err() {
        let _ = child.start_kill();
    }
    let capture = fixture::Span::begin(fixture::Stage::WorkerReap);
    let reaped = match status {
        Some(status) => Ok(status),
        None => {
            tokio::time::timeout(Duration::from_secs(2), async {
                child.wait().await.map_err(|_| ToolError::Operation)
            })
            .await
        }
    };
    if reaped.is_err() {
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    }
    let drained = drain_stderr(&mut stderr_task).await;
    capture.finish(matches!(reaped, Ok(Ok(_))) && drained.is_some());
    if result.is_err()
        && let Some(failure) = drained.as_deref().and_then(bootstrap_failure)
    {
        return Err(failure);
    }
    if !matches!(reaped, Ok(Ok(_))) {
        return Err(ToolError::GuardRejected(GuardStage::LauncherReap));
    }
    if result.is_ok() && !matches!(reaped, Ok(Ok(status)) if status.success()) {
        return Err(ToolError::Operation);
    }
    if drained.is_none() {
        return Err(ToolError::GuardRejected(GuardStage::OutputEof));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_process::BoundedOutput;
    use crate::tools::config::Config;
    use crate::tools::types::ToolLimits;
    use nix::libc;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn system_catalogue_revalidates_pins_and_binds_the_profile() {
        let programs = SystemPrograms::admit().unwrap();
        let mut required = vec![PathBuf::from("/bin/sh"), PathBuf::from("/usr/bin/true")];
        let mut profile = String::new();
        programs.allow(&mut profile, &mut required).unwrap();
        assert!(required.is_empty());
        assert!(
            !programs
                .programs
                .iter()
                .any(|(path, _)| path == "/usr/bin/sudo")
        );
        let bytes = serde_json::to_vec(&programs).unwrap();
        let mut admitted: SystemPrograms = serde_json::from_slice(&bytes).unwrap();
        let enforcement = macos_enforcement_digest(Some(&admitted)).unwrap();
        assert_eq!(
            enforcement,
            macos_enforcement_digest(Some(&programs)).unwrap()
        );
        assert_ne!(enforcement, enforcement_digest().unwrap());
        for case in ["stale", "outside", "noncanonical", "overflow"] {
            admitted.programs = programs.programs.clone();
            match case {
                "stale" => admitted.programs[0].1[1] ^= 1,
                "outside" => admitted.programs[0].0 = "/etc/passwd".into(),
                "noncanonical" => admitted.programs[0].0 = "/usr/bin/./true".into(),
                "overflow" => admitted
                    .programs
                    .resize(SYSTEM_PROGRAMS_CAP + 1, programs.programs[0].clone()),
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    admitted.allow(&mut String::new(), &mut Vec::new()),
                    Err(ToolError::ChangedInput
                        | ToolError::ProtectionUnavailable
                        | ToolError::Path)
                ),
                "{case}"
            );
            if case == "overflow" {
                assert!(matches!(
                    macos_enforcement_digest(Some(&admitted)),
                    Err(ToolError::Limit)
                ));
            } else {
                assert_ne!(
                    enforcement,
                    macos_enforcement_digest(Some(&admitted)).unwrap(),
                    "{case}"
                );
            }
        }
    }

    fn probe(cpu: bool) -> std::process::Output {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("workspace")).unwrap();
        std::fs::write(temp.path().join("outside"), b"synthetic excluded data").unwrap();
        let network = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::guard::macos::tests::native_file_profile_helper",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("ARANY_TEST_FILE_PROFILE_ROOT", temp.path())
            .env(
                "ARANY_TEST_FILE_PROFILE_PORT",
                network.local_addr().unwrap().port().to_string(),
            )
            .env("ARANY_TEST_FILE_PROFILE_CPU", if cpu { "1" } else { "0" })
            .current_dir(temp.path())
            .bounded_output_for(Duration::from_secs(90), 32 * 1024)
            .unwrap()
    }

    #[test]
    fn native_file_profile_denies_host_network_spawn_and_cpu_changes() {
        let output = probe(false);
        assert!(
            output.status.success(),
            "native profile: {:?}; stdout={:?}; stderr={:?}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        crate::test_process::assert_single_test_completed(&output.stdout, "native file profile");
        assert!(output.stderr.is_empty());
    }

    #[test]
    #[cfg_attr(
        not(arany_ci),
        ignore = "native 15-second CPU expiry; finite isolated process"
    )]
    fn native_file_profile_cpu_expiry_cannot_be_disabled() {
        let _gate = crate::test_process::NATIVE_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let output = probe(true);
        assert_eq!(output.status.signal(), Some(libc::SIGXCPU));
        assert!(String::from_utf8_lossy(&output.stdout).contains("CPU readiness"));
        assert!(output.stderr.is_empty());
    }

    #[test]
    #[ignore = "isolated native file profile helper"]
    fn native_file_profile_helper() {
        let root = std::env::var_os("ARANY_TEST_FILE_PROFILE_ROOT").expect("private fixture root");
        let root = Path::new(&root);
        let port = std::env::var("ARANY_TEST_FILE_PROFILE_PORT")
            .unwrap()
            .parse::<u16>()
            .unwrap();
        let cpu = std::env::var("ARANY_TEST_FILE_PROFILE_CPU").unwrap() == "1";
        let workspace = root.join("workspace");
        let (device, inode) =
            crate::tools::fs::identity(&crate::tools::fs::open_directory(&workspace).unwrap())
                .unwrap();
        let config: Config = serde_json::from_value(serde_json::json!({"version":1,
            "workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]}))
        .unwrap();
        let task = Task {
            intent: EffectIntent {
                id: uuid::Uuid::now_v7(),
                run_id: crate::session::RunId::default(),
                agent_run_id: crate::session::AgentRunId::default(),
                policy_digest: config.digest(),
                enforcement_digest: enforcement_digest().unwrap(),
                workspace_device: device,
                workspace_inode: inode,
                call: ToolCall::Write {
                    path: "synthetic".into(),
                    expected_digest: None,
                    content: "synthetic".into(),
                },
                limits: ToolLimits::default(),
                expires_at_ms: crate::tools::now_ms() + 60_000,
                use_count: 1,
            },
            config,
            cache: None,
            workspace,
            guard_executable: std::env::current_exe().unwrap(),
            system_programs: None,
        };
        assert!(Path::new("/usr/bin/true").is_file());
        configure_resources().unwrap();
        let mut stage = super::BootstrapStage::Workspace;
        install(&task, &mut stage).unwrap();
        assert_eq!(
            ffi::process_arguments_errno(std::process::id() as i32),
            Some(libc::EPERM)
        );
        assert_eq!(
            std::fs::read(root.join("outside")).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(crate::tools::fs::open_directory(&task.workspace).is_ok());
        assert_eq!(
            std::fs::metadata(root.join("outside")).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::fs::read_dir(root).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::fs::read_dir("/private/tmp").unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::fs::write(root.join("outside"), b"forbidden")
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
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
        // macOS 15 reports denied binds and execs as ENOENT; later releases report EPERM.
        assert!(matches!(
            std::os::unix::net::UnixListener::bind(root.join("workspace/socket"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
        ));
        assert_eq!(
            std::fs::symlink_metadata(root.join("workspace/socket"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(matches!(
            std::process::Command::new("/usr/bin/true")
                .status()
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
        ));
        if cpu {
            std::io::stdout().write_all(b"CPU readiness\n").unwrap();
            std::io::stdout().flush().unwrap();
            loop {
                std::hint::spin_loop();
            }
        }
    }

    #[test]
    fn native_git_profile_runs_pinned_git_and_denies_host_access() {
        let git = crate::tools::git::discover().expect("pinned git");
        let temp = std::env::temp_dir().canonicalize().unwrap();
        let root = tempfile::Builder::new()
            .prefix("arany-git-root-")
            .tempdir_in(&temp)
            .unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let workspace = tempfile::Builder::new()
            .prefix("arany-git-workspace-")
            .tempdir_in(&temp)
            .unwrap();
        let home = tempfile::tempdir_in(&temp).unwrap();
        let git_in_workspace = |args: &[&str]| {
            let output = std::process::Command::new(&git.executable)
                .args(args)
                .current_dir(workspace.path())
                .env_clear()
                .env("HOME", home.path())
                .env("PATH", "/usr/bin:/bin")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "Arany Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
                .env("GIT_COMMITTER_NAME", "Arany Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git_in_workspace(&["init", "-q"]);
        std::fs::write(workspace.path().join("a.txt"), "one\n").unwrap();
        git_in_workspace(&["add", "a.txt"]);
        git_in_workspace(&["commit", "-q", "-m", "first commit"]);
        std::fs::write(workspace.path().join("a.txt"), "two\n").unwrap();
        std::fs::write(root.path().join("outside"), b"synthetic excluded data").unwrap();
        let network = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::guard::macos::tests::native_git_profile_helper",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("ARANY_TEST_GIT_PROFILE_ROOT", root.path())
            .env("ARANY_TEST_GIT_PROFILE_WORKSPACE", workspace.path())
            .env(
                "ARANY_TEST_GIT_PROFILE_PORT",
                network.local_addr().unwrap().port().to_string(),
            )
            .current_dir(root.path())
            .bounded_output_for(Duration::from_secs(90), 32 * 1024)
            .unwrap();
        assert!(
            output.status.success(),
            "native git profile: {:?}; stdout={:?}; stderr={:?}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        crate::test_process::assert_single_test_completed(&output.stdout, "native git profile");
        assert!(output.stderr.is_empty());
    }

    #[test]
    #[ignore = "isolated native git profile helper"]
    fn native_git_profile_helper() {
        let root = PathBuf::from(
            std::env::var_os("ARANY_TEST_GIT_PROFILE_ROOT").expect("private fixture root"),
        );
        let workspace = PathBuf::from(
            std::env::var_os("ARANY_TEST_GIT_PROFILE_WORKSPACE").expect("fixture workspace"),
        );
        let port = std::env::var("ARANY_TEST_GIT_PROFILE_PORT")
            .unwrap()
            .parse::<u16>()
            .unwrap();
        let git = crate::tools::git::discover().expect("pinned git");
        let (device, inode) =
            crate::tools::fs::identity(&crate::tools::fs::open_directory(&workspace).unwrap())
                .unwrap();
        let config: Config = serde_json::from_value(serde_json::json!({"version":1,
            "workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[],
            "git": git}))
        .unwrap();
        let call = ToolCall::GitStatus { path: ".".into() };
        let task = Task {
            intent: EffectIntent {
                id: uuid::Uuid::now_v7(),
                run_id: crate::session::RunId::default(),
                agent_run_id: crate::session::AgentRunId::default(),
                policy_digest: config.digest(),
                enforcement_digest: enforcement_digest().unwrap(),
                workspace_device: device,
                workspace_inode: inode,
                call: call.clone(),
                limits: ToolLimits::default(),
                expires_at_ms: crate::tools::now_ms() + 60_000,
                use_count: 1,
            },
            config,
            cache: None,
            workspace: workspace.clone(),
            guard_executable: std::env::current_exe().unwrap(),
            system_programs: None,
        };
        configure_resources().unwrap();
        let mut stage = super::BootstrapStage::Workspace;
        assert_eq!(install(&task, &mut stage).unwrap(), root);
        assert_eq!(
            std::fs::read(root.join("outside")).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::fs::read_dir(&root).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(matches!(
            std::process::Command::new("/usr/bin/true")
                .status()
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
        ));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let output = runtime
            .block_on(crate::tools::git::execute(
                &workspace,
                &root,
                &git,
                &call,
                16 * 1024,
                Duration::from_secs(20),
            ))
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(value.get("failed").is_none(), "{output}");
        assert!(
            value["entries"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!({"path": "a.txt", "state": ".M"}))
        );
        std::fs::remove_dir_all(root.join("git-view")).unwrap();
        let log = ToolCall::GitLog {
            path: ".".into(),
            revision: None,
            max_count: 5,
        };
        let output = runtime
            .block_on(crate::tools::git::execute(
                &workspace,
                &root,
                &git,
                &log,
                16 * 1024,
                Duration::from_secs(20),
            ))
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["commits"].as_array().unwrap().len(), 1, "{output}");
    }
}

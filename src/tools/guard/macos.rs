#![allow(unsafe_code)]

use super::{BootstrapFailure, CAPTURE_BYTES, Ready, Task, read_capped, read_line_capped};
use crate::engine::RunCancellation;
use crate::tools::types::{MAX_PROCESSES, MEMORY_BYTES, digest};
use crate::tools::{EffectIntent, ToolCall, ToolError, ToolObservation, ToolRuntime};
use nix::libc;
use std::ffi::CString;
use std::mem::{MaybeUninit, size_of, size_of_val};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::Command;

mod unit;

pub(super) fn unit_main() -> std::process::ExitCode {
    unit::helper_main()
}

const PROC_FLAG_INEXIT: u32 = 4;

const PROFILE: &str = "(version 1)
(deny default)
(allow syscall-unix)
(deny syscall-unix (syscall-number 2) (syscall-number 28) (syscall-number 46) (syscall-number 48) (syscall-number 59) (syscall-number 66) (syscall-number 97) (syscall-number 98) (syscall-number 104) (syscall-number 106) (syscall-number 111) (syscall-number 133) (syscall-number 184) (syscall-number 195) (syscall-number 202) (syscall-number 244) (syscall-number 274) (syscall-number 329) (syscall-number 330) (syscall-number 331) (syscall-number 380) (syscall-number 402) (syscall-number 409) (syscall-number 410) (syscall-number 413) (syscall-number 422) (syscall-number 447) (syscall-number 450) (syscall-number 481))
(allow file-read-metadata)
(allow file-read* file-map-executable (subpath \"/System/Library\") (subpath \"/usr/lib\"))
(allow file-read* (literal \"/dev/random\") (literal \"/dev/urandom\"))
";

const COMMAND_PROFILE: &str = "(version 1)
(deny default)
(allow syscall-unix)
(deny syscall-unix (syscall-number 23) (syscall-number 26) (syscall-number 28) (syscall-number 97) (syscall-number 98) (syscall-number 104) (syscall-number 106) (syscall-number 126) (syscall-number 127) (syscall-number 133) (syscall-number 181) (syscall-number 182) (syscall-number 183) (syscall-number 195) (syscall-number 202) (syscall-number 274) (syscall-number 329) (syscall-number 330) (syscall-number 331) (syscall-number 380) (syscall-number 402) (syscall-number 409) (syscall-number 410) (syscall-number 413) (syscall-number 447) (syscall-number 450) (syscall-number 458) (syscall-number 459) (syscall-number 481) (syscall-number 494) (syscall-number 556))
(allow file-read-metadata)
(allow file-read* file-map-executable (subpath \"/System/Library\") (subpath \"/usr/lib\"))
(allow file-read* (literal \"/\") (literal \"/bin\") (literal \"/usr/bin\") (literal \"/dev/random\") (literal \"/dev/urandom\"))
(allow file-read* file-write* (literal \"/dev/null\"))
(allow process-fork)
(deny process-fork (require-entitlement \"com.apple.private.coalition-spawn\") (require-entitlement \"com.apple.private.persona-mgmt\"))
(deny process-exec-update-label)
(allow signal (target self) (target children))
";

unsafe extern "C" {
    static mach_task_self_: u32;
    fn task_set_special_port(task: u32, which: libc::c_int, port: u32) -> libc::c_int;
    fn task_get_special_port(task: u32, which: libc::c_int, port: *mut u32) -> libc::c_int;
    fn mach_ports_register(task: u32, ports: *mut u32, count: u32) -> libc::c_int;
    fn sandbox_init(
        profile: *const libc::c_char,
        flags: u64,
        error: *mut *mut libc::c_char,
    ) -> libc::c_int;
    fn sandbox_free_error(error: *mut libc::c_char);
    fn sandbox_check(
        pid: libc::pid_t,
        operation: *const libc::c_char,
        filter: libc::c_int,
        ...
    ) -> libc::c_int;
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecCodeInfoEntitlements: core_foundation::string::CFStringRef;
    static kSecCodeInfoEntitlementsDict: core_foundation::string::CFStringRef;
    fn SecCodeCopySigningInformation(
        code: core_foundation::base::CFTypeRef,
        flags: u32,
        information: *mut core_foundation::dictionary::CFDictionaryRef,
    ) -> i32;
}

pub(super) fn check_available() -> Result<(), ToolError> {
    if cfg!(any(target_arch = "aarch64", target_arch = "x86_64")) {
        Ok(())
    } else {
        Err(ToolError::ProtectionUnavailable)
    }
}

pub(super) fn enforcement_digest() -> Result<[u8; 32], ToolError> {
    check_available()?;
    let mut bytes = b"arany-macos-guard-v3:typed-files-pinned-skills;file-no-fork-exec;closed-inherited-fds;live-no-follow-handles;literal-directory-ancestors;workspace-call-write;guard-read-only;file-cpu15-default-unblocked-immutable;nofile64;core0;rss536870912-tasks64-cpu15-supervised10ms;one-use-launchd-coalition;authenticated-private-control;atomic-audit-token-signals;owned-unit-cleanup;exact-child-kill-reap;bounded-go-receipt".to_vec();
    bytes.extend_from_slice(PROFILE.as_bytes());
    bytes.extend_from_slice(COMMAND_PROFILE.as_bytes());
    bytes.extend_from_slice(b"native-commands-mcp:checked-private-cwd;fixed64m-nosuid-project-scratch;fresh-selected-snapshot;read-only-pinned-runtime;bounded-positive-root-owned-non-setid-unentitled-system-exec;dynamic-coalition-persona-entitlement-fork-denial;null-bootstrap-registered-ports;child-only-signals;supervised-cpu");
    Ok(digest(&bytes))
}

pub(super) fn bootstrap() -> Result<(), ToolError> {
    if task_info(std::process::id())?.pti_threadnum != 1 {
        return Err(ToolError::ProtectionUnavailable);
    }
    let mut descriptors = [libc::proc_fdinfo {
        proc_fd: 0,
        proc_fdtype: 0,
    }; 8192];
    // This single-threaded helper owns inherited descriptors before Rust creates any handles.
    let count = unsafe {
        libc::proc_pidinfo(
            std::process::id() as i32,
            libc::PROC_PIDLISTFDS,
            0,
            descriptors.as_mut_ptr().cast(),
            size_of_val(&descriptors) as i32,
        )
    };
    if count <= 0
        || count as usize >= size_of_val(&descriptors)
        || !(count as usize).is_multiple_of(size_of::<libc::proc_fdinfo>())
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    for descriptor in &descriptors[..count as usize / size_of::<libc::proc_fdinfo>()] {
        if descriptor.proc_fd >= 3 {
            nix::unistd::close(descriptor.proc_fd).map_err(|_| ToolError::ProtectionUnavailable)?;
        }
    }
    configure_resources()
}

fn configure_resources() -> Result<(), ToolError> {
    // Darwin's C signal structures have no invalid all-zero representation.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = libc::SIG_DFL;
    let mut mask = MaybeUninit::<libc::sigset_t>::uninit();
    // All pointers refer to live, correctly sized output; no handler or borrowed pointer is retained.
    let configured = unsafe {
        libc::sigemptyset(&mut action.sa_mask) == 0
            && libc::sigaction(libc::SIGXCPU, &action, std::ptr::null_mut()) == 0
            && libc::sigemptyset(mask.as_mut_ptr()) == 0
            && libc::sigaddset(mask.as_mut_ptr(), libc::SIGXCPU) == 0
            && libc::sigprocmask(libc::SIG_UNBLOCK, mask.as_ptr(), std::ptr::null_mut()) == 0
    };
    if !configured {
        return Err(ToolError::ProtectionUnavailable);
    }
    use rustix::process::{Resource, Rlimit, setrlimit};
    for (resource, limit) in [
        (Resource::Cpu, 15),
        (Resource::Nofile, 64),
        (Resource::Core, 0),
    ] {
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
            "(allow file-read-data (literal {}))\n",
            quoted(directory)?
        ));
    }
    Ok(())
}

pub(super) fn install(task: &Task) -> Result<(), ToolError> {
    if matches!(
        task.intent.call,
        ToolCall::Command { .. } | ToolCall::McpList { .. } | ToolCall::McpCall { .. }
    ) {
        return install_command(task);
    }
    let mut profile = PROFILE.to_owned();
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
    for operation in [c"process-fork", c"network-outbound"] {
        // These operations have no filter argument; the PID is the calling process.
        if unsafe { sandbox_check(std::process::id() as i32, operation.as_ptr(), 0) } != 1 {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    for syscall in [
        2_i32, 28, 46, 48, 59, 66, 97, 98, 104, 106, 111, 133, 184, 195, 202, 244, 274, 329, 330,
        331, 380, 402, 409, 410, 413, 422, 447, 450, 481,
    ] {
        // Darwin's syscall-number filter is 14; its variadic argument is a C int.
        if unsafe {
            sandbox_check(
                std::process::id() as i32,
                c"syscall-unix".as_ptr(),
                14,
                syscall,
            )
        } != 1
        {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    verify_cpu_controls()?;
    Ok(())
}

fn install_profile(profile: String) -> Result<(), ToolError> {
    if profile.len() > 96 * 1024 {
        return Err(ToolError::Limit);
    }
    let profile = CString::new(profile).map_err(|_| ToolError::Path)?;
    let mut error = std::ptr::null_mut();
    // The profile lives through the call; the native error allocation is released exactly once.
    let status = unsafe {
        let status = sandbox_init(profile.as_ptr(), 0, &mut error);
        if !error.is_null() {
            sandbox_free_error(error);
        }
        status
    };
    if status != 0 {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

fn command_root(task: &Task) -> Result<PathBuf, ToolError> {
    let root = std::env::current_dir().map_err(|_| ToolError::Path)?;
    crate::store::StateRoot::open_existing(&root).map_err(|_| ToolError::Path)?;
    crate::tools::fs::reject_overlap(&root, &task.workspace)?;
    if crate::tools::fs::identity(&crate::tools::fs::open_directory(&task.workspace)?)?
        != (task.intent.workspace_device, task.intent.workspace_inode)
    {
        return Err(ToolError::ChangedInput);
    }
    unit::verify_storage(&root)?;
    Ok(root)
}

pub(super) fn system_program(path: &str) -> Result<bool, ToolError> {
    use std::os::unix::fs::MetadataExt;

    let path = Path::new(path);
    if !matches!(path.parent(), Some(parent) if [Path::new("/bin"), Path::new("/usr/bin")].contains(&parent))
    {
        return Ok(false);
    }
    let parent = crate::tools::fs::open_directory(path.parent().ok_or(ToolError::Path)?)?;
    let parent = parent
        .into_std_file()
        .metadata()
        .map_err(|_| ToolError::Path)?;
    let file = crate::tools::fs::open_absolute(path.to_str().ok_or(ToolError::Path)?)?;
    let metadata = file.metadata().map_err(|_| ToolError::Path)?;
    if !parent.is_dir()
        || parent.uid() != 0
        || parent.mode() & 0o022 != 0
        || !metadata.is_file()
        || metadata.uid() != 0
        || metadata.nlink() != 1
        || metadata.mode() & 0o6022 != 0
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(true)
}

fn system_program_without_entitlements(path: &Path) -> Result<bool, ToolError> {
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;
    use core_foundation::url::CFURL;
    use security_framework::os::macos::code_signing::{Flags, SecStaticCode};

    let url = CFURL::from_path(path, false).ok_or(ToolError::Path)?;
    let Ok(code) = SecStaticCode::from_path(&url, Flags::NONE) else {
        return Ok(false);
    };
    let mut information = std::ptr::null();
    // Only immutable OS-owned code is inspected; successful Copy transfers one retained dictionary.
    let status = unsafe { SecCodeCopySigningInformation(code.as_CFTypeRef(), 2, &mut information) };
    if status != 0 || information.is_null() {
        return Ok(false);
    }
    // The successful SDK Copy owns this dictionary; exported constant keys use the Get rule.
    let (information, dictionary_key, data_key) = unsafe {
        (
            CFDictionary::<CFString, CFType>::wrap_under_create_rule(information),
            CFString::wrap_under_get_rule(kSecCodeInfoEntitlementsDict),
            CFString::wrap_under_get_rule(kSecCodeInfoEntitlements),
        )
    };
    if let Some(value) = information.find(&dictionary_key) {
        return Ok(value
            .downcast::<CFDictionary>()
            .is_some_and(|value| value.is_empty()));
    }
    Ok(!information.contains_key(&data_key))
}

fn install_command(task: &Task) -> Result<(), ToolError> {
    use std::os::unix::fs::MetadataExt;

    if rustix::process::geteuid().as_raw() == 0
        || rustix::process::getuid() != rustix::process::geteuid()
        || rustix::process::getgid() != rustix::process::getegid()
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    let root = command_root(task)?;
    for program in task
        .config
        .commands
        .iter()
        .chain(task.config.mcp.iter().map(|server| &server.program))
    {
        program.verify()?;
        if system_program(&program.executable)?
            && !system_program_without_entitlements(Path::new(&program.executable))?
        {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    let mut profile = COMMAND_PROFILE.to_owned();
    for name in ["workspace", "scratch"] {
        let path = root.join(name);
        ancestors(&mut profile, &path)?;
        profile.push_str(&format!(
            "(allow file-read* file-write* file-map-executable process-exec (subpath {}))\n",
            quoted(&path)?
        ));
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
    let mut count = 0;
    for directory in ["/bin", "/usr/bin"] {
        let dir = crate::tools::fs::open_directory(Path::new(directory))?;
        use cap_std::fs::MetadataExt;
        let metadata = dir
            .dir_metadata()
            .map_err(|_| ToolError::ProtectionUnavailable)?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(ToolError::ProtectionUnavailable);
        }
        for entry in dir
            .entries()
            .map_err(|_| ToolError::ProtectionUnavailable)?
        {
            count += 1;
            if count > 2048 {
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
            if !metadata.is_file()
                || metadata.uid() != 0
                || metadata.nlink() != 1
                || metadata.mode() & 0o6022 != 0
            {
                continue;
            }
            let path = Path::new(directory).join(entry.file_name());
            if !system_program_without_entitlements(&path)? {
                continue;
            }
            profile.push_str(&format!(
                "(allow file-read* file-map-executable process-exec (literal {}))\n",
                quoted(&path)?
            ));
            if profile.len() > 96 * 1024 {
                return Err(ToolError::Limit);
            }
        }
    }
    profile.push_str(&format!(
        "(deny file-write* (literal {}))\n",
        quoted(&task.guard_executable)?
    ));
    let mut bootstrap = u32::MAX;
    // Mach ports are 32-bit SDK names; these calls retain no borrowed memory or service right.
    let detached = unsafe {
        task_set_special_port(mach_task_self_, 4, 0) == 0
            && mach_ports_register(mach_task_self_, std::ptr::null_mut(), 0) == 0
            && task_get_special_port(mach_task_self_, 4, &mut bootstrap) == 0
            && bootstrap == 0
    };
    if !detached {
        return Err(ToolError::ProtectionUnavailable);
    }
    install_profile(profile)?;
    // The unfiltered query uses this process's PID; entitled actors must lose launch authority.
    if unsafe { sandbox_check(std::process::id() as i32, c"process-fork".as_ptr(), 0) } != 0 {
        return Err(ToolError::ProtectionUnavailable);
    }
    for operation in [
        c"network-outbound",
        c"mach-lookup",
        c"iokit-open-user-client",
        c"process-exec-update-label",
    ] {
        // The default rule rejects unfiltered service/network operations for this process.
        if unsafe { sandbox_check(std::process::id() as i32, operation.as_ptr(), 0) } != 1 {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    for syscall in [202_i32, 274, 458, 459, 494, 556] {
        // The syscall-number filter accepts a C int and no retained pointer.
        if unsafe {
            sandbox_check(
                std::process::id() as i32,
                c"syscall-unix".as_ptr(),
                14,
                syscall,
            )
        } != 1
        {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    Ok(())
}

fn verify_cpu_controls() -> Result<(), ToolError> {
    use rustix::process::{Resource, getrlimit};
    for (resource, limit) in [
        (Resource::Cpu, 15),
        (Resource::Nofile, 64),
        (Resource::Core, 0),
    ] {
        let actual = getrlimit(resource);
        if actual.current != Some(limit) || actual.maximum != Some(limit) {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    // C signal structures are initialized before use and no callback is installed on success.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = libc::SIG_IGN;
    let mut mask = MaybeUninit::<libc::sigset_t>::uninit();
    // A rejected attempt leaves the previously reset and unblocked SIGXCPU unchanged.
    let denied = unsafe {
        libc::sigemptyset(&mut action.sa_mask) == 0
            && libc::sigaction(libc::SIGXCPU, &action, std::ptr::null_mut()) == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
            && libc::sigemptyset(mask.as_mut_ptr()) == 0
            && libc::sigaddset(mask.as_mut_ptr(), libc::SIGXCPU) == 0
            && libc::sigprocmask(libc::SIG_BLOCK, mask.as_ptr(), std::ptr::null_mut()) == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
            && libc::pthread_sigmask(libc::SIG_BLOCK, mask.as_ptr(), std::ptr::null_mut())
                == libc::EPERM
    };
    if !denied {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

fn task_info(pid: u32) -> Result<libc::proc_taskinfo, ToolError> {
    let mut info = MaybeUninit::<libc::proc_taskinfo>::uninit();
    // Callers establish process ownership; a full kernel write initializes the SDK layout.
    let count = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size_of::<libc::proc_taskinfo>() as i32,
        )
    };
    if count as usize != size_of::<libc::proc_taskinfo>() {
        return Err(ToolError::ProtectionUnavailable);
    }
    // Only the exact-size successful native write above permits reading this output.
    Ok(unsafe { info.assume_init() })
}

fn observe(pid: u32) -> Result<bool, ToolError> {
    let info = match task_info(pid) {
        Ok(info) => info,
        Err(_) => {
            let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
            // Callers bind the process identity; exact BSD output establishes kernel exit.
            let count = unsafe {
                libc::proc_pidinfo(
                    pid as i32,
                    libc::PROC_PIDTBSDINFO,
                    1,
                    info.as_mut_ptr().cast(),
                    size_of::<libc::proc_bsdinfo>() as i32,
                )
            };
            if count as usize == size_of::<libc::proc_bsdinfo>() {
                // All fields are initialized by the exact-size native write.
                let info = unsafe { info.assume_init() };
                if info.pbi_pid == pid
                    && (info.pbi_status == libc::SZOMB || info.pbi_flags & PROC_FLAG_INEXIT != 0)
                {
                    return Ok(false);
                }
            }
            return Err(ToolError::ProtectionUnavailable);
        }
    };
    if info.pti_threadnum < 1
        || info.pti_resident_size > MEMORY_BYTES
        || info.pti_threadnum as u32 > MAX_PROCESSES
    {
        return Err(ToolError::Limit);
    }
    Ok(true)
}

pub(super) async fn execute_inner(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    cancellation: RunCancellation,
    dispatched: &mut bool,
) -> Result<ToolObservation, ToolError> {
    crate::tools::fs::validate_workspace(runtime)?;
    unit::execute(runtime, intent, cancellation, dispatched).await
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
    let mut child = Command::new(&runtime.guard_executable)
        .arg("--internal-tool-guard")
        .env_clear()
        .current_dir(
            if matches!(
                intent.call,
                ToolCall::Command { .. } | ToolCall::McpList { .. } | ToolCall::McpCall { .. }
            ) {
                root
            } else {
                Path::new("/")
            },
        )
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
    let execution = async {
        let ready = tokio::time::timeout(Duration::from_secs(5), async {
            stdin
                .write_all(&task)
                .await
                .map_err(|_| ToolError::Operation)?;
            stdin
                .write_all(b"\n")
                .await
                .map_err(|_| ToolError::Operation)?;
            let bytes = read_line_capped(&mut stdout, 2048).await?;
            serde_json::from_slice::<Ready>(&bytes).map_err(|_| ToolError::ProtectionUnavailable)
        })
        .await
        .map_err(|_| ToolError::Limit)??;
        if ready.pid != pid
            || ready.digest != intent.digest()
            || ready.profile_digest != intent.enforcement_digest
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        if !observe(pid).map_err(|_| ToolError::GuardRejected("resource attestation"))? {
            return Err(ToolError::GuardRejected("resource attestation"));
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
        receive_go.await.map_err(|_| ToolError::Operation)?;
        let payload_deadline = tokio::time::Instant::now()
            + Duration::from_millis(u64::from(intent.limits.runtime_ms));
        tokio::time::timeout_at(payload_deadline, async {
            stdin
                .write_all(b"GO\n")
                .await
                .map_err(|_| ToolError::Operation)?;
            stdin.shutdown().await.map_err(|_| ToolError::Operation)?;
            let bytes = read_line_capped(&mut stdout, CAPTURE_BYTES).await?;
            let observation: ToolObservation =
                serde_json::from_slice(&bytes).map_err(|_| ToolError::Operation)?;
            if observation.intent != *intent
                || !observation.valid()
                || observation
                    .guard
                    .as_ref()
                    .is_none_or(|receipt| !receipt.valid_for(intent))
            {
                return Err(ToolError::Operation);
            }
            if !read_capped(stdout, 1).await?.is_empty() {
                return Err(ToolError::Operation);
            }
            Ok(observation)
        })
        .await
        .map_err(|_| ToolError::Limit)?
    };
    let deadline = tokio::time::Instant::now()
        + Duration::from_millis(u64::from(intent.limits.runtime_ms))
        + Duration::from_secs(5);
    let execution = tokio::time::timeout_at(deadline, execution);
    tokio::pin!(execution);
    let mut samples = tokio::time::interval(Duration::from_millis(10));
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
                if let Err(error) = scope.observe() { break Err(error); }
            }
        }
    };
    if result.is_err() {
        let _ = child.start_kill();
    }
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
    let drained = tokio::time::timeout(Duration::from_secs(2), &mut stderr_task).await;
    if drained.is_err() {
        stderr_task.abort();
        let _ = stderr_task.await;
    }
    if !matches!(reaped, Ok(Ok(_))) {
        return Err(ToolError::GuardRejected("launcher reap"));
    }
    if result.is_ok() && !matches!(reaped, Ok(Ok(status)) if status.success()) {
        return Err(ToolError::Operation);
    }
    if !matches!(drained, Ok(Ok(Ok(_)))) {
        return Err(ToolError::GuardRejected("stderr EOF"));
    }
    if result.is_err()
        && let Ok(Ok(Ok(stderr))) = &drained
        && let Ok(failure) = serde_json::from_slice::<BootstrapFailure>(stderr)
    {
        return Err(ToolError::GuardBootstrap(failure));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_process::BoundedOutput;
    use crate::tools::config::Config;
    use crate::tools::types::ToolLimits;
    use std::io::Write;
    use std::os::unix::process::ExitStatusExt;

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
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
        assert!(output.stderr.is_empty());
    }

    #[test]
    #[ignore = "native 15-second CPU expiry; finite isolated process"]
    fn native_file_profile_cpu_expiry_cannot_be_disabled() {
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
            workspace,
            guard_executable: std::env::current_exe().unwrap(),
        };
        configure_resources().unwrap();
        install(&task).unwrap();
        let mut query = [
            libc::CTL_KERN,
            libc::KERN_PROCARGS2,
            std::process::id() as i32,
        ];
        let mut size = 0;
        // This queries only the cleared-environment fixture itself, with live sized output.
        assert_eq!(
            unsafe {
                libc::sysctl(
                    query.as_mut_ptr(),
                    query.len() as u32,
                    std::ptr::null_mut(),
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                )
            },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        assert_eq!(
            std::fs::read(root.join("outside")).unwrap_err().kind(),
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
        assert_eq!(
            std::os::unix::net::UnixListener::bind(root.join("workspace/socket"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::process::Command::new("/usr/bin/true")
                .status()
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        if cpu {
            std::io::stdout().write_all(b"CPU readiness\n").unwrap();
            std::io::stdout().flush().unwrap();
            loop {
                std::hint::spin_loop();
            }
        }
    }
}

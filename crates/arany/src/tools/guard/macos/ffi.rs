use crate::tools::ToolError;
use nix::libc;
use std::ffi::CStr;
use std::mem::{MaybeUninit, size_of, size_of_val};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;

// Persisted coalitions record this boot ID so a later boot never signals their stale identities.
pub(super) fn boot_session() -> Result<uuid::Uuid, ToolError> {
    let mut bytes = [0_u8; 37];
    let mut length = bytes.len();
    // The fixed SDK string buffer lives through the query and includes its terminator.
    if unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || length != bytes.len()
        || bytes[36] != 0
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    uuid::Uuid::try_parse_ascii(&bytes[..36]).map_err(|_| ToolError::ProtectionUnavailable)
}

pub(super) fn mounts() -> Result<Vec<(std::path::PathBuf, std::path::PathBuf)>, ToolError> {
    use std::os::unix::ffi::OsStringExt;
    const MOUNT_CAP: usize = 4096;
    let mut mounts = Vec::<MaybeUninit<libc::statfs>>::with_capacity(MOUNT_CAP);
    // getfsstat initializes only the reported records in this aligned, fixed-capacity buffer.
    let count = unsafe {
        libc::getfsstat(
            mounts.as_mut_ptr().cast(),
            (MOUNT_CAP * size_of::<libc::statfs>()) as i32,
            libc::MNT_NOWAIT,
        )
    };
    if count <= 0 || count as usize >= MOUNT_CAP {
        return Err(ToolError::ProtectionUnavailable);
    }
    // Each record in this prefix was fully initialized by the successful kernel call.
    unsafe { mounts.set_len(count as usize) };
    let path = |bytes: &[libc::c_char]| -> Result<std::path::PathBuf, ToolError> {
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(ToolError::Path)?;
        Ok(
            std::ffi::OsString::from_vec(bytes[..end].iter().map(|byte| *byte as u8).collect())
                .into(),
        )
    };
    mounts
        .into_iter()
        .map(|mount| {
            // Only initialized records from the kernel's reported prefix reach this read.
            let mount = unsafe { mount.assume_init() };
            Ok((path(&mount.f_mntfromname)?, path(&mount.f_mntonname)?))
        })
        .collect()
}

const PROCESS_IDENTITY_INFO: i32 = 17;
const PROCESS_COALITION_INFO: i32 = 20;
const ALL_PROCESSES: u32 = 1;
const AUDIT_TOKEN_PID: usize = 5;
const AUDIT_TOKEN_VERSION: usize = 7;
const PROC_FLAG_INEXIT: u32 = 4;
const SANDBOX_FILTER_SYSCALL: i32 = 14;
const TASK_BOOTSTRAP_PORT: i32 = 4;
const SIGNING_ENTITLEMENTS: u32 = 1 << 1;
const DESCRIPTORS_CAP: usize = 8192;

#[repr(C)]
struct CoalitionInfo {
    ids: [u64; 2],
    reserved: [u64; 3],
}

#[repr(C)]
pub(super) struct ProcessInfo {
    executable_uuid: [u8; 16],
    pub(super) unique_id: u64,
    parent_unique_id: u64,
    pub(super) version: i32,
    parent_version: i32,
    reserved: [u64; 2],
}

#[repr(C)]
pub(super) struct Timebase {
    pub(super) numer: u32,
    pub(super) denom: u32,
}

const _: () = assert!(
    size_of::<CoalitionInfo>() == 40
        && size_of::<ProcessInfo>() == 56
        && size_of::<Timebase>() == 8
);

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
    fn proc_signal_with_audittoken(token: *mut [u32; 8], signal: i32) -> i32;
    fn mach_timebase_info(timebase: *mut Timebase) -> i32;
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

pub(super) fn open_descriptors() -> Result<Vec<i32>, ToolError> {
    let mut descriptors = [libc::proc_fdinfo {
        proc_fd: 0,
        proc_fdtype: 0,
    }; DESCRIPTORS_CAP];
    // The live fixed buffer receives whole SDK records; only the reported prefix is read.
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
    Ok(
        descriptors[..count as usize / size_of::<libc::proc_fdinfo>()]
            .iter()
            .map(|descriptor| descriptor.proc_fd)
            .collect(),
    )
}

pub(super) fn reset_cpu_signal() -> bool {
    // Darwin's C signal structures have no invalid all-zero representation.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = libc::SIG_DFL;
    let mut mask = MaybeUninit::<libc::sigset_t>::uninit();
    // All pointers refer to live, correctly sized output; no handler or borrowed pointer is retained.
    unsafe {
        libc::sigemptyset(&mut action.sa_mask) == 0
            && libc::sigaction(libc::SIGXCPU, &action, std::ptr::null_mut()) == 0
            && libc::sigemptyset(mask.as_mut_ptr()) == 0
            && libc::sigaddset(mask.as_mut_ptr(), libc::SIGXCPU) == 0
            && libc::sigprocmask(libc::SIG_UNBLOCK, mask.as_ptr(), std::ptr::null_mut()) == 0
    }
}

pub(super) fn cpu_signal_changes_denied() -> bool {
    // C signal structures are initialized before use and no callback is installed on success.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = libc::SIG_IGN;
    let mut mask = MaybeUninit::<libc::sigset_t>::uninit();
    // A rejected attempt leaves the previously reset and unblocked SIGXCPU unchanged.
    unsafe {
        libc::sigemptyset(&mut action.sa_mask) == 0
            && libc::sigaction(libc::SIGXCPU, &action, std::ptr::null_mut()) == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
            && libc::sigemptyset(mask.as_mut_ptr()) == 0
            && libc::sigaddset(mask.as_mut_ptr(), libc::SIGXCPU) == 0
            && libc::sigprocmask(libc::SIG_BLOCK, mask.as_ptr(), std::ptr::null_mut()) == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
            && libc::pthread_sigmask(libc::SIG_BLOCK, mask.as_ptr(), std::ptr::null_mut())
                == libc::EPERM
    }
}

pub(super) fn install_sandbox(profile: &CStr) -> bool {
    let mut error = std::ptr::null_mut();
    // The profile lives through the call; the native error allocation is released exactly once.
    unsafe {
        let status = sandbox_init(profile.as_ptr(), 0, &mut error);
        if !error.is_null() {
            sandbox_free_error(error);
        }
        status == 0
    }
}

pub(super) fn sandbox_verdict(operation: &CStr) -> i32 {
    // Unfiltered queries retain no pointer and inspect only the calling process.
    unsafe { sandbox_check(std::process::id() as i32, operation.as_ptr(), 0) }
}

pub(super) fn syscall_denied(syscall: i32) -> bool {
    // The SDK syscall-number filter accepts a C int and no retained pointer.
    unsafe {
        sandbox_check(
            std::process::id() as i32,
            c"syscall-unix".as_ptr(),
            SANDBOX_FILTER_SYSCALL,
            syscall,
        ) == 1
    }
}

pub(super) fn detach_bootstrap_ports() -> bool {
    let mut bootstrap = u32::MAX;
    // Mach ports are 32-bit SDK names; these calls retain no borrowed memory or service right.
    unsafe {
        task_set_special_port(mach_task_self_, TASK_BOOTSTRAP_PORT, 0) == 0
            && mach_ports_register(mach_task_self_, std::ptr::null_mut(), 0) == 0
            && task_get_special_port(mach_task_self_, TASK_BOOTSTRAP_PORT, &mut bootstrap) == 0
            && bootstrap == 0
    }
}

#[cfg(test)]
pub(super) struct MachLookupProbe(u32);

#[cfg(test)]
impl MachLookupProbe {
    pub(super) fn new() -> Self {
        let mut port = 0;
        // SAFETY: The initialized SDK-sized output lives through the call; success owns one send right.
        assert_eq!(
            unsafe { task_get_special_port(mach_task_self_, TASK_BOOTSTRAP_PORT, &mut port) },
            0
        );
        assert_ne!(port, 0);
        Self(port)
    }

    pub(super) fn lookup(&self) -> i32 {
        unsafe extern "C" {
            fn bootstrap_look_up(port: u32, name: *const libc::c_char, service: *mut u32) -> i32;
        }
        let mut service = 0;
        // SAFETY: The retained bootstrap right, fixed C string and initialized output live through the call.
        let status =
            unsafe { bootstrap_look_up(self.0, c"com.apple.system.logger".as_ptr(), &mut service) };
        if status == 0 {
            drop(Self(service));
        }
        status
    }
}

#[cfg(test)]
impl Drop for MachLookupProbe {
    fn drop(&mut self) {
        unsafe extern "C" {
            fn mach_port_deallocate(task: u32, name: u32) -> i32;
        }
        // SAFETY: This wrapper owns one send-right reference in the current task and releases it once.
        assert_eq!(unsafe { mach_port_deallocate(mach_task_self_, self.0) }, 0);
    }
}

pub(super) fn unentitled_code(path: &Path) -> Result<Option<bool>, ToolError> {
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;
    use core_foundation::url::CFURL;
    use security_framework::os::macos::code_signing::{Flags, SecStaticCode};

    let url = CFURL::from_path(path, false).ok_or(ToolError::Path)?;
    let Ok(code) = SecStaticCode::from_path(&url, Flags::NONE) else {
        return Ok(None);
    };
    let mut information = std::ptr::null();
    // Only immutable OS-owned code is inspected; successful Copy transfers one retained dictionary.
    let status = unsafe {
        SecCodeCopySigningInformation(code.as_CFTypeRef(), SIGNING_ENTITLEMENTS, &mut information)
    };
    if status != 0 || information.is_null() {
        return Ok(None);
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
        return Ok(Some(
            value
                .downcast::<CFDictionary>()
                .is_some_and(|value| value.is_empty()),
        ));
    }
    Ok(Some(!information.contains_key(&data_key)))
}

pub(super) fn task_info(pid: u32) -> Result<libc::proc_taskinfo, ToolError> {
    let mut info = MaybeUninit::<libc::proc_taskinfo>::uninit();
    // The live local buffer is writable for exactly the size passed; the kernel writes at most that.
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

pub(super) fn exited(pid: u32) -> bool {
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
    // The live local buffer is writable for exactly the size passed; the kernel writes at most that.
    let count = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            1,
            info.as_mut_ptr().cast(),
            size_of::<libc::proc_bsdinfo>() as i32,
        )
    };
    if count as usize != size_of::<libc::proc_bsdinfo>() {
        return false;
    }
    // All fields are initialized by the exact-size native write.
    let info = unsafe { info.assume_init() };
    info.pbi_pid == pid
        && (info.pbi_status == libc::SZOMB || info.pbi_flags & PROC_FLAG_INEXIT != 0)
}

pub(super) fn process_info(pid: u32) -> Result<ProcessInfo, ToolError> {
    let mut process = MaybeUninit::<ProcessInfo>::uninit();
    // The live local buffer is writable for exactly the size passed; the kernel writes at most that.
    let bytes = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            PROCESS_IDENTITY_INFO,
            0,
            process.as_mut_ptr().cast(),
            size_of::<ProcessInfo>() as i32,
        )
    };
    if bytes != size_of::<ProcessInfo>() as i32 {
        return Err(process_info_error(bytes));
    }
    // Every process field was initialized by the exact-size write above.
    Ok(unsafe { process.assume_init() })
}

pub(super) fn coalition(pid: u32) -> Result<u64, ToolError> {
    let mut coalition = MaybeUninit::<CoalitionInfo>::uninit();
    // The live local buffer is writable for exactly the size passed; the kernel writes at most that.
    let bytes = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            PROCESS_COALITION_INFO,
            0,
            coalition.as_mut_ptr().cast(),
            size_of::<CoalitionInfo>() as i32,
        )
    };
    if bytes != size_of::<CoalitionInfo>() as i32 {
        return Err(process_info_error(bytes));
    }
    // Only the exact-size write permits reading this integer-only output.
    Ok(unsafe { coalition.assume_init() }.ids[0])
}

fn process_info_error(bytes: i32) -> ToolError {
    if bytes <= 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        ToolError::ChangedInput
    } else {
        ToolError::ProtectionUnavailable
    }
}

// Signal the exact PID/version pair atomically so a reused PID cannot receive the signal.
pub(super) fn signal(pid: u32, version: u32, signal: i32) -> Result<(), ToolError> {
    let mut token = [0_u32; 8];
    token[AUDIT_TOKEN_PID] = pid;
    token[AUDIT_TOKEN_VERSION] = version;
    // Darwin resolves the PID/version atomically; the other token fields are not selectors.
    match unsafe { proc_signal_with_audittoken(&mut token, signal) } {
        0 | libc::ESRCH => Ok(()),
        _ => Err(ToolError::ProtectionUnavailable),
    }
}

pub(super) fn timebase() -> Result<Timebase, ToolError> {
    let mut timebase = MaybeUninit::<Timebase>::uninit();
    // Success initializes the SDK's two integer clock-conversion fields.
    if unsafe { mach_timebase_info(timebase.as_mut_ptr()) } != 0 {
        return Err(ToolError::ProtectionUnavailable);
    }
    // Only the successful native call permits reading this output.
    Ok(unsafe { timebase.assume_init() })
}

pub(super) fn max_processes() -> Result<i32, ToolError> {
    let mut maximum = 0_i32;
    let mut length = size_of::<i32>();
    let mut query = [libc::CTL_KERN, libc::KERN_MAXPROC];
    // `query` and the i32 output are live for the call and `length` states the output size.
    if unsafe {
        libc::sysctl(
            query.as_mut_ptr(),
            query.len() as u32,
            (&mut maximum as *mut i32).cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || length != size_of::<i32>()
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(maximum)
}

pub(super) fn list_pids(pids: &mut [i32]) -> Result<&[i32], ToolError> {
    // `pids` is a live, writable, i32-aligned buffer and its exact byte length is passed.
    let bytes = unsafe {
        libc::proc_listpids(
            ALL_PROCESSES,
            0,
            pids.as_mut_ptr().cast(),
            size_of_val(pids) as i32,
        )
    };
    if bytes <= 0
        || bytes as usize >= size_of_val(pids)
        || !(bytes as usize).is_multiple_of(size_of::<i32>())
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(&pids[..bytes as usize / size_of::<i32>()])
}

pub(super) fn rusage(pid: u32) -> Result<libc::rusage_info_v0, ToolError> {
    let mut usage = MaybeUninit::<libc::rusage_info_v0>::uninit();
    // The selected flavor writes the SDK's fixed rusage_info_v0 layout.
    if unsafe { libc::proc_pid_rusage(pid as i32, libc::RUSAGE_INFO_V0, usage.as_mut_ptr().cast()) }
        != 0
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    // The successful flavor-specific write initializes every integer/byte-array field.
    Ok(unsafe { usage.assume_init() })
}

pub(super) fn peer_credentials(stream: &UnixStream) -> Result<(i32, libc::xucred), ToolError> {
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
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    // The exact credential write above initializes the integer-only xucred structure.
    Ok((pid, unsafe { credentials.assume_init() }))
}

#[cfg(test)]
unsafe extern "C" {
    fn posix_spawnattr_setcoalition_np(
        attributes: *const libc::posix_spawnattr_t,
        coalition: u64,
        kind: libc::c_int,
        role: libc::c_int,
    ) -> libc::c_int;
}

#[cfg(test)]
pub(super) fn spawn_in_coalition(program: &CStr, coalition: u64) -> Result<(), i32> {
    let mut attributes = MaybeUninit::<libc::posix_spawnattr_t>::uninit();
    let mut child = 0;
    let mut args = [program.as_ptr().cast_mut(), std::ptr::null_mut()];
    let mut environment = [std::ptr::null_mut()];
    // Initialized attributes and NUL-terminated arrays live through every native call.
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
    match status {
        0 => Ok(()),
        failed => Err(failed),
    }
}

#[cfg(test)]
pub(super) fn ignore_cpu_signal_with_watchdog(seconds: u32) -> bool {
    let mut mask = MaybeUninit::<libc::sigset_t>::uninit();
    // The initialized mask is copied; the process-wide watchdog and disposition retain no pointer.
    unsafe {
        libc::alarm(seconds);
        libc::signal(libc::SIGXCPU, libc::SIG_IGN) != libc::SIG_ERR
            && libc::sigemptyset(mask.as_mut_ptr()) == 0
            && libc::sigaddset(mask.as_mut_ptr(), libc::SIGXCPU) == 0
            && libc::pthread_sigmask(libc::SIG_BLOCK, mask.as_ptr(), std::ptr::null_mut()) == 0
    }
}

#[cfg(test)]
pub(super) fn process_arguments_errno(pid: i32) -> Option<i32> {
    let mut query = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size = 0;
    // A null output buffer requests only the size path and retains no pointer.
    let status = unsafe {
        libc::sysctl(
            query.as_mut_ptr(),
            query.len() as u32,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (status != 0)
        .then(|| std::io::Error::last_os_error().raw_os_error())
        .flatten()
}

#[cfg(test)]
pub(super) fn invalid_argument_syscall_errno(syscall: i32) -> Option<i32> {
    assert!(super::COMMAND_DENIED_SYSCALLS.contains(&syscall));
    // Only profile-denied syscalls with scalar arguments; none dereferences a pointer.
    let status = unsafe { libc::syscall(syscall, -1_i32, 0_usize, 0_usize, 0_usize) };
    (status == -1)
        .then(|| std::io::Error::last_os_error().raw_os_error())
        .flatten()
}

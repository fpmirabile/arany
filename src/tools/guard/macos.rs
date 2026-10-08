#![allow(unsafe_code)]

use super::{BootstrapFailure, CAPTURE_BYTES, Ready, Task, read_capped, read_line_capped};
use crate::engine::RunCancellation;
use crate::tools::types::{MAX_PROCESSES, MEMORY_BYTES, digest};
use crate::tools::{EffectIntent, ToolCall, ToolError, ToolObservation, ToolRuntime};
use nix::libc;
use std::ffi::CString;
use std::mem::{MaybeUninit, size_of, size_of_val};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::Command;

const PROC_FLAG_INEXIT: u32 = 4;

const PROFILE: &str = "(version 1)
(deny default)
(allow syscall-unix)
(deny syscall-unix (syscall-number 2) (syscall-number 46) (syscall-number 48) (syscall-number 59) (syscall-number 66) (syscall-number 111) (syscall-number 184) (syscall-number 195) (syscall-number 244) (syscall-number 329) (syscall-number 330) (syscall-number 331) (syscall-number 380) (syscall-number 410) (syscall-number 422))
(allow file-read-metadata)
(allow file-read* file-map-executable (subpath \"/System/Library\") (subpath \"/usr/lib\"))
(allow sysctl-read)
(allow file-read* (literal \"/dev/random\") (literal \"/dev/urandom\"))
";

unsafe extern "C" {
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

pub(super) fn check_available() -> Result<(), ToolError> {
    if cfg!(any(target_arch = "aarch64", target_arch = "x86_64")) {
        Ok(())
    } else {
        Err(ToolError::ProtectionUnavailable)
    }
}

pub(super) fn enforcement_digest() -> Result<[u8; 32], ToolError> {
    check_available()?;
    let mut bytes = b"arany-macos-file-guard-v1:typed-files-pinned-skills-only;no-fork-exec-network-mach;closed-inherited-fds;live-no-follow-handles;literal-directory-ancestors;workspace-call-write;guard-read-only;cpu15-default-unblocked-immutable;nofile64;core0;rss536870912-tasks64-supervised10ms;exact-child-kill-reap;bounded-go-receipt".to_vec();
    bytes.extend_from_slice(PROFILE.as_bytes());
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
    if !task.config.commands.is_empty()
        || !task.config.mcp.is_empty()
        || matches!(
            task.intent.call,
            ToolCall::Command { .. } | ToolCall::McpList { .. } | ToolCall::McpCall { .. }
        )
    {
        return Err(ToolError::ProtectionUnavailable);
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
    if profile.len() > 96 * 1024 {
        return Err(ToolError::Limit);
    }
    let profile = CString::new(profile).map_err(|_| ToolError::Path)?;
    let mut error = std::ptr::null_mut();
    // The profile is NUL-terminated and lives through the call; Apple owns the error allocation.
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
    for operation in [c"process-fork", c"network-outbound"] {
        // These operations have no filter argument; the PID is the calling process.
        if unsafe { sandbox_check(std::process::id() as i32, operation.as_ptr(), 0) } != 1 {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    for syscall in [2_i32, 46, 48, 59, 66, 111, 184, 195, 244, 329, 330, 331, 380, 410, 422] {
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
    // The parent observes its unreaped owned child; a full native write initializes every field.
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
            // The unreaped PID cannot be reused; exact output proves whether kernel exit has begun.
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
    mut cancellation: RunCancellation,
    dispatched: &mut bool,
) -> Result<ToolObservation, ToolError> {
    crate::tools::fs::validate_workspace(runtime)?;
    if matches!(
        intent.call,
        ToolCall::Command { .. } | ToolCall::McpList { .. } | ToolCall::McpCall { .. }
    ) {
        return Err(ToolError::ProtectionUnavailable);
    }
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
        .current_dir("/")
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
        *dispatched = true;
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
    let mut live = true;
    let result = loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => break Err(ToolError::Operation),
            result = &mut execution => break result.unwrap_or(Err(ToolError::Limit)),
            exited = child.wait(), if status.is_none() => {
                status = Some(exited.map_err(|_| ToolError::Operation));
            }
            _ = samples.tick(), if status.is_none() && live => {
                match observe(pid) {
                    Ok(active) => live = active,
                    Err(error) => match child.try_wait() {
                        Ok(Some(exited)) => status = Some(Ok(exited)),
                        _ => break Err(error),
                    },
                }
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

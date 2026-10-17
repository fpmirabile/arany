use super::{
    CAPTURE_BYTES, GuardStage, RUNTIME_ROOT, Task, bootstrap_failure, capture, drain_stderr,
    encode_task, parse_ready, profile, read_capped, read_line_capped, read_observation,
};
use crate::tools::Cancellation;
use crate::tools::types::{
    EffectIntent, MAX_BUILD_PROCESSES_OR_THREADS, MAX_CHANGESET_BYTES, MAX_CHANGESET_FILES,
    ToolCall, ToolLimits, ToolObservation, ToolResourceClass, digest,
};
use crate::tools::{ToolError, ToolRuntime};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Native preparation and handshake time the Engine adds to a Guard call's action deadline.
pub(in crate::tools) const RESERVATION_OVERHEAD: Duration = Duration::from_secs(5);
const MANAGER_DEADLINE: Duration = Duration::from_secs(4);
const MOUNTINFO_BYTES_CAP: usize = 1024 * 1024;
const MOUNTINFO_ENTRIES_CAP: usize = 4096;
const MOUNTINFO_LINE_CAP: usize = 16 * 1024;
const HARDENED_THREADS_CAP: usize = 64;
const STATUS_BYTES_CAP: usize = 16 * 1024;
const BWRAP_BOOTSTRAP: &[&str] = &[
    "/usr/bin/env",
    "-i",
    "PATH=/usr/bin:/bin",
    // No --keep-caps: retained capabilities make Bubblewrap reject a non-root real UID.
    "/usr/bin/unshare",
    "--user",
    "--map-current-user",
    "--mount",
    "--propagation",
    "private",
    "--",
    "/usr/bin/bwrap",
];

pub(super) fn check_available() -> Result<(), ToolError> {
    profile::programs()?;
    use std::os::unix::fs::MetadataExt;
    for path in [
        "/usr/bin/bwrap",
        "/usr/bin/systemd-run",
        "/usr/bin/systemctl",
        "/usr/bin/env",
        "/usr/bin/unshare",
    ] {
        let metadata =
            std::fs::symlink_metadata(path).map_err(|_| ToolError::ProtectionUnavailable)?;
        if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o6022 != 0 {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    Ok(())
}

pub(super) fn enforcement_digest() -> Result<[u8; 32], ToolError> {
    let mut bytes = b"arany-guard-v15:shared-fifo-capacity;owned-host-seed;retained-reservation;no-buffered-go-before-ready;pinned-skill-digest-pre-go;fixed-root-owned-unshare-bootstrap-user-map-current-recursive-private-no-retained-caps;private-mount,user,pid,net,ipc,uts;no-userns;no-new-privs;zero-caps;tsync-seccomp-all-threads;per-thread-nnp-zero-caps-attested;not-dumpable;cpu25000/100000;memory536870912;swap0;oom-group1;pids64;scratch67108864;workspace67108864;nofile64;core0;no-inherited-host-handles;typed-native-or-fresh-snapshot;namespace-descendant-stop-kill-positive-absence-before-command-changeset;strict-bounded-no-follow-changeset;owned-cgroup-cleanup;runtime-root-/usr-excludes-workspace-state-account;versioned-exact-ro-runtime-cache-mounts;cache-volume-at-cache-limit-sized-tmpfs;toolchains-at-exact-host-paths-rejecting-profile-overlap;bounded-namespace-mountinfo-private-root-and-unique-exact-mounts-no-descendants-before-ready-and-after-go;typed-git-read-pinned-usr-git-after-go-read-only-workspace-scratch-repository-view-no-repository-config-closed-env-descendants-stopped".to_vec();
    bytes.extend_from_slice(&profile::profile_digest()?);
    bytes.extend_from_slice(&(MAX_CHANGESET_FILES as u64).to_le_bytes());
    bytes.extend_from_slice(&(MAX_CHANGESET_BYTES as u64).to_le_bytes());
    bytes.extend_from_slice(&(HARDENED_THREADS_CAP as u64).to_le_bytes());
    bytes.extend_from_slice(&(STATUS_BYTES_CAP as u64).to_le_bytes());
    Ok(digest(&bytes))
}

pub(super) async fn stop_command_descendants(
    parent: &profile::Namespaces,
) -> Result<(), ToolError> {
    use rustix::io::Errno;
    use rustix::process::{Pid, Signal, kill_process_group};

    let own = std::process::id();
    if !matches!(own, 1 | 2) || !profile::Namespaces::read()?.isolated_from(parent) {
        return Err(ToolError::ProtectionUnavailable);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        // Pid::INIT selects kill(-1), excluding this helper and the trusted namespace init.
        for signal in [Signal::STOP, Signal::KILL] {
            match kill_process_group(Pid::INIT, signal) {
                Ok(()) | Err(Errno::SRCH) => {}
                Err(_) => return Err(ToolError::ProtectionUnavailable),
            }
        }
        let mut count = 0usize;
        let mut live = false;
        for (index, entry) in std::fs::read_dir("/proc")
            .map_err(|_| ToolError::ProtectionUnavailable)?
            .enumerate()
        {
            if index >= 512 {
                return Err(ToolError::Limit);
            }
            let entry = entry.map_err(|_| ToolError::ProtectionUnavailable)?;
            let name = entry.file_name();
            let name = name.to_str().ok_or(ToolError::ProtectionUnavailable)?;
            if !name.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            let pid = name
                .parse::<u32>()
                .map_err(|_| ToolError::ProtectionUnavailable)?;
            count += 1;
            if count > MAX_BUILD_PROCESSES_OR_THREADS as usize + 2 {
                return Err(ToolError::Limit);
            }
            live |= pid != 1 && pid != own;
        }
        if !live {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ToolError::ProtectionUnavailable);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Drops capabilities, installs the syscall profile and verifies every thread before Ready.
pub(super) fn harden() -> Result<(), ToolError> {
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    use rustix::process::{Resource, Rlimit, setrlimit};
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(64),
            maximum: Some(64),
        },
    )
    .map_err(|_| ToolError::ProtectionUnavailable)?;
    profile::install()?;
    setrlimit(
        Resource::Core,
        Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )
    .map_err(|_| ToolError::ProtectionUnavailable)?;
    let mut threads = 0usize;
    for entry in
        std::fs::read_dir("/proc/self/task").map_err(|_| ToolError::ProtectionUnavailable)?
    {
        threads += 1;
        if threads > HARDENED_THREADS_CAP {
            return Err(ToolError::ProtectionUnavailable);
        }
        let path = entry
            .map_err(|_| ToolError::ProtectionUnavailable)?
            .path()
            .join("status");
        let mut status = String::new();
        std::fs::File::open(path)
            .and_then(|file| {
                file.take(STATUS_BYTES_CAP as u64 + 1)
                    .read_to_string(&mut status)
            })
            .map_err(|_| ToolError::ProtectionUnavailable)?;
        if status.len() > STATUS_BYTES_CAP {
            return Err(ToolError::ProtectionUnavailable);
        }
        verify_hardened_status(&status)?;
    }
    if threads == 0 {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

fn verify_hardened_status(status: &str) -> Result<(), ToolError> {
    let field = |name: &str| {
        status.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key == name).then_some(value.trim())
        })
    };
    if field("NoNewPrivs") != Some("1")
        || field("CapEff") != Some("0000000000000000")
        || field("CapPrm") != Some("0000000000000000")
        || field("CapInh") != Some("0000000000000000")
        || field("CapAmb") != Some("0000000000000000")
        || field("Seccomp") != Some("2")
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

fn manager_command(executable: &str) -> Command {
    let mut command = Command::new(executable);
    command.env_clear().env("PATH", "/usr/bin:/bin");
    let runtime = format!("/run/user/{}", rustix::process::geteuid().as_raw());
    command.env("XDG_RUNTIME_DIR", &runtime).env(
        "DBUS_SESSION_BUS_ADDRESS",
        format!("unix:path={runtime}/bus"),
    );
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

struct Unit {
    name: String,
    invocation: Option<String>,
    group: Option<cap_std::fs::Dir>,
    kill: Option<cap_std::fs::File>,
    limits: Option<ToolLimits>,
}

impl Unit {
    async fn properties(&self) -> Result<serde_json::Value, ToolError> {
        let mut command = manager_command("/usr/bin/systemctl");
        command.args(["--user", "--no-ask-password", "show", &self.name,
            "--property=Id,LoadState,InvocationID,ControlGroup,MainPID,ActiveState,SubState,ExecMainCode,ExecMainStatus"]);
        let (status, stdout, _) = capture(
            command
                .spawn()
                .map_err(|_| ToolError::ProtectionUnavailable)?,
            4096,
            MANAGER_DEADLINE,
        )
        .await?;
        if !status.success() {
            return Err(ToolError::ProtectionUnavailable);
        }
        let text = std::str::from_utf8(&stdout).map_err(|_| ToolError::ProtectionUnavailable)?;
        let mut values = serde_json::Map::new();
        for line in text.lines() {
            let (key, value) = line
                .split_once('=')
                .ok_or(ToolError::ProtectionUnavailable)?;
            if values
                .insert(key.to_owned(), serde_json::Value::String(value.to_owned()))
                .is_some()
            {
                return Err(ToolError::ProtectionUnavailable);
            }
        }
        Ok(serde_json::Value::Object(values))
    }

    async fn admit(&mut self) -> Result<(), ToolError> {
        let limits = self
            .limits
            .as_ref()
            .ok_or(ToolError::ProtectionUnavailable)?;
        let memory = limits.memory_bytes.to_string();
        let processes = limits.max_processes.to_string();
        let cpu = if limits.resource_class == ToolResourceClass::Build {
            "100000 100000"
        } else {
            "25000 100000"
        };
        let values = self.properties().await?;
        let group = values["ControlGroup"]
            .as_str()
            .ok_or(ToolError::ProtectionUnavailable)?;
        let invocation = values["InvocationID"]
            .as_str()
            .filter(|value| value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or(ToolError::ProtectionUnavailable)?;
        if values["Id"].as_str() != Some(&self.name)
            || values["ActiveState"] != "active"
            || !group.ends_with(&self.name)
            || !group.starts_with("/user.slice/")
            || group.contains("..")
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let dir = crate::tools::fs::open_directory(
            &Path::new("/sys/fs/cgroup").join(group.trim_start_matches('/')),
        )?;
        self.invocation = Some(invocation.to_owned());
        self.group = Some(
            dir.try_clone()
                .map_err(|_| ToolError::ProtectionUnavailable)?,
        );
        let mut options = cap_std::fs::OpenOptions::new();
        options.write(true);
        self.kill = Some(
            dir.open_with("cgroup.kill", &options)
                .map_err(|_| ToolError::ProtectionUnavailable)?,
        );
        for (name, expected) in [
            ("memory.max", memory),
            ("memory.swap.max", "0".into()),
            ("memory.oom.group", "1".into()),
            ("pids.max", processes.clone()),
            ("cpu.max", cpu.into()),
        ] {
            let mut file = dir
                .open(name)
                .map_err(|_| ToolError::ProtectionUnavailable)?;
            let mut actual = String::new();
            Read::by_ref(&mut file)
                .take(256)
                .read_to_string(&mut actual)
                .map_err(|_| ToolError::ProtectionUnavailable)?;
            if actual.trim() != expected {
                return Err(ToolError::ProtectionUnavailable);
            }
        }
        let main_pid = values["MainPID"]
            .as_str()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|pid| *pid > 0)
            .ok_or(ToolError::ProtectionUnavailable)?;
        let mut members = String::new();
        dir.open("cgroup.procs")
            .map_err(|_| ToolError::ProtectionUnavailable)?
            .take(4096)
            .read_to_string(&mut members)
            .map_err(|_| ToolError::ProtectionUnavailable)?;
        if members.lines().count() > limits.max_processes as usize
            || !members
                .lines()
                .any(|line| line.parse::<u32>() == Ok(main_pid))
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        Ok(())
    }

    async fn stop(&self) -> Result<(), ToolError> {
        let values = match self.properties().await {
            Ok(values) => values,
            Err(_) if self.empty()? => return Ok(()),
            Err(error) => return Err(error),
        };
        if values["ActiveState"] == "inactive" && self.empty()? {
            return Ok(());
        }
        if !matches!(values["ActiveState"].as_str(), Some("inactive" | "failed"))
            && self
                .invocation
                .as_ref()
                .is_some_and(|id| values["InvocationID"].as_str() != Some(id))
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let mut command = manager_command("/usr/bin/systemctl");
        command.args(["--user", "--no-ask-password", "stop", &self.name]);
        let (status, _, _) = capture(
            command.spawn().map_err(|_| ToolError::Operation)?,
            4096,
            MANAGER_DEADLINE,
        )
        .await?;
        if !status.success() {
            if self.empty()? {
                return Ok(());
            }
            return Err(ToolError::Operation);
        }
        if self.group.is_some() && !self.empty()? {
            return Err(ToolError::Operation);
        }
        let values = match self.properties().await {
            Ok(values) => values,
            Err(_) if self.empty()? => return Ok(()),
            Err(error) => return Err(error),
        };
        if !matches!(values["ActiveState"].as_str(), Some("inactive" | "failed")) {
            return Err(ToolError::Operation);
        }
        if values["ActiveState"] == "failed" {
            let mut command = manager_command("/usr/bin/systemctl");
            command.args(["--user", "--no-ask-password", "reset-failed", &self.name]);
            let (status, _, _) = capture(
                command.spawn().map_err(|_| ToolError::Operation)?,
                4096,
                MANAGER_DEADLINE,
            )
            .await?;
            if !status.success() {
                return Err(ToolError::Operation);
            }
        }
        Ok(())
    }

    fn empty(&self) -> Result<bool, ToolError> {
        if let Some(group) = &self.group {
            let mut actual = String::new();
            match group.open("cgroup.events") {
                Ok(mut file) => {
                    Read::by_ref(&mut file)
                        .take(1024)
                        .read_to_string(&mut actual)
                        .map_err(|_| ToolError::Operation)?;
                    if !actual.lines().any(|line| line == "populated 0") {
                        return Ok(false);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(ToolError::Operation),
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn kill_group(&self) {
        if let Some(kill) = &self.kill {
            let mut file = kill;
            let _ = file.write_all(b"1");
            return;
        }
        if let Some(group) = &self.group {
            let mut options = cap_std::fs::OpenOptions::new();
            options.write(true);
            if let Ok(mut file) = group.open_with("cgroup.kill", &options) {
                let _ = file.write_all(b"1");
            }
        }
    }
}

impl Drop for Unit {
    fn drop(&mut self) {
        self.kill_group();
    }
}

pub(super) async fn recover_reservation(id: uuid::Uuid) -> Result<bool, ToolError> {
    let mut unit = Unit {
        name: format!("arany-tool-{id}.service"),
        invocation: None,
        group: None,
        kill: None,
        limits: None,
    };
    let values = unit.properties().await?;
    // No loaded unit cannot settle a controller interrupted during its launch request.
    if values["Id"].as_str() != Some(&unit.name) || values["LoadState"] != "loaded" {
        return Ok(false);
    }
    if matches!(values["ActiveState"].as_str(), Some("inactive" | "failed"))
        && values["MainPID"] == "0"
        && values["ControlGroup"] == ""
    {
        unit.stop().await?;
        return Ok(true);
    }
    let group = values["ControlGroup"]
        .as_str()
        .ok_or(ToolError::ProtectionUnavailable)?;
    if !group.starts_with("/user.slice/") || !group.ends_with(&unit.name) || group.contains("..") {
        return Err(ToolError::ProtectionUnavailable);
    }
    unit.group = Some(crate::tools::fs::open_directory(
        &Path::new("/sys/fs/cgroup").join(group.trim_start_matches('/')),
    )?);
    unit.invocation = values["InvocationID"].as_str().map(str::to_owned);
    unit.stop().await?;
    unit.empty()
}

#[cfg(test)]
pub(super) fn stop_seed_fixture(id: uuid::Uuid) {
    use crate::test_process::BoundedOutput;
    let checked = std::panic::catch_unwind(|| {
        let name = format!("arany-tool-{id}.service");
        let mut stop = manager_command("/usr/bin/systemctl");
        stop.args(["--user", "--no-ask-password", "stop", &name]);
        let _ = stop
            .into_std()
            .bounded_output_for(MANAGER_DEADLINE, 4096)
            .unwrap();
        let mut show = manager_command("/usr/bin/systemctl");
        show.args([
            "--user",
            "show",
            &name,
            "--property=ActiveState,MainPID,ControlGroup",
        ]);
        let output = show
            .into_std()
            .bounded_output_for(MANAGER_DEADLINE, 4096)
            .unwrap();
        let text = std::str::from_utf8(&output.stdout).unwrap();
        assert!(
            text.lines()
                .any(|line| line == "ActiveState=inactive" || line == "ActiveState=failed")
        );
        assert!(text.lines().any(|line| line == "MainPID=0"));
        assert!(text.lines().any(|line| line == "ControlGroup="));
    });
    if let Err(error) = checked {
        if std::thread::panicking() {
            eprintln!("native seed fixture cleanup could not be verified: {id}");
        } else {
            std::panic::resume_unwind(error);
        }
    }
}

#[cfg(test)]
pub(super) async fn seed_unit_group(id: uuid::Uuid) -> cap_std::fs::Dir {
    let unit = Unit {
        name: format!("arany-tool-{id}.service"),
        invocation: None,
        group: None,
        kill: None,
        limits: None,
    };
    let values = unit.properties().await.unwrap();
    assert_eq!(values["ActiveState"], "active");
    let group = values["ControlGroup"].as_str().unwrap();
    assert!(group.starts_with("/user.slice/") && group.ends_with(&unit.name));
    let dir = crate::tools::fs::open_directory(
        &Path::new("/sys/fs/cgroup").join(group.trim_start_matches('/')),
    )
    .unwrap();
    assert!(
        dir.read_to_string("cgroup.events")
            .unwrap()
            .lines()
            .any(|line| line == "populated 1")
    );
    dir
}

#[cfg(test)]
pub(super) async fn hold_seed_unit(permit: &super::admission::Permit, seed: &Path) {
    let name = format!("arany-tool-{}.service", permit.id);
    let mut command = manager_command("/usr/bin/systemd-run");
    command
        .args([
            "--user",
            "--no-ask-password",
            "--quiet",
            "--wait",
            "--pipe",
            "--service-type=exec",
            "--unit",
            &name,
            "--property=Restart=no",
            "--property=KillMode=control-group",
            "--property=TimeoutStopSec=2s",
            "--property=RuntimeMaxSec=60s",
        ])
        .args(BWRAP_BOOTSTRAP)
        .args([
            "--unshare-all",
            "--unshare-user",
            "--die-with-parent",
            "--ro-bind",
            "/usr",
            "/usr",
            "--symlink",
            "usr/lib",
            "/lib",
            "--symlink",
            // /lib and /lib64 keep their own targets: mapping both to /usr/lib hides the x86-64 ELF loader.
            "usr/lib64",
            "/lib64",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--ro-bind",
        ])
        .arg(seed)
        .args([
            "/seed",
            "/usr/bin/sh",
            "-c",
            "test -r /seed/workspace/sample && printf 'READY\\n' && exec /usr/bin/sleep 600",
        ]);
    permit.launching().unwrap();
    let mut child = command.spawn().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let ready = tokio::time::timeout(Duration::from_secs(10), read_line_capped(&mut stdout, 64))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ready, b"READY\n");
    permit.seed_checkpoint("native");
    panic!("native seed controller must be killed by its parent");
}

fn unit_command(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    unit_name: &str,
    runtime_path: &str,
    (seed_path, seed_dir): (&Path, &cap_std::fs::Dir),
    (runtime_directories, mounts): RuntimeMounts,
) -> Command {
    let memory_max = format!("--property=MemoryMax={}", intent.limits.memory_bytes);
    let tasks_max = format!("--property=TasksMax={}", intent.limits.max_processes);
    let cpu_quota = if intent.limits.resource_class == ToolResourceClass::Build {
        "--property=CPUQuota=100%"
    } else {
        "--property=CPUQuota=25%"
    };
    let mut command = manager_command("/usr/bin/systemd-run");
    command.args([
        "--user",
        "--no-ask-password",
        "--quiet",
        "--description=Arany bounded Tool worker",
        "--pipe",
        "--wait",
        "--expand-environment=no",
        "--service-type=exec",
        "--unit",
        unit_name,
        "--property=Restart=no",
        "--property=KillMode=control-group",
        "--property=SendSIGKILL=yes",
        "--property=FinalKillSignal=SIGKILL",
        "--property=TimeoutStartSec=5s",
        "--property=TimeoutStopSec=2s",
        "--property=RuntimeRandomizedExtraSec=0",
        "--property=RemainAfterExit=no",
        &memory_max,
        "--property=MemorySwapMax=0",
        "--property=OOMPolicy=kill",
        &tasks_max,
        cpu_quota,
        "--property=CPUQuotaPeriodSec=100ms",
    ]);
    let service_runtime_ms = 10_000_u64
        .saturating_add(u64::from(intent.limits.cache_transfer_ms).saturating_mul(2))
        .saturating_add(u64::from(intent.limits.runtime_ms));
    command
        .arg(format!("--property=RuntimeMaxSec={}ms", service_runtime_ms))
        .args(BWRAP_BOOTSTRAP);
    let native = matches!(
        intent.call,
        ToolCall::Write { .. }
            | ToolCall::Edit { .. }
            | ToolCall::Mkdir { .. }
            | ToolCall::ApplyChangeset { .. }
    ) || intent.call.is_git_read();
    let folders = ["programs", "inputs", "skills"]
        .into_iter()
        .filter(|folder| seed_dir.symlink_metadata(folder).is_ok())
        .collect::<Vec<_>>();
    let workspace_mount = native.then(|| (runtime.workspace.as_path(), intent.call.mutates()));
    profile_arguments(
        &mut command,
        runtime_path,
        seed_path,
        &folders,
        workspace_mount,
        &intent.limits,
        &runtime.guard_executable,
    );
    for directory in runtime_directories {
        command.args(["--dir"]).arg(directory);
    }
    for (source, target) in mounts {
        command.args(["--ro-bind"]).arg(source).arg(target);
    }
    command.args(["--chdir", "/", "/arany", "--internal-tool-guard"]);
    command.stdin(Stdio::piped());
    command
}

async fn settle_launch(
    unit: &mut Unit,
    permit: &super::admission::Permit,
    child: &mut tokio::process::Child,
    stderr_task: &mut tokio::task::JoinHandle<Result<Vec<u8>, ToolError>>,
    deadline: tokio::time::Instant,
    result: Result<(ToolObservation, super::CacheCandidate), ToolError>,
    cache: &mut Option<super::BuildCacheSession>,
) -> Result<ToolObservation, ToolError> {
    let cleanup = tokio::time::timeout_at(deadline, unit.stop()).await;
    if !matches!(cleanup, Ok(Ok(()))) {
        unit.kill_group();
    }
    let _ = child.start_kill();
    let reaped = tokio::time::timeout_at(deadline, child.wait()).await;
    let drained = match tokio::time::timeout_at(deadline, drain_stderr(stderr_task)).await {
        Ok(drained) => drained,
        Err(_) => {
            stderr_task.abort();
            let _ = stderr_task.await;
            None
        }
    };
    if matches!(cleanup, Ok(Ok(()))) {
        permit
            .terminated()
            .map_err(|_| ToolError::GuardRejected(GuardStage::StorageCleanup))?;
        tokio::time::timeout_at(deadline, permit.cleanup())
            .await
            .map_err(|_| ToolError::GuardRejected(GuardStage::StorageCleanup))?
            .map_err(|_| ToolError::GuardRejected(GuardStage::StorageCleanup))?;
    }
    if result.is_err()
        && let Some(failure) = drained.as_deref().and_then(bootstrap_failure)
    {
        return Err(failure);
    }
    if !matches!(cleanup, Ok(Ok(()))) {
        return Err(ToolError::GuardRejected(GuardStage::ProcessCleanup));
    }
    if !matches!(reaped, Ok(Ok(_))) {
        return Err(ToolError::GuardRejected(GuardStage::LauncherReap));
    }
    let Some(stderr) = drained else {
        return Err(ToolError::GuardRejected(GuardStage::OutputEof));
    };
    if result.is_err() {
        let diagnostic = std::str::from_utf8(&stderr).unwrap_or_default();
        let stage = if diagnostic.contains("bwrap:") || diagnostic.contains("unshare:") {
            GuardStage::NamespaceBootstrap
        } else if diagnostic.contains("Failed to") {
            GuardStage::NativeManager
        } else {
            GuardStage::ControlProtocol
        };
        return Err(ToolError::GuardRejected(stage));
    }
    match result {
        Ok((mut observation, candidate)) => {
            super::publish_cache_candidate(&mut observation, candidate, cache.take(), deadline)
                .await;
            Ok(observation)
        }
        Err(error) => Err(error),
    }
}

fn action_deadlines(
    active_window: Option<crate::tools::ActiveToolWindow>,
    intent: &EffectIntent,
    capacity_started: tokio::time::Instant,
) -> (
    tokio::time::Instant,
    tokio::time::Instant,
    tokio::time::Instant,
) {
    let cache_transfer = Duration::from_millis(u64::from(intent.limits.cache_transfer_ms));
    let preparation_deadline = active_window
        .map_or(capacity_started + Duration::from_secs(5), |window| {
            window.preparation_deadline
        });
    let go_deadline = active_window.map_or(preparation_deadline + cache_transfer, |window| {
        window.go_deadline
    });
    let completion = active_window.map_or_else(
        || {
            capacity_started
                + Duration::from_secs(15)
                + cache_transfer.saturating_mul(2)
                + Duration::from_millis(u64::from(intent.limits.runtime_ms))
        },
        |window| window.completion_deadline,
    );
    (preparation_deadline, go_deadline, completion)
}

fn verify_ready(
    ready_bytes: &[u8],
    intent: &EffectIntent,
    parent_namespaces: &profile::Namespaces,
) -> Result<(), ToolError> {
    let ready = parse_ready(ready_bytes, intent)?;
    if ready.pid != 1 && ready.pid != 2 {
        return Err(ToolError::ProtectionUnavailable);
    }
    if !ready.namespaces.isolated_from(parent_namespaces) {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(())
}

pub(super) async fn execute_inner(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    mut cancellation: Cancellation,
    dispatched: &mut bool,
) -> Result<ToolObservation, ToolError> {
    let permit = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(ToolError::Operation),
        permit = super::admission::acquire(intent.id, intent.expires_at_ms, |queued| runtime.queue_changed(intent.id, queued)) => permit?,
    };
    let capacity_started = tokio::time::Instant::now();
    let active_window = runtime.begin_active_action(capacity_started).await?;
    let (preparation_deadline, go_deadline, deadline) =
        action_deadlines(active_window, intent, capacity_started);
    crate::tools::fs::validate_workspace(runtime)?;
    let runtime_grants = if intent.call.is_process() {
        runtime.config.runtime_grants.as_slice()
    } else {
        &[]
    };
    let (runtime_directories, mounts) =
        runtime_mounts(runtime_grants, intent.limits.cache_bytes > 0)?;
    let runtime_path = super::runtime_path(runtime_grants)?;
    let workspace = crate::tools::fs::open_directory(&runtime.workspace)?;
    let mut cache = super::build_cache_session(runtime, intent)?;
    let cache_transfer = cache.as_ref().map(|session| session.transfer.clone());
    let (seed_path, seed_dir) =
        permit.prepare_seed(&workspace, &runtime.config, intent.call.is_process())?;
    let mut unit = Unit {
        name: format!("arany-tool-{}.service", permit.id),
        invocation: None,
        group: None,
        kill: None,
        limits: Some(intent.limits.clone()),
    };
    let mut command = unit_command(
        runtime,
        intent,
        &unit.name,
        &runtime_path,
        (&seed_path, &seed_dir),
        (runtime_directories, mounts),
    );
    let parent_namespaces = profile::Namespaces::read()?;
    permit.launching()?;
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            permit.terminated()?;
            permit.cleanup().await?;
            return Err(ToolError::ProtectionUnavailable);
        }
    };
    let mut stdin = child.stdin.take().ok_or(ToolError::Operation)?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or(ToolError::Operation)?);
    let stderr = child.stderr.take().ok_or(ToolError::Operation)?;
    let mut stderr_task = tokio::spawn(read_capped(stderr, CAPTURE_BYTES));
    let execution = async {
        let task = encode_task(&Task {
            config: runtime.config.clone(),
            intent: intent.clone(),
            cache: cache_transfer,
            parent_namespaces: parent_namespaces.clone(),
        })?;
        let mut ready_bytes = tokio::time::timeout_at(preparation_deadline, async {
            stdin
                .write_all(&task)
                .await
                .map_err(|_| ToolError::Operation)?;
            read_line_capped(&mut stdout, 2048).await
        })
        .await
        .map_err(|_| ToolError::Limit)??;
        if ready_bytes == b"CACHE_READY\n" {
            let session = cache.as_ref().ok_or(ToolError::Operation)?;
            let transfer_deadline = super::cache_transfer_deadline(intent, Some(go_deadline));
            tokio::time::timeout_at(
                transfer_deadline,
                super::send_cache_archive(&mut stdin, &session.lease),
            )
            .await
            .map_err(|_| ToolError::Limit)??;
            ready_bytes = tokio::time::timeout_at(go_deadline, read_line_capped(&mut stdout, 2048))
                .await
                .map_err(|_| ToolError::Limit)??;
        }
        verify_ready(&ready_bytes, intent, &parent_namespaces)?;
        unit.admit()
            .await
            .map_err(|_| ToolError::GuardRejected(GuardStage::ResourceAttestation))?;
        runtime.before_dispatch(active_window, intent.limits.runtime_ms)?;
        *dispatched = true;
        stdin
            .write_all(b"GO\n")
            .await
            .map_err(|_| ToolError::Operation)?;
        stdin.shutdown().await.map_err(|_| ToolError::Operation)?;
        let (observation, candidate) = if let Some(session) = cache.as_ref() {
            let observation = super::read_observation_line(&mut stdout, intent).await?;
            let update_deadline = super::cache_transfer_deadline(
                intent,
                active_window.map(|window| window.completion_deadline),
            );
            let candidate = match tokio::time::timeout_at(
                update_deadline,
                super::receive_cache_candidate(&mut stdout, &session.lease),
            )
            .await
            {
                Ok(Ok(candidate)) => candidate,
                Ok(Err(_)) | Err(_) => super::CacheCandidate::Uncertain,
            };
            if candidate == super::CacheCandidate::Uncertain {
                return Ok((observation, candidate));
            }
            (observation, candidate)
        } else {
            (
                read_observation(&mut stdout, intent).await?,
                super::CacheCandidate::Received,
            )
        };
        if !child
            .wait()
            .await
            .map_err(|_| ToolError::Operation)?
            .success()
        {
            return Err(ToolError::Operation);
        }
        if cache.is_some() && !read_capped(&mut stdout, 1).await?.is_empty() {
            return Err(ToolError::Operation);
        }
        Ok((observation, candidate))
    };
    let result = tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(ToolError::Operation),
        result = tokio::time::timeout_at(deadline, execution) => result.unwrap_or(Err(ToolError::Limit)),
    };
    settle_launch(
        &mut unit,
        &permit,
        &mut child,
        &mut stderr_task,
        deadline,
        result,
        &mut cache,
    )
    .await
}

/// Destinations that `profile_arguments` mounts or creates; a toolchain may not overlap them.
const PROFILE_MOUNT_TARGETS: [&str; 13] = [
    RUNTIME_ROOT,
    "/lib",
    "/lib64",
    "/proc",
    "/dev",
    "/scratch",
    "/cache",
    "/workspace",
    "/seed",
    "/programs",
    "/inputs",
    "/skills",
    "/arany",
];

fn reject_profile_overlap(target: &Path) -> Result<(), ToolError> {
    if PROFILE_MOUNT_TARGETS
        .iter()
        .map(Path::new)
        .any(|fixed| target.starts_with(fixed) || fixed.starts_with(target))
    {
        return Err(ToolError::Configuration);
    }
    Ok(())
}

/// Ancestors of a mount target below `root`, shallowest first, each created as a bwrap directory.
fn mount_directories(target: &Path, root: &Path) -> Vec<PathBuf> {
    let mut directories = target
        .ancestors()
        .take_while(|path| *path != root)
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    directories.sort_by_key(|path| (path.components().count(), path.clone()));
    directories
}

/// Directories to create, then read-only `(source, target)` binds.
type RuntimeMounts = (Vec<PathBuf>, Vec<(PathBuf, PathBuf)>);

/// Verifies every grant before any seed exists; a rejected grant leaves no state behind.
fn runtime_mounts(
    grants: &[super::super::toolchains::RuntimeGrant],
    cache_enabled: bool,
) -> Result<RuntimeMounts, ToolError> {
    let mut directories = Vec::new();
    let mut mounts = Vec::new();
    for grant in grants {
        grant.verify_host()?;
        let target = grant.mounted_path(cache_enabled)?;
        let root = if grant.kind.is_toolchain() {
            reject_profile_overlap(&target)?;
            Path::new("/")
        } else {
            Path::new(super::super::toolchains::sandbox_cache_root(cache_enabled))
        };
        directories.extend(mount_directories(&target, root));
        mounts.push((grant.path.clone(), target));
    }
    directories.sort_by_key(|path| (path.components().count(), path.clone()));
    directories.dedup();
    Ok((directories, mounts))
}

/// The fixed bwrap profile; every destination it mounts must lie under `PROFILE_MOUNT_TARGETS`.
fn profile_arguments(
    command: &mut Command,
    runtime_path: &str,
    seed_path: &Path,
    folders: &[&str],
    workspace: Option<(&Path, bool)>,
    limits: &ToolLimits,
    guard: &Path,
) {
    command.args([
        "--unshare-all",
        "--unshare-user",
        "--disable-userns",
        "--assert-userns-disabled",
        "--new-session",
        "--die-with-parent",
        "--cap-drop",
        "ALL",
        "--clearenv",
        "--setenv",
        "PATH",
        runtime_path,
        "--setenv",
        "HOME",
        "/scratch",
        "--setenv",
        "TMPDIR",
        "/scratch",
        "--setenv",
        "CARGO_TARGET_DIR",
        if limits.cache_bytes > 0 {
            "/cache/target"
        } else {
            "/scratch/target"
        },
        "--ro-bind",
        RUNTIME_ROOT,
        RUNTIME_ROOT,
        "--symlink",
        "usr/lib",
        "/lib",
        "--symlink",
        "usr/lib64",
        "/lib64",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
    ]);
    command
        .args(["--size"])
        .arg(limits.scratch_bytes.to_string())
        .args(["--tmpfs", "/scratch"]);
    if limits.cache_bytes > 0 {
        command
            .args(["--size"])
            .arg(limits.cache_bytes.to_string())
            .args(["--tmpfs", "/cache"]);
    }
    command.args(["--ro-bind"]).arg(seed_path).arg("/seed");
    for folder in folders {
        command
            .args(["--ro-bind"])
            .arg(seed_path.join(folder))
            .arg(format!("/{folder}"));
    }
    match workspace {
        Some((path, true)) => {
            command.args(["--bind"]).arg(path).arg("/workspace");
        }
        Some((path, false)) => {
            command.args(["--ro-bind"]).arg(path).arg("/workspace");
        }
        None => {
            command
                .args(["--size"])
                .arg(limits.project_bytes.to_string())
                .args(["--tmpfs", "/workspace"]);
        }
    }
    command.args(["--ro-bind"]).arg(guard).arg("/arany");
}

pub(super) fn verify_runtime_mounts(
    grants: &[super::super::toolchains::RuntimeGrant],
    cache_enabled: bool,
) -> Result<(), ToolError> {
    use std::io::Read;

    let mut bytes = Vec::new();
    std::fs::File::open("/proc/self/mountinfo")
        .map_err(|_| ToolError::ProtectionUnavailable)?
        .take((MOUNTINFO_BYTES_CAP + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    if bytes.len() > MOUNTINFO_BYTES_CAP {
        return Err(ToolError::Limit);
    }
    let mounts = parse_mountinfo(&bytes)?;
    let targets = grants
        .iter()
        .map(|grant| grant.mounted_path(cache_enabled))
        .collect::<Result<Vec<_>, _>>()?;
    verify_mount_entries(&mounts, &targets)
}

fn verify_mount_entries(
    mounts: &[MountInfoEntry],
    targets: &[std::path::PathBuf],
) -> Result<(), ToolError> {
    let root = Path::new("/");
    if mounts
        .iter()
        .filter(|mount| mount.path.as_path() == root)
        .count()
        != 1
        || mounts
            .iter()
            .find(|mount| mount.path.as_path() == root)
            .is_none_or(|mount| mount.optional_fields != 0)
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    let mut unique_targets = std::collections::BTreeSet::new();
    for target in targets {
        if !unique_targets.insert(target) {
            return Err(ToolError::ProtectionUnavailable);
        }
        if mounts
            .iter()
            .filter(|mount| mount.path.as_path() == target.as_path())
            .count()
            != 1
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let selected = mounts
            .iter()
            .find(|mount| mount.path.as_path() == target.as_path())
            .ok_or(ToolError::ProtectionUnavailable)?;
        if selected.optional_fields != 0
            || mounts.iter().any(|mount| {
                mount.path.as_path() != target.as_path() && mount.path.starts_with(target.as_path())
            })
        {
            return Err(ToolError::ProtectionUnavailable);
        }
    }
    Ok(())
}

struct MountInfoEntry {
    path: std::path::PathBuf,
    optional_fields: usize,
}

fn parse_mountinfo(bytes: &[u8]) -> Result<Vec<MountInfoEntry>, ToolError> {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    if bytes.is_empty() || bytes.len() > MOUNTINFO_BYTES_CAP || bytes.last() != Some(&b'\n') {
        return Err(ToolError::ProtectionUnavailable);
    }
    let mut entries = Vec::new();
    for line in bytes[..bytes.len() - 1].split(|byte| *byte == b'\n') {
        if line.is_empty()
            || line.len() > MOUNTINFO_LINE_CAP
            || entries.len() >= MOUNTINFO_ENTRIES_CAP
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let fields = line.split(|byte| *byte == b' ').collect::<Vec<_>>();
        if fields.iter().any(|field| field.is_empty()) {
            return Err(ToolError::ProtectionUnavailable);
        }
        let separator = fields
            .iter()
            .position(|field| *field == b"-")
            .ok_or(ToolError::ProtectionUnavailable)?;
        if separator < 6 || fields.len() != separator + 4 {
            return Err(ToolError::ProtectionUnavailable);
        }
        parse_decimal(fields[0])?;
        parse_decimal(fields[1])?;
        let device_separator = fields[2]
            .iter()
            .position(|byte| *byte == b':')
            .ok_or(ToolError::ProtectionUnavailable)?;
        let (major, minor) = fields[2].split_at(device_separator);
        let minor = &minor[1..];
        parse_decimal(major)?;
        parse_decimal(minor)?;
        if !visible_ascii(fields[5])
            || !fields[6..separator]
                .iter()
                .all(|field| visible_ascii(field))
            || !visible_ascii(fields[separator + 1])
            || !visible_ascii(fields[separator + 3])
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        let _mount_source = decode_mount_field(fields[separator + 2])?;
        let _mount_root = decode_mount_path(fields[3])?;
        let path = decode_mount_path(fields[4])?;
        entries.push(MountInfoEntry {
            path: std::path::PathBuf::from(OsString::from_vec(path)),
            optional_fields: separator - 6,
        });
    }
    if entries.is_empty() {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(entries)
}

fn parse_decimal(bytes: &[u8]) -> Result<u64, ToolError> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(ToolError::ProtectionUnavailable);
    }
    std::str::from_utf8(bytes)
        .map_err(|_| ToolError::ProtectionUnavailable)?
        .parse()
        .map_err(|_| ToolError::ProtectionUnavailable)
}

fn visible_ascii(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(|byte| (0x21..=0x7e).contains(byte))
}

fn decode_mount_field(bytes: &[u8]) -> Result<Vec<u8>, ToolError> {
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let escape = bytes
            .get(index + 1..index + 4)
            .ok_or(ToolError::ProtectionUnavailable)?;
        if !escape.iter().all(|byte| (b'0'..=b'7').contains(byte)) || escape[0] > b'3' {
            return Err(ToolError::ProtectionUnavailable);
        }
        let value = (escape[0] - b'0') * 64 + (escape[1] - b'0') * 8 + escape[2] - b'0';
        if !matches!(value, b' ' | b'\t' | b'\n' | b'\\') {
            return Err(ToolError::ProtectionUnavailable);
        }
        decoded.push(value);
        index += 4;
    }
    Ok(decoded)
}

fn decode_mount_path(bytes: &[u8]) -> Result<Vec<u8>, ToolError> {
    let path = decode_mount_field(bytes)?;
    if path.contains(&0)
        || !path.starts_with(b"/")
        || path.len() > 4096
        || path.len() > 1 && path.last() == Some(&b'/')
        || path != b"/"
            && path[1..]
                .split(|byte| *byte == b'/')
                .any(|component| component.is_empty() || component == b"." || component == b"..")
    {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardened_status_requires_filter_no_new_privs_and_zero_capabilities() {
        let hardened = "Name:\tarany\nNoNewPrivs:\t1\nCapInh:\t0000000000000000\nCapPrm:\t0000000000000000\nCapEff:\t0000000000000000\nCapBnd:\t000001ffffffffff\nCapAmb:\t0000000000000000\nSeccomp:\t2\nSeccomp_filters:\t2\n";
        assert!(verify_hardened_status(hardened).is_ok());
        for (from, to) in [
            ("Seccomp:\t2\n", "Seccomp:\t0\n"),
            ("Seccomp:\t2\n", "Seccomp:\t1\n"),
            ("Seccomp:\t2\n", ""),
            ("NoNewPrivs:\t1\n", "NoNewPrivs:\t0\n"),
            ("NoNewPrivs:\t1\n", ""),
            ("CapEff:\t0000000000000000", "CapEff:\t0000000000200000"),
            ("CapPrm:\t0000000000000000", "CapPrm:\t0000000000000001"),
            ("CapInh:\t0000000000000000", "CapInh:\t0000000000000001"),
            ("CapAmb:\t0000000000000000", "CapAmb:\t0000000000000001"),
            ("CapAmb:\t0000000000000000\n", ""),
        ] {
            let status = hardened.replacen(from, to, 1);
            assert_ne!(status, hardened);
            assert!(verify_hardened_status(&status).is_err(), "{status}");
        }
        assert!(verify_hardened_status("").is_err());
    }

    #[test]
    fn mountinfo_escapes_decode_only_separator_bytes() {
        assert_eq!(decode_mount_field(br"a\040b").unwrap(), b"a b");
        assert_eq!(decode_mount_field(br"\134").unwrap(), b"\\");
        for field in [&br"\400"[..], &br"\777"[..], &br"\101"[..], &br"\04"[..]] {
            assert!(decode_mount_field(field).is_err());
        }
    }

    #[test]
    fn toolchain_targets_keep_host_paths_and_reject_profile_overlap() {
        assert_eq!(
            mount_directories(
                Path::new("/home/user/.pyenv/versions/3.13.0"),
                Path::new("/")
            ),
            [
                "/home",
                "/home/user",
                "/home/user/.pyenv",
                "/home/user/.pyenv/versions",
                "/home/user/.pyenv/versions/3.13.0",
            ]
            .map(PathBuf::from)
        );
        for target in ["/home/user/.pyenv/versions/3.13.0", "/opt/node/v22"] {
            assert!(
                reject_profile_overlap(Path::new(target)).is_ok(),
                "{target}"
            );
        }
        for target in [
            "/",
            "/usr",
            "/usr/local/node",
            "/lib/x",
            "/dev/shm",
            "/scratch/rust",
            "/workspace",
        ] {
            assert!(
                reject_profile_overlap(Path::new(target)).is_err(),
                "{target}"
            );
        }
    }

    fn mount_destinations(arguments: &[String]) -> Vec<String> {
        arguments
            .iter()
            .enumerate()
            .filter_map(|(index, argument)| match argument.as_str() {
                "--ro-bind" | "--bind" | "--symlink" => arguments.get(index + 2).cloned(),
                "--tmpfs" | "--dir" | "--proc" | "--dev" => arguments.get(index + 1).cloned(),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn profile_mount_destinations_match_reserved_targets() {
        let folders = ["programs", "inputs", "skills"];
        let workspace = Path::new("/host/workspace");
        for mount in [Some((workspace, true)), Some((workspace, false)), None] {
            let mut command = Command::new("/usr/bin/bwrap");
            profile_arguments(
                &mut command,
                "/usr/bin:/bin",
                Path::new("/host/seed"),
                &folders,
                mount,
                &ToolLimits::default(),
                Path::new("/host/guard"),
            );
            let arguments = command
                .as_std()
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            let destinations = mount_destinations(&arguments);
            assert_eq!(
                destinations.len(),
                PROFILE_MOUNT_TARGETS.len(),
                "{destinations:?}"
            );
            for destination in destinations {
                assert!(
                    PROFILE_MOUNT_TARGETS
                        .iter()
                        .any(|fixed| Path::new(&destination).starts_with(fixed)),
                    "{destination}"
                );
            }
        }
    }

    #[test]
    #[cfg_attr(not(arany_ci), ignore = "native Linux Git namespace isolation")]
    fn native_git_launch_keeps_workspace_read_only_and_host_home_absent() {
        use crate::test_process::BoundedOutput;

        let _native_gate = crate::test_process::NATIVE_GATE.lock().unwrap();
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        let home = fixture.path().join("home");
        let seed = fixture.path().join("seed");
        for directory in [&workspace, &home, &seed] {
            std::fs::create_dir(directory).unwrap();
        }
        let host_config = home.join(".gitconfig");
        let config_bytes = b"[user]\n\tname = Synthetic Host\n";
        std::fs::write(&host_config, config_bytes).unwrap();
        assert_eq!(std::fs::read(&host_config).unwrap(), config_bytes);
        let git = crate::tools::git::discover().expect("pinned Git");
        let initialized = std::process::Command::new(&git.executable)
            .args(["init", "--quiet", "--initial-branch=main"])
            .current_dir(&workspace)
            .env_clear()
            .env("HOME", &home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .bounded_output_for(Duration::from_secs(20), 16 * 1024)
            .unwrap();
        assert!(initialized.status.success(), "pinned Git baseline");
        for path in [workspace.join("a.txt"), workspace.join(".git/write-probe")] {
            std::fs::write(&path, b"allowed\n").unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"allowed\n");
        }

        let mut command = Command::new(BWRAP_BOOTSTRAP[0]);
        command.args(&BWRAP_BOOTSTRAP[1..]);
        profile_arguments(
            &mut command,
            "/usr/bin:/bin",
            &seed,
            &[],
            Some((&workspace, false)),
            &ToolLimits::default(),
            &std::env::current_exe().unwrap(),
        );
        command
            .args(["--setenv", "ARANY_TEST_GIT_HOST_CONFIG"])
            .arg(&host_config)
            .args(["--setenv", "ARANY_TEST_GIT_PIN"])
            .arg(serde_json::to_string(&git).unwrap())
            .args(["--setenv", "ARANY_TEST_GIT_PARENT_NAMESPACES"])
            .arg(serde_json::to_string(&profile::Namespaces::read().unwrap()).unwrap())
            .args([
                "--chdir",
                "/",
                "--",
                "/arany",
                "--exact",
                "tools::guard::linux::tests::native_git_launch_probe",
                "--ignored",
                "--nocapture",
            ])
            .env_clear();
        let output = command
            .into_std()
            .bounded_output_for(Duration::from_secs(40), 32 * 1024)
            .expect("native Git namespace probe");
        assert!(
            output.status.success(),
            "Git namespace probe: {:?}; stdout={:?}; stderr={:?}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        crate::test_process::assert_single_test_completed(&output.stdout, "Git namespace probe");
        assert!(output.stderr.is_empty());
        for path in [workspace.join("a.txt"), workspace.join(".git/write-probe")] {
            assert_eq!(std::fs::read(path).unwrap(), b"allowed\n");
        }
        assert_eq!(std::fs::read(host_config).unwrap(), config_bytes);
    }

    #[test]
    #[ignore = "isolated Linux Git namespace probe"]
    fn native_git_launch_probe() {
        let host_config =
            std::env::var_os("ARANY_TEST_GIT_HOST_CONFIG").expect("private synthetic host config");
        let git = serde_json::from_str(&std::env::var("ARANY_TEST_GIT_PIN").unwrap()).unwrap();
        let parent =
            serde_json::from_str(&std::env::var("ARANY_TEST_GIT_PARENT_NAMESPACES").unwrap())
                .unwrap();
        assert!(profile::Namespaces::read().unwrap().isolated_from(&parent));
        harden().expect("production Guard hardening");
        for path in ["/workspace/a.txt", "/workspace/.git/write-probe"] {
            assert_eq!(std::fs::read(path).unwrap(), b"allowed\n");
            assert_eq!(
                std::fs::write(path, b"forbidden")
                    .unwrap_err()
                    .raw_os_error(),
                Some(nix::libc::EROFS),
                "Git Workspace bind must be read-only: {path}"
            );
        }
        assert_eq!(
            std::fs::File::open(host_config).unwrap_err().raw_os_error(),
            Some(nix::libc::ENOENT),
            "synthetic host HOME/.gitconfig must be absent from the Git namespace"
        );
        let view = Path::new("/scratch").join(crate::tools::git::VIEW);
        std::fs::create_dir(&view).unwrap();
        std::fs::write(view.join("write-probe"), b"allowed\n").unwrap();
        assert_eq!(
            std::fs::read(view.join("write-probe")).unwrap(),
            b"allowed\n"
        );
        std::fs::remove_dir_all(view).unwrap();
        let output = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(crate::tools::git::execute(
                Path::new("/workspace"),
                Path::new("/scratch"),
                &git,
                &ToolCall::GitStatus { path: ".".into() },
                16 * 1024,
                Duration::from_secs(20),
            ))
            .unwrap();
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(output.get("failed").is_none(), "{output}");
        assert!(
            output["entries"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!({"path": "a.txt", "state": "??"}))
        );
    }

    fn mountinfo_with_runtime(optional: &str, descendants: &[&str]) -> Vec<u8> {
        let mut bytes = format!(
            "1 0 8:1 / / rw - ext4 /dev/root rw\n2 1 8:1 /cache /opt/toolchains/rust ro{optional} - ext4 /dev/root ro\n"
        )
        .into_bytes();
        for (index, path) in descendants.iter().enumerate() {
            bytes.extend_from_slice(
                format!(
                    "{} 2 8:1 /cache/{index} {path} ro - ext4 /dev/root ro\n",
                    index + 3
                )
                .as_bytes(),
            );
        }
        bytes
    }

    #[test]
    fn runtime_mount_attestation_requires_private_root_and_exact_leaf_mounts() {
        let target = Path::new("/opt/toolchains/rust").to_owned();
        let valid = parse_mountinfo(&mountinfo_with_runtime("", &[])).unwrap();
        assert!(verify_mount_entries(&valid, std::slice::from_ref(&target)).is_ok());

        let shared = mountinfo_with_runtime(" shared:2", &[]);
        assert!(
            verify_mount_entries(
                &parse_mountinfo(&shared).unwrap(),
                std::slice::from_ref(&target)
            )
            .is_err()
        );

        let nested = mountinfo_with_runtime("", &["/opt/toolchains/rust/nested"]);
        assert!(
            verify_mount_entries(
                &parse_mountinfo(&nested).unwrap(),
                std::slice::from_ref(&target)
            )
            .is_err()
        );

        let shared_root = b"1 0 8:1 / / rw shared:1 - ext4 /dev/root rw\n2 1 8:1 /cache /opt/toolchains/rust ro - ext4 /dev/root ro\n";
        assert!(
            verify_mount_entries(
                &parse_mountinfo(shared_root).unwrap(),
                std::slice::from_ref(&target)
            )
            .is_err()
        );
    }

    #[test]
    fn mountinfo_decodes_only_kernel_path_escapes_and_rejects_ambiguous_records() {
        let target = Path::new("/opt/toolchains/a b").to_owned();
        let escaped = b"1 0 8:1 / / rw - ext4 /dev/root rw\n2 1 8:1 /cache/a\\040b /opt/toolchains/a\\040b ro - ext4 /dev/root ro\n";
        let entries = parse_mountinfo(escaped).unwrap();
        assert!(verify_mount_entries(&entries, std::slice::from_ref(&target)).is_ok());

        let invalid_escape = b"1 0 8:1 / / rw - ext4 /dev/root rw\n2 1 8:1 /cache/a\\141b /opt/toolchains/a\\141b ro - ext4 /dev/root ro\n";
        assert!(parse_mountinfo(invalid_escape).is_err());

        let stacked = b"1 0 8:1 / / rw - ext4 /dev/root rw\n2 1 8:1 /cache /opt/toolchains/rust ro - ext4 /dev/root ro\n3 1 8:1 /other /opt/toolchains/rust ro - ext4 /dev/root ro\n";
        assert!(
            verify_mount_entries(
                &parse_mountinfo(stacked).unwrap(),
                &[Path::new("/opt/toolchains/rust").to_owned()]
            )
            .is_err()
        );
    }

    #[test]
    fn selected_runtime_path_and_cache_mounts_are_exact() {
        use crate::tools::toolchains::RuntimeKind;
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let node = home.join(".nvm/versions/node/v22.3.0");
        std::fs::create_dir_all(node.join("bin")).unwrap();
        let executable = node.join("bin/node");
        std::fs::write(&executable, b"synthetic executable").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cargo_cache = home.join(".cargo/registry/cache");
        std::fs::create_dir_all(&cargo_cache).unwrap();

        let candidates = crate::tools::toolchains::discover(&home).unwrap();
        let node = candidates
            .iter()
            .find(|candidate| candidate.grant.kind == RuntimeKind::Node)
            .unwrap();
        let cache = candidates
            .iter()
            .find(|candidate| candidate.grant.kind == RuntimeKind::CargoCache)
            .unwrap();
        let command_path = super::super::runtime_path(std::slice::from_ref(&node.grant)).unwrap();
        assert_eq!(
            command_path,
            format!(
                "{}:/usr/bin:/bin",
                node.grant
                    .mounted_path(false)
                    .unwrap()
                    .join("bin")
                    .display()
            )
        );
        assert_eq!(
            cache.grant.mounted_path(true).unwrap(),
            Path::new("/cache/cargo/registry/cache")
        );
        assert_eq!(
            cache.grant.mounted_path(false).unwrap(),
            Path::new("/scratch/cargo/registry/cache")
        );
        let (directories, mounts) =
            runtime_mounts(std::slice::from_ref(&cache.grant), false).unwrap();
        assert_eq!(
            mounts,
            [(
                cache.grant.path.clone(),
                PathBuf::from("/scratch/cargo/registry/cache")
            )]
        );
        assert!(directories.iter().all(|path| path.starts_with("/scratch")));
        assert!(node.grant.verify_host().is_ok() && cache.grant.verify_host().is_ok());
    }

    fn fixture_pid_identity(path: &Path) -> Result<(i32, i32), String> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(|error| error.to_string())?
            .take(65)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() > 64 {
            return Err("fixture PID identity exceeds its bound".into());
        }
        let identity = String::from_utf8(bytes).map_err(|error| error.to_string())?;
        let fields = identity
            .split_whitespace()
            .map(|field| field.parse::<i32>().map_err(|error| error.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        if fields.len() != 2 || fields.iter().any(|pid| *pid <= 0) {
            return Err("fixture must report one positive host and namespace PID".into());
        }
        Ok((fields[0], fields[1]))
    }

    async fn wait_fixture_pidfd(
        path: &Path,
        deadline: tokio::time::Instant,
    ) -> Result<((i32, i32), tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>), String> {
        tokio::time::timeout_at(deadline, async {
            loop {
                if path.is_file() {
                    let identity = fixture_pid_identity(path)?;
                    let pidfd = bind_fixture_pidfd(identity)?;
                    return Ok((identity, pidfd));
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_| "fixture PID identity gate timed out".to_string())?
    }

    fn process_namespace_ids(pid: i32) -> Result<Vec<i32>, String> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .map_err(|error| error.to_string())?;
        let value = status
            .lines()
            .find_map(|line| line.strip_prefix("NSpid:"))
            .ok_or_else(|| "fixture process lacks NSpid status field".to_string())?;
        value
            .split_whitespace()
            .map(|field| field.parse::<i32>().map_err(|error| error.to_string()))
            .collect()
    }

    fn bind_fixture_pidfd(
        identity: (i32, i32),
    ) -> Result<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>, String> {
        let (host_pid, namespace_pid) = identity;
        let pid = rustix::process::Pid::from_raw(host_pid)
            .ok_or_else(|| "fixture host PID must be positive".to_string())?;
        let before = process_namespace_ids(host_pid)?;
        if before.first() != Some(&host_pid) || before.last() != Some(&namespace_pid) {
            return Err("fixture process namespace identity changed before pidfd binding".into());
        }
        let pidfd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
            .map_err(|error| error.to_string())?;
        if process_namespace_ids(host_pid)? != before {
            return Err("fixture process namespace identity changed during pidfd binding".into());
        }
        tokio::io::unix::AsyncFd::new(pidfd).map_err(|error| error.to_string())
    }

    fn bind_host_pidfd(pid: u32) -> Result<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>, String> {
        let pid = i32::try_from(pid).map_err(|error| error.to_string())?;
        let namespace_ids = process_namespace_ids(pid)?;
        if namespace_ids.first() != Some(&pid) {
            return Err("direct namespace owner host PID changed before binding".into());
        }
        let namespace_pid = *namespace_ids
            .last()
            .ok_or_else(|| "direct namespace owner has no namespace PID".to_string())?;
        bind_fixture_pidfd((pid, namespace_pid))
    }

    async fn await_fixture_exit(
        pidfd: &tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
    ) -> Result<(), String> {
        await_fixture_exit_until(pidfd, tokio::time::Instant::now() + Duration::from_secs(10)).await
    }

    async fn await_fixture_exit_until(
        pidfd: &tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
        deadline: tokio::time::Instant,
    ) -> Result<(), String> {
        let readiness = tokio::time::timeout_at(deadline, pidfd.readable())
            .await
            .map_err(|_| "fixture process did not exit before cleanup deadline".to_string())?
            .map_err(|error| error.to_string())?;
        drop(readiness);
        Ok(())
    }

    fn publish_fixture_gate(fixture: &Path, name: &str) -> std::io::Result<()> {
        use std::io::Write;

        let temporary = fixture.join(format!("{name}.next"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(b"continue")?;
        std::fs::rename(temporary, fixture.join(name))
    }

    async fn capture_runtime_mount_topology(
        mut command: Command,
        fixture: &Path,
    ) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), (String, bool)> {
        const CAPTURE: Duration = Duration::from_secs(40);
        const OWNER_DEADLINE: Duration = Duration::from_secs(60);
        const CAPTURE_CLEANUP_RESERVE: Duration = Duration::from_secs(10);
        const OUTPUT_CAP: usize = 64 * 1024;

        let deadline = tokio::time::Instant::now() + OWNER_DEADLINE;
        let child = command.spawn().map_err(|error| (error.to_string(), true))?;
        let owner_pidfd = child.id().and_then(|pid| bind_host_pidfd(pid).ok());
        let init_deadline = std::cmp::min(
            deadline - CAPTURE_CLEANUP_RESERVE,
            tokio::time::Instant::now() + Duration::from_secs(10),
        );
        let (init_identity, init_pidfd) =
            match wait_fixture_pidfd(&fixture.join("init.pid"), init_deadline).await {
                Ok(identity) => identity,
                Err(error) => {
                    let capture = capture(child, OUTPUT_CAP, Duration::from_millis(100)).await;
                    if let Some(owner_pidfd) = owner_pidfd.as_ref() {
                        let _ = await_fixture_exit_until(owner_pidfd, deadline).await;
                    }
                    let suffix = match capture {
                        Ok((status, _, _)) => format!("; setup exited with {status}"),
                        Err(capture_error) => format!("; setup capture failed: {capture_error:?}"),
                    };
                    return Err((
                        format!("read topology namespace init identity: {error}{suffix}"),
                        false,
                    ));
                }
            };
        let mut setup_error = None;
        if init_identity.1 != 1 {
            setup_error = Some("topology init identity did not report namespace PID 1".to_string());
        }
        if owner_pidfd.is_none() {
            setup_error.get_or_insert_with(|| "topology owner pidfd could not be bound".into());
        }
        if setup_error.is_none()
            && let Err(error) = publish_fixture_gate(fixture, "init-go")
        {
            setup_error = Some(format!("release topology init gate: {error}"));
        }
        if let Some(error) = setup_error {
            let capture = capture(child, OUTPUT_CAP, Duration::from_millis(100)).await;
            let owner_exited = match owner_pidfd.as_ref() {
                Some(pidfd) => await_fixture_exit_until(pidfd, deadline).await.is_ok(),
                None => false,
            };
            let init_exited = await_fixture_exit_until(&init_pidfd, deadline)
                .await
                .is_ok();
            let capture_detail = match capture {
                Ok((status, _, _)) => format!("; setup exited with {status}"),
                Err(capture_error) => format!("; setup capture failed: {capture_error:?}"),
            };
            return Err((
                format!("topology setup failed: {error}{capture_detail}"),
                owner_exited && init_exited && init_identity.1 == 1,
            ));
        }

        let capture_deadline = std::cmp::min(
            deadline - CAPTURE_CLEANUP_RESERVE,
            tokio::time::Instant::now() + CAPTURE,
        );
        let capture_duration =
            capture_deadline.saturating_duration_since(tokio::time::Instant::now());
        let capture_future = capture(child, OUTPUT_CAP, capture_duration);
        tokio::pin!(capture_future);
        let mut capture_result = None;
        let mut worker_pidfds = Vec::with_capacity(2);
        let mut cleanup_errors = Vec::new();
        let mut coordination_error = None;
        for phase in ["exercise", "reject"] {
            let identity_path = fixture.join(format!("worker-{phase}.pid"));
            let identity = tokio::select! {
                result = &mut capture_future => {
                    capture_result = Some(result);
                    coordination_error = Some(format!("topology capture ended before {phase} worker identity"));
                    break;
                }
                result = wait_fixture_pidfd(&identity_path, capture_deadline) => result,
            };
            let (identity, pidfd) = match identity {
                Ok(identity) => identity,
                Err(error) => {
                    coordination_error = Some(format!("bind {phase} worker identity: {error}"));
                    break;
                }
            };
            worker_pidfds.push((pidfd, phase));
            if identity.1 <= 1 || identity.0 == init_identity.0 {
                cleanup_errors.push(format!(
                    "{phase} worker identity was not distinct and nested"
                ));
                coordination_error = Some(format!("invalid {phase} worker identity"));
                break;
            }
            if let Err(error) = publish_fixture_gate(fixture, &format!("worker-{phase}-go")) {
                coordination_error = Some(format!("release {phase} worker gate: {error}"));
                break;
            }
        }
        if capture_result.is_none() {
            capture_result = Some(capture_future.await);
        }

        match owner_pidfd.as_ref() {
            Some(pidfd) => {
                if let Err(error) = await_fixture_exit_until(pidfd, deadline).await {
                    cleanup_errors.push(format!("await topology owner exit: {error}"));
                }
            }
            None => cleanup_errors.push("topology owner pidfd was not bound".into()),
        }
        if let Err(error) = await_fixture_exit_until(&init_pidfd, deadline).await {
            cleanup_errors.push(format!("await topology init exit: {error}"));
        }
        for (pidfd, phase) in &worker_pidfds {
            if let Err(error) = await_fixture_exit_until(pidfd, deadline).await {
                cleanup_errors.push(format!("await {phase} worker exit: {error}"));
            }
        }
        if worker_pidfds.len() != 2 {
            cleanup_errors.push("topology did not bind both worker identities".into());
        }

        if !cleanup_errors.is_empty() {
            return Err((cleanup_errors.join("; "), false));
        }
        if let Some(error) = coordination_error {
            return Err((error, true));
        }
        let capture_result = match capture_result {
            Some(result) => result,
            None => return Err(("topology capture future did not complete".into(), true)),
        };
        match capture_result {
            Ok(output) => Ok(output),
            Err(error) => Err((format!("topology capture failed: {error:?}"), true)),
        }
    }

    async fn force_timeout_namespace(
        mut command: Command,
        fixture: &tempfile::TempDir,
    ) -> Result<(), String> {
        let child = command.spawn().map_err(|error| error.to_string())?;
        let owner_pidfd = child
            .id()
            .ok_or_else(|| "timeout namespace owner has no process ID".to_string())
            .and_then(bind_host_pidfd);
        let ready = fixture.path().join("timeout-ready");
        let init_identity_path = fixture.path().join("init.pid");
        let held_identity_path = fixture.path().join("held.pid");
        let gate_opened = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if ready.is_file() && init_identity_path.is_file() && held_identity_path.is_file() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok();
        let mut errors = Vec::new();
        if !gate_opened {
            errors.push("namespace timeout ready gate did not open".to_string());
        }
        let init_identity = if gate_opened {
            match fixture_pid_identity(&init_identity_path) {
                Ok(identity) => Some(identity),
                Err(error) => {
                    errors.push(format!("read namespace init identity: {error}"));
                    None
                }
            }
        } else {
            None
        };
        let held_identity = if gate_opened {
            match fixture_pid_identity(&held_identity_path) {
                Ok(identity) => Some(identity),
                Err(error) => {
                    errors.push(format!("read held descendant identity: {error}"));
                    None
                }
            }
        } else {
            None
        };
        if let Some((_, namespace_pid)) = init_identity
            && namespace_pid != 1
        {
            errors.push("reported namespace init did not have PID 1".into());
        }
        if let Some((_, namespace_pid)) = held_identity
            && namespace_pid <= 1
        {
            errors.push("reported held descendant was not below namespace init".into());
        }
        let init_pidfd = init_identity.and_then(|identity| match bind_fixture_pidfd(identity) {
            Ok(pidfd) => Some(pidfd),
            Err(error) => {
                errors.push(format!("bind namespace init identity: {error}"));
                None
            }
        });
        let held_pidfd = held_identity.and_then(|identity| match bind_fixture_pidfd(identity) {
            Ok(pidfd) => Some(pidfd),
            Err(error) => {
                errors.push(format!("bind held descendant identity: {error}"));
                None
            }
        });

        let timeout_result = capture(child, 64 * 1024, Duration::from_millis(100)).await;
        if !matches!(timeout_result.as_ref(), Err(ToolError::Limit)) {
            errors.push(format!(
                "capture did not enforce the forced timeout: {timeout_result:?}"
            ));
        }
        match owner_pidfd {
            Ok(owner_pidfd) => {
                if let Err(error) = await_fixture_exit(&owner_pidfd).await {
                    errors.push(format!("await namespace owner exit: {error}"));
                }
            }
            Err(error) => errors.push(format!("bind namespace owner identity: {error}")),
        }
        for (pidfd, role) in [
            (init_pidfd.as_ref(), "namespace init"),
            (held_pidfd.as_ref(), "held descendant"),
        ] {
            match pidfd {
                Some(pidfd) => {
                    if let Err(error) = await_fixture_exit(pidfd).await {
                        errors.push(format!("await {role} exit: {error}"));
                    }
                }
                None => errors.push(format!("{role} identity was not bound before timeout")),
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    #[test]
    #[cfg_attr(
        not(arany_ci),
        ignore = "native private mount propagation fixture; requires unprivileged user namespaces"
    )]
    fn native_runtime_mount_attestation() {
        let _native_gate = crate::test_process::NATIVE_GATE.lock().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(native_runtime_mount_attestation_inner());
    }

    async fn native_runtime_mount_attestation_inner() {
        use crate::tools::toolchains::RuntimeGrant;

        const MODE: &str = "ARANY_TEST_RUNTIME_MOUNT_MODE";
        if let Some(mode) = std::env::var_os(MODE) {
            let mode = mode.to_string_lossy();
            if let Some(mode) = mode.strip_prefix("launch-") {
                let uid_mapping =
                    std::fs::read_to_string("/proc/self/uid_map").expect("fixture UID mapping");
                let initial_uid = uid_mapping
                    .split_whitespace()
                    .nth(1)
                    .and_then(|value| value.parse::<u32>().ok())
                    .expect("fixture mapped initial UID");
                assert_ne!(initial_uid, 0, "fixture must launch from a non-root UID");
                assert_ne!(
                    nix::unistd::getuid().as_raw(),
                    0,
                    "production-prefix fixture UID must remain non-root"
                );
                let grant_path = std::path::PathBuf::from(
                    std::env::var_os("ARANY_TEST_RUNTIME_MOUNT_GRANT").expect("fixture grant path"),
                );
                assert!(grant_path.is_file(), "fixture grant exists");
                let fixture =
                    std::env::var_os("ARANY_TEST_RUNTIME_MOUNT_FIXTURE").expect("fixture root");
                let alias =
                    std::env::var_os("ARANY_TEST_RUNTIME_MOUNT_ALIAS").expect("runtime alias");
                let target =
                    std::env::var_os("ARANY_TEST_RUNTIME_MOUNT_TARGET").expect("runtime target");
                let binary = std::env::var_os("ARANY_TEST_RUNTIME_MOUNT_BINARY")
                    .expect("fixture test binary");
                let binary_metadata = std::fs::metadata(&binary).expect("test binary metadata");
                assert!(binary_metadata.is_file());
                let mut command = Command::new(BWRAP_BOOTSTRAP[0]);
                command.args(&BWRAP_BOOTSTRAP[1..]);
                profile_arguments(
                    &mut command,
                    &format!("{}/bin:/usr/bin:/bin", Path::new(&target).display()),
                    Path::new(&fixture),
                    &[],
                    None,
                    &ToolLimits::default(),
                    Path::new(&binary),
                );
                for directory in mount_directories(Path::new(&target), Path::new("/")) {
                    command.args(["--dir"]).arg(directory);
                }
                command
                    .arg("--ro-bind")
                    .arg(alias)
                    .arg(&target)
                    .args(["--dir", "/fixture", "--bind"])
                    .arg(&fixture)
                    .args(["/fixture", "--ro-bind"])
                    .arg(binary)
                    .args([
                        "/fixture/arany-test-bin",
                        "--setenv",
                        "ARANY_TEST_RUNTIME_MOUNT_MODE",
                        mode,
                        "--setenv",
                        "ARANY_TEST_RUNTIME_MOUNT_GRANT",
                        "/fixture/grant.json",
                        "--setenv",
                        "ARANY_TEST_RUNTIME_MOUNT_RESULT",
                        "/fixture/result",
                        "--setenv",
                        "ARANY_TEST_RUNTIME_MOUNT_READY",
                        "/fixture/ready",
                        "--setenv",
                        "ARANY_TEST_RUNTIME_MOUNT_GO",
                        "/fixture/go",
                        "--",
                        "/fixture/arany-test-bin",
                        "--exact",
                        "tools::guard::linux::tests::native_runtime_mount_attestation",
                        "--include-ignored",
                        "--nocapture",
                    ]);
                let child = command
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .expect("production Bwrap prefix launch");
                let (status, _, stderr) = capture(child, 64 * 1024, Duration::from_secs(30))
                    .await
                    .expect("bounded production-prefix fixture");
                assert!(
                    status.success(),
                    "production unshare/Bubblewrap prefix: {}",
                    String::from_utf8_lossy(&stderr)
                );
                return;
            }

            let grant_path = std::env::var_os("ARANY_TEST_RUNTIME_MOUNT_GRANT").unwrap();
            let result_path = std::env::var_os("ARANY_TEST_RUNTIME_MOUNT_RESULT").unwrap();
            let ready_path = std::env::var_os("ARANY_TEST_RUNTIME_MOUNT_READY").unwrap();
            let go_path = std::env::var_os("ARANY_TEST_RUNTIME_MOUNT_GO").unwrap();
            let grant: RuntimeGrant =
                serde_json::from_slice(&std::fs::read(grant_path).expect("fixture grant"))
                    .expect("fixture grant schema");
            let target = grant.mounted_path(false).unwrap();
            grant.verify_at(&target).expect("runtime alias identity");
            let call = ToolCall::Command {
                program: "node".into(),
                args: Vec::new(),
                cwd: String::new(),
            };
            let grants = std::slice::from_ref(&grant);
            let mut stage = super::super::BootstrapStage::Workspace;
            if super::super::attest_runtime_mounts(&call, grants, false, &mut stage).is_err() {
                let failure = super::super::BootstrapFailure { stage };
                std::fs::write(
                    result_path,
                    serde_json::to_vec(&failure).expect("bounded stage encoding"),
                )
                .expect("fixture failure result");
                return;
            }
            if mode == "exercise" {
                std::fs::write(ready_path, b"ready").expect("fixture Ready marker");
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while !Path::new(&go_path).exists() {
                    assert!(std::time::Instant::now() < deadline, "fixture GO deadline");
                    std::thread::sleep(Duration::from_millis(10));
                }
                super::super::attest_runtime_mounts(&call, grants, false, &mut stage)
                    .expect("late shared-source mount must not propagate");
            }
            std::fs::write(result_path, b"PASS").expect("fixture success result");
            return;
        }

        use crate::tools::toolchains::{RuntimeKind, RuntimeManager};
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("home/.nvm/versions/node/v22.3.0");
        std::fs::create_dir_all(source.join("bin")).unwrap();
        std::fs::create_dir_all(source.join("late")).unwrap();
        std::fs::create_dir_all(source.join("nested")).unwrap();
        let node = source.join("bin/node");
        std::fs::write(&node, b"fixture runtime executable").unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o700)).unwrap();
        let alias = fixture.path().join("runtime-alias");
        std::fs::create_dir(&alias).unwrap();
        let late_source = fixture.path().join("late-source");
        let nested_source = fixture.path().join("nested-source");
        std::fs::create_dir(&late_source).unwrap();
        std::fs::create_dir(&nested_source).unwrap();
        let metadata = std::fs::metadata(&source).unwrap();
        let grant = RuntimeGrant {
            kind: RuntimeKind::Node,
            manager: RuntimeManager::Nvm,
            version: Some("v22.3.0".into()),
            path: source.clone(),
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        grant.validate_shape().unwrap();
        let grant_path = fixture.path().join("grant.json");
        let target = grant.mounted_path(false).unwrap();
        let result_path = fixture.path().join("result");
        let test_binary = std::env::current_exe().unwrap();
        std::fs::write(fixture.path().join("arany-test-bin"), b"placeholder").unwrap();
        std::fs::write(&grant_path, serde_json::to_vec(&grant).unwrap()).unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/runtime-mounts.sh");
        let mut command = Command::new("/usr/bin/unshare");
        // The outer namespace owns fixture mount setup; the BWRAP_BOOTSTRAP worker drops these caps.
        command
            .args([
                "--user",
                "--map-current-user",
                "--mount",
                "--pid",
                "--fork",
                "--kill-child=SIGKILL",
                "--propagation",
                "shared",
                "--keep-caps",
                "--",
                "/usr/bin/bash",
            ])
            .arg(&script)
            .args([
                source.as_os_str(),
                alias.as_os_str(),
                late_source.as_os_str(),
                nested_source.as_os_str(),
                fixture.path().as_os_str(),
                test_binary.as_os_str(),
                target.as_os_str(),
            ])
            .arg("topology")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let (status, stdout, stderr) = match capture_runtime_mount_topology(command, fixture.path())
            .await
        {
            Ok(output) => output,
            Err((error, true)) => panic!("native mount topology failed after cleanup: {error}"),
            Err((error, false)) => {
                let retained = fixture.keep();
                panic!(
                    "native mount topology cleanup was not confirmed: {error}; fixture retained at {}",
                    retained.display()
                );
            }
        };
        assert!(
            status.success(),
            "native mount attestation fixture: {}",
            String::from_utf8_lossy(&stderr)
        );
        assert_eq!(
            stdout,
            b"private-source-ok\nlate-mount-isolated\nnested-mount-rejected-at-runtime-stage\n"
        );
        crate::test_process::assert_single_test_completed(
            &std::fs::read(fixture.path().join("worker-exercise.log")).unwrap(),
            "runtime mount exercise helper",
        );
        crate::test_process::assert_single_test_completed(
            &std::fs::read(fixture.path().join("worker-reject.log")).unwrap(),
            "runtime mount rejection helper",
        );
        assert_eq!(
            std::fs::read(result_path).unwrap(),
            b"{\"stage\":\"runtime_mounts\"}"
        );

        let mut timeout_command = Command::new("/usr/bin/unshare");
        timeout_command
            .args([
                "--user",
                "--map-current-user",
                "--mount",
                "--pid",
                "--fork",
                "--kill-child=SIGKILL",
                "--propagation",
                "shared",
                "--keep-caps",
                "--",
                "/usr/bin/bash",
            ])
            .arg(&script)
            .args([
                source.as_os_str(),
                alias.as_os_str(),
                late_source.as_os_str(),
                nested_source.as_os_str(),
                fixture.path().as_os_str(),
                test_binary.as_os_str(),
                target.as_os_str(),
            ])
            .arg("timeout")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Err(error) = force_timeout_namespace(timeout_command, &fixture).await {
            // Keep the private gate if process exit could not be positively established.
            let retained = fixture.keep();
            panic!(
                "native namespace timeout cleanup failed: {error}; fixture retained at {}",
                retained.display()
            );
        }
    }

    #[tokio::test]
    #[ignore = "native Linux retained failed-unit recovery; requires systemd user manager and delegated cgroup v2"]
    async fn retained_failed_unit_is_reset_before_capacity_is_reclaimed() {
        check_available().unwrap();
        let id = uuid::Uuid::now_v7();
        let unit = Unit {
            name: format!("arany-tool-{id}.service"),
            invocation: None,
            group: None,
            kill: None,
            limits: None,
        };
        let name = unit.name.clone();
        let result = tokio::spawn(async move {
            let mut command = manager_command("/usr/bin/systemd-run");
            command.args([
                "--user",
                "--no-ask-password",
                "--quiet",
                "--wait",
                "--service-type=exec",
                "--unit",
                &unit.name,
                "--property=Restart=no",
                "--property=KillMode=control-group",
                "--property=TimeoutStopSec=2s",
                "--property=RuntimeMaxSec=5s",
                "/usr/bin/false",
            ]);
            let (status, _, _) = capture(command.spawn().unwrap(), 4096, Duration::from_secs(10))
                .await
                .unwrap();
            assert!(!status.success());
            let failed = unit.properties().await.unwrap();
            assert_eq!(failed["ActiveState"], "failed");
            assert_eq!(failed["MainPID"], "0");
            assert_eq!(failed["ControlGroup"], "");
            assert!(recover_reservation(id).await.unwrap());
            let reset = unit.properties().await.unwrap();
            assert_ne!(reset["ActiveState"], "failed");
            assert_eq!(reset["MainPID"], "0");
            assert_eq!(reset["ControlGroup"], "");
        })
        .await;
        for action in ["stop", "reset-failed"] {
            let mut command = manager_command("/usr/bin/systemctl");
            command.args(["--user", "--no-ask-password", action, &name]);
            if let Ok(child) = command.spawn() {
                let _ = capture(child, 4096, MANAGER_DEADLINE).await;
            }
        }
        result.unwrap();
    }
}

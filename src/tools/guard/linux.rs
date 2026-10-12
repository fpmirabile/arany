use super::{
    CAPTURE_BYTES, GuardStage, RUNTIME_ROOT, Task, bootstrap_failure, capture, drain_stderr,
    encode_task, parse_ready, profile, read_capped, read_line_capped, read_observation,
};
use crate::engine::RunCancellation;
use crate::tools::types::{
    EffectIntent, MAX_PROCESSES_OR_THREADS, MEMORY_BYTES, ToolCall, ToolObservation, digest,
};
use crate::tools::{ToolError, ToolRuntime};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::Command;

const MANAGER_DEADLINE: Duration = Duration::from_secs(4);

pub(super) fn check_available() -> Result<(), ToolError> {
    profile::programs()?;
    use std::os::unix::fs::MetadataExt;
    for path in [
        "/usr/bin/bwrap",
        "/usr/bin/systemd-run",
        "/usr/bin/systemctl",
        "/usr/bin/env",
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
    let mut bytes = b"arany-guard-v3:no-buffered-go-before-ready;pinned-skill-digest-pre-go;private-mount,user,pid,net,ipc,uts;no-userns;no-new-privs;zero-caps;not-dumpable;cpu25000/100000;memory536870912;swap0;oom-group1;pids64;scratch67108864;workspace67108864;nofile64;core0;no-inherited-host-handles;typed-native-or-fresh-snapshot;owned-cgroup-cleanup;runtime-root-/usr-excludes-workspace-state-account".to_vec();
    bytes.extend_from_slice(&profile::profile_digest()?);
    Ok(digest(&bytes))
}

/// Drops capabilities, installs the syscall profile and verifies the result before Ready.
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
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|_| ToolError::ProtectionUnavailable)?;
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
}

impl Unit {
    async fn properties(&self) -> Result<serde_json::Value, ToolError> {
        let mut command = manager_command("/usr/bin/systemctl");
        command.args(["--user", "--no-ask-password", "show", &self.name,
            "--property=Id,InvocationID,ControlGroup,MainPID,ActiveState,SubState,ExecMainCode,ExecMainStatus"]);
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
            ("memory.max", MEMORY_BYTES.to_string()),
            ("memory.swap.max", "0".into()),
            ("memory.oom.group", "1".into()),
            ("pids.max", MAX_PROCESSES_OR_THREADS.to_string()),
            ("cpu.max", "25000 100000".into()),
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
        if members.lines().count() > MAX_PROCESSES_OR_THREADS as usize
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

pub(super) async fn execute_inner(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    mut cancellation: RunCancellation,
    dispatched: &mut bool,
) -> Result<ToolObservation, ToolError> {
    let deadline = tokio::time::Instant::now()
        + Duration::from_millis(u64::from(intent.limits.runtime_ms))
        + Duration::from_secs(5);
    crate::tools::fs::validate_workspace(runtime)?;
    let workspace = crate::tools::fs::open_directory(&runtime.workspace)?;
    let snapshot =
        crate::tools::fs::Snapshot::create(&workspace, &runtime.config, intent.call.is_process())?;
    let mut unit = Unit {
        name: format!("arany-tool-{}.service", intent.id),
        invocation: None,
        group: None,
        kill: None,
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
        &unit.name,
        "--property=Restart=no",
        "--property=KillMode=control-group",
        "--property=SendSIGKILL=yes",
        "--property=FinalKillSignal=SIGKILL",
        "--property=TimeoutStartSec=5s",
        "--property=TimeoutStopSec=2s",
        "--property=RuntimeRandomizedExtraSec=0",
        "--property=RemainAfterExit=no",
        "--property=MemoryMax=536870912",
        "--property=MemorySwapMax=0",
        "--property=OOMPolicy=kill",
        "--property=TasksMax=64",
        "--property=CPUQuota=25%",
        "--property=CPUQuotaPeriodSec=100ms",
    ]);
    command
        .arg(format!(
            "--property=RuntimeMaxSec={}ms",
            intent.limits.runtime_ms
        ))
        .args([
            "/usr/bin/env",
            "-i",
            "PATH=/usr/bin:/bin",
            "/usr/bin/bwrap",
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
            "/usr/bin:/bin",
            "--setenv",
            "HOME",
            "/scratch",
            "--setenv",
            "TMPDIR",
            "/scratch",
            "--setenv",
            "CARGO_TARGET_DIR",
            "/scratch/target",
            "--ro-bind",
            RUNTIME_ROOT,
            RUNTIME_ROOT,
            "--symlink",
            "usr/lib",
            "/lib",
            "--symlink",
            "usr/lib",
            "/lib64",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--size",
            "67108864",
            "--tmpfs",
            "/scratch",
            "--ro-bind",
        ]);
    command.arg(&snapshot.path).arg("/seed");
    for folder in ["programs", "inputs", "skills"] {
        if snapshot.dir.symlink_metadata(folder).is_ok() {
            command
                .args(["--ro-bind"])
                .arg(snapshot.path.join(folder))
                .arg(format!("/{folder}"));
        }
    }
    let native = matches!(
        intent.call,
        ToolCall::List { .. }
            | ToolCall::Read { .. }
            | ToolCall::Search { .. }
            | ToolCall::Write { .. }
            | ToolCall::Edit { .. }
            | ToolCall::Mkdir { .. }
    );
    if native {
        command
            .arg(if intent.call.mutates() {
                "--bind"
            } else {
                "--ro-bind"
            })
            .arg(&runtime.workspace)
            .arg("/workspace");
    } else {
        command.args(["--size", "67108864", "--tmpfs", "/workspace"]);
    }
    command
        .arg("--ro-bind")
        .arg(&runtime.guard_executable)
        .arg("/arany")
        .args(["--chdir", "/", "/arany", "--internal-tool-guard"]);
    command.stdin(Stdio::piped());
    let parent_namespaces = profile::Namespaces::read()?;
    let mut child = command
        .spawn()
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    let mut stdin = child.stdin.take().ok_or(ToolError::Operation)?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or(ToolError::Operation)?);
    let stderr = child.stderr.take().ok_or(ToolError::Operation)?;
    let mut stderr_task = tokio::spawn(read_capped(stderr, CAPTURE_BYTES));
    let execution = async {
        let task = encode_task(&Task {
            config: runtime.config.clone(),
            intent: intent.clone(),
            parent_namespaces: parent_namespaces.clone(),
        })?;
        stdin
            .write_all(&task)
            .await
            .map_err(|_| ToolError::Operation)?;
        let ready = parse_ready(&read_line_capped(&mut stdout, 2048).await?, intent)?;
        if ready.pid != 1 && ready.pid != 2 {
            return Err(ToolError::ProtectionUnavailable);
        }
        if !ready.namespaces.isolated_from(&parent_namespaces) {
            return Err(ToolError::ProtectionUnavailable);
        }
        unit.admit()
            .await
            .map_err(|_| ToolError::GuardRejected(GuardStage::ResourceAttestation))?;
        *dispatched = true;
        stdin
            .write_all(b"GO\n")
            .await
            .map_err(|_| ToolError::Operation)?;
        stdin.shutdown().await.map_err(|_| ToolError::Operation)?;
        let observation = read_observation(&mut stdout, intent).await?;
        if !child
            .wait()
            .await
            .map_err(|_| ToolError::Operation)?
            .success()
        {
            return Err(ToolError::Operation);
        }
        Ok(observation)
    };
    let result = tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(ToolError::Operation),
        result = tokio::time::timeout_at(deadline, execution) => result.unwrap_or(Err(ToolError::Limit)),
    };
    let cleanup = tokio::time::timeout(Duration::from_secs(5), unit.stop()).await;
    if !matches!(cleanup, Ok(Ok(()))) {
        unit.kill_group();
    }
    let _ = child.start_kill();
    let reaped = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    let drained = drain_stderr(&mut stderr_task).await;
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
        let stage = if diagnostic.contains("bwrap:") {
            GuardStage::NamespaceBootstrap
        } else if diagnostic.contains("Failed to") {
            GuardStage::NativeManager
        } else {
            GuardStage::ControlProtocol
        };
        return Err(ToolError::GuardRejected(stage));
    }
    result
}

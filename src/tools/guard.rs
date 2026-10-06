use super::config::Config;
use super::types::*;
use super::{ToolError, ToolRuntime, now_ms};
use crate::engine::RunCancellation;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{ExitCode, Stdio};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

const CAPTURE_BYTES: usize = 48 * 1024;
const MANAGER_DEADLINE: Duration = Duration::from_secs(4);
const PAYLOAD_DEADLINE: Duration = Duration::from_secs(50);
pub(super) const RUNTIME_ROOT: &str = "/usr";

#[cfg(target_os = "linux")]
mod profile;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    config: Config,
    intent: EffectIntent,
    #[cfg(target_os = "linux")]
    parent_namespaces: profile::Namespaces,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ready {
    digest: [u8; 32],
    pid: u32,
    #[cfg(target_os = "linux")]
    namespaces: profile::Namespaces,
    profile_digest: [u8; 32],
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BootstrapStage {
    Admission,
    Workspace,
    Capabilities,
    Handshake,
    Payload,
    Receipt,
}

impl BootstrapStage {
    fn label(&self) -> &'static str {
        match self {
            Self::Admission => "admission",
            Self::Workspace => "Workspace identity",
            Self::Capabilities => "kernel capability",
            Self::Handshake => "handshake",
            Self::Payload => "payload",
            Self::Receipt => "receipt",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BootstrapFailure {
    stage: BootstrapStage,
}

pub(super) fn check_available() -> Result<(), ToolError> {
    #[cfg(target_os = "linux")]
    {
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
    #[cfg(not(target_os = "linux"))]
    {
        Err(ToolError::ProtectionUnavailable)
    }
}

pub(super) fn enforcement_digest() -> Result<[u8; 32], ToolError> {
    #[cfg(target_os = "linux")]
    {
        let mut bytes = b"arany-guard-v1:private-mount,user,pid,net,ipc,uts;no-userns;no-new-privs;zero-caps;not-dumpable;cpu25000/100000;memory536870912;swap0;oom-group1;pids64;scratch67108864;workspace67108864;nofile64;core0;no-inherited-host-handles;typed-native-or-fresh-snapshot;owned-cgroup-cleanup;runtime-root-/usr-excludes-workspace-state-account".to_vec();
        bytes.extend_from_slice(&profile::profile_digest()?);
        Ok(digest(&bytes))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(ToolError::ProtectionUnavailable)
    }
}

fn manager_command(executable: &str) -> Command {
    let mut command = Command::new(executable);
    command.env_clear().env("PATH", "/usr/bin:/bin");
    #[cfg(target_os = "linux")]
    {
        let runtime = format!("/run/user/{}", rustix::process::geteuid().as_raw());
        command.env("XDG_RUNTIME_DIR", &runtime).env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={runtime}/bus"),
        );
    }
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
        let dir = super::fs::open_directory(
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
            ("pids.max", MAX_PROCESSES.to_string()),
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
        if members.lines().count() > MAX_PROCESSES as usize
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

pub(super) async fn execute(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    cancellation: RunCancellation,
) -> Result<ToolObservation, ToolError> {
    let mut dispatched = false;
    match execute_inner(runtime, intent, cancellation, &mut dispatched).await {
        Err(error) if !dispatched => Ok(super::unstarted_observation(intent.clone(), error)),
        result => result,
    }
}

async fn execute_inner(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    mut cancellation: RunCancellation,
    dispatched: &mut bool,
) -> Result<ToolObservation, ToolError> {
    let deadline = tokio::time::Instant::now()
        + Duration::from_millis(u64::from(intent.limits.runtime_ms))
        + Duration::from_secs(5);
    super::fs::validate_workspace(runtime)?;
    let workspace = super::fs::open_directory(&runtime.workspace)?;
    let snapshot = super::fs::Snapshot::create(
        &workspace,
        &runtime.config,
        matches!(
            intent.call,
            ToolCall::Command { .. } | ToolCall::McpList { .. } | ToolCall::McpCall { .. }
        ),
    )?;
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
    #[cfg(target_os = "linux")]
    let parent_namespaces = profile::Namespaces::read()?;
    let mut child = command
        .spawn()
        .map_err(|_| ToolError::ProtectionUnavailable)?;
    let mut stdin = child.stdin.take().ok_or(ToolError::Operation)?;
    let mut stdout = BufReader::new(child.stdout.take().ok_or(ToolError::Operation)?);
    let stderr = child.stderr.take().ok_or(ToolError::Operation)?;
    let mut stderr_task = tokio::spawn(read_capped(stderr, CAPTURE_BYTES));
    let execution = async {
        let task = serde_json::to_vec(&Task {
            config: runtime.config.clone(),
            intent: intent.clone(),
            #[cfg(target_os = "linux")]
            parent_namespaces: parent_namespaces.clone(),
        })
        .map_err(|_| ToolError::Configuration)?;
        if task.len() > 96 * 1024 {
            return Err(ToolError::Limit);
        }
        stdin
            .write_all(&task)
            .await
            .map_err(|_| ToolError::Operation)?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|_| ToolError::Operation)?;
        let ready_bytes = read_line_capped(&mut stdout, 2048).await?;
        let ready: Ready =
            serde_json::from_slice(&ready_bytes).map_err(|_| ToolError::ProtectionUnavailable)?;
        if ready.digest != intent.digest() || ready.pid != 1 && ready.pid != 2 {
            return Err(ToolError::ProtectionUnavailable);
        }
        #[cfg(target_os = "linux")]
        if !ready.namespaces.isolated_from(&parent_namespaces)
            || ready.profile_digest != intent.enforcement_digest
        {
            return Err(ToolError::ProtectionUnavailable);
        }
        unit.admit()
            .await
            .map_err(|_| ToolError::GuardRejected("resource attestation"))?;
        *dispatched = true;
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
        let tail = read_capped(stdout, 1).await?;
        if !tail.is_empty() {
            return Err(ToolError::Operation);
        }
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
    let drained = tokio::time::timeout(Duration::from_secs(2), &mut stderr_task).await;
    if drained.is_err() {
        stderr_task.abort();
        let _ = stderr_task.await;
    }
    if result.is_err()
        && let Ok(Ok(Ok(stderr))) = &drained
        && let Ok(failure) = serde_json::from_slice::<BootstrapFailure>(stderr)
    {
        return Err(ToolError::GuardRejected(failure.stage.label()));
    }
    if !matches!(cleanup, Ok(Ok(()))) {
        return Err(ToolError::GuardRejected("process-unit cleanup"));
    }
    if !matches!(reaped, Ok(Ok(_))) {
        return Err(ToolError::GuardRejected("launcher reap"));
    }
    if !matches!(drained, Ok(Ok(Ok(_)))) {
        return Err(ToolError::GuardRejected("stderr EOF"));
    }
    if result.is_err()
        && let Ok(Ok(Ok(stderr))) = &drained
    {
        let diagnostic = std::str::from_utf8(stderr).unwrap_or_default();
        let stage = if diagnostic.contains("bwrap:") {
            "namespace bootstrap"
        } else if diagnostic.contains("Failed to") {
            "native manager"
        } else {
            "bounded control protocol"
        };
        return Err(ToolError::GuardRejected(stage));
    }
    result
}

pub(super) async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    cap: usize,
) -> Result<Vec<u8>, ToolError> {
    let mut output = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let count = reader
            .read(&mut chunk)
            .await
            .map_err(|_| ToolError::Operation)?;
        if count == 0 {
            return Ok(output);
        }
        if output.len() + count > cap {
            return Err(ToolError::Limit);
        }
        output.extend_from_slice(&chunk[..count]);
    }
}

pub(super) async fn read_line_capped<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> Result<Vec<u8>, ToolError> {
    let mut line = Vec::new();
    loop {
        let part = reader.fill_buf().await.map_err(|_| ToolError::Operation)?;
        if part.is_empty() {
            return Err(ToolError::Operation);
        }
        let count = part
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(part.len(), |index| index + 1);
        let complete = part[count - 1] == b'\n';
        if line.len() + count > cap {
            return Err(ToolError::Limit);
        }
        line.extend_from_slice(&part[..count]);
        reader.consume(count);
        if complete {
            return Ok(line);
        }
    }
}

async fn capture(
    mut child: Child,
    cap: usize,
    deadline: Duration,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), ToolError> {
    let out = child.stdout.take().ok_or(ToolError::Operation)?;
    let err = child.stderr.take().ok_or(ToolError::Operation)?;
    let result = tokio::time::timeout(deadline, async {
        tokio::try_join!(
            async { child.wait().await.map_err(|_| ToolError::Operation) },
            read_capped(out, cap),
            read_capped(err, cap)
        )
    })
    .await;
    let error = match result {
        Ok(Ok(value)) => return Ok(value),
        Ok(Err(error)) => error,
        Err(_) => ToolError::Limit,
    };
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
    Err(error)
}

pub(super) fn helper_main() -> ExitCode {
    let mut stage = BootstrapStage::Admission;
    let result = (|| -> Result<(), ToolError> {
        let mut stdin = std::io::stdin().lock();
        let mut task = Vec::new();
        loop {
            let mut byte = [0];
            if stdin.read(&mut byte).map_err(|_| ToolError::Operation)? != 1 {
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
        task.config.validate()?;
        if !task.intent.valid()
            || task.intent.policy_digest != task.config.digest()
            || task.intent.expires_at_ms <= now_ms()
            || !task.config.allows(&task.intent.call)
            || task.intent.enforcement_digest != enforcement_digest()?
        {
            return Err(ToolError::Configuration);
        }
        stage = BootstrapStage::Workspace;
        if matches!(
            task.intent.call,
            ToolCall::List { .. }
                | ToolCall::Read { .. }
                | ToolCall::Search { .. }
                | ToolCall::Write { .. }
                | ToolCall::Edit { .. }
                | ToolCall::Mkdir { .. }
        ) && super::fs::identity(&super::fs::open_directory(Path::new("/workspace"))?)?
            != (task.intent.workspace_device, task.intent.workspace_inode)
        {
            return Err(ToolError::ChangedInput);
        }
        #[cfg(target_os = "linux")]
        {
            stage = BootstrapStage::Capabilities;
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
        }
        stage = BootstrapStage::Handshake;
        let ready = Ready {
            digest: task.intent.digest(),
            pid: std::process::id(),
            #[cfg(target_os = "linux")]
            namespaces: profile::Namespaces::read()?,
            #[cfg(target_os = "linux")]
            profile_digest: enforcement_digest()?,
            #[cfg(not(target_os = "linux"))]
            profile_digest: [0; 32],
        };
        #[cfg(target_os = "linux")]
        if !ready.namespaces.isolated_from(&task.parent_namespaces) {
            return Err(ToolError::ProtectionUnavailable);
        }
        let mut stdout = std::io::stdout().lock();
        serde_json::to_writer(&mut stdout, &ready).map_err(|_| ToolError::Operation)?;
        stdout
            .write_all(b"\n")
            .and_then(|()| stdout.flush())
            .map_err(|_| ToolError::Operation)?;
        let mut go = [0; 3];
        stdin
            .read_exact(&mut go)
            .map_err(|_| ToolError::Operation)?;
        if go != *b"GO\n" || task.intent.expires_at_ms <= now_ms() {
            return Err(ToolError::Configuration);
        }
        drop(stdin);
        stage = BootstrapStage::Payload;
        if matches!(
            task.intent.call,
            ToolCall::Command { .. } | ToolCall::McpList { .. } | ToolCall::McpCall { .. }
        ) {
            super::fs::copy_seed()?;
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| ToolError::Operation)?;
        let result = runtime.block_on(async {
            match &task.intent.call {
                ToolCall::Command { program, args, cwd } => {
                    let definition = task.config.commands.iter().find(|value| &value.name == program).ok_or(ToolError::Configuration)?;
                    let mut command = payload(definition, args, cwd)?;
                    let (status, out, err) = capture(command.spawn().map_err(|_| ToolError::Operation)?, (task.intent.limits.result_bytes as usize / 2).saturating_sub(128), PAYLOAD_DEADLINE).await?;
                    Ok(serde_json::json!({"exit_code":status.code(),"stdout":String::from_utf8(out).map_err(|_| ToolError::Operation)?,"stderr":String::from_utf8(err).map_err(|_| ToolError::Operation)?,"workspace_changes":"discarded"}).to_string())
                }
                ToolCall::Skill { name, resource } => {
                    let path = format!("/skills/{name}/{}", resource.as_deref().unwrap_or("SKILL.md"));
                    let bytes = super::fs::read_regular(super::fs::open_absolute(&path)?, MAX_TOOL_RESULT_BYTES)?;
                    if resource.is_none() || resource.as_deref() == Some("SKILL.md") { super::skills::metadata(name, &bytes)?; }
                    String::from_utf8(bytes).map_err(|_| ToolError::Operation)
                }
                ToolCall::McpList { .. } | ToolCall::McpCall { .. } => super::mcp::execute(&task.intent.call, &task.config).await,
                call => super::fs::native(call, &task.config),
            }
        });
        let (disposition, output) = match result {
            Ok(output) if output.len() <= task.intent.limits.result_bytes as usize => {
                let failed = match &task.intent.call {
                    ToolCall::Command { .. } => serde_json::from_str::<serde_json::Value>(&output)
                        .map_or(true, |value| value["exit_code"] != 0),
                    ToolCall::McpCall { .. } => serde_json::from_str::<serde_json::Value>(&output)
                        .map_or(true, |value| value["is_error"] == true),
                    _ => false,
                };
                (
                    if failed {
                        ToolDisposition::Failed
                    } else {
                        ToolDisposition::Succeeded
                    },
                    output,
                )
            }
            Ok(_) | Err(ToolError::Limit) => (
                ToolDisposition::Limit,
                "Tool result or resource limit reached".into(),
            ),
            Err(ToolError::Conflict) => (
                ToolDisposition::Conflict,
                "File changed or edit did not match exactly once; read it again".into(),
            ),
            Err(ToolError::Path) => (
                ToolDisposition::Denied,
                "File path is unavailable or outside the admitted regular-file boundary".into(),
            ),
            Err(ToolError::Uncertain) => (
                ToolDisposition::Uncertain,
                "Filesystem change may have occurred; inspect current state before another attempt"
                    .into(),
            ),
            Err(_) => (
                ToolDisposition::Failed,
                "Tool operation failed; no automatic retry".into(),
            ),
        };
        stage = BootstrapStage::Receipt;
        let receipt = GuardReceipt {
            contract_version: 1,
            intent_digest: task.intent.digest(),
            enforcement_digest: task.intent.enforcement_digest,
            limits: task.intent.limits.clone(),
        };
        let mut observation = ToolObservation {
            intent: task.intent,
            disposition,
            output,
            guard: Some(receipt),
        };
        if !observation.valid() {
            observation.disposition = ToolDisposition::Limit;
            observation.output = "Tool result exceeded its reserved serialized capacity".into();
        }
        if !observation.valid() {
            return Err(ToolError::Limit);
        }
        serde_json::to_writer(&mut stdout, &observation).map_err(|_| ToolError::Operation)?;
        stdout
            .write_all(b"\n")
            .and_then(|()| stdout.flush())
            .map_err(|_| ToolError::Operation)?;
        Ok(())
    })();
    if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        let _ = serde_json::to_writer(std::io::stderr().lock(), &BootstrapFailure { stage });
        ExitCode::FAILURE
    }
}

pub(super) fn payload(
    program: &super::config::Program,
    args: &[String],
    cwd: &str,
) -> Result<Command, ToolError> {
    let executable = format!("/programs/{}", program.name);
    let bytes =
        super::fs::read_regular(super::fs::open_absolute(&executable)?, MAX_SNAPSHOT_BYTES)?;
    if hex_digest(&bytes) != program.sha256 {
        return Err(ToolError::ChangedInput);
    }
    let mut command = Command::new(executable);
    command
        .args(args)
        .current_dir(Path::new("/workspace").join(cwd))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/scratch")
        .env("TMPDIR", "/scratch")
        .env("CARGO_HOME", "/scratch/cargo")
        .env("CARGO_TARGET_DIR", "/scratch/target")
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(command)
}

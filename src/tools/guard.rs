use super::config::Config;
use super::types::*;
use super::{ToolError, ToolRuntime, now_ms};
use crate::engine::RunCancellation;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{ExitCode, Stdio};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::process::{Child, Command};

const CAPTURE_BYTES: usize = 48 * 1024;
const TASK_BYTES_CAP: usize = 96 * 1024;
const PAYLOAD_DEADLINE: Duration = Duration::from_secs(50);
pub(super) const RUNTIME_ROOT: &str = "/usr";

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod profile;

#[cfg(target_os = "macos")]
mod macos;

pub(super) fn runtime_roots() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["/System/Library", "/usr/lib", "/bin", "/usr/bin"]
    } else {
        &[RUNTIME_ROOT]
    }
}

pub(super) fn native_system_program(path: &str) -> Result<bool, ToolError> {
    #[cfg(target_os = "macos")]
    {
        macos::system_program(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        Ok(false)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    config: Config,
    intent: EffectIntent,
    #[cfg(target_os = "linux")]
    parent_namespaces: profile::Namespaces,
    #[cfg(target_os = "macos")]
    workspace: std::path::PathBuf,
    #[cfg(target_os = "macos")]
    guard_executable: std::path::PathBuf,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapFailure {
    stage: BootstrapStage,
}

impl std::fmt::Display for BootstrapFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.stage.label())
    }
}

impl BootstrapFailure {
    pub(super) fn failure_stage(&self) -> &'static str {
        match self.stage {
            BootstrapStage::Admission => "admission",
            BootstrapStage::Workspace => "pinned input",
            BootstrapStage::Capabilities => "native protection",
            BootstrapStage::Handshake => "handshake",
            BootstrapStage::Payload => "execution",
            BootstrapStage::Receipt => "receipt",
        }
    }
}

/// A Guard stage that failed after bootstrap reported success, or outside its child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardStage {
    ResourceAttestation,
    ProcessCleanup,
    LauncherReap,
    OutputEof,
    NamespaceBootstrap,
    NativeManager,
    ControlProtocol,
}

impl GuardStage {
    pub(super) fn failure_stage(self) -> &'static str {
        match self {
            Self::ResourceAttestation => "resource attestation",
            Self::ProcessCleanup => "process cleanup",
            Self::LauncherReap => "launcher reap",
            Self::OutputEof => "output EOF",
            Self::NamespaceBootstrap => "namespace bootstrap",
            Self::NativeManager => "native manager",
            Self::ControlProtocol => "control protocol",
        }
    }
}

impl std::fmt::Display for GuardStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.failure_stage())
    }
}

pub(super) fn check_available() -> Result<(), ToolError> {
    #[cfg(target_os = "linux")]
    {
        linux::check_available()
    }
    #[cfg(target_os = "macos")]
    {
        macos::check_available()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(ToolError::ProtectionUnavailable)
    }
}

pub(super) fn enforcement_digest() -> Result<[u8; 32], ToolError> {
    #[cfg(target_os = "linux")]
    {
        linux::enforcement_digest()
    }
    #[cfg(target_os = "macos")]
    {
        macos::enforcement_digest()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(ToolError::ProtectionUnavailable)
    }
}

pub(super) async fn execute(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    cancellation: RunCancellation,
) -> Result<ToolObservation, ToolError> {
    let mut dispatched = false;
    #[cfg(target_os = "macos")]
    let result = macos::execute_inner(runtime, intent, cancellation, &mut dispatched).await;
    #[cfg(target_os = "linux")]
    let result = linux::execute_inner(runtime, intent, cancellation, &mut dispatched).await;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let result = Err(ToolError::ProtectionUnavailable);
    match result {
        Err(error) if !dispatched => Ok(super::unstarted_observation(intent.clone(), error)),
        result => result,
    }
}

/// Waits briefly for the launcher's bounded stderr; `None` means it did not reach EOF.
pub(super) async fn drain_stderr(
    task: &mut tokio::task::JoinHandle<Result<Vec<u8>, ToolError>>,
) -> Option<Vec<u8>> {
    match tokio::time::timeout(Duration::from_secs(2), &mut *task).await {
        Ok(Ok(Ok(stderr))) => Some(stderr),
        Ok(_) => None,
        Err(_) => {
            task.abort();
            let _ = task.await;
            None
        }
    }
}

pub(super) fn bootstrap_failure(stderr: &[u8]) -> Option<ToolError> {
    serde_json::from_slice::<BootstrapFailure>(stderr)
        .ok()
        .map(ToolError::GuardBootstrap)
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

fn parse_ready(bytes: &[u8], intent: &EffectIntent) -> Result<Ready, ToolError> {
    let ready: Ready =
        serde_json::from_slice(bytes).map_err(|_| ToolError::ProtectionUnavailable)?;
    if ready.digest != intent.digest() || ready.profile_digest != intent.enforcement_digest {
        return Err(ToolError::ProtectionUnavailable);
    }
    Ok(ready)
}

async fn read_observation<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    intent: &EffectIntent,
) -> Result<ToolObservation, ToolError> {
    let bytes = read_line_capped(reader, CAPTURE_BYTES).await?;
    let observation: ToolObservation =
        serde_json::from_slice(&bytes).map_err(|_| ToolError::Operation)?;
    if !observation.attested_for(intent) || !read_capped(&mut *reader, 1).await?.is_empty() {
        return Err(ToolError::Operation);
    }
    Ok(observation)
}

/// Reaps a child this process owns. On macOS it closes stdin first and polls `try_wait`
/// at the supervision interval, because Seatbelt blocks Tokio's signal wake socket;
/// elsewhere it awaits exit.
pub(super) async fn reap_owned_child(
    child: &mut Child,
) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(target_os = "macos")]
    {
        drop(child.stdin.take());
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            tokio::time::sleep(macos::SUPERVISION_INTERVAL).await;
        }
    }
    #[cfg(not(target_os = "macos"))]
    child.wait().await
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
            async {
                reap_owned_child(&mut child)
                    .await
                    .map_err(|_| ToolError::Operation)
            },
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
    let _ = tokio::time::timeout(Duration::from_secs(1), reap_owned_child(&mut child)).await;
    Err(error)
}

fn encode_task(task: &Task) -> Result<Vec<u8>, ToolError> {
    let mut bytes = serde_json::to_vec(task).map_err(|_| ToolError::Configuration)?;
    if bytes.len() > TASK_BYTES_CAP {
        return Err(ToolError::Limit);
    }
    bytes.push(b'\n');
    Ok(bytes)
}

fn read_task(reader: &mut std::io::BufReader<impl Read>) -> Result<Task, ToolError> {
    let mut task = Vec::new();
    std::io::BufRead::read_until(
        &mut Read::take(&mut *reader, (TASK_BYTES_CAP + 1) as u64),
        b'\n',
        &mut task,
    )
    .map_err(|_| ToolError::Operation)?;
    if task.last() != Some(&b'\n') {
        return Err(if task.len() > TASK_BYTES_CAP {
            ToolError::Limit
        } else {
            ToolError::Operation
        });
    }
    if !reader.buffer().is_empty() {
        return Err(ToolError::Configuration);
    }
    serde_json::from_slice(&task).map_err(|_| ToolError::Configuration)
}

/// Returns the payload root and the Workspace this helper's call operates on.
#[cfg(target_os = "macos")]
fn bootstrap_locations(
    task: &Task,
    stage: &mut BootstrapStage,
) -> Result<(std::path::PathBuf, std::path::PathBuf), ToolError> {
    *stage = BootstrapStage::Capabilities;
    let root = macos::install(task)?;
    let workspace = if task.intent.call.is_process() {
        root.join("workspace")
    } else {
        task.workspace.clone()
    };
    Ok((root, workspace))
}

#[cfg(not(target_os = "macos"))]
fn bootstrap_locations(
    _task: &Task,
    _stage: &mut BootstrapStage,
) -> Result<(std::path::PathBuf, std::path::PathBuf), ToolError> {
    Ok((
        Path::new("/").to_owned(),
        Path::new("/workspace").to_owned(),
    ))
}

pub(super) fn helper_main() -> ExitCode {
    let mut stage = BootstrapStage::Admission;
    let result = (|| -> Result<(), ToolError> {
        #[cfg(target_os = "macos")]
        macos::bootstrap()?;
        let mut stdin = std::io::BufReader::new(std::io::stdin().lock());
        let task = read_task(&mut stdin)?;
        task.config.validate()?;
        if !task.intent.valid()
            || task.intent.policy_digest != task.config.digest()
            || task.intent.expires_at_ms <= now_ms()
            || !task.config.allows(&task.intent.call)
            || task.intent.enforcement_digest != enforcement_digest()?
            || task.intent.limits.resource_profile != ToolLimits::default().resource_profile
        {
            return Err(ToolError::Configuration);
        }
        let (root, workspace) = bootstrap_locations(&task, &mut stage)?;
        let workspace = workspace.as_path();
        stage = BootstrapStage::Workspace;
        if (matches!(
            task.intent.call,
            ToolCall::List { .. }
                | ToolCall::Read { .. }
                | ToolCall::Search { .. }
                | ToolCall::Write { .. }
                | ToolCall::Edit { .. }
                | ToolCall::Mkdir { .. }
        ) || cfg!(target_os = "macos") && matches!(task.intent.call, ToolCall::Skill { .. }))
            && super::fs::identity(&super::fs::open_directory(workspace)?)?
                != (task.intent.workspace_device, task.intent.workspace_inode)
        {
            return Err(ToolError::ChangedInput);
        }
        let skill = if matches!(task.intent.call, ToolCall::Skill { .. }) {
            Some(load_skill_bytes(&task)?)
        } else {
            None
        };
        #[cfg(target_os = "linux")]
        {
            stage = BootstrapStage::Capabilities;
            linux::harden()?;
        }
        stage = BootstrapStage::Handshake;
        let ready = Ready {
            digest: task.intent.digest(),
            pid: std::process::id(),
            #[cfg(target_os = "linux")]
            namespaces: profile::Namespaces::read()?,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            profile_digest: enforcement_digest()?,
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
        #[cfg(not(target_os = "macos"))]
        if task.intent.call.is_process() {
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
                    let mut command = payload(definition, args, cwd, &root)?;
                    let (status, out, err) = capture(command.spawn().map_err(|_| ToolError::Operation)?, (task.intent.limits.result_bytes as usize / 2).saturating_sub(128), PAYLOAD_DEADLINE).await?;
                    Ok(serde_json::json!({"exit_code":status.code(),"stdout":String::from_utf8(out).map_err(|_| ToolError::Operation)?,"stderr":String::from_utf8(err).map_err(|_| ToolError::Operation)?,"workspace_changes":"discarded"}).to_string())
                }
                ToolCall::Skill { name, resource } => skill_text(name, resource.as_deref(), skill.ok_or(ToolError::Configuration)?),
                ToolCall::McpList { .. } | ToolCall::McpCall { .. } => super::mcp::execute(&task.intent.call, &task.config, &root).await,
                call => super::fs::native(workspace, call, &task.config),
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

#[cfg(target_os = "macos")]
pub(super) fn unit_main() -> ExitCode {
    macos::unit_main()
}

fn load_skill_bytes(task: &Task) -> Result<Vec<u8>, ToolError> {
    let ToolCall::Skill { name, resource } = &task.intent.call else {
        return Err(ToolError::Configuration);
    };
    let resource = resource.as_deref().unwrap_or("SKILL.md");
    #[cfg(target_os = "macos")]
    let path = {
        let skill = task
            .config
            .skills
            .iter()
            .find(|skill| &skill.name == name)
            .ok_or(ToolError::Configuration)?;
        Path::new(&skill.directory)
            .join(resource)
            .to_str()
            .ok_or(ToolError::Path)?
            .to_owned()
    };
    #[cfg(not(target_os = "macos"))]
    let path = format!("/skills/{name}/{resource}");
    let bytes = super::fs::read_regular(super::fs::open_absolute(&path)?, MAX_TOOL_RESULT_BYTES)?;
    if task
        .config
        .skills
        .iter()
        .find(|skill| &skill.name == name)
        .and_then(|skill| skill.files.get(resource))
        != Some(&hex_digest(&bytes))
    {
        return Err(ToolError::ChangedInput);
    }
    Ok(bytes)
}

fn skill_text(name: &str, resource: Option<&str>, bytes: Vec<u8>) -> Result<String, ToolError> {
    if resource.unwrap_or("SKILL.md") == "SKILL.md" {
        super::skills::metadata(name, &bytes)?;
    }
    String::from_utf8(bytes).map_err(|_| ToolError::Operation)
}

pub(super) fn payload(
    program: &super::config::Program,
    args: &[String],
    cwd: &str,
    root: &Path,
) -> Result<Command, ToolError> {
    let resources = if cfg!(target_os = "macos") {
        root.join("runtime")
    } else {
        root.to_owned()
    };
    let workspace = root.join("workspace");
    let scratch = root.join("scratch");
    let executable = if native_system_program(&program.executable)? {
        Path::new(&program.executable).to_owned()
    } else {
        resources.join("programs").join(&program.name)
    };
    let bytes = super::fs::read_regular(
        super::fs::open_absolute(executable.to_str().ok_or(ToolError::Path)?)?,
        MAX_SNAPSHOT_BYTES,
    )?;
    if hex_digest(&bytes) != program.sha256 {
        return Err(ToolError::ChangedInput);
    }
    let mut command = Command::new(executable);
    command
        .args(args.iter().map(|arg| {
            if cfg!(target_os = "macos") {
                for (alias, target) in [
                    ("workspace", &workspace),
                    ("scratch", &scratch),
                    ("inputs", &resources.join("inputs")),
                    ("skills", &resources.join("skills")),
                    ("programs", &resources.join("programs")),
                ] {
                    if let Ok(relative) = Path::new(arg).strip_prefix(format!("/{alias}")) {
                        return target.join(relative).into_os_string();
                    }
                }
            }
            std::ffi::OsString::from(arg)
        }))
        .current_dir(workspace.join(cwd))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &scratch)
        .env("TMPDIR", &scratch)
        .env("CARGO_HOME", scratch.join("cargo"))
        .env("CARGO_TARGET_DIR", scratch.join("target"))
        .env("ARANY_TOOL_WORKSPACE", &workspace)
        .env("ARANY_TOOL_INPUTS", resources.join("inputs"))
        .env("ARANY_TOOL_SKILLS", resources.join("skills"))
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(command)
}

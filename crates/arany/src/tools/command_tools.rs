mod import;
pub(crate) mod prefix;
pub use import::{CommandToolImport, import_command_tools};
pub use prefix::{CommandWrap, WrapStage};

use super::config::Program;
use super::toolchains::RuntimeGrant;
use super::{ToolCall, ToolError, ToolRuntime};
use crate::store::{RecordFormat, RecordKind, StateRoot};
use crate::validate::{digest, hex_digest, valid_digest};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

pub const MAX_COMMAND_TOOL_CANDIDATES: usize = 256;
pub const MAX_COMMAND_TOOLS: usize = 8;
pub const PROBE_COMMAND: &str = "cargo test";
pub(crate) const TOOL_NAME: &str = "command-tool";
pub(crate) const TIMEOUT_MS: u32 = (super::permissions::MAX_TIMEOUT_SECONDS * 1000) as u32;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandToolRole {
    Rewrite,
    Prefix,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewriteStage {
    Rewritten,
    Unchanged,
    PinRefused,
    InputLimit,
    NonZeroExit,
    InvalidOutput,
    NotCompleted,
    Limit,
    Uncertain,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRewrite {
    pub hook_intent_id: Option<uuid::Uuid>,
    pub original_sha256: [u8; 32],
    pub tool_sha256: String,
    pub stage: RewriteStage,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandTool {
    pub selected_path: String,
    pub executable: String,
    pub args: Vec<String>,
    pub role: CommandToolRole,
    pub device: u64,
    pub inode: u64,
    pub sha256: String,
    pub enabled: bool,
    pub last_probe: Option<RewriteStage>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandToolProbe {
    pub tool: CommandTool,
    pub intent: super::EffectIntent,
    pub outcome: Option<super::permissions::HookOutcome>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandTools {
    tools: Vec<CommandTool>,
}

pub(crate) const COMMAND_TOOLS: RecordFormat = RecordFormat {
    kind: RecordKind::CommandTools,
    accepts: |bytes| CommandTools::parse(bytes).is_ok(),
};

impl CommandToolRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rewrite => "Rewrite hook",
            Self::Prefix => "Prefix wrapper",
        }
    }
}

impl RewriteStage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rewritten => "rewritten",
            Self::Unchanged => "no rewrite",
            Self::PinRefused => "needs re-pin",
            Self::InputLimit => "input limit",
            Self::NonZeroExit => "non-zero exit",
            Self::InvalidOutput => "malformed output",
            Self::NotCompleted => "not completed",
            Self::Limit => "limit reached",
            Self::Uncertain => "unknown outcome",
        }
    }
}

impl CommandTools {
    pub fn load(state: &StateRoot) -> Result<Self, ToolError> {
        state
            .record(COMMAND_TOOLS.kind)
            .read()
            .map_err(|_| ToolError::Configuration)?
            .map_or_else(|| Ok(Self::default()), |bytes| Self::parse(&bytes))
    }

    pub fn entries(&self) -> &[CommandTool] {
        &self.tools
    }

    pub fn edit(
        state: &StateRoot,
        change: impl FnOnce(&mut Self) -> Result<(), ToolError>,
    ) -> Result<Self, ToolError> {
        state
            .with_record_upgrade_lock(|| {
                let mut record = Self::load(state)
                    .map_err(|_| crate::store::StoreError::InvalidStateDirectory)?;
                let result = change(&mut record);
                if let Err(error) = result {
                    return Ok(Err(error));
                }
                let bytes = serde_json::to_vec(&record).expect("typed record is serializable");
                Self::parse(&bytes).map_err(|_| crate::store::StoreError::InvalidStateDirectory)?;
                state.record(RecordKind::CommandTools).replace(&bytes)?;
                Ok(Ok(record))
            })
            .map_err(|error| match error {
                crate::store::StoreError::ReplaceUnconfirmed => ToolError::Uncertain,
                _ => ToolError::Configuration,
            })?
    }

    pub fn select(&mut self, tool: CommandTool) -> Result<(), ToolError> {
        if self.tools.len() >= MAX_COMMAND_TOOLS {
            return Err(ToolError::Limit);
        }
        if tool.enabled
            || tool.last_probe.is_some()
            || self
                .tools
                .iter()
                .any(|entry| entry.selected_path == tool.selected_path && entry.role == tool.role)
        {
            return Err(ToolError::Configuration);
        }
        self.tools.push(tool);
        Ok(())
    }

    pub fn remove(&mut self, index: usize) -> Result<(), ToolError> {
        if index >= self.tools.len() {
            return Err(ToolError::ChangedInput);
        }
        self.tools.remove(index);
        Ok(())
    }

    pub fn replace(
        &mut self,
        index: usize,
        expected: &CommandTool,
        tool: CommandTool,
    ) -> Result<(), ToolError> {
        let current = self.tools.get_mut(index).ok_or(ToolError::ChangedInput)?;
        if current != expected {
            return Err(ToolError::ChangedInput);
        }
        *current = tool;
        Ok(())
    }

    pub(crate) fn digest(&self) -> [u8; 32] {
        digest(&serde_json::to_vec(self).expect("typed record is serializable"))
    }

    fn parse(bytes: &[u8]) -> Result<Self, ToolError> {
        let value = super::parse_json(bytes, RecordKind::CommandTools.limit())?;
        let record: Self = serde_json::from_value(value).map_err(|_| ToolError::Configuration)?;
        if record.tools.len() > MAX_COMMAND_TOOLS
            || record
                .tools
                .iter()
                .filter(|tool| tool.enabled && tool.role == CommandToolRole::Prefix)
                .nth(1)
                .is_some()
            || record.tools.iter().enumerate().any(|(index, tool)| {
                !tool.valid()
                    || record.tools[..index].iter().any(|prior| {
                        prior.selected_path == tool.selected_path && prior.role == tool.role
                    })
            })
        {
            return Err(ToolError::Configuration);
        }
        Ok(record)
    }
}

impl CommandTool {
    pub const ARGUMENT_LIMIT: usize = super::process::MAX_ARGUMENT_BYTES;
    pub const ARGUMENT_COUNT_LIMIT: usize = super::process::MAX_COMMAND_ARGS;

    pub(crate) fn call(&self) -> ToolCall {
        match self.role {
            CommandToolRole::Rewrite => ToolCall::Command {
                program: TOOL_NAME.into(),
                args: self.args.clone(),
                cwd: String::new(),
                resource_class: None,
                timeout_ms: None,
                additional_reads: None,
            },
            CommandToolRole::Prefix => prefix::probe_call(self),
        }
    }

    pub(crate) fn valid(&self) -> bool {
        super::config::valid_absolute(&self.selected_path)
            && super::config::valid_absolute(&self.executable)
            && self.inode != 0
            && valid_digest(&self.sha256)
            && super::process::valid_command(TOOL_NAME, &self.args, "")
            && (self.role != CommandToolRole::Prefix
                || self.args.len() < Self::ARGUMENT_COUNT_LIMIT)
            && (self.role != CommandToolRole::Prefix || prefix::probe_call(self).valid())
            && (!self.enabled
                || matches!(
                    self.last_probe,
                    Some(RewriteStage::Rewritten | RewriteStage::Unchanged)
                ))
    }

    pub fn pin(
        state: &StateRoot,
        workspace: &Path,
        executable: &str,
        args: Vec<String>,
    ) -> Result<Self, ToolError> {
        if !super::process::valid_command(TOOL_NAME, &args, "") {
            return Err(ToolError::Configuration);
        }
        let granted = granted_directories(state, workspace)?;
        let selected_path = executable.to_owned();
        let (executable, device, inode, sha256) = pin_in(&granted, executable)?;
        let tool = Self {
            selected_path,
            executable,
            args,
            role: CommandToolRole::Rewrite,
            device,
            inode,
            sha256,
            enabled: false,
            last_probe: None,
        };
        if !tool.valid() {
            return Err(ToolError::Configuration);
        }
        Ok(tool)
    }

    pub fn pin_status(&self, state: &StateRoot, workspace: &Path) -> bool {
        granted_directories(state, workspace).is_ok_and(|granted| self.verify(&granted).is_ok())
    }

    pub(crate) fn verify(&self, grants: &[RuntimeGrant]) -> Result<(), ToolError> {
        let (path, device, inode, sha256) = pin_in(grants, &self.selected_path)?;
        if path != self.executable
            || (device, inode, sha256.as_str()) != (self.device, self.inode, self.sha256.as_str())
        {
            return Err(ToolError::ChangedInput);
        }
        Ok(())
    }
}

fn granted_directories(
    state: &StateRoot,
    workspace: &Path,
) -> Result<Vec<RuntimeGrant>, ToolError> {
    let permissions = super::WorkspacePermissions::load(workspace, state)
        .map_err(|_| ToolError::Configuration)?;
    if !permissions.is_trusted() {
        return Err(ToolError::Configuration);
    }
    let account = StateRoot::account_path().map_err(|_| ToolError::Configuration)?;
    let protected = [workspace, state.path(), account.as_path()];
    let saved = permissions
        .runtime_grants
        .iter()
        .filter(|grant| grant.kind.is_multi())
        .cloned()
        .collect::<Vec<_>>();
    for grant in &saved {
        grant.reject_overlap(&protected)?;
    }
    let path = super::toolchains::path_grants(
        &super::CommandEnvironment::load(state)?.path_string(),
        &saved,
        &protected,
    )?;
    Ok(saved.into_iter().chain(path).collect())
}

fn program_root(grant: &RuntimeGrant) -> (&Path, u64, u64) {
    match grant
        .root
        .as_ref()
        .filter(|root| grant.path.starts_with(&root.path))
    {
        Some(root) => (&root.path, root.device, root.inode),
        None => (&grant.path, grant.device, grant.inode),
    }
}

fn pinned_root(grant: &RuntimeGrant) -> Result<(&Path, cap_std::fs::Dir), ToolError> {
    grant.verify_host()?;
    let (path, device, inode) = program_root(grant);
    let directory = super::fs::open_directory(path)?;
    if super::fs::identity(&directory)? != (device, inode) {
        return Err(ToolError::ChangedInput);
    }
    Ok((path, directory))
}

fn pin_in(
    grants: &[RuntimeGrant],
    executable: &str,
) -> Result<(String, u64, u64, String), ToolError> {
    let path = Path::new(executable);
    let grant = grants
        .iter()
        .find(|grant| grant.kind.is_multi() && path.starts_with(program_root(grant).0))
        .ok_or(ToolError::Path)?;
    let (root, directory) = pinned_root(grant)?;
    let relative = path.strip_prefix(root).map_err(|_| ToolError::Path)?;
    let resolved = directory
        .canonicalize(relative)
        .map_err(|_| ToolError::Path)?;
    let file = super::fs::open_file(&directory, resolved.to_str().ok_or(ToolError::Path)?)?;
    let metadata = file.metadata().map_err(|_| ToolError::Path)?;
    if !metadata.is_file() || metadata.mode() & 0o111 == 0 || metadata.mode() & 0o6000 != 0 {
        return Err(ToolError::Path);
    }
    let bytes = super::fs::read_regular(file, super::config::MAX_EXECUTABLE_BYTES as usize)?;
    grant.verify_host()?;
    let path = root
        .join(resolved)
        .to_str()
        .ok_or(ToolError::Path)?
        .to_owned();
    Ok((path, metadata.dev(), metadata.ino(), hex_digest(&bytes)))
}

pub fn command_tool_candidates(
    state: &StateRoot,
    workspace: &Path,
) -> Result<Vec<String>, ToolError> {
    use cap_std::fs::MetadataExt as _;
    let granted = granted_directories(state, workspace)?;
    let mut paths = Vec::new();
    let mut scanned = 0;
    for grant in granted.iter().filter(|grant| grant.kind.on_path()) {
        let (root, access) = pinned_root(grant)?;
        let relative = grant.path.strip_prefix(root).map_err(|_| ToolError::Path)?;
        let relative = if relative.as_os_str().is_empty() {
            Path::new(".")
        } else {
            relative
        };
        let directory = access.open_dir(relative).map_err(|_| ToolError::Path)?;
        if super::fs::identity(&directory)? != (grant.device, grant.inode) {
            return Err(ToolError::ChangedInput);
        }
        for entry in directory.entries().map_err(|_| ToolError::Path)? {
            scanned += 1;
            if scanned > MAX_COMMAND_TOOL_CANDIDATES {
                return Err(ToolError::Limit);
            }
            let entry = entry.map_err(|_| ToolError::Path)?;
            let Ok(resolved) = access.canonicalize(relative.join(entry.file_name())) else {
                continue;
            };
            let metadata = access
                .symlink_metadata(&resolved)
                .map_err(|_| ToolError::Path)?;
            if metadata.is_file() && metadata.mode() & 0o111 != 0 && metadata.mode() & 0o6000 == 0 {
                let path = grant.path.join(entry.file_name());
                paths.push(path.to_str().ok_or(ToolError::Path)?.to_owned());
            }
        }
        grant.verify_host()?;
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

pub(crate) fn result(observation: &super::ToolObservation) -> super::permissions::HookResult {
    use super::permissions::{HookIgnoreReason, HookResult};
    if prefix::is_prefix_probe(&observation.intent) {
        return prefix::probe_result(observation);
    }
    if !matches!(
        observation.disposition,
        super::ToolDisposition::Succeeded | super::ToolDisposition::Failed
    ) || observation.attestation.is_none()
    {
        return HookResult::Ignored {
            why: HookIgnoreReason::NotCompleted,
        };
    }
    let parsed = serde_json::from_str::<super::types::CommandResult>(&observation.output)
        .map_err(|_| RewriteStage::InvalidOutput)
        .and_then(|output| parse_output(output.exit_code, &output.stdout));
    match parsed {
        Ok(Some(command)) => HookResult::Narrowed {
            call: ToolCall::Command {
                program: "sh".into(),
                args: vec!["-c".into(), command],
                cwd: String::new(),
                resource_class: None,
                timeout_ms: None,
                additional_reads: None,
            },
        },
        Ok(None) => HookResult::Passed,
        Err(RewriteStage::NonZeroExit) => HookResult::Ignored {
            why: HookIgnoreReason::NonZeroExit,
        },
        Err(_) => HookResult::Ignored {
            why: HookIgnoreReason::InvalidJson,
        },
    }
}

pub(crate) fn script(call: &ToolCall) -> Option<String> {
    let (program, args) = match call {
        ToolCall::Command { program, args, .. }
        | ToolCall::CommandUnguarded { program, args, .. } => (program, args),
        _ => return None,
    };
    if program == "sh" && args.len() == 2 && args[0] == "-c" {
        return Some(args[1].clone());
    }
    Some(
        std::iter::once(program)
            .chain(args)
            .map(|word| {
                if !word.is_empty()
                    && word
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_./:@,%+-".contains(&byte))
                {
                    word.clone()
                } else {
                    format!("'{}'", word.replace('\'', "'\\''"))
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    )
}

pub(crate) fn input(call: &ToolCall) -> Result<String, ToolError> {
    let command = script(call).ok_or(ToolError::Configuration)?;
    let input = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": command}
    })
    .to_string();
    if input.len() > super::permissions::MAX_HOOK_INPUT_BYTES {
        return Err(ToolError::Limit);
    }
    Ok(input)
}

pub(crate) fn rewritten(call: &ToolCall, command: String) -> ToolCall {
    let args = vec!["-c".into(), command];
    match call {
        ToolCall::Command {
            cwd,
            resource_class,
            timeout_ms,
            ..
        } => ToolCall::Command {
            program: "sh".into(),
            args,
            cwd: cwd.clone(),
            resource_class: *resource_class,
            timeout_ms: *timeout_ms,
            additional_reads: None,
        },
        ToolCall::CommandUnguarded {
            cwd, justification, ..
        } => ToolCall::CommandUnguarded {
            program: "sh".into(),
            args,
            cwd: cwd.clone(),
            justification: justification.clone(),
            prefix_rule: None,
        },
        _ => call.clone(),
    }
}

#[derive(Deserialize)]
struct Reply {
    #[serde(rename = "hookSpecificOutput")]
    specific: Option<Specific>,
}
#[derive(Deserialize)]
struct Specific {
    #[serde(rename = "updatedInput")]
    updated: Option<Updated>,
}
#[derive(Deserialize)]
struct Updated {
    command: String,
}

pub(crate) fn parse_output(
    exit: Option<i32>,
    stdout: &str,
) -> Result<Option<String>, RewriteStage> {
    if exit != Some(0) {
        return Err(RewriteStage::NonZeroExit);
    }
    if stdout.trim().is_empty() {
        return Ok(None);
    }
    let value = super::parse_json(
        stdout.as_bytes(),
        super::permissions::HOOK_RESULT_BYTES as usize,
    )
    .map_err(|_| RewriteStage::InvalidOutput)?;
    let reply: Reply = serde_json::from_value(value).map_err(|_| RewriteStage::InvalidOutput)?;
    let command = reply
        .specific
        .and_then(|specific| specific.updated)
        .map(|updated| updated.command);
    if command.as_ref().is_some_and(|command| {
        command.is_empty()
            || command.contains('\0')
            || command.len() > super::process::MAX_ARGUMENT_BYTES
    }) {
        return Err(RewriteStage::InvalidOutput);
    }
    Ok(command)
}

impl super::permissions::HookResult {
    pub fn command_rewrite(
        &self,
        disposition: super::ToolDisposition,
    ) -> (RewriteStage, Option<&str>) {
        use super::permissions::{HookIgnoreReason, HookResult};
        if disposition == super::ToolDisposition::Limit {
            return (RewriteStage::Limit, None);
        }
        match self {
            HookResult::Narrowed {
                call: ToolCall::Command { program, args, .. },
            } if program == "sh" && args.len() == 2 && args[0] == "-c" => {
                (RewriteStage::Rewritten, Some(&args[1]))
            }
            HookResult::Passed => (RewriteStage::Unchanged, None),
            HookResult::Ignored {
                why: HookIgnoreReason::NonZeroExit,
            } => (RewriteStage::NonZeroExit, None),
            HookResult::Ignored {
                why: HookIgnoreReason::InvalidJson,
            } => (RewriteStage::InvalidOutput, None),
            _ => (RewriteStage::NotCompleted, None),
        }
    }
}

impl ToolRuntime {
    pub(crate) fn command_tools(&self) -> &CommandTools {
        &self.command_tools
    }

    pub(crate) fn command_tool_runtime(
        &self,
        tool: &CommandTool,
        stdin: &str,
    ) -> Result<Self, ToolError> {
        if !tool.valid() {
            return Err(ToolError::Configuration);
        }
        tool.verify(&self.config.runtime_grants)?;
        let mut runtime = self.clone().with_hook_stdin(stdin);
        runtime.config.commands = vec![Program {
            name: match tool.role {
                CommandToolRole::Rewrite => TOOL_NAME,
                CommandToolRole::Prefix => prefix::PREFIX_NAME,
            }
            .into(),
            executable: tool.executable.clone(),
            sha256: tool.sha256.clone(),
            interpreter: false,
            inputs: Vec::new(),
        }];
        runtime.config.git = None;
        runtime.config.mcp.clear();
        runtime.config.skills.clear();
        runtime.config.network = None;
        runtime.config.write = false;
        let policy = super::process_policy(&runtime.config, runtime.guard_available)?;
        runtime.process_enforcement_digest = policy.digest;
        #[cfg(target_os = "macos")]
        {
            runtime.system_programs = policy.programs;
        }
        Ok(runtime)
    }
}

#[cfg(test)]
mod tests;

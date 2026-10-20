use super::ToolError;
use super::fs::{open_absolute, read_regular};
use super::toolchains::{RuntimeGrant, validate_selection};
use super::types::*;
use crate::store::RecordKind;
use crate::store::StateRoot;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub version: u32,
    pub workspace_paths: Vec<String>,
    pub write: bool,
    #[serde(default, skip_serializing_if = "UnguardedGrant::is_denied")]
    pub child_unguarded: UnguardedGrant,
    pub commands: Vec<Program>,
    pub skills: Vec<Skill>,
    pub mcp: Vec<Server>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime_grants: Vec<RuntimeGrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<Program>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Program {
    pub name: String,
    pub executable: String,
    pub sha256: String,
    pub interpreter: bool,
    pub inputs: Vec<ProgramInput>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProgramInput {
    pub path: String,
    pub destination: String,
    pub sha256: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Skill {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_model_invocation: bool,
    pub name: String,
    pub description: String,
    pub directory: String,
    pub files: BTreeMap<String, String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Server {
    pub name: String,
    pub program: Program,
    pub args: Vec<String>,
    pub tools: Vec<String>,
}

impl Config {
    pub(crate) fn load(state: &StateRoot) -> Result<Self, ToolError> {
        let bytes = state
            .record(RecordKind::ToolsConfig)
            .read()
            .map_err(|_| ToolError::Configuration)?
            .ok_or(ToolError::MissingConfiguration)?;
        let value =
            super::skills::parse_json(&bytes, 64 * 1024).map_err(|_| ToolError::Configuration)?;
        let config: Self = serde_json::from_value(value).map_err(|_| ToolError::Configuration)?;
        config.validate()?;
        if !config.runtime_grants.is_empty() {
            return Err(ToolError::Configuration);
        }
        Ok(config)
    }

    pub(crate) fn validate(&self) -> Result<(), ToolError> {
        if self.child_unguarded == UnguardedGrant::HumanOnly && self.commands.is_empty()
            || self.version != 1
            || self.workspace_paths.is_empty()
            || self.workspace_paths.len() > 32
            || self.workspace_paths.iter().any(|path| {
                !valid_relative(path, false) && !(path == "." && self.workspace_paths.len() == 1)
            })
            || self.commands.len() > 8
            || self.skills.len() > MAX_SKILLS
            || self.mcp.len() > 4
        {
            return Err(ToolError::Configuration);
        }
        validate_selection(&self.runtime_grants)?;
        let roots: BTreeSet<_> = self.workspace_paths.iter().collect();
        if roots.len() != self.workspace_paths.len()
            || roots.iter().any(|left| {
                roots
                    .iter()
                    .any(|right| left != right && std::path::Path::new(left).starts_with(right))
            })
        {
            return Err(ToolError::Configuration);
        }
        for program in self
            .commands
            .iter()
            .chain(self.mcp.iter().map(|server| &server.program))
        {
            program.validate()?;
        }
        if let Some(git) = &self.git {
            git.validate()?;
            if git.name != "git"
                || git.interpreter
                || !git.inputs.is_empty()
                || !super::executable::git_root(std::path::Path::new(&git.executable))
            {
                return Err(ToolError::Configuration);
            }
        }
        for server in &self.mcp {
            if !valid_name(&server.name)
                || server.tools.is_empty()
                || server.tools.len() > 64
                || server.tools.iter().collect::<BTreeSet<_>>().len() != server.tools.len()
                || server.tools.iter().any(|tool| {
                    tool.is_empty() || tool.len() > 128 || tool.chars().any(char::is_control)
                })
                || server.args.len() > 64
                || server
                    .args
                    .iter()
                    .any(|arg| arg.len() > 4096 || arg.contains('\0'))
            {
                return Err(ToolError::Configuration);
            }
        }
        for skill in &self.skills {
            if !valid_name(&skill.name)
                || skill.description.trim().is_empty()
                || skill.description.len() > 1024
                || !valid_absolute(&skill.directory)
                || !skill.files.contains_key("SKILL.md")
                || skill.files.len() > 32
                || skill
                    .files
                    .iter()
                    .any(|(path, hash)| !valid_relative(path, false) || !valid_digest(hash))
            {
                return Err(ToolError::Configuration);
            }
        }
        for names in [
            self.commands
                .iter()
                .chain(self.mcp.iter().map(|server| &server.program))
                .map(|item| &item.name)
                .collect::<Vec<_>>(),
            self.skills.iter().map(|item| &item.name).collect(),
            self.mcp.iter().map(|item| &item.name).collect(),
        ] {
            if names.iter().collect::<BTreeSet<_>>().len() != names.len() {
                return Err(ToolError::Configuration);
            }
        }
        Ok(())
    }

    pub(super) fn resource_paths(&self) -> impl Iterator<Item = &str> {
        self.commands
            .iter()
            .chain(self.mcp.iter().map(|server| &server.program))
            .chain(self.git.iter())
            .flat_map(|program| {
                std::iter::once(program.executable.as_str())
                    .chain(program.inputs.iter().map(|input| input.path.as_str()))
            })
            .chain(self.skills.iter().map(|skill| skill.directory.as_str()))
    }

    // Git output spans the repository, so typed Git reads require the whole-Workspace grant.
    pub(crate) fn git_available(&self) -> bool {
        self.git.is_some() && self.workspace_paths == ["."]
    }

    pub(super) fn discover_skills(&mut self, workspace: &std::path::Path) -> Result<(), ToolError> {
        if !self.permits_project_skills() {
            return Ok(());
        }
        super::skills::discover(workspace, &mut self.skills)?;
        if serde_json::to_vec(self)
            .map_err(|_| ToolError::Configuration)?
            .len()
            > 64 * 1024
        {
            return Err(ToolError::Limit);
        }
        self.validate()
    }

    pub(super) fn permits_project_skills(&self) -> bool {
        self.workspace_paths
            .iter()
            .any(|path| path == "." || std::path::Path::new(".agents/skills").starts_with(path))
    }

    pub(crate) fn digest(&self) -> [u8; 32] {
        digest(&serde_json::to_vec(self).expect("typed config is serializable"))
    }

    pub(super) fn receipt(&self) -> ToolPolicyReceipt {
        ToolPolicyReceipt {
            contract_version: 1,
            config_digest: self.digest(),
            permissions_digest: None,
            max_context_bytes: MAX_TOOL_CONTEXT_BYTES as u32,
            unguarded_rules_digest: None,
            unguarded: if self.commands.is_empty() {
                UnguardedGrant::Denied
            } else {
                UnguardedGrant::HumanOnly
            },
            child_unguarded: self.child_unguarded,
        }
    }

    pub(crate) fn in_workspace_scope(&self, path: &str) -> bool {
        self.workspace_paths == ["."]
            || self
                .workspace_paths
                .iter()
                .any(|root| std::path::Path::new(path).starts_with(root))
    }

    pub(crate) fn allows(&self, call: &ToolCall) -> bool {
        if !call.valid() || call.mutates() && !self.write {
            return false;
        }
        let path = match call {
            ToolCall::PrepareWorker { .. }
            | ToolCall::ApplyChangeset {
                source:
                    crate::session::worker::ChangesetSource::WorkerHandover { .. }
                    | crate::session::worker::ChangesetSource::Rewind { .. },
            } => return false,
            ToolCall::ParseAgentDefinition { .. } => return true,
            ToolCall::List { path }
            | ToolCall::Read { path, .. }
            | ToolCall::Search { path, .. }
            | ToolCall::Mkdir { path }
            | ToolCall::Write { path, .. }
            | ToolCall::Edit { path, .. } => Some(path),
            ToolCall::Command { program, .. } | ToolCall::CommandUnguarded { program, .. } => {
                return self.commands.iter().any(|value| &value.name == program);
            }
            ToolCall::ApplyChangeset {
                source: crate::session::worker::ChangesetSource::CommandReceipt { changeset, .. },
            } => {
                return changeset
                    .files
                    .iter()
                    .all(|change| self.in_workspace_scope(&change.path));
            }
            ToolCall::Skill { name, resource } => {
                return self.skills.iter().any(|value| {
                    &value.name == name
                        && value
                            .files
                            .contains_key(resource.as_deref().unwrap_or("SKILL.md"))
                });
            }
            ToolCall::McpList { server } => {
                return self.mcp.iter().any(|value| &value.name == server);
            }
            ToolCall::GitStatus { .. } | ToolCall::GitDiff { .. } | ToolCall::GitLog { .. } => {
                return self.git_available();
            }
            ToolCall::McpCall { server, tool, .. } => {
                return self
                    .mcp
                    .iter()
                    .any(|value| &value.name == server && value.tools.contains(tool));
            }
        };
        let path = path.expect("file operation has path");
        (path.is_empty() || path == ".") && !call.mutates() || self.in_workspace_scope(path)
    }
}

impl Program {
    fn validate(&self) -> Result<(), ToolError> {
        if !valid_name(&self.name)
            || !valid_absolute(&self.executable)
            || !valid_digest(&self.sha256)
            || self.inputs.len() > 16
            || self.inputs.iter().any(|input| {
                !valid_absolute(&input.path)
                    || !valid_relative(&input.destination, false)
                    || !valid_digest(&input.sha256)
            })
            || self
                .inputs
                .iter()
                .map(|input| &input.destination)
                .collect::<BTreeSet<_>>()
                .len()
                != self.inputs.len()
        {
            return Err(ToolError::Configuration);
        }
        Ok(())
    }

    pub(super) fn verify(&self) -> Result<(), ToolError> {
        let file = open_absolute(&self.executable)?;
        let bytes = read_regular(file, MAX_SNAPSHOT_BYTES)?;
        let native_binary =
            super::executable::has_magic(&bytes, super::executable::EXECUTABLES.program_magics);
        if hex_digest(&bytes) != self.sha256 || !native_binary {
            return Err(ToolError::ChangedInput);
        }
        for input in &self.inputs {
            if hex_digest(&read_regular(open_absolute(&input.path)?, MAX_FILE_BYTES)?)
                != input.sha256
            {
                return Err(ToolError::ChangedInput);
            }
        }
        Ok(())
    }
}

pub(crate) fn valid_absolute(value: &str) -> bool {
    value
        .strip_prefix('/')
        .is_some_and(|value| valid_relative(value, false))
}

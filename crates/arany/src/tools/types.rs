use super::catalog::{Family, Site};
use crate::session::worker::{ChangesetSource, WorkerDataRef, WorkerMutation, WorkerPreparation};
use crate::session::{AgentRunId, EffectTarget, RunId};
use crate::validate::{digest, forbidden, hex_digest, valid_digest, valid_relative};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

// Write content or combined Edit/MultiEdit strings, in decoded UTF-8 bytes.
pub(crate) const MAX_EDIT_CONTENT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_MULTI_EDITS: usize = 64;
// Six escaped bytes per content byte plus room for the path, digest and keys.
pub const MAX_TOOL_ARGUMENT_BYTES: usize = 400 * 1024;
// Ordinary operations use this bound; command-receipt integration has its own ceiling.
pub(crate) const MAX_ORDINARY_TOOL_ARGUMENT_BYTES: usize = 16 * 1024;
pub const MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;
pub const DEFAULT_PR_STATE: PrState = PrState::Open;
pub const DEFAULT_PR_LIMIT: u8 = 30;
pub const MAX_PR_LIST_LIMIT: u8 = 50;
pub(crate) const MAX_READ_PAGE_LINES: u32 = 2000;
pub(crate) const MAX_READ_LINE_CHARS: usize = 2000;
// ToolObservation::valid checks every producer against this serialized bound.
pub(crate) const MAX_TOOL_OBSERVATION_BYTES: usize =
    2 * MAX_TOOL_RESULT_BYTES + MAX_TOOL_ARGUMENT_BYTES;
pub const MAX_TOOL_CONTEXT_BYTES: usize = crate::session::MAX_CONTEXT_CONTENT_BYTES;
pub const MAX_SKILLS: usize = 64;
pub(crate) const MAX_TOOL_CATALOG_BYTES: usize = 16 * 1024;
pub(crate) const MIN_TOOL_CONTEXT_BYTES: usize = MAX_TOOL_CATALOG_BYTES + 512;
pub(crate) const MAX_FILE_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const MAX_SNAPSHOT_FILES: usize = 2048;
pub(crate) const COMMAND_SNAPSHOT_BYTES: usize = 128 * 1024 * 1024;
pub(crate) const COMMAND_SNAPSHOT_FILES: usize = 16_384;
pub(crate) const COMMAND_SCAN_ENTRIES: usize = 131_072;
pub(crate) const COMMAND_SNAPSHOT_DEPTH: usize = 32;
pub(crate) const COMMAND_PATH_METADATA_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const DEFAULT_COMMAND_PROJECT_BYTES: u64 = 256 * 1024 * 1024;
pub(crate) const MAX_RUNTIME_BYTES: usize = 96 * 1024 * 1024;
pub(crate) const SCRATCH_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MEMORY_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const MAX_PROCESSES_OR_THREADS: u32 = 64;
pub const MAX_BUILD_MEMORY_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const MAX_BUILD_PROJECT_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_BUILD_CACHE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const MAX_BUILD_PROCESSES_OR_THREADS: u32 = 256;
pub const MAX_COMMAND_TIMEOUT_MS: u32 = 600_000;
const BYTES_PER_MEBIBYTE: u64 = 1024 * 1024;
pub(crate) const MAX_CHANGESET_FILES: usize = 64;
pub(crate) const MAX_CHANGESET_BYTES: usize = 32 * 1024;
pub(crate) const MAX_CHANGESET_SERIALIZED_BYTES: usize = 48 * 1024;
pub(crate) const MAX_COMMAND_INTEGRATION_ARGUMENT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileVersion {
    pub sha256: String,
    pub content: String,
    pub executable: bool,
}

impl FileVersion {
    pub(crate) fn from_bytes(bytes: &[u8], executable: bool) -> Self {
        Self {
            sha256: hex_digest(bytes),
            content: base64::engine::general_purpose::STANDARD.encode(bytes),
            executable,
        }
    }

    pub fn bytes(&self) -> Option<Vec<u8>> {
        self.bytes_with_limit(MAX_CHANGESET_BYTES)
    }

    pub fn bytes_with_limit(&self, limit: usize) -> Option<Vec<u8>> {
        if !valid_digest(&self.sha256) || self.content.len() > limit.div_ceil(3) * 4 {
            return None;
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.content)
            .ok()?;
        (bytes.len() <= limit
            && hex_digest(&bytes) == self.sha256
            && base64::engine::general_purpose::STANDARD.encode(&bytes) == self.content)
            .then_some(bytes)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileChange {
    pub path: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub before: Option<FileVersion>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub after: Option<FileVersion>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandChangeset {
    pub files: Vec<FileChange>,
}

impl CommandChangeset {
    pub(crate) fn valid(&self) -> bool {
        if self.files.len() > MAX_CHANGESET_FILES {
            return false;
        }
        let mut bytes = 0usize;
        for (index, change) in self.files.iter().enumerate() {
            if !valid_relative(&change.path, false)
                || change.path.split('/').count() > COMMAND_SNAPSHOT_DEPTH
                || change.before == change.after
                || self.files[..index].iter().any(|other| {
                    other.path >= change.path || Path::new(&change.path).starts_with(&other.path)
                })
            {
                return false;
            }
            for version in change.before.iter().chain(change.after.iter()) {
                let Some(content) = version.bytes() else {
                    return false;
                };
                bytes = bytes.saturating_add(content.len());
                if bytes > MAX_CHANGESET_BYTES {
                    return false;
                }
            }
        }
        serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= MAX_CHANGESET_SERIALIZED_BYTES)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputTotal {
    pub bytes: u64,
    pub lines: u64,
}

impl OutputTotal {
    pub(crate) fn valid_for(self, text: &str) -> bool {
        super::output::valid_total(self, text)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandResult {
    #[serde(deserialize_with = "Option::deserialize")]
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_total: OutputTotal,
    pub stderr_total: OutputTotal,
    pub changeset: CommandChangeset,
    #[serde(deserialize_with = "Option::deserialize")]
    pub selection: Option<CommandSelectionSummary>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSelectionSummary {
    pub ignored_entries: u64,
    pub pruned_directories: u64,
    pub oversized_inputs: u64,
    pub discarded_outputs: u64,
}

impl CommandSelectionSummary {
    pub(crate) fn valid(self) -> bool {
        let scanned = COMMAND_SCAN_ENTRIES as u64;
        self.pruned_directories <= self.ignored_entries
            && self
                .ignored_entries
                .checked_add(self.oversized_inputs)
                .is_some_and(|count| count <= scanned)
            && self.discarded_outputs <= scanned
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheUpdate {
    NotApplicable,
    Discarded,
    NotAttempted,
    Published,
    Rejected,
    Failed,
    Uncertain,
}

impl CommandResult {
    pub(crate) fn fit_migrated(&self, budget: usize) -> Option<String> {
        if !self.changeset.valid() || !self.selection.is_none_or(CommandSelectionSummary::valid) {
            return None;
        }
        let stdout = super::output::Window::from_recorded(&self.stdout, self.stdout_total)?;
        let stderr = super::output::Window::from_recorded(&self.stderr, self.stderr_total)?;
        super::output::fit_streams(&stdout, &stderr, budget, |stdout, stderr| {
            serde_json::to_string(&Self {
                stdout: stdout.into(),
                stderr: stderr.into(),
                ..self.clone()
            })
            .map_err(|_| super::ToolError::Operation)
        })
        .ok()
    }

    pub(crate) fn valid(&self) -> bool {
        self.changeset.valid()
            && self.selection.is_none_or(CommandSelectionSummary::valid)
            && self.stdout_total.valid_for(&self.stdout)
            && self.stderr_total.valid_for(&self.stderr)
            && serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= MAX_TOOL_RESULT_BYTES)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiteralEdit {
    pub old_string: String,
    pub new_string: String,
    #[serde(default, deserialize_with = "or_default")]
    pub replace_all: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolCall {
    ParseAgentDefinition {
        index: u16,
        raw_digest: [u8; 32],
    },
    List {
        path: String,
    },
    Read {
        path: String,
        #[serde(default, deserialize_with = "Option::deserialize")]
        offset: Option<u32>,
        #[serde(default, deserialize_with = "Option::deserialize")]
        limit: Option<u32>,
    },
    // Omitted options take their defaults and serialize omitted, so default calls keep one canonical form.
    Search {
        path: String,
        query: String,
        #[serde(
            default,
            skip_serializing_if = "is_false",
            deserialize_with = "or_default"
        )]
        regex: bool,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "required_nullable"
        )]
        include: Option<String>,
        #[serde(
            default = "yes",
            skip_serializing_if = "is_true",
            deserialize_with = "or_true"
        )]
        case_sensitive: bool,
        #[serde(
            default,
            skip_serializing_if = "is_zero",
            deserialize_with = "or_default"
        )]
        offset: u32,
        #[serde(
            default = "search_limit",
            skip_serializing_if = "is_search_limit",
            deserialize_with = "or_search_limit"
        )]
        limit: u32,
    },
    Mkdir {
        path: String,
    },
    Write {
        path: String,
        #[serde(deserialize_with = "required_nullable")]
        expected_digest: Option<String>,
        content: String,
    },
    Edit {
        path: String,
        expected_digest: String,
        old: String,
        new: String,
    },
    #[serde(rename = "multiedit")]
    MultiEdit {
        file_path: String,
        expected_digest: String,
        edits: Vec<LiteralEdit>,
    },
    Delete {
        path: String,
        expected_digest: String,
    },
    Move {
        source: String,
        destination: String,
        expected_digest: String,
    },
    Command {
        program: String,
        args: Vec<String>,
        cwd: String,
    },
    CommandUnguarded {
        program: String,
        args: Vec<String>,
        cwd: String,
        justification: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix_rule: Option<Vec<String>>,
    },
    ApplyChangeset {
        source: ChangesetSource,
    },
    PrepareWorker {
        preparation: WorkerPreparation,
    },
    Skill {
        name: String,
        #[serde(deserialize_with = "required_nullable")]
        resource: Option<String>,
    },
    McpList {
        server: String,
    },
    McpCall {
        server: String,
        tool: String,
        schema_digest: String,
        arguments: String,
    },
    GitStatus {
        path: String,
    },
    GitDiff {
        path: String,
        #[serde(deserialize_with = "required_nullable")]
        base: Option<String>,
        #[serde(deserialize_with = "required_nullable")]
        head: Option<String>,
        staged: bool,
    },
    GitLog {
        path: String,
        #[serde(deserialize_with = "required_nullable")]
        revision: Option<String>,
        max_count: u32,
    },
    GitCommit {
        message: String,
        paths: Vec<String>,
        staged: bool,
    },
    GitBranch {
        #[serde(deserialize_with = "required_nullable")]
        name: Option<String>,
        #[serde(deserialize_with = "required_nullable")]
        start_point: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_count: Option<u32>,
    },
    GitSwitch {
        branch: String,
    },
    GitPush {
        remote: String,
        branch: String,
    },
    GhPrCreate {
        repository: String,
        base: String,
        head: String,
        title: String,
        body: String,
        draft: bool,
    },
    GhPrList {
        repository: String,
        #[serde(default = "default_pr_state", deserialize_with = "pr_state_or_default")]
        state: PrState,
        #[serde(default, deserialize_with = "required_nullable")]
        base: Option<String>,
        #[serde(default, deserialize_with = "required_nullable")]
        head: Option<String>,
        #[serde(default = "default_pr_limit", deserialize_with = "pr_limit_or_default")]
        limit: u8,
    },
    GhPrView {
        repository: String,
        number: u32,
    },
    // Null or omitted path and limit normalize to their defaults before intent hashing.
    Glob {
        pattern: String,
        #[serde(
            default = "super::glob::default_path",
            deserialize_with = "super::glob::path_or_default"
        )]
        path: String,
        #[serde(
            default = "super::glob::default_limit",
            deserialize_with = "super::glob::limit_or_default"
        )]
        limit: u32,
    },
}

fn required_nullable<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Closed,
    Merged,
    All,
}

impl PrState {
    pub const ALL: [Self; 4] = [Self::Open, Self::Closed, Self::Merged, Self::All];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Merged => "merged",
            Self::All => "all",
        }
    }
}

fn default_pr_state() -> PrState {
    DEFAULT_PR_STATE
}

fn pr_state_or_default<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<PrState, D::Error> {
    Ok(Option::<PrState>::deserialize(deserializer)?.unwrap_or_else(default_pr_state))
}

fn default_pr_limit() -> u8 {
    DEFAULT_PR_LIMIT
}

fn pr_limit_or_default<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u8, D::Error> {
    Ok(Option::<u8>::deserialize(deserializer)?.unwrap_or_else(default_pr_limit))
}

pub const MAX_SEARCH_ROWS: u32 = 100;
pub(crate) const MAX_SEARCH_OFFSET: u32 = MAX_SNAPSHOT_BYTES as u32;

fn or_default<'de, D: serde::Deserializer<'de>, T: Deserialize<'de> + Default>(
    deserializer: D,
) -> Result<T, D::Error> {
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

fn or_true<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    Option::<bool>::deserialize(deserializer).map(|value| value.unwrap_or(true))
}

fn or_search_limit<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    Option::<u32>::deserialize(deserializer).map(|value| value.unwrap_or(MAX_SEARCH_ROWS))
}

fn yes() -> bool {
    true
}

fn search_limit() -> u32 {
    MAX_SEARCH_ROWS
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

fn is_search_limit(value: &u32) -> bool {
    *value == MAX_SEARCH_ROWS
}

pub(crate) fn parse_mcp_arguments(arguments: &str) -> Result<Value, super::ToolError> {
    let value = super::skills::parse_json(arguments.as_bytes(), 8 * 1024)?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(super::ToolError::Operation)
    }
}

impl ToolCall {
    pub(crate) const fn is_host_pr_read(&self) -> bool {
        matches!(self, Self::GhPrList { .. } | Self::GhPrView { .. })
    }

    pub(crate) fn is_process(&self) -> bool {
        self.facts().spawns_process
    }

    pub fn is_host(&self) -> bool {
        self.facts().site == Site::Host
    }

    pub fn is_unguarded(&self) -> bool {
        self.facts().site == Site::Unguarded
    }

    // Engine preparation steps over snapshot-pinned input; never a model Tool, so no hook sees or narrows them.
    pub(crate) fn is_internal(&self) -> bool {
        self.facts().family == Family::Preparation
    }

    pub(crate) fn is_git(&self) -> bool {
        self.facts().family == Family::Git
    }

    pub(crate) fn is_child_research(&self) -> bool {
        self.facts().researcher
    }

    pub(crate) fn argument_limit(&self) -> usize {
        match self {
            Self::Write { .. } | Self::Edit { .. } | Self::MultiEdit { .. } => {
                MAX_TOOL_ARGUMENT_BYTES
            }
            Self::ApplyChangeset {
                source: ChangesetSource::CommandReceipt { .. },
            } => MAX_COMMAND_INTEGRATION_ARGUMENT_BYTES,
            _ => MAX_ORDINARY_TOOL_ARGUMENT_BYTES,
        }
    }

    pub(crate) fn path_argument(&self) -> Option<(&str, bool)> {
        match self {
            Self::List { path } | Self::Search { path, .. } | Self::Glob { path, .. } => {
                Some((path, true))
            }
            Self::Read { path, .. }
            | Self::Mkdir { path }
            | Self::Write { path, .. }
            | Self::Edit { path, .. }
            | Self::Delete { path, .. }
            | Self::MultiEdit {
                file_path: path, ..
            } => Some((path, false)),
            Self::Command { cwd, .. } | Self::CommandUnguarded { cwd, .. } => Some((cwd, true)),
            Self::Move { .. }
            | Self::ParseAgentDefinition { .. }
            | Self::ApplyChangeset { .. }
            | Self::PrepareWorker { .. }
            | Self::Skill { .. }
            | Self::McpList { .. }
            | Self::McpCall { .. }
            | Self::GitStatus { .. }
            | Self::GitDiff { .. }
            | Self::GitLog { .. }
            | Self::GitCommit { .. }
            | Self::GitBranch { .. }
            | Self::GitSwitch { .. }
            | Self::GitPush { .. }
            | Self::GhPrCreate { .. }
            | Self::GhPrList { .. }
            | Self::GhPrView { .. } => None,
        }
    }

    pub(crate) fn valid(&self) -> bool {
        if !serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= self.argument_limit()) {
            return false;
        }
        match self.facts().family {
            Family::File => super::fs::valid(self),
            Family::Process => super::process::valid(self),
            Family::Skill => super::skills::valid(self),
            Family::Mcp => super::mcp::valid(self),
            Family::Git => super::git::valid(self),
            Family::Preparation => {
                matches!(self, Self::ParseAgentDefinition { index, .. } if usize::from(*index) < 128)
                    || matches!(self, Self::PrepareWorker { preparation } if preparation.valid())
            }
        }
    }

    pub(crate) fn mutates(&self) -> bool {
        self.facts().mutates
    }

    pub(crate) fn is_goal_plan_prelude(&self) -> bool {
        self.facts().prelude
    }

    pub fn label(&self) -> &'static str {
        self.facts().name
    }
}

pub(crate) const MAX_GIT_LOG_COMMITS: u32 = 50;
pub(crate) fn validate_mention_path(value: &str, root: bool) -> Result<(), super::ToolError> {
    if Path::new(value).is_absolute() || value.split('/').any(|part| part == "..") {
        return Err(super::ToolError::OutsideWorkspace.at_path(value));
    }
    if value.split('/').any(forbidden) {
        return Err(super::ToolError::ProtectedPath.at_path(value));
    }
    if !valid_relative(value, root) {
        return Err(super::ToolError::Path.at_path(value));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicyReceipt {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_prefixes: Vec<super::permissions::HostPrefixRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_epoch: Option<Uuid>,
    pub contract_version: u32,
    pub config_digest: [u8; 32],
    #[serde(deserialize_with = "Option::deserialize")]
    pub permissions_digest: Option<[u8; 32]>,
    pub max_context_bytes: u32,
    #[serde(deserialize_with = "Option::deserialize")]
    pub unguarded_rules_digest: Option<[u8; 32]>,
    pub unguarded: UnguardedGrant,
    pub child_unguarded: UnguardedGrant,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnguardedGrant {
    #[default]
    Denied,
    HumanOnly,
}

impl UnguardedGrant {
    pub const fn narrows(self, parent: Self) -> bool {
        matches!(self, Self::Denied) || matches!(parent, Self::HumanOnly)
    }

    pub(crate) fn is_denied(&self) -> bool {
        *self == Self::Denied
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResourceClass {
    #[default]
    Default,
    Build,
}

impl ToolPolicyReceipt {
    pub(crate) fn valid(&self) -> bool {
        self.contract_version == 1
            && self.host_prefixes.len() <= super::permissions::MAX_RULES
            && (self.host_prefixes.is_empty() || self.permissions_digest.is_some())
            && self
                .host_prefixes
                .iter()
                .enumerate()
                .all(|(index, rule)| rule.valid() && !self.host_prefixes[..index].contains(rule))
            && self.config_digest != [0; 32]
            && self.permissions_digest != Some([0; 32])
            && self.unguarded_rules_digest != Some([0; 32])
            && self.child_unguarded == UnguardedGrant::Denied
            && self
                .host_epoch
                .is_none_or(|epoch| epoch.get_version() == Some(uuid::Version::SortRand))
            && (MIN_TOOL_CONTEXT_BYTES as u32..=crate::session::MAX_WINDOW_CONTENT_BYTES as u32)
                .contains(&self.max_context_bytes)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCommandPin {
    pub state_roots: [String; 2],
    #[serde(deserialize_with = "Option::deserialize")]
    pub permissions_digest: Option<[u8; 32]>,
    pub workspace: String,
    pub executable: String,
    pub device: u64,
    pub inode: u64,
    pub sha256: String,
    pub path: String,
    pub environment_digest: [u8; 32],
}

#[cfg(test)]
pub(crate) fn test_host_pin() -> HostCommandPin {
    HostCommandPin {
        state_roots: ["/state".into(), "/account".into()],
        permissions_digest: None,
        workspace: "/test-workspace".into(),
        executable: "/usr/bin/sh".into(),
        device: 1,
        inode: 2,
        sha256: "a".repeat(64),
        path: "/usr/bin:/bin".into(),
        environment_digest: [4; 32],
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectIntent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostIntent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_command: Option<HostCommandPin>,
    pub target: EffectTarget,
    pub id: Uuid,
    pub run_id: RunId,
    pub agent_run_id: AgentRunId,
    pub policy_digest: [u8; 32],
    pub enforcement_digest: [u8; 32],
    pub workspace_device: u64,
    pub workspace_inode: u64,
    pub call: ToolCall,
    pub limits: ToolLimits,
    pub use_count: u8,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostIntent {
    pub source_oid: String,
    pub refspec: String,
}

impl EffectIntent {
    pub fn effective_timeout_ms(&self) -> u32 {
        let cap = match self.call {
            ToolCall::GhPrList { .. } | ToolCall::GhPrView { .. } => GIT_TIMEOUT_MS,
            _ if self.call.is_host() => 600_000,
            _ if self.call.is_git() => GIT_TIMEOUT_MS,
            ToolCall::McpList { .. } | ToolCall::McpCall { .. } => MCP_TIMEOUT_MS,
            _ => self.limits.runtime_ms,
        };
        self.limits.runtime_ms.min(cap)
    }

    pub(crate) fn digest(&self) -> [u8; 32] {
        digest(&serde_json::to_vec(self).expect("typed intent is serializable"))
    }

    pub(crate) fn valid(&self) -> bool {
        self.target.valid()
            && self.id.get_version() == Some(uuid::Version::SortRand)
            && self.policy_digest != [0; 32]
            && self.enforcement_digest != [0; 32]
            && self.workspace_inode > 0
            && self.use_count == 1
            && self.call.valid()
            && (self.limits.network == NetworkGrant::Host) == self.call.is_host()
            && self
                .host
                .as_ref()
                .is_none_or(|host| super::git::host::valid_pin(&self.call, host))
            && self.limits.valid()
            && self
                .host_command
                .as_ref()
                .is_none_or(|host| self.call.is_unguarded() && host.valid())
    }
}

pub(super) const GIT_TIMEOUT_MS: u32 = 20_000;
pub(crate) const MCP_TIMEOUT_MS: u32 = 40_000;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolLimits {
    pub result_bytes: u32,
    pub runtime_ms: u32,
    pub cache_transfer_ms: u32,
    pub memory_bytes: u64,
    pub max_processes: u32,
    #[serde(default = "one_job", skip_serializing_if = "is_one_job")]
    pub build_jobs: u32,
    pub scratch_bytes: u64,
    pub project_bytes: u64,
    pub cache_requested_bytes: u64,
    pub cache_bytes: u64,
    pub cache_candidate_bytes: u64,
    pub network: NetworkGrant,
    pub resource_class: ToolResourceClass,
    pub resource_profile: ToolResourceProfile,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResourceProfile {
    LinuxKernel,
    MacosSupervised,
}

pub(crate) const DEFAULT_COMMAND_TIMEOUT_MS: u32 = 120_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResourceSettings {
    pub default_command_timeout_ms: u32,
    pub max_command_timeout_ms: u32,
    pub build_memory_bytes: u64,
    pub build_processes: u32,
    #[serde(default = "one_setting", skip_serializing_if = "is_one_setting")]
    pub build_jobs: Option<u32>,
    pub build_project_bytes: u64,
    pub build_cache_bytes: u64,
}

impl Default for ToolResourceSettings {
    fn default() -> Self {
        Self {
            default_command_timeout_ms: DEFAULT_COMMAND_TIMEOUT_MS,
            max_command_timeout_ms: MAX_COMMAND_TIMEOUT_MS,
            build_memory_bytes: MAX_BUILD_MEMORY_BYTES,
            build_processes: MAX_BUILD_PROCESSES_OR_THREADS,
            build_jobs: one_setting(),
            build_project_bytes: MAX_BUILD_PROJECT_BYTES,
            build_cache_bytes: MAX_BUILD_CACHE_BYTES,
        }
    }
}

impl ToolResourceSettings {
    pub fn valid(self) -> bool {
        (1_000..=self.max_command_timeout_ms).contains(&self.default_command_timeout_ms)
            && (1_000..=MAX_COMMAND_TIMEOUT_MS).contains(&self.max_command_timeout_ms)
            && (1..=MAX_BUILD_MEMORY_BYTES).contains(&self.build_memory_bytes)
            && (1..=MAX_BUILD_PROCESSES_OR_THREADS).contains(&self.build_processes)
            && self.build_jobs != Some(0)
            && (BYTES_PER_MEBIBYTE..=MAX_BUILD_PROJECT_BYTES).contains(&self.build_project_bytes)
            && self.build_project_bytes.is_multiple_of(BYTES_PER_MEBIBYTE)
            && self.build_cache_bytes <= MAX_BUILD_CACHE_BYTES
            && (self.build_cache_bytes == 0
                || self.build_cache_bytes.is_multiple_of(BYTES_PER_MEBIBYTE))
    }
}

const fn one_job() -> u32 {
    1
}
fn is_one_job(jobs: &u32) -> bool {
    *jobs == one_job()
}
const fn one_setting() -> Option<u32> {
    Some(one_job())
}
fn is_one_setting(jobs: &Option<u32>) -> bool {
    *jobs == one_setting()
}

pub(crate) fn command_timeout_limit_message(requested_ms: u32, maximum_ms: u32) -> String {
    format!(
        "Timeout {requested_ms} ms exceeds pinned max {maximum_ms} ms. Lower request or change /settings; Goal remains pinned."
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkGrant {
    None,
    Host,
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            result_bytes: MAX_TOOL_RESULT_BYTES as u32,
            runtime_ms: 60_000,
            cache_transfer_ms: 0,
            memory_bytes: MEMORY_BYTES,
            max_processes: MAX_PROCESSES_OR_THREADS,
            build_jobs: one_job(),
            scratch_bytes: SCRATCH_BYTES,
            project_bytes: SCRATCH_BYTES,
            cache_requested_bytes: crate::store::BUILD_CACHE_ARCHIVE_BYTES,
            cache_bytes: crate::store::BUILD_CACHE_ARCHIVE_BYTES,
            cache_candidate_bytes: crate::store::BUILD_CACHE_ARCHIVE_BYTES,
            network: NetworkGrant::None,
            resource_class: ToolResourceClass::Default,
            resource_profile: if cfg!(target_os = "macos") {
                ToolResourceProfile::MacosSupervised
            } else {
                ToolResourceProfile::LinuxKernel
            },
        }
    }
}

impl ToolLimits {
    fn valid(&self) -> bool {
        (128..=MAX_TOOL_RESULT_BYTES as u32).contains(&self.result_bytes)
            && (1_000..=MAX_COMMAND_TIMEOUT_MS).contains(&self.runtime_ms)
            && (1..=self.max_processes).contains(&self.build_jobs)
            && (self.resource_class == ToolResourceClass::Build || self.build_jobs == one_job())
            && self.scratch_bytes == SCRATCH_BYTES
            && match self.resource_class {
                ToolResourceClass::Default => {
                    self.memory_bytes == MEMORY_BYTES
                        && self.max_processes == MAX_PROCESSES_OR_THREADS
                        && matches!(
                            self.project_bytes,
                            SCRATCH_BYTES | DEFAULT_COMMAND_PROJECT_BYTES
                        )
                        && self.cache_requested_bytes <= crate::store::BUILD_CACHE_ARCHIVE_BYTES
                        && self.cache_bytes <= crate::store::BUILD_CACHE_ARCHIVE_BYTES
                        && self.cache_bytes <= self.cache_requested_bytes
                        && self.cache_candidate_bytes <= self.cache_bytes
                        && self
                            .cache_candidate_bytes
                            .is_multiple_of(BYTES_PER_MEBIBYTE)
                        && (self.cache_transfer_ms == 0
                            || self.cache_transfer_ms == self.runtime_ms)
                        && (self.cache_bytes != 0 || self.cache_transfer_ms == 0)
                }
                ToolResourceClass::Build => {
                    (1..=MAX_BUILD_MEMORY_BYTES).contains(&self.memory_bytes)
                        && (1..=MAX_BUILD_PROCESSES_OR_THREADS).contains(&self.max_processes)
                        && (BYTES_PER_MEBIBYTE..=MAX_BUILD_PROJECT_BYTES)
                            .contains(&self.project_bytes)
                        && self.project_bytes.is_multiple_of(BYTES_PER_MEBIBYTE)
                        && self.cache_requested_bytes <= MAX_BUILD_CACHE_BYTES
                        && (self.cache_requested_bytes == 0
                            || self
                                .cache_requested_bytes
                                .is_multiple_of(BYTES_PER_MEBIBYTE))
                        && self.cache_bytes <= MAX_BUILD_CACHE_BYTES
                        && self.cache_bytes <= self.cache_requested_bytes
                        && self.cache_candidate_bytes <= self.cache_bytes
                        && (self.cache_bytes == 0
                            || self.cache_bytes.is_multiple_of(BYTES_PER_MEBIBYTE))
                        && self
                            .cache_candidate_bytes
                            .is_multiple_of(BYTES_PER_MEBIBYTE)
                        && self.cache_transfer_ms
                            == if self.cache_bytes == 0 {
                                0
                            } else {
                                self.runtime_ms
                            }
                }
            }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDisposition {
    Succeeded,
    Denied,
    Failed,
    Conflict,
    Limit,
    Cancelled,
    Uncertain,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardReceipt {
    pub contract_version: u32,
    pub intent_digest: [u8; 32],
    pub enforcement_digest: [u8; 32],
    pub limits: ToolLimits,
}

impl GuardReceipt {
    pub(crate) fn valid_for(&self, intent: &EffectIntent) -> bool {
        self.contract_version == 1
            && self.intent_digest == intent.digest()
            && self.enforcement_digest == intent.enforcement_digest
            && self.limits == intent.limits
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileReceipt {
    pub intent_digest: [u8; 32],
    pub before: Option<[u8; 32]>,
    pub after: Option<[u8; 32]>,
    pub executable: bool,
}

impl FileReceipt {
    pub(crate) fn enforcement_digest() -> [u8; 32] {
        digest(b"arany-in-process-file-v1")
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnguardedReceipt {
    pub contract_version: u32,
    pub intent_digest: [u8; 32],
    pub executable_sha256: String,
    pub pid: u32,
    #[serde(deserialize_with = "Option::deserialize")]
    pub exit_code: Option<i32>,
    pub descendants_confirmed_stopped: bool,
}

impl UnguardedReceipt {
    pub(crate) fn valid_for(&self, intent: &EffectIntent) -> bool {
        self.contract_version == 1
            && self.intent_digest == intent.digest()
            && valid_digest(&self.executable_sha256)
    }
}

/// Who executed an operation: the Engine in process, the Guard with its receipt, or an approved host run.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Attestation {
    InProcess {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        receipt: Option<FileReceipt>,
        policy_digest: [u8; 32],
        workspace_device: u64,
        workspace_inode: u64,
    },
    Guard(GuardReceipt),
    WorkerCommand {
        receipt: GuardReceipt,
    },
    WorkerRead {
        intent_digest: [u8; 32],
        #[serde(deserialize_with = "Option::deserialize")]
        guard: Option<Box<GuardReceipt>>,
    },
    WorkerMutation {
        receipt: GuardReceipt,
        mutation: Box<WorkerMutation>,
    },
    WorkerData {
        #[serde(deserialize_with = "Option::deserialize")]
        guard: Option<Box<GuardReceipt>>,
        intent_digest: [u8; 32],
        data: WorkerDataRef,
    },
    Unguarded(UnguardedReceipt),
    Host(UnguardedReceipt),
}

impl Attestation {
    pub(crate) fn valid_for(&self, intent: &EffectIntent) -> bool {
        match self {
            Self::WorkerCommand { receipt } => {
                matches!(intent.target, EffectTarget::WorkerView { .. })
                    && matches!(intent.call, ToolCall::Command { .. })
                    && receipt.valid_for(intent)
            }
            Self::WorkerRead {
                intent_digest,
                guard,
            } => {
                matches!(intent.target, EffectTarget::WorkerView { .. })
                    && !intent.call.mutates()
                    && intent.call.is_child_research()
                    && *intent_digest == intent.digest()
                    && match intent.call.facts().site {
                        Site::InProcess => {
                            guard.is_none()
                                || super::discovery::possible(&intent.call)
                                    && guard.as_ref().is_some_and(|r| r.valid_for(intent))
                        }
                        Site::Guard => guard.as_ref().is_some_and(|r| r.valid_for(intent)),
                        Site::Unguarded | Site::Host => false,
                    }
            }
            Self::WorkerMutation { receipt, mutation } => {
                matches!(intent.target, EffectTarget::WorkerView { revision, .. } if revision == mutation.previous_revision && revision.checked_add(1) == Some(mutation.next_revision) && mutation.next_revision <= 16)
                    && matches!(
                        intent.call,
                        ToolCall::Mkdir { .. }
                            | ToolCall::Write { .. }
                            | ToolCall::Edit { .. }
                            | ToolCall::MultiEdit { .. }
                            | ToolCall::Delete { .. }
                            | ToolCall::Move { .. }
                    )
                    && receipt.valid_for(intent)
                    && mutation.data.assignment_id == intent.agent_run_id
                    && mutation.data.valid()
                    && mutation.data.purpose == crate::session::worker::WorkerDataPurpose::Mutation
            }
            Self::WorkerData {
                guard,
                intent_digest,
                data,
            } => {
                matches!(&intent.call, ToolCall::PrepareWorker { preparation } if preparation.assignment_id == data.assignment_id && (preparation.purpose == data.purpose || preparation.purpose == crate::session::worker::WorkerDataPurpose::Handover && data.purpose == crate::session::worker::WorkerDataPurpose::Mutation))
                    && *intent_digest == intent.digest()
                    && data.valid()
                    && match intent.call.site() {
                        Site::InProcess => guard.is_none(),
                        Site::Guard => guard.as_ref().is_some_and(|r| r.valid_for(intent)),
                        Site::Unguarded | Site::Host => false,
                    }
            }
            Self::InProcess {
                receipt,
                policy_digest,
                workspace_device,
                workspace_inode,
            } => {
                intent.target == EffectTarget::Workspace
                    && intent.call.facts().site == Site::InProcess
                    && (!intent.call.facts().mutates || super::fs::in_process_mutation(intent))
                    && !matches!(intent.call, ToolCall::List { .. } | ToolCall::Glob { .. })
                    && receipt.as_ref().is_none_or(|receipt| {
                        intent.call.facts().mutates && receipt.intent_digest == intent.digest()
                    })
                    && *policy_digest == intent.policy_digest
                    && *workspace_device == intent.workspace_device
                    && *workspace_inode == intent.workspace_inode
            }
            Self::Guard(receipt) => {
                intent.target == EffectTarget::Workspace
                    && !matches!(intent.call, ToolCall::PrepareWorker { .. })
                    && (intent.call.facts().site == Site::Guard
                        || super::discovery::possible(&intent.call)
                        || intent.call.facts().family == super::catalog::Family::File
                            && intent.call.facts().mutates
                            && !super::fs::in_process_mutation(intent))
                    && receipt.valid_for(intent)
            }
            Self::Host(receipt) => {
                intent.target == EffectTarget::Workspace
                    && intent.call.is_host()
                    && intent.host.is_some()
                    && receipt.valid_for(intent)
            }
            Self::Unguarded(receipt) => {
                intent.target == EffectTarget::Workspace
                    && intent.call.facts().site == Site::Unguarded
                    && intent
                        .host_command
                        .as_ref()
                        .is_some_and(|pin| pin.sha256 == receipt.executable_sha256)
                    && receipt.valid_for(intent)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn for_site(intent: &EffectIntent) -> Self {
        let site = if matches!(intent.call, ToolCall::List { .. } | ToolCall::Glob { .. })
            || intent.call.facts().mutates
                && intent.call.facts().family == super::catalog::Family::File
                && !super::fs::in_process_mutation(intent)
        {
            Site::Guard
        } else {
            intent.call.facts().site
        };
        match site {
            Site::InProcess => Self::InProcess {
                receipt: None,
                policy_digest: intent.policy_digest,
                workspace_device: intent.workspace_device,
                workspace_inode: intent.workspace_inode,
            },
            Site::Guard => Self::Guard(GuardReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                enforcement_digest: intent.enforcement_digest,
                limits: intent.limits.clone(),
            }),
            Site::Host => Self::Host(UnguardedReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                executable_sha256: "0".repeat(64),
                pid: 1,
                exit_code: Some(0),
                descendants_confirmed_stopped: true,
            }),
            Site::Unguarded => Self::Unguarded(UnguardedReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                executable_sha256: intent
                    .host_command
                    .as_ref()
                    .map_or_else(|| "0".repeat(64), |pin| pin.sha256.clone()),
                pid: 1,
                exit_code: Some(0),
                descendants_confirmed_stopped: true,
            }),
        }
    }

    pub fn guard(&self) -> Option<&GuardReceipt> {
        match self {
            Self::Guard(receipt) => Some(receipt),
            Self::InProcess { .. }
            | Self::Unguarded(_)
            | Self::Host(_)
            | Self::WorkerCommand { .. }
            | Self::WorkerRead { .. }
            | Self::WorkerMutation { .. }
            | Self::WorkerData { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementCause {
    FilesystemDenied,
    GuardBootstrap,
}

impl EnforcementCause {
    pub fn diagnostic(self) -> &'static str {
        match self {
            Self::FilesystemDenied => "Guard enforcement: protected path or set-ID entry refused",
            Self::GuardBootstrap => "Guard enforcement: bootstrap failed before dispatch",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitCause {
    Timeout,
    OutputCeiling,
    ContextFull,
    ReadCapacity,
    RequestedTime,
    Resource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalCause {
    Cancelled,
    Paused,
    ChannelUnavailable,
    Declined,
    ChildTimeout,
}

#[derive(Clone, Copy)]
pub(crate) enum DenialFeedback {
    UserDeclined,
    ChannelUnavailable,
    Cancelled,
    Paused,
    ChildTimeout,
    RuleDenied,
    ModeDenied,
    HookDenied,
}

impl DenialFeedback {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::UserDeclined => "user_declined",
            Self::ChannelUnavailable => "channel_unavailable",
            Self::Cancelled => "cancelled",
            Self::Paused => "paused",
            Self::ChildTimeout => "delegation_limit",
            Self::RuleDenied => "permission_rule_denied",
            Self::ModeDenied => "permission_mode_denied",
            Self::HookDenied => "hook_denied",
        }
    }

    pub(crate) fn output(self) -> &'static str {
        match self {
            Self::UserDeclined => "user_declined: no operation was dispatched",
            Self::ChannelUnavailable => "channel_unavailable: no operation was dispatched",
            Self::Cancelled => "Run cancelled before this operation; no operation was dispatched",
            Self::Paused => "Goal paused before this operation; no operation was dispatched",
            Self::ChildTimeout => "Child approval wait timed out; no operation was dispatched",
            Self::RuleDenied => "permission_rule_denied: no operation was dispatched",
            Self::ModeDenied => "permission_mode_denied: no operation was dispatched",
            Self::HookDenied => "hook_denied: no operation was dispatched",
        }
    }
}

impl ApprovalCause {
    pub(crate) fn denial_feedback(self) -> DenialFeedback {
        match self {
            Self::Cancelled => DenialFeedback::Cancelled,
            Self::Paused => DenialFeedback::Paused,
            Self::ChannelUnavailable => DenialFeedback::ChannelUnavailable,
            Self::Declined => DenialFeedback::UserDeclined,
            Self::ChildTimeout => DenialFeedback::ChildTimeout,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolObservation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spill: Option<super::spill::OutputSpill>,
    pub intent: EffectIntent,
    pub disposition: ToolDisposition,
    #[serde(deserialize_with = "Option::deserialize")]
    pub enforcement_cause: Option<EnforcementCause>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub limit_cause: Option<LimitCause>,
    pub output: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub attestation: Option<Attestation>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub approval_cause: Option<ApprovalCause>,
    pub cache_update: CacheUpdate,
}

const GUARD_ALIASES: [&str; 8] = [
    "/workspace",
    "/scratch",
    "/cache",
    "/seed",
    "/programs",
    "/inputs",
    "/skills",
    "/arany",
];

// Inside the Guard these roots name private mounts; on the host the same bytes name other or no paths.
pub(crate) fn uses_guard_alias(args: &[String]) -> bool {
    // The Guard exports these variables; the host environment sets only ARANY_TOOL_WORKSPACE.
    if args.iter().any(|arg| {
        arg.match_indices("ARANY_TOOL_")
            .any(|(at, _)| !arg[at..].starts_with("ARANY_TOOL_WORKSPACE"))
    }) {
        return true;
    }
    let name_byte = |byte: u8| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte);
    args.iter().any(|arg| {
        let bytes = arg.as_bytes();
        GUARD_ALIASES.iter().any(|alias| {
            arg.match_indices(alias).any(|(at, _)| {
                let before = at.checked_sub(1).map(|index| bytes[index]);
                let after = bytes.get(at + alias.len()).copied();
                !before.is_some_and(name_byte) && !after.is_some_and(name_byte)
            })
        })
    })
}

impl ToolObservation {
    pub(super) fn cache_publication_eligible(&self) -> bool {
        self.intent.target == EffectTarget::Workspace
            && self.intent.call.is_process()
            && self.attestation.is_some()
            && (self.disposition == ToolDisposition::Succeeded
                || self.disposition == ToolDisposition::Limit
                    && self.limit_cause == Some(LimitCause::Timeout)
                    && matches!(self.attestation, Some(Attestation::Guard(_)))
                    && matches!(self.intent.call, ToolCall::Command { .. })
                    && self.intent.limits.resource_class == ToolResourceClass::Build
                    && serde_json::from_str::<CommandResult>(&self.output)
                        .is_ok_and(|result| result.valid() && result.changeset.files.is_empty()))
    }

    pub fn enforcement_diagnostic(&self) -> Option<&'static str> {
        self.enforcement_cause.map(|cause| {
            if cause == EnforcementCause::FilesystemDenied
                && super::fs::in_process_mutation(&self.intent)
            {
                "File enforcement: protected path or set-ID entry refused"
            } else {
                cause.diagnostic()
            }
        })
    }

    pub(crate) fn attested_for(&self, intent: &EffectIntent) -> bool {
        self.intent == *intent && self.valid() && self.attestation.is_some()
    }
    pub(crate) fn stops_run(&self) -> bool {
        if matches!(self.intent.target, EffectTarget::WorkerView { .. })
            && matches!(self.intent.call, ToolCall::Command { .. })
        {
            return matches!(
                self.disposition,
                ToolDisposition::Uncertain | ToolDisposition::Cancelled
            ) || self.cache_update == CacheUpdate::Uncertain;
        }
        matches!(
            self.disposition,
            ToolDisposition::Uncertain | ToolDisposition::Cancelled
        ) || (self.disposition == ToolDisposition::Limit
            && self.limit_cause == Some(LimitCause::Resource)
            && self.attestation.is_none()
            && self.intent.target == EffectTarget::Workspace
            && matches!(&self.intent.call, ToolCall::Command { .. }))
            || (self.disposition == ToolDisposition::Failed
                && self.attestation.is_none()
                && self.enforcement_cause != Some(EnforcementCause::GuardBootstrap)
                && !matches!(&self.intent.call, ToolCall::PrepareWorker { preparation } if preparation.purpose == crate::session::worker::WorkerDataPurpose::Base))
            || self.cache_update == CacheUpdate::Uncertain
    }

    pub(crate) fn valid(&self) -> bool {
        let private_command = matches!(self.intent.target, EffectTarget::WorkerView { .. })
            && matches!(self.intent.call, ToolCall::Command { .. });
        self.intent.valid()
            && self.spill.as_ref().is_none_or(|spill| spill.valid(self))
            && (self.approval_cause.is_none()
                || self.disposition == ToolDisposition::Denied
                    && self.attestation.is_none()
                    && self.cache_update == CacheUpdate::NotApplicable)
            && (self.limit_cause.is_some() == (self.disposition == ToolDisposition::Limit))
            && (self.disposition != ToolDisposition::Limit
                || self.attestation.is_some()
                || self.cache_update == CacheUpdate::NotApplicable)
            && (private_command
                || !matches!(
                    self.intent.call,
                    ToolCall::Command { .. } | ToolCall::CommandUnguarded { .. }
                )
                || !(self.disposition == ToolDisposition::Succeeded
                    || serde_json::from_str::<Value>(&self.output)
                        .is_ok_and(|v| v.get("exit_code").is_some()))
                || serde_json::from_str::<CommandResult>(&self.output).is_ok_and(|r| r.valid()))
            && (!matches!(
                self.intent.call,
                ToolCall::McpCall { .. } | ToolCall::McpList { .. }
            ) || self.disposition != ToolDisposition::Succeeded
                || super::output::valid_mcp(&self.output))
            && (!private_command
                || matches!(
                    self.cache_update,
                    CacheUpdate::Discarded | CacheUpdate::NotApplicable | CacheUpdate::NotAttempted
                ))
            && (!private_command
                || self.attestation.is_none()
                || self.cache_update == CacheUpdate::Discarded)
            && (!private_command
                || self.attestation.is_none()
                || serde_json::from_str::<crate::session::worker::WorkerCommandResult>(
                    &self.output,
                )
                .is_ok_and(|r| {
                    r.valid()
                        && (self.disposition == ToolDisposition::Succeeded
                            || matches!(
                                r.private_capture,
                                crate::session::worker::PrivateCapture::None {}
                            ))
                }))
            && self.output.len() <= self.intent.limits.result_bytes as usize
            && serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= MAX_TOOL_OBSERVATION_BYTES)
            && self
                .attestation
                .as_ref()
                .is_none_or(|attestation| attestation.valid_for(&self.intent))
            && (self.disposition != ToolDisposition::Succeeded || self.attestation.is_some())
            && (!super::fs::in_process_mutation(&self.intent)
                || self.disposition != ToolDisposition::Succeeded
                || matches!(&self.attestation, Some(Attestation::InProcess { receipt: Some(receipt), .. })
                    if super::fs::file_receipt(&self.intent, &self.output).is_ok_and(|expected| expected == *receipt)))
            && match self.enforcement_cause {
                None => true,
                Some(EnforcementCause::FilesystemDenied) => {
                    self.disposition == ToolDisposition::Denied && self.attestation.is_some()
                }
                Some(EnforcementCause::GuardBootstrap) => {
                    self.disposition == ToolDisposition::Failed
                        && self.attestation.is_none()
                        && !super::fs::in_process_mutation(&self.intent)
                        && (self.intent.call.facts().site == super::catalog::Site::Guard
                            || super::discovery::possible(&self.intent.call)
                            || self.intent.call.facts().family == super::catalog::Family::File
                                && self.intent.call.facts().mutates)
                }
            }
            && (!self.intent.call.is_host()
                || self.cache_update == CacheUpdate::NotApplicable
                    && self.enforcement_cause.is_none()
                    && match &self.attestation {
                        Some(Attestation::Host(receipt)) => {
                            receipt.pid > 0
                                && (self.disposition == ToolDisposition::Uncertain
                                    || self.disposition == ToolDisposition::Failed
                                        && self.intent.call.is_host_pr_read()
                                        && receipt.descendants_confirmed_stopped
                                        && valid_host_pr_read_failure(self, receipt)
                                    || self.disposition == ToolDisposition::Failed
                                        && matches!(self.intent.call, ToolCall::GitPush { .. })
                                        && receipt.exit_code == Some(1)
                                        && receipt.descendants_confirmed_stopped
                                        && self.output == super::git::host::NON_FAST_FORWARD
                                    || self.disposition == ToolDisposition::Succeeded
                                        && receipt.exit_code == Some(0)
                                        && receipt.descendants_confirmed_stopped
                                        && match self.intent.call {
                                            ToolCall::GhPrList { limit, .. } => {
                                                super::git::host::valid_pr_list_output(
                                                    &self.output,
                                                    limit,
                                                )
                                            }
                                            ToolCall::GhPrView {
                                                ref repository,
                                                number,
                                            } => super::git::host::valid_pr_view_output(
                                                &self.output,
                                                super::git::host::PrViewIdentity {
                                                    repository: repository.clone(),
                                                    number,
                                                },
                                            ),
                                            _ => true,
                                        })
                        }
                        None => {
                            self.disposition != ToolDisposition::Succeeded
                                && !(matches!(self.intent.call, ToolCall::GhPrView { .. })
                                    && self.output == super::git::host::PR_VIEW_RESULT_BUDGET)
                        }
                        _ => false,
                    })
            && (!self.intent.call.is_unguarded()
                || self.cache_update == CacheUpdate::NotApplicable
                    && self.enforcement_cause.is_none())
            && match self.cache_update {
                CacheUpdate::Discarded => private_command && self.attestation.is_some(),
                CacheUpdate::NotApplicable => true,
                CacheUpdate::NotAttempted | CacheUpdate::Uncertain => self.intent.call.is_process(),
                CacheUpdate::Published | CacheUpdate::Rejected | CacheUpdate::Failed => {
                    self.cache_publication_eligible()
                }
            }
    }
}

fn valid_host_pr_read_failure(observation: &ToolObservation, receipt: &UnguardedReceipt) -> bool {
    match &observation.intent.call {
        ToolCall::GhPrList { .. } => {
            observation.output == super::git::host::PR_LIST_CAPTURE_LIMIT
                || observation.output == super::git::host::PR_LIST_FAILED
                    && receipt.exit_code != Some(0)
                || observation.output == super::git::host::PR_LIST_INVALID
                    && receipt.exit_code == Some(0)
        }
        ToolCall::GhPrView { .. } => {
            observation.output == super::git::host::PR_VIEW_CAPTURE_LIMIT
                || observation.output == super::git::host::PR_VIEW_FAILED
                || (observation.output == super::git::host::PR_VIEW_INVALID
                    || observation.output == super::git::host::PR_VIEW_RESULT_BUDGET)
                    && receipt.exit_code == Some(0)
        }
        _ => false,
    }
}

/// One Tool call the Provider asked for; the id correlates its result in the next request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRequest {
    pub id: crate::provider::ToolUseId,
    pub call: ToolCall,
}

/// One Provider turn and the observations of the requests it declared, in request order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolStep {
    pub cleared: bool,
    pub refusal: Option<crate::session::ProposalRefusal>,
    pub refusal_index: Option<usize>,
    pub narrowed_observations: std::collections::BTreeSet<usize>,
    pub plan: Option<crate::provider::PlanRequest>,
    pub turn: crate::provider::ProviderTurn,
    /// Empty for a Delegate step.
    pub requests: Vec<ToolRequest>,
    /// Empty for a refused proposal; otherwise the acknowledged request prefix and Engine integrations.
    pub observations: Vec<ToolObservation>,
    /// Goal facts written when the next call is prepared; `None` for Researchers.
    pub facts: Option<crate::provider::GoalContextFacts>,
    /// Notes first delivered with this step's results.
    pub steering_notes: Vec<String>,
    /// Researcher Tool calls left once this step is answered; `None` for the primary.
    pub calls_remaining: Option<usize>,
    /// PostToolUse feedback lines by observation index, re-derived from `HookFinished` results.
    pub hook_feedback: std::collections::BTreeMap<usize, Vec<String>>,
}

/// Live Run state only: replayed turns are never serialized or journaled.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolContext {
    pub invoked_skill: Option<ToolObservation>,
    catalog: String,
    git_reads: bool,
    pub history_summary: Option<String>,
    pub steps: Vec<ToolStep>,
    pub stop_bytes: usize,
}

impl ToolStep {
    pub(crate) fn feedback(&self, index: usize) -> &[String] {
        self.hook_feedback.get(&index).map_or(&[], Vec::as_slice)
    }

    // A Tool a hook narrowed is denied and directly followed by its replacement, which answers the request.
    pub(crate) fn narrowed(&self, index: usize) -> bool {
        self.narrowed_observations.contains(&index)
    }
}

pub(crate) const UNDISPATCHED_STEP_OUTPUT: &str =
    "not_dispatched: step stopped before this call; no operation was dispatched";
pub(crate) const MAX_TOOL_REFUSAL_BYTES: usize = 2048;
const RESULT_ENVELOPE_BYTES: usize = 128;

fn skill_context_bytes(skill: &ToolObservation) -> usize {
    model_observation(skill).to_string().len() + RESULT_ENVELOPE_BYTES
}

impl ToolContext {
    pub fn new(catalog: Value) -> Self {
        let mut context = Self {
            invoked_skill: None,
            catalog: String::new(),
            git_reads: false,
            history_summary: None,
            steps: Vec::new(),
            stop_bytes: 0,
        };
        context.set_catalog(&catalog);
        context
    }

    pub fn catalog(&self) -> &str {
        &self.catalog
    }

    pub fn set_catalog(&mut self, catalog: &Value) {
        self.catalog = catalog.to_string();
        self.git_reads = self.catalog.len() <= MAX_TOOL_CATALOG_BYTES
            && super::catalog::has_git_reads(&super::catalog::catalog_call_schemas(catalog));
    }

    fn command_git_hint(&self, call: &ToolCall) -> Option<String> {
        (self.git_reads && matches!(call, ToolCall::Command { .. }))
            .then(|| super::catalog::COMMAND_GIT_HINT.concat())
    }

    pub(crate) fn model_result(&self, step: &ToolStep, index: usize) -> Value {
        let observation = &step.observations[index];
        if step.cleared {
            cleared_model_result(observation)
        } else {
            let mut value = model_result(observation, step.feedback(index));
            if let Some(hint) = self.command_git_hint(&observation.intent.call)
                && value["output"]["exit_code"]
                    .as_i64()
                    .is_some_and(|code| code != 0)
                && value["output"]["stderr"]
                    .as_str()
                    .is_some_and(|stderr| stderr.contains("not a git repository"))
            {
                value["hint"] = json!(hint);
                if value.to_string().len() > MAX_TOOL_RESULT_BYTES {
                    value.as_object_mut().expect("model result").remove("hint");
                }
            }
            value
        }
    }

    fn step_len(&self, step: &ToolStep) -> usize {
        step.turn.len()
            + usize::from(step.refusal.is_some())
                * step.requests.len().max(1)
                * (MAX_TOOL_REFUSAL_BYTES + RESULT_ENVELOPE_BYTES)
            + step.plan.as_ref().map_or(0, |plan| {
                plan.result().to_string().len() + RESULT_ENVELOPE_BYTES
            })
            + step
                .observations
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    self.model_result(step, index).to_string().len() + RESULT_ENVELOPE_BYTES
                })
                .sum::<usize>()
            + step
                .facts
                .and_then(|facts| serde_json::to_string(&facts).ok())
                .map_or(0, |facts| facts.len() + RESULT_ENVELOPE_BYTES)
            + step.steering_notes.iter().map(String::len).sum::<usize>()
    }

    pub(crate) fn with_step_len(&self, step: &ToolStep) -> usize {
        self.rendered_len() + self.step_len(step)
    }

    pub(crate) fn with_invoked_skill_len(&self, skill: &ToolObservation) -> usize {
        self.rendered_len() + skill_context_bytes(skill)
    }

    /// Researcher Tool calls the next request may still make: the newest step's count, else the
    /// catalog's own; `None` for the primary.
    pub fn calls_remaining(&self) -> Option<usize> {
        match self.steps.last() {
            Some(step) => step.calls_remaining,
            None => serde_json::from_str::<Value>(&self.catalog)
                .ok()?
                .get("tool_calls_remaining")?
                .as_u64()
                .and_then(|count| usize::try_from(count).ok()),
        }
    }

    /// A researcher with no Tool calls left may only Answer.
    pub fn answer_only(&self) -> bool {
        self.calls_remaining() == Some(0)
    }

    /// Every observation of the live steps, in order.
    pub fn observations(&self) -> Vec<&ToolObservation> {
        self.steps
            .iter()
            .flat_map(|step| &step.observations)
            .collect()
    }

    pub fn observation_count(&self) -> usize {
        self.steps.iter().map(|step| step.observations.len()).sum()
    }

    // Each Stop continuation re-renders the Answer turn beside its feedback.
    pub(crate) fn has_stop_room(&self, turn: &crate::provider::ProviderTurn, limit: usize) -> bool {
        self.rendered_len()
            + turn.len()
            + super::permissions::MAX_HOOK_REASON_BYTES
            + RESULT_ENVELOPE_BYTES
            <= limit
    }

    pub(crate) fn add_stop_feedback(&mut self, turn: &crate::provider::ProviderTurn, text: &str) {
        self.stop_bytes += turn.len() + text.len() + RESULT_ENVELOPE_BYTES;
    }

    pub(crate) fn rendered_len(&self) -> usize {
        self.catalog.len()
            + self.invoked_skill.as_ref().map_or(0, skill_context_bytes)
            + self.stop_bytes
            + self.history_summary.as_ref().map_or(0, String::len)
            + self
                .steps
                .iter()
                .map(|step| self.step_len(step))
                .sum::<usize>()
    }

    pub(crate) fn projected_len(&self, summary: Option<&str>, steps: usize) -> usize {
        self.catalog.len()
            + self.invoked_skill.as_ref().map_or(0, skill_context_bytes)
            + self.stop_bytes
            + summary.map_or(0, str::len)
            + self.steps[steps..]
                .iter()
                .map(|step| self.step_len(step))
                .sum::<usize>()
    }

    pub(crate) fn reserved_len(&self, step: &ToolStep, calls: &[&ToolCall]) -> usize {
        self.rendered_len()
            + self.step_len(step)
            + calls
                .iter()
                .map(|call| {
                    let effect = if call.mutates() { "applied" } else { "none" };
                    let mut value = model_projection(
                        call,
                        ToolDisposition::Succeeded,
                        &json!(""),
                        effect,
                        CacheUpdate::NotApplicable,
                    );
                    if let Some(hint) = self.command_git_hint(call) {
                        value["hint"] = json!(hint);
                    }
                    value.to_string().len() + RESULT_ENVELOPE_BYTES
                })
                .sum::<usize>()
    }

    pub(crate) fn model_observations(&self, steps: usize) -> Vec<Value> {
        self.steps[..steps]
            .iter()
            .flat_map(|step| {
                step.refusal
                    .iter()
                    .map(|refusal| {
                        json!({
                            "refused_tool": refusal,
                            "execution": "refused; nothing dispatched",
                        })
                    })
                    .chain(
                        step.observations
                            .iter()
                            .enumerate()
                            .map(|(index, _)| self.model_result(step, index)),
                    )
            })
            .collect()
    }
}

pub(crate) fn model_call(call: &ToolCall) -> Value {
    match call {
        ToolCall::GitBranch {
            name,
            start_point,
            scope,
            base,
            max_count,
        } => json!({
            "operation": "git_branch", "name": name, "start_point": start_point,
            "scope": scope, "base": base, "max_count": max_count,
        }),
        ToolCall::ApplyChangeset {
            source: ChangesetSource::CommandReceipt { changeset, .. },
        } => json!({"operation":"apply_changeset", "changeset": changeset}),
        _ => json!(call),
    }
}

pub(crate) fn cleared_model_result(observation: &ToolObservation) -> Value {
    let original = model_observation(observation);
    let mut value = json!({
        "operation": "Result cleared to save context; call the Tool again for current content",
        "call": model_call(&observation.intent.call),
        "disposition": observation.disposition,
        "effect": original["workspace_effect"],
        "cleared": true,
    });
    if observation.disposition == ToolDisposition::Uncertain
        || original["workspace_effect"] == "uncertain"
    {
        value["execution"] = json!("uncertain; never replay");
    }
    if let Some(spill) = original.get("spill") {
        value["spill"] = spill.clone();
    }
    if let Some(target) = original.get("target") {
        value["target"] = target.clone();
    }
    if let Some(effect) = original.get("private_effect") {
        value["private_effect"] = effect.clone();
    }
    value
}

pub(crate) fn model_result(observation: &ToolObservation, feedback: &[String]) -> Value {
    model_result_with_output(observation, feedback, &observation.output)
}

pub(crate) fn model_result_with_output(
    observation: &ToolObservation,
    feedback: &[String],
    output: &str,
) -> Value {
    let mut value = model_observation_with_output(observation, output);
    if !feedback.is_empty() {
        value["hook_feedback"] = json!(feedback);
    }
    value
}

pub(crate) fn model_observation(observation: &ToolObservation) -> Value {
    model_observation_with_output(observation, &observation.output)
}

fn model_observation_with_output(observation: &ToolObservation, output: &str) -> Value {
    let mut output = super::skills::parse_json(output.as_bytes(), MAX_TOOL_RESULT_BYTES)
        .unwrap_or_else(|_| json!(output));
    if matches!(
        observation.intent.call,
        ToolCall::Write { .. } | ToolCall::Edit { .. } | ToolCall::MultiEdit { .. }
    ) && let Some(o) = output.as_object_mut()
    {
        o.remove("before");
        o.remove("executable");
    }
    let private = matches!(observation.intent.target, EffectTarget::WorkerView { .. });
    let effect = if private {
        "none"
    } else if observation.intent.call.is_host() {
        match observation.disposition {
            ToolDisposition::Uncertain => "uncertain",
            ToolDisposition::Succeeded => "host",
            _ => "not_confirmed",
        }
    } else if observation.intent.call.is_unguarded() {
        match observation.disposition {
            ToolDisposition::Uncertain | ToolDisposition::Cancelled => "uncertain",
            _ if matches!(observation.attestation, Some(Attestation::Unguarded(_))) => "unguarded",
            _ => "not_confirmed",
        }
    } else if !observation.intent.call.mutates() {
        "none"
    } else {
        match observation.disposition {
            ToolDisposition::Succeeded
                if observation
                    .attestation
                    .as_ref()
                    .is_some_and(|attestation| attestation.valid_for(&observation.intent)) =>
            {
                "applied"
            }
            ToolDisposition::Uncertain | ToolDisposition::Cancelled => "uncertain",
            _ => "not_confirmed",
        }
    };
    let mut value = model_projection(
        &observation.intent.call,
        observation.disposition,
        &output,
        effect,
        observation.cache_update,
    );
    if private {
        value["target"] = json!(observation.intent.target);
        let command_capture =
            serde_json::from_str::<crate::worker::WorkerCommandResult>(&observation.output).ok();
        value["private_effect"] = json!(match observation.disposition {
            ToolDisposition::Succeeded
                if (observation.intent.call.mutates()
                    || command_capture.as_ref().is_some_and(|r| matches!(
                        r.private_capture,
                        crate::worker::PrivateCapture::Sealed { .. }
                    )))
                    && observation.attested_for(&observation.intent) =>
                "applied",
            ToolDisposition::Uncertain => "uncertain",
            _ => "none",
        });
    }
    if let Some(spill) = &observation.spill {
        value["spill"] = json!(spill);
    }
    if let Some(cause) = observation.limit_cause {
        value["limit_cause"] = json!(cause);
    }
    if let Some(diagnostic) = observation.enforcement_diagnostic() {
        value["enforcement_cause"] = json!(diagnostic);
    }
    value
}

fn model_projection(
    call: &ToolCall,
    disposition: ToolDisposition,
    output: &Value,
    effect: &str,
    cache_update: CacheUpdate,
) -> Value {
    let call = model_call(call);
    json!({"call":call,"disposition":disposition,"output":output,"workspace_effect":effect,"cache_update":cache_update})
}

#[cfg(test)]
mod model_call_tests {
    use super::*;

    #[test]
    fn model_call_strips_only_the_changeset_source_digest() {
        let changeset = ToolCall::ApplyChangeset {
            source: crate::session::worker::ChangesetSource::CommandReceipt {
                source_intent_digest: [7; 32],
                changeset: CommandChangeset::default(),
            },
        };
        let value = model_call(&changeset);
        assert!(value.get("source_intent_digest").is_none());
        assert!(value.get("changeset").is_some());

        let read = model_call(&ToolCall::Read {
            path: "src/main.rs".into(),
            offset: None,
            limit: Some(32),
        });
        assert_eq!(read["operation"], "read");
        assert_eq!(read["path"], "src/main.rs");
    }

    #[test]
    fn typed_publication_metadata_keeps_model_bytes_identical() {
        for call in [
            ToolCall::Write {
                path: "file".into(),
                expected_digest: None,
                content: "new".into(),
            },
            ToolCall::Edit {
                path: "file".into(),
                expected_digest: hex_digest(b"old"),
                old: "old".into(),
                new: "new".into(),
            },
            ToolCall::MultiEdit {
                file_path: "file".into(),
                expected_digest: hex_digest(b"old"),
                edits: vec![LiteralEdit {
                    old_string: "old".into(),
                    new_string: "new".into(),
                    replace_all: false,
                }],
            },
        ] {
            let intent = EffectIntent {
                host: None,
                host_command: None,
                target: EffectTarget::Workspace,
                id: Uuid::now_v7(),
                run_id: RunId::new(),
                agent_run_id: AgentRunId::new(),
                policy_digest: [1; 32],
                enforcement_digest: [2; 32],
                workspace_device: 1,
                workspace_inode: 2,
                call,
                limits: ToolLimits::default(),
                use_count: 1,
            };
            let mut o = ToolObservation {
                spill: None,
                attestation: Some(Attestation::for_site(&intent)),
                intent,
                disposition: ToolDisposition::Succeeded,
                output: json!({"sha256":hex_digest(b"new"),"bytes":3}).to_string(),
                approval_cause: None,
                cache_update: CacheUpdate::NotApplicable,
                enforcement_cause: None,
                limit_cause: None,
            };
            let original = serde_json::to_vec(&model_observation(&o)).unwrap();
            o.output = json!({"sha256":hex_digest(b"new"),"bytes":3,"executable":true,"before":{"sha256":hex_digest(b"old"),"bytes":3,"executable":true}}).to_string();
            assert_eq!(
                serde_json::to_vec(&model_observation(&o)).unwrap(),
                original
            );
        }
    }
}

#[cfg(test)]
mod unguarded_vocabulary_tests {
    use super::*;

    fn intent(call: ToolCall) -> EffectIntent {
        EffectIntent {
            host: None,
            host_command: call.is_unguarded().then(test_host_pin),
            target: crate::session::worker::EffectTarget::Workspace,
            id: Uuid::now_v7(),
            run_id: RunId::new(),
            agent_run_id: AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call,
            limits: ToolLimits::default(),
            use_count: 1,
        }
    }

    fn command(args: &[&str], cwd: &str) -> ToolCall {
        ToolCall::Command {
            program: "sh".into(),
            args: args.iter().map(|arg| (*arg).into()).collect(),
            cwd: cwd.into(),
        }
    }

    fn unguarded(args: &[&str], cwd: &str) -> ToolCall {
        ToolCall::CommandUnguarded {
            program: "sh".into(),
            args: args.iter().map(|arg| (*arg).into()).collect(),
            cwd: cwd.into(),
            justification: "Run the failed command once".into(),
            prefix_rule: None,
        }
    }

    fn unguarded_receipt(intent: &EffectIntent) -> UnguardedReceipt {
        UnguardedReceipt {
            contract_version: 1,
            intent_digest: intent.digest(),
            executable_sha256: "a".repeat(64),
            pid: 42,
            exit_code: Some(0),
            descendants_confirmed_stopped: true,
        }
    }

    #[test]
    fn unguarded_attestation_only_attests_unguarded_intents_and_guard_receipts_never_do() {
        let host = intent(unguarded(&["-c", "exit 0"], ""));
        let guarded_command = intent(command(&["-c", "exit 0"], ""));
        let receipt = unguarded_receipt(&host);

        let attestation = Attestation::Unguarded(receipt.clone());
        assert!(attestation.valid_for(&host));
        assert!(!attestation.valid_for(&guarded_command));
        assert!(
            !Attestation::Unguarded(UnguardedReceipt {
                intent_digest: [9; 32],
                ..receipt.clone()
            })
            .valid_for(&host)
        );
        assert!(
            !Attestation::Unguarded(UnguardedReceipt {
                executable_sha256: "A".repeat(64),
                ..receipt
            })
            .valid_for(&host)
        );

        let guard_receipt = Attestation::Guard(GuardReceipt {
            contract_version: 1,
            intent_digest: host.digest(),
            enforcement_digest: host.enforcement_digest,
            limits: host.limits.clone(),
        });
        assert!(!guard_receipt.valid_for(&host));
        assert!(
            !Attestation::InProcess {
                receipt: None,
                policy_digest: host.policy_digest,
                workspace_device: host.workspace_device,
                workspace_inode: host.workspace_inode,
            }
            .valid_for(&host)
        );
        assert!(Attestation::for_site(&host).valid_for(&host));

        let mut observation = ToolObservation {
            spill: None,
            intent: host.clone(),
            disposition: ToolDisposition::Succeeded,
            enforcement_cause: None,
            limit_cause: None,
            output: serde_json::to_string(&CommandResult {
                selection: Some(Default::default()),
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                stdout_total: Default::default(),
                stderr_total: Default::default(),
                changeset: Default::default(),
            })
            .unwrap(),
            attestation: Some(attestation),
            approval_cause: None,
            cache_update: CacheUpdate::NotApplicable,
        };
        assert!(observation.valid());
        assert_eq!(
            model_observation(&observation)["workspace_effect"],
            "unguarded"
        );
        observation.cache_update = CacheUpdate::Published;
        assert!(!observation.valid());
        observation.cache_update = CacheUpdate::NotApplicable;
        observation.enforcement_cause = Some(EnforcementCause::FilesystemDenied);
        assert!(!observation.valid());
    }

    #[test]
    fn attested_unguarded_runs_project_as_unguarded_and_unattested_as_not_confirmed() {
        let host = intent(unguarded(&["-c", "exit 0"], ""));
        let mut observation = ToolObservation {
            spill: None,
            intent: host.clone(),
            disposition: ToolDisposition::Succeeded,
            enforcement_cause: None,
            limit_cause: None,
            output: String::new(),
            attestation: Some(Attestation::Unguarded(unguarded_receipt(&host))),
            approval_cause: None,
            cache_update: CacheUpdate::NotApplicable,
        };
        assert_eq!(
            model_observation(&observation)["workspace_effect"],
            "unguarded"
        );
        observation.disposition = ToolDisposition::Failed;
        assert_eq!(
            model_observation(&observation)["workspace_effect"],
            "unguarded"
        );
        observation.attestation = None;
        assert_eq!(
            model_observation(&observation)["workspace_effect"],
            "not_confirmed"
        );
        observation.disposition = ToolDisposition::Cancelled;
        assert_eq!(
            model_observation(&observation)["workspace_effect"],
            "uncertain"
        );
    }

    #[test]
    fn guard_path_aliases_in_argv_never_qualify_for_a_host_rerun() {
        let owned = |args: &[&str]| args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        for args in [
            &["/workspace"][..],
            &["-c", "cd /workspace && make"],
            &["--out=/scratch/x"],
            &["/inputs/sh/data"],
            &["a:/programs/b"],
            &["$ARANY_TOOL_INPUTS/build"],
            &["${ARANY_TOOL_SKILLS}/x"],
        ] {
            assert!(uses_guard_alias(&owned(args)), "{args:?}");
        }
        for args in [
            &["/home/me/workspace/x"][..],
            &["./workspace"],
            &["/workspaces/x"],
            &["$ARANY_TOOL_WORKSPACE/x"],
            &["src/cache"],
        ] {
            assert!(!uses_guard_alias(&owned(args)), "{args:?}");
        }
    }
}

#[cfg(test)]
mod internal_call_tests {
    use super::*;

    #[test]
    fn engine_preparation_calls_are_internal_and_model_calls_are_not() {
        let preparation = ToolCall::PrepareWorker {
            preparation: crate::session::worker::WorkerPreparation {
                assignment_id: crate::session::AgentRunId::new(),
                purpose: crate::session::worker::WorkerDataPurpose::Base,
                max_bytes: 1,
                max_chunks: 1,
            },
        };
        let parse = ToolCall::ParseAgentDefinition {
            index: 0,
            raw_digest: [1; 32],
        };
        assert!(preparation.is_internal() && parse.is_internal());
        assert!(
            !ToolCall::Read {
                path: "a".into(),
                offset: None,
                limit: Some(1)
            }
            .is_internal()
        );
    }
}

pub(crate) fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

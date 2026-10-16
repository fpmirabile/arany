use crate::session::{AgentRunId, RunId};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Component, Path};
use uuid::Uuid;

pub const MAX_TOOL_ARGUMENT_BYTES: usize = 16 * 1024;
pub const MAX_TOOL_RESULT_BYTES: usize = 16 * 1024;
pub const MAX_TOOL_CONTEXT_BYTES: usize = crate::session::MAX_CONTEXT_CONTENT_BYTES;
pub const MAX_SKILLS: usize = 64;
pub(crate) const MAX_TOOL_CATALOG_BYTES: usize = 16 * 1024;
pub(crate) const MIN_TOOL_CONTEXT_BYTES: usize = MAX_TOOL_CATALOG_BYTES + 512;
pub(crate) const MAX_FILE_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const MAX_SNAPSHOT_FILES: usize = 2048;
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
pub(crate) const MAX_CHANGESET_BYTES: usize = 8 * 1024;

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

    pub(crate) fn bytes(&self) -> Option<Vec<u8>> {
        if !valid_digest(&self.sha256) || self.content.len() > MAX_CHANGESET_BYTES.div_ceil(3) * 4 {
            return None;
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.content)
            .ok()?;
        (bytes.len() <= MAX_CHANGESET_BYTES
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
                || change.path.split('/').count() > 16
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
        serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= MAX_TOOL_ARGUMENT_BYTES - 256)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandResult {
    #[serde(deserialize_with = "Option::deserialize")]
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub changeset: CommandChangeset,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheUpdate {
    NotApplicable,
    NotAttempted,
    Published,
    Rejected,
    Failed,
    Uncertain,
}

impl CommandResult {
    pub(crate) fn valid(&self) -> bool {
        self.changeset.valid()
            && serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= MAX_TOOL_RESULT_BYTES)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolCall {
    List {
        path: String,
    },
    Read {
        path: String,
        offset: u32,
        limit: u32,
    },
    Search {
        path: String,
        query: String,
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
    Command {
        program: String,
        args: Vec<String>,
        cwd: String,
    },
    ApplyChangeset {
        source_intent_digest: [u8; 32],
        changeset: CommandChangeset,
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
}

fn required_nullable<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
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
    pub(crate) fn is_process(&self) -> bool {
        matches!(
            self,
            Self::Command { .. } | Self::McpList { .. } | Self::McpCall { .. }
        )
    }

    pub(crate) fn is_git_read(&self) -> bool {
        matches!(
            self,
            Self::GitStatus { .. } | Self::GitDiff { .. } | Self::GitLog { .. }
        )
    }

    /// Read-only calls a researcher child may make; Git reads stay with the primary (R7-8).
    pub(crate) fn is_child_research(&self) -> bool {
        child_research_operation(self.label())
    }
    pub(crate) fn valid(&self) -> bool {
        if !serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= MAX_TOOL_ARGUMENT_BYTES) {
            return false;
        }
        match self {
            Self::List { path } => valid_relative(path, true),
            Self::Read { path, limit, .. } => {
                valid_relative(path, false) && (1..=4096).contains(limit)
            }
            Self::Search { path, query } => {
                valid_relative(path, true) && !query.is_empty() && query.len() <= 512
            }
            Self::Mkdir { path } => valid_relative(path, false),
            Self::Write {
                path,
                expected_digest,
                content,
            } => {
                valid_relative(path, false)
                    && content.len() <= 8 * 1024
                    && expected_digest
                        .as_ref()
                        .is_none_or(|digest| valid_digest(digest))
            }
            Self::Edit {
                path,
                expected_digest,
                old,
                new,
            } => {
                valid_relative(path, false)
                    && valid_digest(expected_digest)
                    && !old.is_empty()
                    && old.len() + new.len() <= 8 * 1024
            }
            Self::Command { program, args, cwd } => {
                valid_name(program)
                    && valid_relative(cwd, true)
                    && args.len() <= 64
                    && args
                        .iter()
                        .all(|arg| arg.len() <= 4096 && !arg.contains('\0'))
            }
            Self::ApplyChangeset {
                source_intent_digest,
                changeset,
            } => {
                *source_intent_digest != [0; 32] && !changeset.files.is_empty() && changeset.valid()
            }
            Self::Skill { name, resource } => {
                valid_name(name)
                    && resource
                        .as_ref()
                        .is_none_or(|path| valid_relative(path, false))
            }
            Self::McpList { server } => valid_name(server),
            Self::McpCall {
                server,
                tool,
                schema_digest,
                arguments,
            } => {
                valid_name(server)
                    && valid_digest(schema_digest)
                    && !tool.is_empty()
                    && tool.len() <= 128
                    && tool.chars().all(|ch| !ch.is_control())
                    && parse_mcp_arguments(arguments).is_ok()
            }
            Self::GitStatus { path } => valid_relative(path, true),
            Self::GitDiff {
                path,
                base,
                head,
                staged,
            } => {
                valid_relative(path, true)
                    && base.as_deref().is_none_or(valid_revision)
                    && head.as_deref().is_none_or(valid_revision)
                    && (head.is_none() || base.is_some() && !staged)
            }
            Self::GitLog {
                path,
                revision,
                max_count,
            } => {
                valid_relative(path, true)
                    && revision.as_deref().is_none_or(valid_revision)
                    && (1..=MAX_GIT_LOG_COMMITS).contains(max_count)
            }
        }
    }

    pub(crate) fn mutates(&self) -> bool {
        matches!(
            self,
            Self::Write { .. }
                | Self::Edit { .. }
                | Self::Mkdir { .. }
                | Self::ApplyChangeset { .. }
        )
    }

    pub(crate) fn is_goal_plan_prelude(&self) -> bool {
        GOAL_PLAN_PRELUDE_OPERATIONS.contains(&self.label())
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::List { .. } => "list",
            Self::Read { .. } => "read",
            Self::Search { .. } => "search",
            Self::Mkdir { .. } => "mkdir",
            Self::Write { .. } => "write",
            Self::Edit { .. } => "edit",
            Self::Command { .. } => "command",
            Self::ApplyChangeset { .. } => "apply_changeset",
            Self::Skill { .. } => "skill",
            Self::McpList { .. } => "mcp_list",
            Self::McpCall { .. } => "mcp_call",
            Self::GitStatus { .. } => "git_status",
            Self::GitDiff { .. } => "git_diff",
            Self::GitLog { .. } => "git_log",
        }
    }
}

/// The single predicate for read-only researcher children, by wire operation name.
pub(crate) fn child_research_operation(operation: &str) -> bool {
    GOAL_PLAN_PRELUDE_OPERATIONS.contains(&operation) && !operation.starts_with("git_")
}

pub(crate) const MAX_GIT_LOG_COMMITS: u32 = 50;
const MAX_REVISION_BYTES: usize = 128;

/// Path components no Tool reads; Git reads also exclude them through pathspecs.
pub(crate) const FORBIDDEN_NAMES: [&str; 7] = [
    ".git", ".aws", ".ssh", ".gnupg", ".env", ".npmrc", ".pypirc",
];

/// Operations allowed before a Goal plan contract; each name is the wire `operation` and `label()`.
const GOAL_PLAN_PRELUDE_OPERATIONS: [&str; 8] = [
    "list",
    "read",
    "search",
    "skill",
    "mcp_list",
    "git_status",
    "git_diff",
    "git_log",
];

/// `HEAD`, a ref name or an object name with at most four `~N`/`^N` steps; no ranges, `:`, `@{` or options.
pub(crate) fn valid_revision(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_REVISION_BYTES {
        return false;
    }
    let (name, mut suffix) = value.split_at(value.find(['~', '^']).unwrap_or(value.len()));
    let name_valid = !name.is_empty()
        && !name.contains("..")
        && name.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with(['.', '-'])
                && !part.ends_with('.')
                && !part.ends_with(".lock")
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        });
    let mut steps = 0;
    while let Some(rest) = suffix.strip_prefix(['~', '^']) {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 4 {
            return false;
        }
        suffix = &rest[digits..];
        steps += 1;
    }
    name_valid && suffix.is_empty() && steps <= 4
}

pub(crate) fn valid_relative(value: &str, root: bool) -> bool {
    if value.is_empty() || value == "." {
        return root;
    }
    value.len() <= 512
        && !value.chars().any(char::is_control)
        && !value.contains('\\')
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".." && !forbidden(part))
        && Path::new(value)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

pub(crate) fn forbidden(value: &str) -> bool {
    FORBIDDEN_NAMES.contains(&value) || value.starts_with(".env.")
}

pub(crate) fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

pub(crate) fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicyReceipt {
    pub contract_version: u32,
    pub config_digest: [u8; 32],
    pub max_context_bytes: u32,
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
            && self.config_digest != [0; 32]
            && (MIN_TOOL_CONTEXT_BYTES as u32..=MAX_TOOL_CONTEXT_BYTES as u32)
                .contains(&self.max_context_bytes)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectIntent {
    pub id: Uuid,
    pub run_id: RunId,
    pub agent_run_id: AgentRunId,
    pub policy_digest: [u8; 32],
    pub enforcement_digest: [u8; 32],
    pub workspace_device: u64,
    pub workspace_inode: u64,
    pub call: ToolCall,
    pub limits: ToolLimits,
    pub expires_at_ms: u64,
    pub use_count: u8,
}

impl EffectIntent {
    pub(crate) fn digest(&self) -> [u8; 32] {
        digest(&serde_json::to_vec(self).expect("typed intent is serializable"))
    }

    pub(crate) fn valid(&self) -> bool {
        self.id.get_version() == Some(uuid::Version::SortRand)
            && self.policy_digest != [0; 32]
            && self.enforcement_digest != [0; 32]
            && self.workspace_inode > 0
            && self.expires_at_ms > 0
            && self.use_count == 1
            && self.call.valid()
            && self.limits.valid()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolLimits {
    pub result_bytes: u32,
    pub runtime_ms: u32,
    pub cache_transfer_ms: u32,
    pub memory_bytes: u64,
    pub max_processes: u32,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResourceSettings {
    pub default_command_timeout_ms: u32,
    pub max_command_timeout_ms: u32,
    pub build_memory_bytes: u64,
    pub build_processes: u32,
    pub build_project_bytes: u64,
    pub build_cache_bytes: u64,
}

impl Default for ToolResourceSettings {
    fn default() -> Self {
        Self {
            default_command_timeout_ms: 120_000,
            max_command_timeout_ms: MAX_COMMAND_TIMEOUT_MS,
            build_memory_bytes: MAX_BUILD_MEMORY_BYTES,
            build_processes: MAX_BUILD_PROCESSES_OR_THREADS,
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
            && (BYTES_PER_MEBIBYTE..=MAX_BUILD_PROJECT_BYTES).contains(&self.build_project_bytes)
            && self.build_project_bytes.is_multiple_of(BYTES_PER_MEBIBYTE)
            && self.build_cache_bytes <= MAX_BUILD_CACHE_BYTES
            && (self.build_cache_bytes == 0
                || self.build_cache_bytes.is_multiple_of(BYTES_PER_MEBIBYTE))
    }
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
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            result_bytes: MAX_TOOL_RESULT_BYTES as u32,
            runtime_ms: 60_000,
            cache_transfer_ms: 0,
            memory_bytes: MEMORY_BYTES,
            max_processes: MAX_PROCESSES_OR_THREADS,
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
            && self.scratch_bytes == SCRATCH_BYTES
            && self.network == NetworkGrant::None
            && match self.resource_class {
                ToolResourceClass::Default => {
                    self.memory_bytes == MEMORY_BYTES
                        && self.max_processes == MAX_PROCESSES_OR_THREADS
                        && self.project_bytes == SCRATCH_BYTES
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
pub struct ToolObservation {
    pub intent: EffectIntent,
    pub disposition: ToolDisposition,
    pub output: String,
    pub guard: Option<GuardReceipt>,
    pub cache_update: CacheUpdate,
}

impl ToolObservation {
    pub(crate) fn attested_for(&self, intent: &EffectIntent) -> bool {
        self.intent == *intent && self.valid() && self.guard.is_some()
    }
    pub(crate) fn stops_run(&self) -> bool {
        matches!(
            self.disposition,
            ToolDisposition::Uncertain | ToolDisposition::Cancelled
        ) || (self.disposition == ToolDisposition::Failed && self.guard.is_none())
            || self.cache_update == CacheUpdate::Uncertain
    }

    pub(crate) fn valid(&self) -> bool {
        self.intent.valid()
            && self.output.len() <= self.intent.limits.result_bytes as usize
            && serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= 48 * 1024)
            && self
                .guard
                .as_ref()
                .is_none_or(|guard| guard.valid_for(&self.intent))
            && (self.disposition != ToolDisposition::Succeeded || self.guard.is_some())
            && match self.cache_update {
                CacheUpdate::NotApplicable => true,
                CacheUpdate::NotAttempted | CacheUpdate::Uncertain => self.intent.call.is_process(),
                CacheUpdate::Published | CacheUpdate::Rejected | CacheUpdate::Failed => {
                    self.intent.call.is_process()
                        && self.guard.is_some()
                        && self.disposition == ToolDisposition::Succeeded
                }
            }
    }
}

/// One Tool call the Provider asked for; the id correlates its result in the next request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRequest {
    pub id: crate::provider::ToolUseId,
    pub call: ToolCall,
    pub plan_contract: Option<Box<crate::session::GoalPlanContract>>,
}

/// One Provider turn and the observations of the requests it declared, in request order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolStep {
    pub turn: crate::provider::ProviderTurn,
    /// Empty for a Delegate step.
    pub requests: Vec<ToolRequest>,
    /// A prefix of `requests` plus any Engine integration observation; shorter only when the Run stopped.
    pub observations: Vec<ToolObservation>,
    /// Goal facts written when the next call is prepared; `None` for Researchers.
    pub facts: Option<crate::provider::GoalContextFacts>,
    /// Notes first delivered with this step's results.
    pub steering_notes: Vec<String>,
}

/// Live Run state only: replayed turns are never serialized or journaled.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolContext {
    pub catalog: String,
    pub history_summary: Option<String>,
    pub steps: Vec<ToolStep>,
}

impl ToolStep {
    fn rendered_len(&self) -> usize {
        self.turn.len()
            + self
                .observations
                .iter()
                .map(|observation| {
                    model_observation(observation).to_string().len() + RESULT_ENVELOPE_BYTES
                })
                .sum::<usize>()
            + self
                .facts
                .and_then(|facts| serde_json::to_string(&facts).ok())
                .map_or(0, |facts| facts.len() + RESULT_ENVELOPE_BYTES)
            + self.steering_notes.iter().map(String::len).sum::<usize>()
    }
}

const RESULT_ENVELOPE_BYTES: usize = 128;

impl ToolContext {
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

    /// Bytes this context adds to a request: catalog, summary and every step's turn, results, facts and notes.
    pub(crate) fn rendered_len(&self) -> usize {
        self.catalog.len()
            + self.history_summary.as_ref().map_or(0, String::len)
            + self.steps.iter().map(ToolStep::rendered_len).sum::<usize>()
    }

    /// `rendered_len` after the first `steps` steps are replaced by `summary`.
    pub(crate) fn projected_len(&self, summary: Option<&str>, steps: usize) -> usize {
        self.catalog.len()
            + summary.map_or(0, str::len)
            + self.steps[steps..]
                .iter()
                .map(ToolStep::rendered_len)
                .sum::<usize>()
    }

    /// Model projection of the observations of the first `steps` steps.
    pub(crate) fn model_observations(&self, steps: usize) -> Vec<Value> {
        self.steps[..steps]
            .iter()
            .flat_map(|step| step.observations.iter().map(model_observation))
            .collect()
    }
}

pub(crate) fn model_observation(observation: &ToolObservation) -> Value {
    let output = super::skills::parse_json(observation.output.as_bytes(), MAX_TOOL_RESULT_BYTES)
        .unwrap_or_else(|_| json!(observation.output));
    let effect = if !observation.intent.call.mutates() {
        "none"
    } else {
        match observation.disposition {
            ToolDisposition::Succeeded
                if observation
                    .guard
                    .as_ref()
                    .is_some_and(|guard| guard.valid_for(&observation.intent)) =>
            {
                "applied"
            }
            ToolDisposition::Uncertain | ToolDisposition::Cancelled => "uncertain",
            _ => "not_confirmed",
        }
    };
    let mut call = json!(observation.intent.call);
    if let ToolCall::ApplyChangeset { .. } = observation.intent.call {
        call.as_object_mut()
            .expect("typed call")
            .remove("source_intent_digest");
    }
    json!({"call":call,"disposition":observation.disposition,"output":output,"workspace_effect":effect,"cache_update":observation.cache_update})
}

/// Typed call schemas the catalog admits, in the fixed order every request offers them.
pub(crate) fn catalog_call_schemas(catalog: &Value) -> Vec<Value> {
    let mut calls = call_schemas();
    calls.retain_mut(|call| {
        let (key, field, cap) = match call["properties"]["operation"]["enum"][0].as_str() {
            Some("write" | "edit" | "mkdir") => return catalog["write"] == true,
            Some("command") => ("commands", "program", 8),
            Some("skill") => ("skills", "name", MAX_SKILLS),
            Some("mcp_list" | "mcp_call") => ("mcp_servers", "server", 4),
            Some("git_status" | "git_diff" | "git_log") => return catalog["git"].is_object(),
            _ => return true,
        };
        let names: Vec<_> = catalog[key]
            .as_array()
            .filter(|rows| rows.len() <= cap)
            .into_iter()
            .flatten()
            .filter_map(|row| row["name"].as_str())
            .filter(|name| valid_name(name))
            .collect();
        if names.is_empty() {
            return false;
        }
        call["properties"][field]["enum"] = json!(names);
        true
    });
    calls
}

/// Schema of the optional Goal plan contract that Primary Tools carry; `null` when no contract is proposed.
pub(crate) fn plan_contract_property(calls: &[Value]) -> Value {
    json!({"anyOf":[plan_contract_schema(goal_success_call_schema(calls).as_ref()), {"type":"null"}]})
}

/// Calls that can satisfy a Goal success criterion, taken from the catalog-restricted set.
/// `None` when the catalog admits none, because JSON Schema `anyOf` must not be empty.
fn goal_success_call_schema(calls: &[Value]) -> Option<Value> {
    let calls: Vec<_> = calls
        .iter()
        .filter(|call| {
            matches!(
                call["properties"]["operation"]["enum"][0].as_str(),
                Some("mkdir" | "write" | "edit" | "command" | "skill" | "mcp_call")
            )
        })
        .cloned()
        .collect();
    (!calls.is_empty()).then(|| json!({"anyOf": calls}))
}

fn plan_contract_schema(call_schema: Option<&Value>) -> Value {
    let path = "Workspace-relative file path, at most 512 bytes; paths must cover it.";
    let calls = json!({"type":["integer","null"], "description": "Positive integer within goal_budget.max_provider_calls; null only when the pinned Goal budget sets no provider-call ceiling."});
    let tokens = json!({"type":["integer","null"], "description": "Positive integer within goal_budget.max_tokens; null only when the pinned Goal budget sets no token ceiling."});
    let active_ms = json!({"type":["integer","null"], "description": "Positive integer within goal_budget.max_active_ms; null only when the pinned Goal budget sets no active-time ceiling."});
    let mut criteria = Vec::with_capacity(3);
    if let Some(call_schema) = call_schema {
        criteria.push(json!({
            "type":"object",
            "properties":{
                "kind":{"type":"string","enum":["tool_call_succeeded"]},
                "call":call_schema.clone()
            },
            "required":["kind","call"],
            "additionalProperties":false
        }));
    }
    criteria.push(json!({
        "type":"object",
        "properties":{
            "kind":{"type":"string","enum":["workspace_file_digest"]},
            "path":{"type":"string", "description": path},
            "sha256":{"type":"string", "description": "Exact lowercase 64-character SHA-256."}
        },
        "required":["kind","path","sha256"],
        "additionalProperties":false
    }));
    criteria.push(json!({
        "type":"object",
        "properties":{
            "kind":{"type":"string","enum":["workspace_file_content"]},
            "path":{"type":"string", "description": path},
            "content":{"type":"string", "description": "UTF-8 text, at most 8192 bytes; the whole plan contract must serialize within 8 KiB."}
        },
        "required":["kind","path","content"],
        "additionalProperties":false
    }));
    json!({
        "type": "object",
        "properties": {
            "mission": {"type":"string", "description": "Non-empty UTF-8 Goal mission, at most 2048 bytes."},
            "paths": {"type":"array", "description": "Up to 64 distinct Workspace-relative paths in ascending order, each at most 512 bytes. Use \".\" alone for the whole Workspace; nested paths are rejected.", "items":{"type":"string"}},
            "commands": {"type":"array", "description": "Up to 8 distinct admitted command names in ascending order; each uses lowercase letters, digits, '-' or '_', at most 64 bytes.", "items":{"type":"string"}},
            "resource_class": {"type":"string", "enum":["default","build"]},
            "command_timeout_ms": {"type":["integer","null"], "description": "Integer 1000 through 600000 inclusive, or null for the pinned default."},
            "skills": {"type":"array", "description": "Up to 64 distinct admitted Skill bindings in ascending name and resource order. Names use lowercase letters, digits, '-' or '_', at most 64 bytes; resource is null for SKILL.md or a relative path of at most 512 bytes.", "items":{
                "type":"object",
                "properties": {
                    "name":{"type":"string"},
                    "resource":{"type":["string", "null"]}
                },
                "required":["name","resource"],
                "additionalProperties":false
            }},
            "mcp_tools": {"type":"array", "description": "Up to 64 distinct admitted MCP bindings in ascending server and tool order. Server names are at most 64 bytes and tool names at most 128 bytes, each using lowercase letters, digits, '-' or '_'.", "items":{
                "type":"object",
                "properties": {
                    "server":{"type":"string"},
                    "tool":{"type":"string"}
                },
                "required":["server","tool"],
                "additionalProperties":false
            }},
            "success_criterion": {"anyOf": criteria},
            "budget": {
                "type":"object",
                "properties": {
                    "max_provider_calls": calls,
                    "max_tokens": tokens,
                    "max_active_ms": active_ms,
                    "max_runs": {"type":"integer", "description": "Integer 1 through goal_budget.max_runs inclusive."}
                },
                "required":["max_provider_calls","max_tokens","max_active_ms","max_runs"],
                "additionalProperties":false
            }
        },
        "required":["mission","paths","commands","resource_class","command_timeout_ms","skills","mcp_tools","success_criterion","budget"],
        "additionalProperties":false
    })
}

pub(crate) fn call_schemas() -> Vec<Value> {
    let string = json!({"type": "string"});
    let nullable = json!({"type": ["string", "null"]});
    let git_path = json!({"type":"string", "description":"Workspace-relative path that limits the result; \".\" for the whole repository."});
    let revision = json!({"type":["string","null"], "description":"null, or HEAD, a branch or tag name, or a 7-64 character hex commit name, with at most four ~N or ^N ancestry steps (N at most 9999). No ranges, ':' or '@{'. At most 128 bytes."});
    let definitions = [
        ("list", vec![("path", string.clone())]),
        ("mkdir", vec![("path", string.clone())]),
        (
            "read",
            vec![
                ("path", string.clone()),
                (
                    "offset",
                    json!({"type":"integer", "description":"Zero-based byte offset, 0 through 4294967295. Start at 0; page using the returned next offset."}),
                ),
                (
                    "limit",
                    json!({"type":"integer", "description":"Number of UTF-8 bytes to read: integer 1 through 4096 inclusive. Use 4096 for a normal page; never request the whole file with a larger limit."}),
                ),
            ],
        ),
        (
            "search",
            vec![("path", string.clone()), ("query", string.clone())],
        ),
        (
            "write",
            vec![
                ("path", string.clone()),
                ("expected_digest", nullable.clone()),
                ("content", string.clone()),
            ],
        ),
        (
            "edit",
            vec![
                ("path", string.clone()),
                (
                    "expected_digest",
                    json!({"type":"string", "description":"Exact lowercase 64-character SHA-256 from the file include or a successful Read. Never invent it."}),
                ),
                (
                    "old",
                    json!({"type":"string", "description":"Non-empty exact literal text occurring once in the current file. To append, select a unique ending and include that ending plus the appended text in new."}),
                ),
                (
                    "new",
                    json!({"type":"string", "description":"Replacement literal text. Combined old and new UTF-8 size must be at most 8192 bytes."}),
                ),
            ],
        ),
        (
            "command",
            vec![
                ("program", string.clone()),
                ("args", json!({"type":"array", "items": {"type":"string"}})),
                ("cwd", string.clone()),
            ],
        ),
        (
            "skill",
            vec![("name", string.clone()), ("resource", nullable)],
        ),
        ("mcp_list", vec![("server", string.clone())]),
        (
            "mcp_call",
            vec![
                ("server", string.clone()),
                ("tool", string.clone()),
                ("schema_digest", string.clone()),
                ("arguments", string),
            ],
        ),
        ("git_status", vec![("path", git_path.clone())]),
        (
            "git_diff",
            vec![
                ("path", git_path.clone()),
                ("base", revision.clone()),
                ("head", revision.clone()),
                (
                    "staged",
                    json!({"type":"boolean", "description":"true compares the index with base, or HEAD when base is null; head must then be null. false compares the working tree with the index (no revisions), with base, or compares base with head."}),
                ),
            ],
        ),
        (
            "git_log",
            vec![
                ("path", git_path),
                ("revision", revision),
                (
                    "max_count",
                    json!({"type":"integer", "description":"Number of commits: integer 1 through 50 inclusive."}),
                ),
            ],
        ),
    ];
    let mut calls: Vec<Value> = definitions.into_iter().map(|(operation, fields)| {
        let mut properties = serde_json::Map::new();
        properties.insert("operation".into(), json!({"type":"string", "enum":[operation]}));
        let mut required = vec!["operation"];
        for (name, schema) in fields { properties.insert(name.into(), schema); required.push(name); }
        json!({"type":"object", "properties":properties, "required":required, "additionalProperties":false})
    }).collect();
    calls.sort_by_key(|call| {
        CALL_ORDER
            .iter()
            .position(|name| call["properties"]["operation"]["enum"][0] == *name)
    });
    calls
}

/// Offered order of the model Tools; identical for every request so the Tool prefix stays byte-stable.
const CALL_ORDER: [&str; 13] = [
    "list",
    "read",
    "search",
    "git_status",
    "git_diff",
    "git_log",
    "skill",
    "mcp_list",
    "mkdir",
    "write",
    "edit",
    "command",
    "mcp_call",
];

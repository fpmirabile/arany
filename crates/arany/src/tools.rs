#![cfg_attr(not(test), warn(clippy::too_many_lines))]

mod approval;
mod cancel;
pub(crate) mod catalog;
pub(crate) mod config;
pub(crate) mod definitions;
mod display;
pub use display::DisplayFacts;
mod discovery;
mod dispatch;
mod editors;
pub use discovery::{
    FileSuggestionSource, SUGGESTION_LIFETIME, SUGGESTION_QUERY_TIME, SuggestionIndex,
};
pub(crate) mod command_environment;
pub(crate) mod command_tools;
pub use command_tools::{
    CommandRewrite, CommandTool, CommandToolImport, CommandToolProbe, CommandToolRole,
    CommandTools, CommandWrap, PROBE_COMMAND, RewriteStage, WrapStage, command_tool_candidates,
    import_command_tools,
};
mod executable;
mod fs;
mod git;
mod glob;
mod grant;
pub use command_environment::{CommandEnvironment, CommandEnvironmentError};
mod guard;
pub use guard::recover_build_cache;
pub(crate) use guard::transfer::transfer_time;
mod mcp;
mod network;
mod web;
pub(crate) use mcp::FRAME_BYTES;
pub(crate) use mcp::MAX_MCP_RESPONSE_BYTES;
pub(crate) use mcp::oauth::{MAX_ACCESS_TOKEN_BYTES, MAX_METADATA_BYTES, MCP_OAUTH};
pub use mcp::oauth::{McpAuthError, mcp_login, mcp_logout};
pub(crate) use network::{
    MAX_NETWORK_BYTES, MAX_NETWORK_CONNECTIONS, MAX_NETWORK_PATTERNS, NETWORK_IDLE_MS,
};
pub(crate) use web::{
    MAX_QUERY_BYTES, MAX_SEARCH_FIELD_CHARS, MAX_SESSION_SEARCHES, MAX_URL_BYTES,
    WEB_DOWNLOAD_BYTES, WEB_FETCH_CHARS, WEB_SEARCH_RESULTS,
};
mod output;
pub(crate) mod spill;
pub use spill::OutputSpill;
pub mod permissions;
mod process;
mod search;
mod selection;
mod skills;
pub use definitions::{DefinitionFilePin, DefinitionRegistryPin};
pub use grant::ToolGrant;
pub(crate) mod toolchains;
pub(crate) mod types;
pub use types::{DEFAULT_PR_LIMIT, PrState};
pub(crate) mod unguarded;
pub(crate) mod unguarded_rules;
mod workspace;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "tools/fs/tests.rs"]
mod fs_tests;

#[cfg(test)]
#[path = "tools/fs/read_tests.rs"]
mod read_tests;

#[cfg(test)]
#[path = "tools/fs/command/selection_tests.rs"]
mod command_selection_tests;

#[cfg(test)]
#[path = "tools/search/tests.rs"]
mod search_tests;

#[cfg(test)]
#[path = "tools/glob/runtime_tests.rs"]
mod glob_tests;

#[cfg(test)]
#[path = "tools/discovery/runtime_tests.rs"]
mod discovery_tests;

#[cfg(test)]
#[path = "tools/permissions/runtime_tests.rs"]
mod permission_tests;

#[cfg(test)]
#[path = "tools/skills/runtime_tests.rs"]
mod skill_tests;

#[cfg(test)]
#[path = "tools/catalog/runtime_tests.rs"]
mod catalog_tests;

#[cfg(test)]
#[path = "tools/types/tests.rs"]
mod type_tests;

#[cfg(test)]
#[path = "tools/config/tests.rs"]
mod config_tests;

#[cfg(test)]
#[path = "tools/git/admission_tests.rs"]
mod git_tests;

#[cfg(test)]
#[path = "tools/process/admission_tests.rs"]
mod process_tests;

#[cfg(test)]
#[path = "tools/guard/receipt_tests.rs"]
mod cache_receipt_tests;

use crate::session::{AgentRunId, RunId, WorkspaceIdentity};
use crate::store::StateRoot;
pub use approval::{
    ApprovalInbox, ApprovalMode, ApprovalReply, ToolApproval, ToolApprovals, UnguardedPrompt,
};
pub(crate) use approval::{ApprovalWaitReply, ApprovalWithdrawal};
pub use cancel::Cancellation;
use config::Config;
pub(crate) use git::stale_locks;
pub use skills::{ProjectSkills, project_skills};
pub(crate) use skills::{parse_json, parse_json_bounded};
pub(crate) use workspace::WORKSPACE_PERMISSIONS;
pub use workspace::{
    GitHooksQuestion, RuntimeProfileOption, TrustRecordError, WorkspacePermissions, mention_paths,
};

pub fn configured_skill_names(state: &StateRoot) -> Result<Vec<String>, ToolError> {
    Ok(Config::load(state)?
        .skills
        .into_iter()
        .map(|skill| skill.name)
        .collect())
}

pub fn runtime_skill_names(state: &StateRoot, workspace: &Path) -> Result<Vec<String>, ToolError> {
    let config = Config::load(state)?;
    let project = if config.permits_project_skills() {
        project_skills(workspace)?.names
    } else {
        Vec::new()
    };
    let mut names = config
        .skills
        .into_iter()
        .map(|skill| skill.name)
        .collect::<Vec<_>>();
    for name in project {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.len() > MAX_SKILLS {
        return Err(ToolError::Limit);
    }
    Ok(names)
}

pub(crate) fn workspace_identity(workspace: &Path) -> Result<WorkspaceIdentity, ToolError> {
    let root = fs::open_directory(workspace)?;
    fs::workspace_identity(&root)
}
use catalog::Site;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
pub use types::{
    ApprovalCause, Attestation, BuildCacheSize, CacheUpdate, CommandChangeset, EffectIntent,
    EnforcementCause, FailureStage, FileChange, FileReceipt, FileVersion, GuardReceipt,
    GuardStageFact, HostIntent, LimitCause, LiteralEdit, MAX_SEARCH_ROWS, MAX_SKILLS, NetworkGrant,
    OutputTotal, ToolCall, ToolContext, ToolDisposition, ToolLimits, ToolObservation,
    ToolPolicyReceipt, ToolRequest, ToolResourceClass, ToolResourceProfile, ToolResourceSettings,
    ToolStep, UnguardedGrant, UnguardedReceipt,
};

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("no spill for this Run")]
    SpillUnavailable,
    #[error("{path:?}: {source}")]
    AtPath {
        path: String,
        #[source]
        source: Box<ToolError>,
    },
    #[error("tools require a private tools.json; see docs/tools.md")]
    MissingConfiguration,
    #[error("Tool configuration is invalid or unsafe")]
    Configuration,
    #[error(
        "Saved host command rule {rule} conflicts with a declared interpreter or Git executable; open /permissions > Host command rules to review and remove it"
    )]
    HostPrefixConflict { rule: usize },
    #[error(
        "Inactive host-rule archive at archive/inactive-host-commands is unconfirmed. Stop Arany, preserve the source and archive intact, and restore a verified backup or obtain manual recovery; never remove pending markers to resume."
    )]
    InactiveHostArchiveUnconfirmed,
    #[error(
        "Inactive host-rule archive at archive/inactive-host-commands conflicts with its source. Stop Arany, preserve the source and archive intact, and restore a verified backup or obtain manual recovery; never overwrite either copy."
    )]
    InactiveHostArchiveConflict,
    #[error(
        "Creating command-environment.json is unconfirmed; open /permissions > Command environment to verify it before relying on Tools."
    )]
    CommandEnvironmentUnconfirmed,
    #[error("another process is updating private account state; retry when that update finishes")]
    AccountBusy,
    #[error("required native Tool protection is unavailable")]
    ProtectionUnavailable,
    #[error("Guard waiting queue is full; retry when capacity is available")]
    QueueFull,
    #[error("pinned Tool input changed")]
    ChangedInput,
    #[error("Tool path is not admitted")]
    Path,
    #[error("path is outside workspace")]
    OutsideWorkspace,
    #[error("protected path")]
    ProtectedPath,
    #[error("path is a symbolic link")]
    SymbolicLink,
    #[error("path crosses a filesystem boundary")]
    FilesystemBoundary,
    #[error("not a directory")]
    NotDirectory,
    #[error("unterminated quoted path")]
    UnterminatedQuotedPath,
    #[error("Tool file or directory not found")]
    NotFound,
    #[error("Guard filesystem boundary denied access")]
    FilesystemDenied,
    #[error("Tool resource limit reached")]
    Limit,
    #[error("Command output passed the 1 GiB per-stream drain ceiling")]
    OutputCeiling,
    #[error("Tool runtime expired")]
    Timeout(String),
    #[error("{0}")]
    DiskSpace(String),
    #[error("Tool file precondition changed")]
    Conflict,
    #[error("Workspace files changed since the editor started: {0}")]
    Conflicts(String),
    #[error("Search pattern refused: {0}")]
    Pattern(&'static str),
    #[error("Tool operation failed")]
    Operation,
    #[error("File operation cancelled before publication")]
    Cancelled,
    #[error("Tool effect completion is uncertain")]
    Uncertain,
    #[error(
        "Git locks left by an interrupted write: {0}; remove them after checking that no git process is running"
    )]
    StaleGitLocks(String),
    #[error("Guard rejected its {0} stage")]
    GuardRejected(guard::GuardStage),
    #[error("Guard rejected at {0}")]
    GuardBootstrap(guard::BootstrapFailure),
    #[error("{0}")]
    ProjectSelection(SelectionCause),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionCause {
    TimedOut,
    InputVanished,
    InputConflict,
}

impl SelectionCause {
    fn stage(self) -> FailureStage {
        match self {
            Self::TimedOut => FailureStage::SelectionTimedOut,
            Self::InputVanished => FailureStage::SelectionInputVanished,
            Self::InputConflict => FailureStage::SelectionInputConflict,
        }
    }
}

impl std::fmt::Display for SelectionCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.stage().label())
    }
}

impl ToolError {
    pub(crate) fn variant(&self) -> &'static str {
        match self {
            Self::SpillUnavailable => "SpillUnavailable",
            Self::AtPath { source, .. } => source.variant(),
            Self::MissingConfiguration => "MissingConfiguration",
            Self::Configuration => "Configuration",
            Self::HostPrefixConflict { .. } => "HostPrefixConflict",
            Self::InactiveHostArchiveUnconfirmed => "InactiveHostArchiveUnconfirmed",
            Self::InactiveHostArchiveConflict => "InactiveHostArchiveConflict",
            Self::CommandEnvironmentUnconfirmed => "CommandEnvironmentUnconfirmed",
            Self::AccountBusy => "AccountBusy",
            Self::ProtectionUnavailable => "ProtectionUnavailable",
            Self::QueueFull => "QueueFull",
            Self::ChangedInput => "ChangedInput",
            Self::Path => "Path",
            Self::OutsideWorkspace => "OutsideWorkspace",
            Self::ProtectedPath => "ProtectedPath",
            Self::SymbolicLink => "SymbolicLink",
            Self::FilesystemBoundary => "FilesystemBoundary",
            Self::NotDirectory => "NotDirectory",
            Self::UnterminatedQuotedPath => "UnterminatedQuotedPath",
            Self::NotFound => "NotFound",
            Self::FilesystemDenied => "FilesystemDenied",
            Self::Limit => "Limit",
            Self::OutputCeiling => "OutputCeiling",
            Self::Timeout(_) => "Timeout",
            Self::DiskSpace(_) => "DiskSpace",
            Self::Conflict => "Conflict",
            Self::Conflicts(_) => "Conflicts",
            Self::Pattern(_) => "Pattern",
            Self::Operation => "Operation",
            Self::Cancelled => "Cancelled",
            Self::Uncertain => "Uncertain",
            Self::StaleGitLocks(_) => "StaleGitLocks",
            Self::GuardRejected(_) => "GuardRejected",
            Self::GuardBootstrap(_) => "GuardBootstrap",
            Self::ProjectSelection(_) => "ProjectSelection",
        }
    }

    pub(crate) fn in_selection(self) -> Self {
        match self {
            Self::Timeout(_) => Self::ProjectSelection(SelectionCause::TimedOut),
            Self::NotFound => Self::ProjectSelection(SelectionCause::InputVanished),
            Self::Conflict => Self::ProjectSelection(SelectionCause::InputConflict),
            error => error,
        }
    }

    pub(crate) fn at_path(self, path: &str) -> Self {
        let source = match self {
            Self::AtPath { source, .. } => source,
            error => Box::new(error),
        };
        Self::AtPath {
            path: path.to_owned(),
            source,
        }
    }
}

#[derive(Clone)]
pub(crate) struct PermissionSnapshot {
    config: std::sync::Arc<permissions::PermissionConfig>,
    pub(crate) digest: [u8; 32],
}

#[derive(Clone)]
pub(crate) struct ToolRuntime {
    pub(crate) host_settings_admitted: bool,
    host_settings_saved: std::sync::Arc<std::sync::Mutex<Option<[u8; 32]>>>,
    guard_available: bool,
    config: Config,
    git_environment: Option<std::sync::Arc<git::Environment>>,
    // Hooks the user accepted for this folder that still match on disk; `None` keeps hooks off.
    git_hooks: Option<std::sync::Arc<git::Hooks>>,
    // Whether a human can answer approvals in this Run.
    human_approvals: bool,
    handover: Option<std::sync::Arc<[u8]>>,
    pub(crate) definitions: std::sync::Arc<definitions::DefinitionSnapshots>,
    permissions: Option<PermissionSnapshot>,
    // A runtime rebuilt inside a Guard unit has no settings, only its Task's result filter.
    result_filter: Option<permissions::HiddenPaths>,
    unguarded_rules: Option<unguarded_rules::ExactCommandRules>,
    workspace: PathBuf,
    // Set at admission; `None` only in a runtime rebuilt inside a Guard unit, which never reads in process.
    reads: Option<InProcessReads>,
    workspace_identity: WorkspaceIdentity,
    guard_executable: PathBuf,
    enforcement_digest: [u8; 32],
    process_enforcement_digest: [u8; 32],
    // Shared by clones within a Run; admission replaces it, so a raise never reaches another Run.
    context_limit_bytes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    queue_progress: Option<tokio::sync::watch::Sender<Option<uuid::Uuid>>>,
    active_action_gate: Option<tokio::sync::mpsc::Sender<ActiveActionRequest>>,
    hook_stdin: Option<std::sync::Arc<str>>,
    checkpoint_gate: Option<tokio::sync::mpsc::Sender<CheckpointRequest>>,
    cache_state: Option<std::sync::Arc<StateRoot>>,
    command_environment: std::sync::Arc<CommandEnvironment>,
    command_tools: std::sync::Arc<CommandTools>,
    spill_access: Option<std::sync::Arc<crate::store::SpillAccess>>,
    #[cfg(target_os = "macos")]
    system_programs: Option<std::sync::Arc<guard::SystemPrograms>>,
}

struct RuntimeAdmission<'a> {
    config: Config,
    permissions: Option<PermissionSnapshot>,
    unguarded_rules: Option<unguarded_rules::ExactCommandRules>,
    workspace: &'a Path,
    state: &'a StateRoot,
    guard_executable: PathBuf,
}

struct RuntimeState {
    root: cap_std::fs::Dir,
    command_environment: std::sync::Arc<CommandEnvironment>,
    command_tools: std::sync::Arc<CommandTools>,
    workspace_identity: WorkspaceIdentity,
    guard_available: bool,
    enforcement_digest: [u8; 32],
    process_enforcement_digest: [u8; 32],
    git_environment: Option<std::sync::Arc<git::Environment>>,
    definitions: definitions::DefinitionSnapshots,
    #[cfg(target_os = "macos")]
    system_programs: Option<std::sync::Arc<guard::SystemPrograms>>,
}

fn admission_paths(
    workspace: &Path,
    state: &StateRoot,
    config: &Config,
    root: &cap_std::fs::Dir,
) -> Result<PathBuf, ToolError> {
    state
        .ensure_outside_workspace(root)
        .map_err(|_| ToolError::Path)?;
    fs::reject_overlap(workspace, state.path())?;
    let account = StateRoot::account_path().map_err(|_| ToolError::Path)?;
    fs::reject_overlap(workspace, &account)?;
    for protected in [workspace, state.path(), account.as_path()] {
        for runtime in guard::runtime_roots() {
            fs::reject_overlap(Path::new(runtime), protected)?;
        }
    }
    for grant in &config.runtime_grants {
        grant.reject_overlap(&[workspace, state.path(), account.as_path()])?;
    }
    Ok(account)
}

// Each admission resolves Git from the user's PATH, so readmission pins the same Git or refuses.
fn git_environment(
    workspace: &Path,
    state: &StateRoot,
) -> Option<std::sync::Arc<git::Environment>> {
    git::Environment::capture(workspace, &state.path().join(editors::EDITORS))
        .map(std::sync::Arc::new)
}

fn skill_roots(
    config: &Config,
) -> std::collections::BTreeMap<String, Option<std::sync::Arc<cap_std::fs::Dir>>> {
    config
        .skills
        .iter()
        .map(|skill| {
            (
                skill.name.clone(),
                fs::open_directory(Path::new(&skill.directory))
                    .ok()
                    .map(std::sync::Arc::new),
            )
        })
        .collect()
}

fn admission_resources(
    config: &Config,
    state: &StateRoot,
    account: &Path,
) -> Result<(), ToolError> {
    for program in config.commands.iter().chain(
        config
            .mcp
            .iter()
            .filter_map(|server| server.program.as_ref()),
    ) {
        program.verify()?;
    }
    for path in config.resource_paths() {
        fs::reject_overlap(Path::new(path), state.path())?;
        fs::reject_overlap(Path::new(path), account)?;
    }
    Ok(())
}

// The pinned configuration digest covers the effective domain set, so every admission and re-admission merges the same way.
fn merge_network(
    config: &mut Config,
    permissions: Option<&PermissionSnapshot>,
) -> Result<(), ToolError> {
    let Some(network) = config.network.take() else {
        return Ok(());
    };
    let (allow, restrict) = permissions.map_or_else(Default::default, |snapshot| {
        snapshot.config.network_domains()
    });
    let network = network.merged(&allow, &restrict);
    if !network.valid() {
        return Err(ToolError::Configuration);
    }
    config.network = Some(network);
    Ok(())
}

struct ProcessPolicy {
    digest: [u8; 32],
    #[cfg(target_os = "macos")]
    programs: Option<std::sync::Arc<guard::SystemPrograms>>,
}

fn process_policy(config: &Config, guard_available: bool) -> Result<ProcessPolicy, ToolError> {
    #[cfg(target_os = "macos")]
    {
        let programs = if !guard_available || config.commands.is_empty() && config.mcp.is_empty() {
            None
        } else {
            Some(std::sync::Arc::new(guard::SystemPrograms::admit()?))
        };
        Ok(ProcessPolicy {
            digest: guard::macos_runtime_enforcement_digest(
                programs.as_deref(),
                &config.runtime_grants,
            )?,
            programs,
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = guard_available;
        Ok(ProcessPolicy {
            digest: guard::enforcement_digest(&config.runtime_grants)?,
        })
    }
}

pub(crate) struct RewindPin {
    pub(crate) workspace_device: u64,
    pub(crate) workspace_inode: u64,
    pub(crate) policy_digest: [u8; 32],
    pub(crate) enforcement_digest: [u8; 32],
}

pub(crate) struct CheckpointRequest {
    pub(crate) reply: tokio::sync::oneshot::Sender<()>,
    pub(crate) go_deadline: tokio::time::Instant,
}

pub(crate) struct ActiveActionRequest {
    pub(crate) admitted_at: tokio::time::Instant,
    pub(crate) reply: tokio::sync::oneshot::Sender<Result<ActiveToolWindow, ToolError>>,
}

#[derive(Clone, Copy)]
pub(crate) struct ActiveToolWindow {
    pub(crate) preparation_deadline: tokio::time::Instant,
    pub(crate) go_deadline: tokio::time::Instant,
    pub(crate) payload_deadline: tokio::time::Instant,
    pub(crate) completion_deadline: tokio::time::Instant,
}

pub(crate) fn load_permissions(state: &StateRoot) -> Result<Option<PermissionSnapshot>, ToolError> {
    state
        .record(crate::store::RecordKind::Permissions)
        .read()
        .map_err(|_| ToolError::Configuration)?
        .map(|bytes| {
            permissions::PermissionConfig::parse(&bytes)
                .map(|config| PermissionSnapshot {
                    config: std::sync::Arc::new(config),
                    digest: crate::validate::digest(&bytes),
                })
                .map_err(|_| ToolError::Configuration)
        })
        .transpose()
}

impl ToolRuntime {
    pub(crate) fn admit_spills(
        &mut self,
        state: &StateRoot,
        run: RunId,
        history: &[crate::session::EventEnvelope],
    ) {
        match state.admit_spills(run, history) {
            Ok(access) => self.spill_access = Some(std::sync::Arc::new(access)),
            Err(_) => crate::diagnostics::record_development_failure(
                crate::diagnostics::DevelopmentFailure::OutputSpill,
            ),
        }
    }

    pub(crate) async fn persist_spill(
        &self,
        observation: &mut ToolObservation,
        mut cancellation: Cancellation,
    ) {
        let Some(mut spill) = observation.spill.take() else {
            return;
        };
        let Some(bytes) = spill.captured.take() else {
            return;
        };
        if !spill::has_excerpt(observation) || cancellation.is_cancelled() {
            return;
        }
        let Some(access) = self.spill_access.clone() else {
            crate::diagnostics::record_development_failure(
                crate::diagnostics::DevelopmentFailure::OutputSpill,
            );
            return;
        };
        let gate = self
            .cache_state
            .as_ref()
            .map(|state| crate::fault::spill_write_gate(state.path()))
            .unwrap_or_default();
        let writer = spill::Writer::start(
            std::sync::Arc::clone(&access),
            spill.r#ref.clone(),
            bytes,
            gate,
        );
        let retained = if let Some(mut writer) = writer {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => false,
                result = tokio::time::timeout(Duration::from_millis(spill::SPILL_PERSIST_TIMEOUT_MS), &mut writer.result) => {
                    matches!(result, Ok(Ok(Ok(())))) && !cancellation.is_cancelled()
                }
            }
        } else {
            false
        };
        if retained {
            observation.spill = Some(spill);
        } else {
            crate::diagnostics::record_development_failure(
                crate::diagnostics::DevelopmentFailure::OutputSpill,
            );
        }
    }

    pub(crate) fn retain_spill(&self, observation: &ToolObservation) {
        if let Some(spill) = &observation.spill
            && self
                .spill_access
                .as_ref()
                .is_some_and(|access| access.retain(&spill.r#ref).is_err())
        {
            crate::diagnostics::record_development_failure(
                crate::diagnostics::DevelopmentFailure::OutputSpill,
            );
        }
    }

    pub(crate) fn with_queue_progress(
        mut self,
        sender: Option<tokio::sync::watch::Sender<Option<uuid::Uuid>>>,
    ) -> Self {
        self.queue_progress = sender;
        self
    }

    pub(crate) fn with_active_action_gate(
        mut self,
        sender: tokio::sync::mpsc::Sender<ActiveActionRequest>,
    ) -> Self {
        self.active_action_gate = Some(sender);
        self
    }

    pub(crate) fn with_checkpoint_gate(
        mut self,
        sender: tokio::sync::mpsc::Sender<CheckpointRequest>,
    ) -> Self {
        self.checkpoint_gate = Some(sender);
        self
    }

    pub(crate) fn rewind_path_allowed(&self, path: &str) -> bool {
        self.config.write
            && self.config.in_workspace_scope(path)
            && self.permissions().is_none_or(|permissions| {
                permissions
                    .evaluate(&ToolCall::Write {
                        path: path.into(),
                        expected_digest: None,
                        content: String::new(),
                    })
                    .0
                    != permissions::Decision::Deny
            })
    }

    pub(crate) fn rewind_pin(&self) -> RewindPin {
        RewindPin {
            workspace_device: self.workspace_identity.device,
            workspace_inode: self.workspace_identity.inode,
            policy_digest: self.config.digest(),
            enforcement_digest: FileReceipt::enforcement_digest(),
        }
    }

    pub(crate) fn rewind_batch(&self, files: &[FileChange]) -> Result<(), ToolError> {
        fs::validate_workspace(self)?;
        let reads = self
            .reads
            .as_ref()
            .ok_or(ToolError::ProtectionUnavailable)?;
        let _permit = reads
            .slots
            .try_acquire()
            .map_err(|_| ToolError::QueueFull)?;
        fs::worker_publication_paths(&reads.root, files)?;
        fs::validate_workspace(self)
    }

    pub(crate) fn rewind_file(&self, path: &str) -> Result<Option<FileVersion>, ToolError> {
        if !self.rewind_path_allowed(path) {
            return Err(ToolError::Path);
        }
        fs::validate_workspace(self)?;
        let reads = self
            .reads
            .as_ref()
            .ok_or(ToolError::ProtectionUnavailable)?;
        let _permit = reads
            .slots
            .try_acquire()
            .map_err(|_| ToolError::QueueFull)?;
        let file = fs::rewind_file(&reads.root, path)?;
        fs::validate_workspace(self)?;
        Ok(file)
    }

    pub(crate) fn checkpoint_bytes(
        &self,
        intent: &EffectIntent,
        paths: &[(String, String)],
    ) -> Result<Vec<(String, Vec<u8>)>, ToolError> {
        let root = self.pinned_root(intent)?;
        let _permit = self
            .reads
            .as_ref()
            .ok_or(ToolError::ProtectionUnavailable)?
            .slots
            .try_acquire()
            .map_err(|_| ToolError::Operation)?;
        let files = match &intent.call {
            ToolCall::Write { .. }
            | ToolCall::Edit { .. }
            | ToolCall::MultiEdit { .. }
            | ToolCall::Delete { .. }
            | ToolCall::Move { .. }
            | ToolCall::ApplyChangeset {
                source: crate::session::worker::ChangesetSource::EditorWorkspace { .. },
            } => paths
                .iter()
                .map(|(path, expected)| {
                    let bytes =
                        fs::read_regular(fs::open_file(&root, path)?, types::MAX_FILE_BYTES)?;
                    if crate::validate::hex_digest(&bytes) != *expected {
                        return Err(ToolError::ChangedInput);
                    }
                    Ok((path.clone(), bytes))
                })
                .collect::<Result<Vec<_>, ToolError>>()?,
            ToolCall::ApplyChangeset {
                source: crate::session::worker::ChangesetSource::CommandReceipt { changeset, .. },
            } => paths
                .iter()
                .map(|(path, expected)| {
                    let before = changeset
                        .files
                        .iter()
                        .find(|f| &f.path == path)
                        .and_then(|f| f.before.as_ref())
                        .ok_or(ToolError::ChangedInput)?;
                    if &before.sha256 != expected {
                        return Err(ToolError::ChangedInput);
                    }
                    Ok((path.clone(), before.bytes().ok_or(ToolError::ChangedInput)?))
                })
                .collect::<Result<Vec<_>, ToolError>>()?,
            _ => return Err(ToolError::Configuration),
        };
        self.pinned_root(intent)?;
        Ok(files)
    }

    pub(crate) fn with_hook_stdin(mut self, stdin: &str) -> Self {
        self.hook_stdin = Some(stdin.into());
        self
    }

    pub(crate) fn hook_task_fits(&self, intent: &EffectIntent) -> bool {
        guard::task_fits(self, intent)
    }

    pub(crate) async fn begin_active_action(
        &self,
        admitted_at: tokio::time::Instant,
    ) -> Result<Option<ActiveToolWindow>, ToolError> {
        let Some(sender) = &self.active_action_gate else {
            return Ok(None);
        };
        let (reply, receipt) = tokio::sync::oneshot::channel();
        sender
            .send(ActiveActionRequest { admitted_at, reply })
            .await
            .map_err(|_| ToolError::Operation)?;
        receipt
            .await
            .map_err(|_| ToolError::Operation)
            .and_then(|result| result.map(Some))
    }

    pub(crate) async fn before_dispatch(
        &self,
        window: Option<ActiveToolWindow>,
        runtime_ms: u32,
        cancellation: &Cancellation,
    ) -> Result<(), ToolError> {
        if cancellation.is_cancelled() {
            return Err(ToolError::Operation);
        }
        if let Some(sender) = &self.checkpoint_gate {
            let (reply, receipt) = tokio::sync::oneshot::channel();
            let deadline = window.map_or_else(
                || tokio::time::Instant::now() + Duration::from_millis(u64::from(runtime_ms)),
                |window| window.go_deadline,
            );
            tokio::time::timeout_at(deadline, async {
                sender
                    .send(CheckpointRequest {
                        reply,
                        go_deadline: deadline,
                    })
                    .await
                    .map_err(|_| ToolError::Operation)?;
                receipt.await.map_err(|_| ToolError::Operation)
            })
            .await
            .map_err(|_| ToolError::Limit)??;
        }
        if cancellation.is_cancelled() {
            return Err(ToolError::Operation);
        }
        if window.is_some_and(|window| {
            let now = tokio::time::Instant::now();
            now >= window.go_deadline
                || now
                    .checked_add(std::time::Duration::from_millis(u64::from(runtime_ms)))
                    .is_none_or(|payload_end| payload_end > window.payload_deadline)
        }) {
            return Err(ToolError::Limit);
        }
        Ok(())
    }

    pub(crate) fn queue_changed(&self, id: uuid::Uuid, queued: bool) {
        if let Some(sender) = &self.queue_progress {
            sender.send_replace(queued.then_some(id));
        }
    }
    pub(crate) fn workspace_identity(&self) -> WorkspaceIdentity {
        self.workspace_identity
    }

    pub(crate) fn set_context_limit(&mut self, limit: usize) -> Result<(), ToolError> {
        if !(types::MIN_TOOL_CONTEXT_BYTES..=crate::session::MAX_WINDOW_CONTENT_BYTES)
            .contains(&limit)
        {
            return Err(ToolError::Limit);
        }
        self.context_limit_bytes = std::sync::Arc::new(limit.into());
        Ok(())
    }

    // Calibration only raises the allowance, within the hard window ceiling.
    pub(crate) fn raise_context_limit(&self, limit: usize) {
        self.context_limit_bytes.fetch_max(
            limit.min(crate::session::MAX_WINDOW_CONTENT_BYTES),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    pub(crate) fn context_limit_bytes(&self) -> usize {
        self.context_limit_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn result_capacity(&self, overhead: usize, reserved: usize) -> Option<usize> {
        let capacity = self
            .context_limit_bytes()
            .saturating_sub(overhead.saturating_add(reserved));
        (capacity >= 128).then_some(capacity)
    }

    pub(crate) fn continuation_reserve(turn_bytes: usize, feedback: usize) -> usize {
        types::MAX_TOOL_RESULT_BYTES + turn_bytes + Self::step_context_reserve(true, feedback)
    }

    pub(crate) fn step_context_reserve(command_integration: bool, feedback: usize) -> usize {
        512 + feedback
            + if command_integration {
                types::MAX_COMMAND_INTEGRATION_ARGUMENT_BYTES + 128 * 6 + 512
            } else {
                0
            }
    }
    pub(crate) fn allows(&self, call: &ToolCall) -> bool {
        match call {
            _ if call.facts().site == Site::Git
                && call.mutates()
                && !git::offers_write(self, call) =>
            {
                false
            }
            ToolCall::Command { program, .. } if Path::new(program).is_absolute() => {
                self.command_tools.entries().iter().any(|tool| {
                    tool.enabled
                        && tool.role == CommandToolRole::Prefix
                        && tool.executable == *program
                })
            }
            _ => self.config.allows(call),
        }
    }
    pub(crate) fn allows_goal_contract(&self, contract: &crate::session::GoalPlanContract) -> bool {
        let criterion_allowed = match &contract.success_criterion {
            crate::session::GoalSuccessCriterion::ToolCallSucceeded { call } => {
                self.config.allows(call)
            }
            crate::session::GoalSuccessCriterion::WorkspaceFileDigest { path, .. }
            | crate::session::GoalSuccessCriterion::WorkspaceFileContent { path, .. } => {
                self.config.in_workspace_scope(path)
            }
        };
        criterion_allowed
            && contract
                .paths
                .iter()
                .all(|path| self.config.in_workspace_scope(path))
            && contract.commands.iter().all(|command| {
                self.config
                    .commands
                    .iter()
                    .any(|program| program.name == *command)
            })
            && contract.skills.iter().all(|binding| {
                self.config.allows(&ToolCall::Skill {
                    name: binding.name.clone(),
                    resource: binding.resource.clone(),
                })
            })
            && contract.mcp_tools.iter().all(|binding| {
                self.config.mcp.iter().any(|server| {
                    server.name == binding.server && server.tools.contains(&binding.tool)
                })
            })
    }

    // The view is the disk headroom above the Guard reserve; a set size caps only publication.
    pub(crate) fn effective_cache_bytes(
        &self,
        requested: BuildCacheSize,
    ) -> Result<(u64, u64), ToolError> {
        if !requested.enabled() {
            return Ok((0, 0));
        }
        let Some(state) = self.cache_state.as_ref() else {
            return Ok((0, 0));
        };
        let identity = crate::store::BuildCacheIdentity::new(
            &self.workspace,
            self.workspace_identity.device,
            self.workspace_identity.inode,
        )
        .map_err(|_| ToolError::Operation)?;
        match state.build_cache_headroom(&identity, types::GUARD_DISK_RESERVE_BYTES) {
            Ok(view) => Ok((
                view,
                match requested {
                    BuildCacheSize::Bytes(limit) => view.min(limit),
                    BuildCacheSize::Unlimited => view,
                },
            )),
            Err(crate::store::StoreError::StorageFull) => Ok((0, 0)),
            Err(_) => Err(ToolError::Operation),
        }
    }

    pub(crate) fn readmit_compaction_hooks(
        state: &StateRoot,
        workspace: &Path,
        guard: Option<PathBuf>,
        pin: &ToolPolicyReceipt,
    ) -> Result<Option<Self>, ToolError> {
        let Some(digest) = pin.permissions_digest else {
            return Ok(None);
        };
        let snapshot = load_permissions(state)?.ok_or(ToolError::ChangedInput)?;
        if snapshot.digest != digest {
            return Err(ToolError::ChangedInput);
        }
        if snapshot
            .config
            .hooks()
            .groups(permissions::HookEvent::PreCompact)
            .is_empty()
        {
            return Ok(None);
        }
        let environment = CommandEnvironment::load(state)?;
        let git = git_environment(workspace, state)
            .as_deref()
            .and_then(git::resolve);
        let mut config = match Config::load(state) {
            Ok(mut config) => {
                config.discover_skills(workspace)?;
                config.bind_environment(&environment);
                merge_network(&mut config, Some(&snapshot))?;
                config.git = git.clone();
                if config.digest() == pin.config_digest {
                    config
                } else {
                    WorkspacePermissions::open(workspace)
                        .map_err(|_| ToolError::ChangedInput)?
                        .config(workspace)?
                }
            }
            Err(ToolError::MissingConfiguration) => WorkspacePermissions::open(workspace)
                .map_err(|_| ToolError::ChangedInput)?
                .config(workspace)?,
            Err(error) => return Err(error),
        };
        config.discover_skills(workspace)?;
        config.bind_environment(&environment);
        merge_network(&mut config, Some(&snapshot))?;
        config.git = git;
        if config.digest() != pin.config_digest {
            return Err(ToolError::ChangedInput);
        }
        let runtime = Self::admit(
            config,
            Some(snapshot),
            Self::load_exact_command_rules(state)?,
            workspace,
            state,
            guard.ok_or(ToolError::ProtectionUnavailable)?,
        )?;
        if runtime.receipt().config_digest != pin.config_digest {
            return Err(ToolError::ChangedInput);
        }
        Ok(Some(runtime))
    }

    pub(crate) fn load_exact_command_rules(
        state: &StateRoot,
    ) -> Result<Option<unguarded_rules::ExactCommandRules>, ToolError> {
        unguarded_rules::ExactCommandRules::load(state)
    }

    pub(crate) fn admit(
        config: Config,
        permissions: Option<PermissionSnapshot>,
        unguarded_rules: Option<unguarded_rules::ExactCommandRules>,
        workspace: &Path,
        state: &StateRoot,
        guard_executable: PathBuf,
    ) -> Result<Self, ToolError> {
        Self::admit_runtime(RuntimeAdmission {
            config,
            permissions,
            unguarded_rules,
            workspace,
            state,
            guard_executable,
        })
    }

    fn admit_runtime(admission: RuntimeAdmission<'_>) -> Result<Self, ToolError> {
        let RuntimeAdmission {
            mut config,
            permissions,
            unguarded_rules,
            workspace,
            state,
            guard_executable,
        } = admission;
        #[cfg(target_os = "macos")]
        let capture = guard::FixtureSpan::begin(guard::FixtureStage::ToolAdmission);
        let command_environment = std::sync::Arc::new(CommandEnvironment::load(state)?);
        let command_tools = std::sync::Arc::new(CommandTools::load(state)?);
        let guard_available = guard::check_available().is_ok();
        let enforcement_digest = guard::enforcement_digest(&[])?;
        let root = fs::open_directory(workspace)?;
        let account = admission_paths(workspace, state, &config, &root)?;
        let workspace_identity = fs::workspace_identity(&root)?;
        let git_environment = git_environment(workspace, state);
        config.git = git_environment.as_deref().and_then(git::resolve);
        let permissions = permissions
            .map(|mut snapshot| {
                let mut scoped = (*snapshot.config).clone();
                scoped.scope_to(workspace, workspace_identity);
                if let Some(index) = snapshot.config.host.iter().position(|rule| {
                    scoped.host.contains(rule)
                        && (!rule.admitted_by(&config)
                            || !rule.admitted_by_command_tools(&command_tools))
                }) {
                    return Err(ToolError::HostPrefixConflict { rule: index + 1 });
                }
                snapshot.config = std::sync::Arc::new(scoped);
                Ok(snapshot)
            })
            .transpose()?;
        merge_network(&mut config, permissions.as_ref())?;
        config.discover_skills(workspace)?;
        config.bind_environment(&command_environment);
        let definitions = definitions::snapshot(&root, state, &config)?;
        if fs::workspace_identity(&fs::open_directory(workspace)?)? != workspace_identity {
            return Err(ToolError::ChangedInput);
        }
        admission_resources(&config, state, &account)?;
        let ProcessPolicy {
            digest: process_enforcement_digest,
            #[cfg(target_os = "macos")]
                programs: system_programs,
        } = process_policy(&config, guard_available)?;
        #[cfg(target_os = "macos")]
        capture.complete();
        Self::from_admission(
            RuntimeAdmission {
                config,
                permissions,
                unguarded_rules,
                workspace,
                state,
                guard_executable,
            },
            RuntimeState {
                root,
                command_environment,
                command_tools,
                workspace_identity,
                guard_available,
                enforcement_digest,
                process_enforcement_digest,
                git_environment,
                definitions,
                #[cfg(target_os = "macos")]
                system_programs,
            },
        )
    }

    fn from_admission(
        admission: RuntimeAdmission<'_>,
        resources: RuntimeState,
    ) -> Result<Self, ToolError> {
        let RuntimeAdmission {
            config,
            permissions,
            unguarded_rules,
            workspace,
            state,
            guard_executable,
        } = admission;
        let RuntimeState {
            root,
            command_environment,
            command_tools,
            workspace_identity,
            guard_available,
            enforcement_digest,
            process_enforcement_digest,
            git_environment,
            definitions,
            #[cfg(target_os = "macos")]
            system_programs,
        } = resources;
        let skills = skill_roots(&config);
        Ok(Self {
            host_settings_admitted: true,
            host_settings_saved: Default::default(),
            guard_available,
            definitions: std::sync::Arc::new(definitions),
            config,
            git_environment,
            git_hooks: None,
            human_approvals: false,
            handover: None,
            permissions,
            result_filter: None,
            unguarded_rules,
            queue_progress: None,
            active_action_gate: None,
            hook_stdin: None,
            checkpoint_gate: None,
            spill_access: None,
            cache_state: Some(std::sync::Arc::new(
                state
                    .try_clone()
                    .map_err(|_| ToolError::ProtectionUnavailable)?,
            )),
            command_environment,
            command_tools,
            workspace: workspace.to_owned(),
            reads: Some(InProcessReads {
                skills,
                root: std::sync::Arc::new(root),
                slots: std::sync::Arc::clone(&IN_PROCESS_READS),
                editors: Default::default(),
            }),
            workspace_identity,
            guard_executable,
            enforcement_digest,
            process_enforcement_digest,
            context_limit_bytes: std::sync::Arc::new(types::MAX_TOOL_CONTEXT_BYTES.into()),
            #[cfg(target_os = "macos")]
            system_programs,
        })
    }

    pub(crate) fn stop_feedback_reserve(&self) -> usize {
        if self.permissions().is_some_and(|config| {
            !config
                .hooks()
                .groups(permissions::HookEvent::Stop)
                .is_empty()
        }) {
            2 * permissions::MAX_STOP_FEEDBACK_BYTES + 256
        } else {
            0
        }
    }

    pub(crate) fn receipt(&self) -> ToolPolicyReceipt {
        let mut receipt = self.config.receipt();
        receipt.max_context_bytes = self.context_limit_bytes() as u32;
        receipt.permissions_digest = self.permissions.as_ref().map(|snapshot| snapshot.digest);
        receipt.unguarded_rules_digest = None;
        receipt.command_tools_digest = Some(self.command_tools.digest());
        receipt.host_prefixes = self
            .permissions()
            .map_or_else(Vec::new, |permissions| permissions.host.clone());
        if let Some(git) = &mut receipt.git {
            git.hooks = self.git_hooks.as_ref().map(|hooks| hooks.digest);
        }
        receipt
    }

    // Accepted hooks run only while they match what the user accepted for this folder.
    pub(crate) async fn admit_git_hooks(
        &mut self,
        accepted: Option<[u8; 32]>,
        human_approvals: bool,
    ) {
        self.human_approvals = human_approvals;
        self.git_hooks = git::accepted_hooks(self, accepted)
            .await
            .map(std::sync::Arc::new);
    }

    pub(crate) fn git_hooks_pinned(&self) -> bool {
        self.git_hooks.is_some()
    }

    pub(crate) fn hidden_paths(&self, call: &ToolCall) -> permissions::HiddenPaths {
        match (&self.result_filter, self.permissions()) {
            (Some(filter), _) => filter.clone(),
            (None, Some(config)) => config.hidden_paths(call),
            (None, None) => permissions::HiddenPaths::default(),
        }
    }

    pub(crate) fn skill_workspace_path(&self, call: &ToolCall) -> Option<String> {
        let ToolCall::Skill { name, resource } = call else {
            return None;
        };
        self.skill_file_path(name, resource.as_deref().unwrap_or("SKILL.md"))
    }

    fn skill_file_path(&self, name: &str, file: &str) -> Option<String> {
        let skill = self.config.skills.iter().find(|skill| skill.name == name)?;
        Path::new(&skill.directory)
            .strip_prefix(&self.workspace)
            .ok()?
            .join(file)
            .to_str()
            .map(str::to_owned)
    }

    pub(crate) fn permissions(&self) -> Option<&permissions::PermissionConfig> {
        self.permissions
            .as_ref()
            .map(|snapshot| snapshot.config.as_ref())
    }

    pub(crate) fn has_inactive_host_rules(&self) -> bool {
        self.unguarded_rules.is_some()
    }

    pub(crate) fn command_executable(&self, name: &str) -> Option<&str> {
        self.config
            .commands
            .iter()
            .find(|program| program.name == name)
            .map(|program| program.executable.as_str())
    }

    pub(crate) fn record_typed_prefix_rule(
        &self,
        prefix: permissions::TypedGitPrefix,
        expected: Option<[u8; 32]>,
    ) -> Result<[u8; 32], ToolError> {
        fs::validate_workspace(self)?;
        let state = self
            .cache_state
            .as_deref()
            .ok_or(ToolError::Configuration)?;
        permissions::PermissionSettings::remember(
            state,
            &self.workspace,
            self.workspace_identity,
            prefix,
            expected,
        )
        .map_err(|error| match error {
            permissions::SettingsError::Busy => ToolError::AccountBusy,
            permissions::SettingsError::NotConfirmed => ToolError::Uncertain,
            _ => ToolError::Configuration,
        })
    }

    pub(crate) fn host_prefix_candidate(&self, intent: &EffectIntent) -> bool {
        let ToolCall::CommandUnguarded {
            prefix_rule: Some(prefix),
            ..
        } = &intent.call
        else {
            return false;
        };
        prefix.len() >= permissions::MIN_HOST_PREFIX_TOKENS
            && permissions::HostPrefixRule::from_intent(intent, prefix.len())
                .is_ok_and(|rule| rule.prefix == *prefix && rule.admitted_by(&self.config))
    }

    pub(crate) fn record_host_prefix_rule(
        &self,
        intent: &EffectIntent,
        expected: Option<[u8; 32]>,
    ) -> Result<[u8; 32], ToolError> {
        if !self.host_prefix_candidate(intent) {
            return Err(ToolError::Configuration);
        }
        fs::validate_workspace(self)?;
        let state = self
            .cache_state
            .as_deref()
            .ok_or(ToolError::Configuration)?;
        let digest = permissions::PermissionSettings::remember_host(state, intent, expected)
            .map_err(|error| match error {
                permissions::SettingsError::Busy => ToolError::AccountBusy,
                permissions::SettingsError::NotConfirmed => ToolError::Uncertain,
                _ => ToolError::Configuration,
            })?;
        *self
            .host_settings_saved
            .lock()
            .expect("saved host settings") = Some(digest);
        Ok(digest)
    }

    pub(crate) fn allows_skill(&self, name: &str) -> bool {
        self.guard_available
            && self.config.allows(&ToolCall::Skill {
                name: name.to_owned(),
                resource: None,
            })
    }

    pub(crate) fn hides_skill_from_model(&self, name: &str) -> bool {
        self.config
            .skills
            .iter()
            .any(|skill| skill.name == name && skill.disable_model_invocation)
    }

    fn skill_rows(&self) -> Vec<serde_json::Value> {
        self.config.skills.iter().filter(|skill| self.guard_available && !skill.disable_model_invocation
            && !self.skill_file_path(&skill.name, "SKILL.md").is_some_and(|path| self.permissions().is_some_and(|config| config.restricts_read(&path)))).map(|skill| serde_json::json!({"name":skill.name,"description":skill.description.chars().take(96).collect::<String>()})).collect()
    }

    fn command_rows(&self, names: Option<&[String]>) -> Vec<serde_json::Value> {
        self.config.commands.iter()
            .filter(|_| self.guard_available)
            .filter(|p| names.is_none_or(|names| names.contains(&p.name)))
            .map(|program| serde_json::json!({"name":program.name,"interpreter":program.interpreter,
                "inputs":program.inputs.iter().map(|input| format!("/inputs/{}/{}", program.name, input.destination)).collect::<Vec<_>>()}))
            .collect()
    }

    pub(crate) fn context(&self) -> Result<ToolContext, ToolError> {
        let mut catalog = serde_json::json!({
            "host_clock": {"unix_ms": now_ms(), "timezone":"UTC"},
            "workspace_paths": self.config.workspace_paths,
            "write": self.config.write && self.guard_available,
            "file_tools": self.guard_available,
            "commands": self.command_rows(None),
            "command_contract": catalog::COMMAND_CONTRACT,
            "host_commands": true,
            "skills": self.skill_rows(),
            "mcp_servers": self.config.mcp.iter().filter(|_| self.guard_available).map(|server| server.catalog_row(catalog::MCP_PROTOCOL)).collect::<Vec<_>>(),
            "git": if self.config.git_available() {
                git::catalog(self)
            } else {
                serde_json::json!("unavailable")
            },
        });
        if let Some(network) = self
            .config
            .network
            .as_ref()
            .filter(|_| self.guard_available)
        {
            catalog["network"] = serde_json::json!({
                "allow": network.allow,
                "deny": network.deny,
                "contract": catalog::NETWORK_CONTRACT,
            });
        }
        if self.guard_available && cfg!(target_os = "linux") {
            catalog["web"] = serde_json::json!({
                "web_fetch": true,
                "web_search": self.config.web_search.is_some(),
                "contract": catalog::WEB_CONTRACT,
            });
        }
        let context = ToolContext::new(catalog);
        if context.catalog().len() > types::MAX_TOOL_CATALOG_BYTES {
            return Err(ToolError::Limit);
        }
        Ok(context)
    }

    pub(crate) fn worker_policy(&self) -> Option<crate::session::worker::WorkerPolicy> {
        use crate::session::worker::*;
        (self.guard_available && self.config.write && self.definitions.pin().is_some())
            .then(|| WorkerPolicy {
                parent_grant: WorkerGrant {
                    commands: {
                        let mut names: Vec<_> = self
                            .config
                            .commands
                            .iter()
                            .map(|c| c.name.clone())
                            .collect();
                        names.sort();
                        names
                    },
                    read_paths: self.config.workspace_paths.clone(),
                    write_paths: self.config.workspace_paths.clone(),
                    tools: [
                        WorkerTool::List,
                        WorkerTool::Read,
                        WorkerTool::Search,
                        WorkerTool::Glob,
                        WorkerTool::Mkdir,
                        WorkerTool::Write,
                        WorkerTool::Edit,
                        WorkerTool::MultiEdit,
                        WorkerTool::Delete,
                        WorkerTool::Move,
                        WorkerTool::Skill,
                        WorkerTool::McpList,
                        WorkerTool::Command,
                    ]
                    .into_iter()
                    .chain(self.config.worker_web_tools())
                    .collect(),
                    skills: self.config.skills.iter().map(|s| s.name.clone()).collect(),
                    mcp_servers: self
                        .config
                        .mcp
                        .iter()
                        .filter(|_| self.guard_available)
                        .map(|s| s.name.clone())
                        .collect(),
                    permission_mode: self.permissions().map(permissions::PermissionConfig::mode),
                },
                concurrency: 4,
                max_turns: 17,
                max_tools: 16,
            })
            .filter(WorkerPolicy::valid)
    }

    pub(crate) fn with_handover(mut self, bytes: std::sync::Arc<[u8]>) -> Self {
        self.handover = Some(bytes);
        self
    }

    pub(crate) fn editor_context(
        &self,
        grant: &crate::session::worker::WorkerGrant,
        remaining: usize,
    ) -> Result<ToolContext, ToolError> {
        let mut context = self.research_context(remaining)?;
        let mut catalog: serde_json::Value =
            serde_json::from_str(context.catalog()).map_err(|_| ToolError::Configuration)?;
        catalog.as_object_mut().expect("catalog").remove("web");
        catalog["workspace_paths"] = serde_json::json!(grant.read_paths);
        catalog["write"] = serde_json::json!(self.guard_available);
        catalog["skills"] = serde_json::json!([]);
        catalog["mcp_servers"] = serde_json::json!([]);
        catalog["file_tools"] = serde_json::json!(self.guard_available);
        catalog["commands"] = serde_json::json!(self.command_rows(Some(&grant.commands)));
        if !grant.commands.is_empty() {
            catalog["command_contract"] = serde_json::json!(catalog::WORKER_COMMAND_CONTRACT);
        }
        catalog["operations"] = serde_json::json!(
            grant
                .tools
                .iter()
                .filter(|tool| **tool != crate::worker::WorkerTool::Command
                    || !grant.commands.is_empty())
                .map(|tool| tool.label())
                .collect::<Vec<_>>()
        );
        context.set_catalog(&catalog);
        if context.catalog().len() > types::MAX_TOOL_CATALOG_BYTES {
            return Err(ToolError::Limit);
        }
        Ok(context)
    }

    pub(crate) fn research_context(
        &self,
        tool_calls_remaining: usize,
    ) -> Result<ToolContext, ToolError> {
        let mut catalog = serde_json::json!({
            "host_clock": {"unix_ms": now_ms(), "timezone": "UTC"},
            "workspace_paths": self.config.workspace_paths,
            "write": false,
            "file_tools": self.guard_available,
            "commands": [],
            "skills": self.skill_rows(),
            "mcp_servers": self.config.mcp.iter().filter(|server| self.guard_available && server.url.is_none()).map(|server| serde_json::json!({"name":server.name,"tools":server.tools,"protocol":catalog::RESEARCH_MCP_PROTOCOL})).collect::<Vec<_>>(),
            "tool_calls_remaining": tool_calls_remaining,
        });
        if self.config.child_web && self.guard_available && cfg!(target_os = "linux") {
            catalog["web"] = serde_json::json!({
                "web_fetch": self.config.network.is_some(),
                "web_search": self.config.web_search.is_some(),
                "contract": catalog::WEB_CONTRACT,
            });
        }
        let context = ToolContext::new(catalog);
        if context.catalog().len() > types::MAX_TOOL_CATALOG_BYTES {
            return Err(ToolError::Limit);
        }
        Ok(context)
    }

    pub(crate) fn intent(
        &self,
        run_id: RunId,
        agent_run_id: AgentRunId,
        call: ToolCall,
    ) -> EffectIntent {
        let enforcement_digest = if call.facts().family == catalog::Family::File
            && call.facts().mutates
            && call.facts().site == Site::InProcess
        {
            FileReceipt::enforcement_digest()
        } else if call.is_process() {
            self.process_enforcement_digest
        } else if call.facts().site == Site::Git {
            git::enforcement_digest()
        } else {
            self.enforcement_digest
        };
        let limits = if call.is_host() {
            ToolLimits {
                network: NetworkGrant::Host,
                runtime_ms: 600_000,
                cache_requested_bytes: BuildCacheSize::DISABLED,
                cache_bytes: 0,
                cache_candidate_bytes: 0,
                ..ToolLimits::default()
            }
        } else {
            ToolLimits::default()
        };
        EffectIntent {
            host: git::host::pin(
                &self.workspace,
                &call,
                self.config
                    .git
                    .as_ref()
                    .zip(self.git_environment.as_deref()),
            )
            .ok(),
            wrap: None,
            rewrite: Vec::new(),
            host_command: match &call {
                ToolCall::CommandUnguarded { program, .. } => {
                    let requested = self.command_executable(program).unwrap_or(program);
                    unguarded::pin(requested, &self.workspace, &self.command_environment)
                        .ok()
                        .map(|mut pin| {
                            pin.state_roots = [
                                self.cache_state
                                    .as_ref()
                                    .map(|state| state.path().to_string_lossy().into_owned())
                                    .unwrap_or_default(),
                                StateRoot::account_path()
                                    .map(|path| path.to_string_lossy().into_owned())
                                    .unwrap_or_default(),
                            ];
                            pin.permissions_digest =
                                self.permissions.as_ref().map(|snapshot| snapshot.digest);
                            pin
                        })
                }
                _ => None,
            },
            target: self.editor_root(agent_run_id).map_or(
                crate::session::worker::EffectTarget::Workspace,
                |root| crate::session::worker::EffectTarget::EditorWorkspace {
                    assignment_id: agent_run_id,
                    device: root.identity.device,
                    inode: root.identity.inode,
                },
            ),
            id: uuid::Uuid::now_v7(),
            run_id,
            agent_run_id,
            policy_digest: self.config.digest(),
            enforcement_digest,
            workspace_device: self.workspace_identity.device,
            workspace_inode: self.workspace_identity.inode,
            call,
            limits,
            use_count: 1,
        }
    }

    pub(crate) fn host_settings_current(&self, pin: &types::HostCommandPin) -> bool {
        self.cache_state.as_ref().is_some_and(|state| {
            load_permissions(state).is_ok_and(|snapshot| {
                snapshot.map(|value| value.digest)
                    == (*self
                        .host_settings_saved
                        .lock()
                        .expect("saved host settings"))
                    .or(pin.permissions_digest)
            })
        })
    }

    pub(crate) async fn host_interpreter_admitted(&self, intent: &EffectIntent) -> bool {
        let ToolCall::CommandUnguarded { program, .. } = &intent.call else {
            return false;
        };
        let Some(pin) = intent.host_command.as_ref() else {
            return false;
        };
        if (unguarded::interpreter(program) || unguarded::interpreter(&pin.executable))
            && !self.config.commands.iter().any(|command| {
                command.interpreter
                    && command.executable == pin.executable
                    && command.sha256 == pin.sha256
            })
        {
            return false;
        }
        if intent.wrap.is_none()
            && !unguarded::wrapper(program)
            && !unguarded::wrapper(&pin.executable)
        {
            return true;
        }
        let intent = intent.clone();
        let commands = self.config.commands.clone();
        let environment = std::sync::Arc::clone(&self.command_environment);
        tokio::task::spawn_blocking(move || {
            unguarded::wrapper_interpreters_admitted(&intent, &commands, &environment)
        })
        .await
        .unwrap_or(false)
    }

    pub(crate) fn reservation_overhead(&self, call: &ToolCall) -> Duration {
        match call.facts().site {
            Site::InProcess | Site::Unguarded | Site::Host | Site::Git => Duration::ZERO,
            Site::Guard => guard::RESERVATION_OVERHEAD,
        }
    }

    pub(crate) async fn execute(
        &self,
        intent: EffectIntent,
        cancellation: Cancellation,
    ) -> ToolObservation {
        Box::pin(self.execute_inner(intent, cancellation)).await
    }

    async fn execute_inner(
        &self,
        intent: EffectIntent,
        cancellation: Cancellation,
    ) -> ToolObservation {
        if matches!(intent.call.facts().site, Site::Guard | Site::InProcess)
            && !self.guard_available
        {
            return unstarted_observation(intent, ToolError::ProtectionUnavailable);
        }
        if matches!(
            intent.call,
            ToolCall::ApplyChangeset {
                source: crate::session::worker::ChangesetSource::Rewind { .. }
            }
        ) && let Some(bytes) = &self.handover
        {
            return self
                .clone()
                .with_handover(bytes.clone())
                .run_file(intent, cancellation)
                .await;
        }
        if let Err(error) = self.verify_wrap_async(&intent).await {
            return unstarted_observation(intent, error);
        }
        if !self.target_admitted(&intent) || !command_tools::prefix::allowed(&self.config, &intent)
        {
            return ToolObservation {
                spill: None,
                enforcement_cause: None,
                limit_cause: None,
                intent,
                disposition: ToolDisposition::Denied,
                output: "Tool operation is outside the pinned grant".into(),
                attestation: None,
                approval_cause: None,
                guard_stage: None,
                cache_update: crate::tools::CacheUpdate::NotApplicable,
            };
        }
        match intent.call.facts().site {
            Site::InProcess if intent.call.facts().mutates => {
                self.run_file(intent, cancellation).await
            }
            Site::InProcess => self.run_in_process(intent, cancellation).await,
            Site::Host => {
                if let Err(error) = self.pinned_root(&intent) {
                    return unstarted_observation(intent, error);
                }
                git::host::run(
                    &self.config,
                    self.git_environment.as_deref(),
                    intent,
                    cancellation,
                )
                .await
            }
            Site::Git => {
                if let Err(error) = self.pinned_root(&intent) {
                    return unstarted_observation(intent, error);
                }
                // Boxed so Git's state machine stays off the dispatching task's stack.
                Box::pin(git::run(self, intent, cancellation)).await
            }
            Site::Unguarded => self.run_unguarded(intent, cancellation).await,
            Site::Guard => {
                if let Err(error) = crate::fault::guard_before_dispatch(&intent.call) {
                    return unstarted_observation(intent, error);
                }
                match guard::execute(self, &intent, cancellation).await {
                    Ok(observation) => observation,
                    Err(error) => {
                        let cache_update =
                            guard_error_cache_update(&intent.call, self.cache_state.is_some());
                        uncertain_observation(intent, error, cache_update)
                    }
                }
            }
        }
    }

    async fn run_file(&self, intent: EffectIntent, cancellation: Cancellation) -> ToolObservation {
        if !self.guard_available {
            return unstarted_observation(intent, ToolError::ProtectionUnavailable);
        }
        if !fs::in_process_mutation(&intent) {
            return unstarted_observation(intent, ToolError::Configuration);
        }
        let started = tokio::time::Instant::now();
        let window = match self.begin_active_action(started).await {
            Ok(window) => window,
            Err(error) => return unstarted_observation(intent, error),
        };
        let deadline = started + Duration::from_millis(u64::from(intent.limits.runtime_ms));
        let deadline = window.map_or(deadline, |window| deadline.min(window.payload_deadline));
        if let Err(error) = self.before_dispatch(window, 0, &cancellation).await {
            return if cancellation.is_cancelled() {
                unstarted_observation(intent, ToolError::Cancelled)
            } else if matches!(error, ToolError::Limit) {
                unattested_limit(intent, LimitCause::Timeout, "Tool runtime expired")
            } else {
                unstarted_observation(intent, error)
            };
        }
        let control = fs::MutationControl::new(intent.id, cancellation, deadline);
        let Some(Ok(permit)) = self
            .reads
            .as_ref()
            .map(|reads| reads.slots.clone().try_acquire_owned())
        else {
            return unattested_limit(
                intent,
                LimitCause::ReadCapacity,
                "In-process file capacity is full; nothing was changed",
            );
        };
        let blocking_control = control.clone();
        let (runtime, file_intent) = (self.clone(), intent.clone());
        let writing = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let control = blocking_control;
            let root = runtime.pinned_root(&file_intent)?;
            let result = if let Some(bytes) = &runtime.handover {
                let changeset =
                    guard::transfer::decode_handover(&file_intent, bytes, &runtime.config)?;
                fs::apply_worker_changeset(&root, &changeset, &control)
            } else if let ToolCall::ApplyChangeset {
                source: crate::session::worker::ChangesetSource::EditorWorkspace { files },
            } = &file_intent.call
            {
                let tree = runtime
                    .editor_root(file_intent.agent_run_id)
                    .ok_or(ToolError::Configuration)?
                    .dir;
                if !runtime.config.allows(&file_intent.call) {
                    return Err(ToolError::Path);
                }
                fs::apply_editor_changes(&root, &tree, files, &control)
            } else {
                fs::mutate(&root, &file_intent.call, &runtime.config, &control)
            };
            Ok::<_, ToolError>(result)
        });
        let result = match tokio::time::timeout_at(deadline, writing).await {
            Ok(Ok(Ok(result))) => result,
            Ok(Ok(Err(error))) => return unstarted_observation(intent, error),
            Ok(Err(_)) => Err(ToolError::Uncertain),
            Err(_) => Err(control.expire()),
        };
        let observation = self.in_process_observation(intent.clone(), result);
        crate::fault::guard_cache_report(&observation).await;
        match crate::fault::guard_result(observation.intent.id, Ok(observation)) {
            Ok(observation) => observation,
            Err(error) => uncertain_observation(intent, error, CacheUpdate::NotApplicable),
        }
    }

    async fn run_unguarded(
        &self,
        intent: EffectIntent,
        cancellation: Cancellation,
    ) -> ToolObservation {
        let ToolCall::CommandUnguarded {
            program, args, cwd, ..
        } = &intent.call
        else {
            return unstarted_observation(intent, ToolError::Configuration);
        };
        if let Err(error) = self.pinned_root(&intent) {
            return unstarted_observation(intent, error);
        }
        if unguarded::prohibited_intent(&intent) {
            let output = unguarded::manual_handoff(&intent.call);
            return ToolObservation {
                spill: None,
                enforcement_cause: None,
                limit_cause: None,
                approval_cause: None,
                guard_stage: None,
                intent,
                disposition: ToolDisposition::Denied,
                output,
                attestation: None,
                cache_update: CacheUpdate::NotApplicable,
            };
        }
        let Some(pin) = &intent.host_command else {
            return unstarted_observation(intent, ToolError::Configuration);
        };
        if !self.host_interpreter_admitted(&intent).await {
            return unstarted_observation(intent, ToolError::Configuration);
        }
        if !self.host_settings_admitted || !self.host_settings_current(pin) {
            return unstarted_observation(intent, ToolError::ChangedInput);
        }
        let checked_pin = pin.clone();
        let workspace = self.workspace.clone();
        let environment = std::sync::Arc::clone(&self.command_environment);
        if !tokio::task::spawn_blocking(move || checked_pin.verify(&workspace, &environment))
            .await
            .is_ok_and(|result| result.is_ok())
        {
            return unstarted_observation(intent, ToolError::ChangedInput);
        }
        let pinned = config::Program {
            name: program.clone(),
            executable: pin.executable.clone(),
            sha256: pin.sha256.clone(),
            interpreter: true,
            inputs: Vec::new(),
        };
        let outcome = unguarded::run(
            unguarded::Request {
                launcher: &self.guard_executable,
                program: &pinned,
                host: Some(pin),
                environment: &self.command_environment,
                args,
                cwd,
                workspace: &self.workspace,
                workspace_identity: self.workspace_identity,
                runtime_grants: &self.config.runtime_grants,
                result_bytes: intent.limits.result_bytes,
                runtime_ms: intent.limits.runtime_ms,
            },
            cancellation,
        )
        .await;
        let attestation = outcome.spawned.then(|| {
            Attestation::Unguarded(types::UnguardedReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                executable_sha256: pinned.sha256.clone(),
                pid: outcome.pid,
                exit_code: outcome.exit_code,
                descendants_confirmed_stopped: outcome.descendants_stopped,
            })
        });
        let mut observation = ToolObservation {
            spill: None,
            enforcement_cause: None,
            limit_cause: outcome.limit_cause,
            attestation,
            intent,
            disposition: outcome.disposition,
            output: outcome.output,
            approval_cause: None,
            guard_stage: None,
            cache_update: CacheUpdate::NotApplicable,
        };
        if self.hook_stdin.is_none() {
            observation.spill = spill::OutputSpill::captured(&observation, outcome.captured);
        }
        if !observation.valid() {
            observation.spill = None;
            observation.disposition = ToolDisposition::Limit;
            observation.limit_cause = Some(crate::tools::LimitCause::Resource);
            observation.output = "Tool result exceeded its reserved serialized capacity".into();
        }
        #[cfg(any(test, feature = "test-support"))]
        if observation.attestation.is_some() {
            let intent = observation.intent.clone();
            return crate::fault::guard_result(intent.id, Ok(observation)).unwrap_or_else(
                |error| uncertain_observation(intent, error, CacheUpdate::NotApplicable),
            );
        }
        observation
    }

    fn pinned_root(
        &self,
        intent: &EffectIntent,
    ) -> Result<std::sync::Arc<cap_std::fs::Dir>, ToolError> {
        let (root, identity) = match intent.target {
            crate::session::worker::EffectTarget::Workspace => (
                self.reads
                    .as_ref()
                    .map(|reads| std::sync::Arc::clone(&reads.root))
                    .ok_or(ToolError::ProtectionUnavailable)?,
                (intent.workspace_device, intent.workspace_inode),
            ),
            crate::session::worker::EffectTarget::EditorWorkspace {
                assignment_id,
                device,
                inode,
            } => (
                self.editor_root(assignment_id)
                    .ok_or(ToolError::Configuration)?
                    .dir,
                (device, inode),
            ),
        };
        if !intent.valid()
            || intent.policy_digest != self.config.digest()
            || intent.enforcement_digest
                != if fs::in_process_mutation(intent) {
                    FileReceipt::enforcement_digest()
                } else if intent.call.is_process() {
                    self.build_jobs_digest(intent)?
                } else if intent.call.facts().family == catalog::Family::Web {
                    self.web_digest(intent)?
                } else if intent.call.facts().site == Site::Git {
                    git::enforcement_digest()
                } else {
                    self.enforcement_digest
                }
        {
            return Err(ToolError::Configuration);
        }
        if fs::identity(&root)? != identity {
            return Err(ToolError::ChangedInput);
        }
        fs::validate_workspace(self)?;
        Ok(root)
    }

    pub(crate) fn attach_editor(&self, id: AgentRunId, path: &Path) -> Result<(), ToolError> {
        let reads = self
            .reads
            .as_ref()
            .ok_or(ToolError::ProtectionUnavailable)?;
        let dir = fs::open_directory(path)?;
        let identity = fs::workspace_identity(&dir)?;
        reads
            .editors
            .lock()
            .map_err(|_| ToolError::Operation)?
            .insert(
                id,
                EditorRoot {
                    path: path.to_owned(),
                    dir: std::sync::Arc::new(dir),
                    identity,
                },
            );
        Ok(())
    }

    pub(crate) fn detach_editor(&self, id: AgentRunId) {
        if let Some(reads) = &self.reads
            && let Ok(mut editors) = reads.editors.lock()
        {
            editors.remove(&id);
        }
    }

    fn editor_root(&self, id: AgentRunId) -> Option<EditorRoot> {
        self.reads.as_ref()?.editors.lock().ok()?.get(&id).cloned()
    }

    // An editor-workspace effect needs that editor's attached root; only typed file operations and commands run there.
    fn target_admitted(&self, intent: &EffectIntent) -> bool {
        match intent.target {
            crate::session::worker::EffectTarget::Workspace => true,
            crate::session::worker::EffectTarget::EditorWorkspace { assignment_id, .. } => {
                self.editor_root(assignment_id).is_some()
                    && (matches!(intent.call, ToolCall::Command { .. })
                        || intent.call.facts().family == catalog::Family::File)
            }
        }
    }

    pub(crate) fn editor_tree(&self, intent: &EffectIntent) -> Option<PathBuf> {
        match intent.target {
            crate::session::worker::EffectTarget::EditorWorkspace { assignment_id, .. } => {
                self.editor_root(assignment_id).map(|root| root.path)
            }
            crate::session::worker::EffectTarget::Workspace => None,
        }
    }

    async fn run_in_process(
        &self,
        intent: EffectIntent,
        mut cancellation: Cancellation,
    ) -> ToolObservation {
        let deadline = tokio::time::Instant::now()
            + Duration::from_millis(u64::from(intent.limits.runtime_ms));
        if cancellation.is_cancelled() {
            return cancelled_read(intent);
        }
        // The owned permit moves into the blocking task, so a timed-out read keeps its slot.
        let Some(Ok(permit)) = self
            .reads
            .as_ref()
            .map(|reads| std::sync::Arc::clone(&reads.slots).try_acquire_owned())
        else {
            return unattested_limit(
                intent,
                LimitCause::ReadCapacity,
                "In-process read capacity is full; nothing was read",
            );
        };
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return cancelled_read(intent),
            ready = tokio::time::timeout_at(deadline, crate::fault::in_process_read(&intent)) => {
                if ready.is_err() {
                    return unattested_limit(intent, LimitCause::Timeout, "Tool runtime expired");
                }
            },
        }
        let (runtime, read_intent, walk) = (self.clone(), intent.clone(), cancellation.clone());
        let read = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let root = runtime.pinned_root(&read_intent)?;
            if let ToolCall::Skill { name, .. } = &read_intent.call {
                let skill = runtime
                    .config
                    .skills
                    .iter()
                    .find(|skill| &skill.name == name)
                    .ok_or(ToolError::Configuration)?;
                let skill_root = runtime
                    .reads
                    .as_ref()
                    .and_then(|reads| reads.skills.get(name))
                    .and_then(Option::as_ref)
                    .ok_or(ToolError::Path)?;
                return Ok(skills::read(
                    skill_root,
                    skill,
                    &read_intent.call,
                    read_intent.limits.result_bytes as usize,
                ));
            }
            if let ToolCall::Read {
                path,
                offset,
                limit,
            } = &read_intent.call
                && let Some(reference) = spill::read_reference(path)
            {
                let file = runtime
                    .spill_access
                    .as_ref()
                    .ok_or(ToolError::SpillUnavailable)
                    .and_then(|access| {
                        access
                            .read(reference)
                            .map_err(|_| ToolError::SpillUnavailable)
                    });
                return Ok(file.and_then(|file| {
                    fs::read_page(
                        file,
                        *offset,
                        *limit,
                        read_intent.limits.result_bytes as usize,
                    )
                }));
            }
            let hidden = runtime.hidden_paths(&read_intent.call);
            let deadline = std::time::Instant::now()
                + Duration::from_millis(u64::from(read_intent.limits.runtime_ms));
            if discovery::possible(&read_intent.call)
                && matches!(discovery::exact_search(&root, &read_intent.call), Ok(false))
            {
                return Ok(discovery::inspect(discovery::Request {
                    root: &root,
                    call: &read_intent.call,
                    config: &runtime.config,
                    hidden: &|path| hidden.covers(path),
                    result_bytes: read_intent.limits.result_bytes as usize,
                    deadline,
                    output: discovery::Output::Tool,
                    cancellation: Some(&walk),
                }));
            }
            Ok(fs::inspect(
                &root,
                &read_intent.call,
                &runtime.config,
                &|path| hidden.covers(path),
                read_intent.limits.result_bytes as usize,
                deadline,
            ))
        });
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => return cancelled_read(intent),
            joined = tokio::time::timeout_at(deadline, read) => match joined {
                Ok(Ok(Ok(result))) => result,
                Ok(Ok(Err(error))) => return unstarted_observation(intent, error),
                Ok(Err(_)) => Err(ToolError::Operation),
                Err(_) => return unattested_limit(intent, LimitCause::Timeout, "Tool runtime expired"),
            },
        };
        self.in_process_observation(intent, result)
    }

    fn in_process_observation(
        &self,
        intent: EffectIntent,
        result: Result<String, ToolError>,
    ) -> ToolObservation {
        let (disposition, output, enforcement_cause, limit_cause) =
            dispatch::outcome(&intent, result);
        let receipt =
            if fs::in_process_mutation(&intent) && disposition == ToolDisposition::Succeeded {
                match fs::file_receipt(&intent, &output) {
                    Ok(receipt) => Some(receipt),
                    Err(error) => {
                        return uncertain_observation(intent, error, CacheUpdate::NotApplicable);
                    }
                }
            } else {
                None
            };
        let mut observation = ToolObservation {
            spill: None,
            attestation: Some(Attestation::InProcess {
                policy_digest: intent.policy_digest,
                workspace_device: intent.workspace_device,
                workspace_inode: intent.workspace_inode,
                receipt,
            }),
            intent,
            disposition,
            output,
            enforcement_cause,
            limit_cause,
            approval_cause: None,
            guard_stage: None,
            cache_update: CacheUpdate::NotApplicable,
        };
        if !observation.valid() {
            observation.disposition = ToolDisposition::Limit;
            observation.limit_cause = Some(LimitCause::Resource);
            observation.enforcement_cause = None;
            observation.output = "Tool result exceeded its reserved serialized capacity".into();
        }
        observation
    }
}

#[derive(Clone)]
struct InProcessReads {
    skills: std::collections::BTreeMap<String, Option<std::sync::Arc<cap_std::fs::Dir>>>,
    root: std::sync::Arc<cap_std::fs::Dir>,
    slots: std::sync::Arc<tokio::sync::Semaphore>,
    // Shared by the Run's clones; each live editor works only in its own workspace.
    editors: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<AgentRunId, EditorRoot>>>,
}

#[derive(Clone)]
struct EditorRoot {
    path: PathBuf,
    dir: std::sync::Arc<cap_std::fs::Dir>,
    identity: WorkspaceIdentity,
}

const MAX_IN_PROCESS_READS: usize = 16;
static IN_PROCESS_READS: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| {
        std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_IN_PROCESS_READS))
    });

fn unattested_limit(intent: EffectIntent, cause: LimitCause, output: &str) -> ToolObservation {
    ToolObservation {
        spill: None,
        enforcement_cause: None,
        limit_cause: Some(cause),
        intent,
        disposition: ToolDisposition::Limit,
        output: output.into(),
        attestation: None,
        approval_cause: None,
        guard_stage: None,
        cache_update: CacheUpdate::NotApplicable,
    }
}

fn cancelled_read(intent: EffectIntent) -> ToolObservation {
    ToolObservation {
        spill: None,
        enforcement_cause: None,
        limit_cause: None,
        intent,
        disposition: ToolDisposition::Cancelled,
        output: "Read cancelled; nothing in the Workspace changed".into(),
        attestation: None,
        approval_cause: None,
        guard_stage: None,
        cache_update: CacheUpdate::NotApplicable,
    }
}

pub(super) fn guard_error_cache_update(call: &ToolCall, cache_selected: bool) -> CacheUpdate {
    if cache_selected && matches!(call, ToolCall::Command { .. }) {
        CacheUpdate::NotAttempted
    } else {
        CacheUpdate::NotApplicable
    }
}

#[cfg(all(any(test, feature = "test-support"), target_os = "linux"))]
pub(crate) fn selection_failure_observation(intent: EffectIntent) -> ToolObservation {
    guard::selection_failure(intent)
}

pub(crate) fn failure_stage(error: &ToolError) -> FailureStage {
    match error {
        ToolError::GuardBootstrap(failure) => failure.failure_stage(),
        ToolError::GuardRejected(stage) => stage.failure_stage(),
        ToolError::ProjectSelection(cause) => cause.stage(),
        ToolError::Configuration
        | ToolError::HostPrefixConflict { .. }
        | ToolError::InactiveHostArchiveUnconfirmed
        | ToolError::InactiveHostArchiveConflict
        | ToolError::CommandEnvironmentUnconfirmed => FailureStage::Admission,
        ToolError::ChangedInput | ToolError::Path | ToolError::FilesystemDenied => {
            FailureStage::PinnedInput
        }
        ToolError::ProtectionUnavailable => FailureStage::NativeProtection,
        ToolError::Limit | ToolError::QueueFull => FailureStage::ResourceLimit,
        ToolError::Cancelled => FailureStage::Cancelled,
        _ => FailureStage::Execution,
    }
}

pub(crate) fn failure_check(error: &ToolError) -> Option<guard::GuardCheck> {
    match error {
        ToolError::GuardBootstrap(failure) => failure.check(),
        _ => None,
    }
}

pub(crate) fn failure_detail(error: &ToolError) -> Option<&str> {
    match error {
        ToolError::GuardBootstrap(failure) => failure.detail(),
        _ => None,
    }
}

fn record_failure(site: &'static str, intent: &EffectIntent, error: &ToolError) {
    crate::diagnostics::tool_failure(site, error);
    crate::fault::record_tool_failure(
        intent.id,
        site,
        failure_stage(error).label(),
        failure_check(error).map(guard::GuardCheck::label),
        failure_detail(error),
    );
}

fn unstarted_observation(intent: EffectIntent, error: ToolError) -> ToolObservation {
    if matches!(error, ToolError::Cancelled) {
        let mut observation = cancelled_read(intent);
        observation.output = "File operation cancelled; nothing in the Workspace changed".into();
        return observation;
    }
    if matches!(error, ToolError::Timeout(_)) {
        return unattested_limit(intent, LimitCause::Timeout, "Tool runtime expired");
    }
    if let ToolError::DiskSpace(message) = &error {
        return unattested_limit(intent, LimitCause::Resource, message);
    }
    let resource_limit =
        matches!(&error, ToolError::Limit) && matches!(&intent.call, ToolCall::Command { .. });
    let enforcement_cause =
        matches!(&error, ToolError::GuardBootstrap(failure) if failure.sandbox_setup())
            .then_some(EnforcementCause::GuardBootstrap);
    let site = match intent.call.facts().site {
        Site::InProcess if intent.call.facts().mutates => "File",
        Site::InProcess => "Read",
        Site::Guard => "Guard",
        Site::Unguarded => "Unguarded",
        Site::Host => "host",
        Site::Git => "Git",
    };
    record_failure(site, &intent, &error);
    let mut output = if resource_limit {
        format!("{site} resource limit reached before dispatch; no operation was dispatched.")
    } else {
        match &error {
            ToolError::QueueFull => format!("{error}. No operation was dispatched."),
            ToolError::ProjectSelection(cause) => {
                format!("{site} {cause} before dispatch; no operation was dispatched.")
            }
            _ => format!(
                "{site} {} unavailable before dispatch; no operation was dispatched.",
                failure_stage(&error).label()
            ),
        }
    };
    if intent.call.facts().site == Site::Guard
        && !resource_limit
        && !matches!(error, ToolError::QueueFull)
    {
        let cause = failure_check(&error).map_or_else(
            || match &error {
                ToolError::Operation => "Guard startup failed; exact cause was not reported",
                _ => failure_stage(&error).label(),
            },
            guard::GuardCheck::label,
        );
        let next = match &error {
            ToolError::GuardBootstrap(failure) if failure.namespace_remedy().is_some() => {
                failure.namespace_remedy().unwrap_or_default()
            }
            ToolError::GuardRejected(guard::GuardStage::NamespaceBootstrap) => {
                "Enable unprivileged user namespaces and Bubblewrap for Arany, then resend your message."
            }
            ToolError::GuardRejected(guard::GuardStage::NativeManager) => {
                "Restore the user service manager and its resource controls, then resend your message."
            }
            ToolError::ChangedInput | ToolError::Path | ToolError::FilesystemDenied => {
                "Check the admitted files and folder permissions, then resend your message to pin them again."
            }
            ToolError::ProjectSelection(_) => {
                "Resend your message; the Guard executable is not involved."
            }
            _ => {
                "Check the Guard executable and native sandbox prerequisites, then resend your message."
            }
        };
        let diagnostic = if cfg!(debug_assertions) {
            " For details, inspect development.log in the Session State directory."
        } else {
            ""
        };
        let detail = format!(" Cause: {cause}. {next}{diagnostic}");
        if output.len() + detail.len() <= intent.limits.result_bytes as usize {
            output.push_str(&detail);
        }
    }
    let guard_stage = (intent.call.facts().site == Site::Guard).then(|| GuardStageFact {
        stage: failure_stage(&error),
        check: failure_check(&error),
    });
    let limit_cause = if resource_limit {
        Some(LimitCause::Resource)
    } else if matches!(error, ToolError::ProjectSelection(SelectionCause::TimedOut)) {
        Some(LimitCause::Timeout)
    } else {
        None
    };
    ToolObservation {
        spill: None,
        enforcement_cause,
        limit_cause,
        guard_stage,
        output,
        intent,
        disposition: if limit_cause.is_some() {
            ToolDisposition::Limit
        } else {
            ToolDisposition::Failed
        },
        attestation: None,
        approval_cause: None,
        cache_update: crate::tools::CacheUpdate::NotApplicable,
    }
}

pub(super) fn uncertain_observation(
    intent: EffectIntent,
    error: ToolError,
    cache_update: crate::tools::CacheUpdate,
) -> ToolObservation {
    record_failure("Guard", &intent, &error);
    let stage = failure_stage(&error).label();
    ToolObservation {
        spill: None,
        enforcement_cause: None,
        limit_cause: None,
        intent,
        disposition: ToolDisposition::Uncertain,
        output: format!(
            "Guard {stage} unconfirmed; effects may have occurred. Do not retry automatically."
        ),
        attestation: None,
        approval_cause: None,
        guard_stage: None,
        cache_update,
    }
}

#[cfg(any(test, feature = "test-support"))]
std::thread_local! {
    static TEST_CLOCK_MS: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn set_test_clock_ms(value: Option<u64>) -> Option<u64> {
    TEST_CLOCK_MS.with(|clock| clock.replace(value))
}

pub fn now_ms() -> u64 {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(now) = TEST_CLOCK_MS.with(std::cell::Cell::get) {
        return now;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub use unguarded::tool_host_main;

pub fn tool_guard_main() -> std::process::ExitCode {
    guard::helper_main()
}

#[cfg(target_os = "macos")]
pub fn tool_guard_unit_main() -> std::process::ExitCode {
    guard::unit_main()
}

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
pub use discovery::{
    FileSuggestionSource, SUGGESTION_LIFETIME, SUGGESTION_QUERY_TIME, SuggestionIndex,
};
mod executable;
mod fs;
mod git;
mod glob;
mod grant;
mod guard;
pub(crate) use guard::transfer::{recover_private, transfer_time};
mod mcp;
pub(crate) use mcp::FRAME_BYTES;
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

use crate::session::{AgentRunId, RunId, WorkspaceIdentity};
use crate::store::StateRoot;
pub use approval::{
    ApprovalInbox, ApprovalMode, ApprovalReply, ToolApproval, ToolApprovals, UnguardedPrompt,
};
pub(crate) use approval::{ApprovalWaitReply, ApprovalWithdrawal};
pub use cancel::Cancellation;
use config::Config;
pub(crate) use git::host::sweep_stale as sweep_host_scratch;
pub(crate) use git::recover_locks as report_git_locks;
pub use skills::{ProjectSkills, project_skills};
pub(crate) use skills::{parse_json, parse_json_bounded};
pub(crate) use workspace::WORKSPACE_PERMISSIONS;
pub use workspace::{
    RuntimeProfileOption, TrustRecordError, TrustReset, WorkspacePermissions, mention_paths,
};
/// Whether this OS implements admitted native Tool protection.
pub const fn native_protection_supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
}

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
    ApprovalCause, Attestation, CacheUpdate, CommandChangeset, EffectIntent, EnforcementCause,
    FileChange, FileReceipt, FileVersion, GuardReceipt, HostIntent, LimitCause, LiteralEdit,
    MAX_SEARCH_ROWS, MAX_SKILLS, NetworkGrant, OutputTotal, ToolCall, ToolContext, ToolDisposition,
    ToolLimits, ToolObservation, ToolPolicyReceipt, ToolRequest, ToolResourceClass,
    ToolResourceProfile, ToolResourceSettings, ToolStep, UnguardedGrant, UnguardedReceipt,
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
    #[error("No admitted credential-helper search directory")]
    HostHelperSearch,
    #[error("Credential includes cannot be safely omitted")]
    CredentialIncludes,
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
    #[error("Tool file precondition changed")]
    Conflict,
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
}

impl ToolError {
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
    git_writes_available: bool,
    host_authentication: std::sync::Arc<Option<git::host::Authentication>>,
    worker: Option<std::sync::Arc<guard::transfer::Action>>,
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
    context_limit_bytes: usize,
    queue_progress: Option<tokio::sync::watch::Sender<Option<uuid::Uuid>>>,
    active_action_gate: Option<tokio::sync::mpsc::Sender<ActiveActionRequest>>,
    hook_stdin: Option<std::sync::Arc<str>>,
    checkpoint_gate: Option<tokio::sync::mpsc::Sender<CheckpointRequest>>,
    cache_state: Option<std::sync::Arc<StateRoot>>,
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
    workspace_identity: WorkspaceIdentity,
    guard_available: bool,
    enforcement_digest: [u8; 32],
    process_enforcement_digest: [u8; 32],
    host_authentication: std::sync::Arc<Option<git::host::Authentication>>,
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
    for program in config
        .commands
        .iter()
        .chain(config.mcp.iter().map(|server| &server.program))
    {
        program.verify()?;
    }
    if let Some(git) = &config.git {
        git::verify_host_executable(git)?;
    }
    for path in config.resource_paths() {
        fs::reject_overlap(Path::new(path), state.path())?;
        fs::reject_overlap(Path::new(path), account)?;
    }
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
            | ToolCall::Move { .. } => paths
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
        self.context_limit_bytes = limit;
        Ok(())
    }

    pub(crate) fn context_limit_bytes(&self) -> usize {
        self.context_limit_bytes
    }

    pub(crate) fn result_capacity(&self, overhead: usize, reserved: usize) -> Option<usize> {
        let capacity = self
            .context_limit_bytes
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
            _ if call.is_host() => {
                self.config.allows(call)
                    && self
                        .host_authentication
                        .as_ref()
                        .as_ref()
                        .is_some_and(|auth| auth.allows(call))
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

    pub(crate) fn effective_cache_bytes(&self, requested: u64) -> Result<(u64, u64), ToolError> {
        if requested == 0 {
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
        match state.effective_build_cache_bytes(&identity, requested) {
            Ok(limits) => Ok(limits),
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
        let mut config = match Config::load(state) {
            Ok(mut config) => {
                config.discover_skills(workspace)?;
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
        let guard_available = guard::check_available().is_ok();
        let enforcement_digest = guard::enforcement_digest(&[])?;
        let root = fs::open_directory(workspace)?;
        let account = admission_paths(workspace, state, &config, &root)?;
        let workspace_identity = fs::workspace_identity(&root)?;
        let permissions = permissions
            .map(|mut snapshot| {
                let mut scoped = (*snapshot.config).clone();
                scoped.scope_to(workspace, workspace_identity);
                if let Some(index) = snapshot
                    .config
                    .host
                    .iter()
                    .position(|rule| scoped.host.contains(rule) && !rule.admitted_by(&config))
                {
                    return Err(ToolError::HostPrefixConflict { rule: index + 1 });
                }
                snapshot.config = std::sync::Arc::new(scoped);
                Ok(snapshot)
            })
            .transpose()?;
        let host_authentication = std::sync::Arc::new(git::host::Authentication::admit(&config));
        config.discover_skills(workspace)?;
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
                workspace_identity,
                guard_available,
                enforcement_digest,
                process_enforcement_digest,
                host_authentication,
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
            workspace_identity,
            guard_available,
            enforcement_digest,
            process_enforcement_digest,
            host_authentication,
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
            git_writes_available: guard::git_writes_available(),
            host_authentication,
            worker: None,
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
            workspace: workspace.to_owned(),
            reads: Some(InProcessReads {
                skills,
                root: std::sync::Arc::new(root),
                slots: std::sync::Arc::clone(&IN_PROCESS_READS),
            }),
            workspace_identity,
            guard_executable,
            enforcement_digest,
            process_enforcement_digest,
            context_limit_bytes: types::MAX_TOOL_CONTEXT_BYTES,
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
        receipt.max_context_bytes = self.context_limit_bytes as u32;
        receipt.permissions_digest = self.permissions.as_ref().map(|snapshot| snapshot.digest);
        receipt.unguarded_rules_digest = None;
        receipt.host_prefixes = self
            .permissions()
            .map_or_else(Vec::new, |permissions| permissions.host.clone());
        receipt
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
        let catalog = serde_json::json!({
            "host_clock": {"unix_ms": now_ms(), "timezone":"UTC"},
            "workspace_paths": self.config.workspace_paths,
            "write": self.config.write && self.guard_available,
            "file_tools": if self.guard_available { catalog::FILE_TOOLS } else { "" },
            "commands": self.command_rows(None),
            "command_contract": catalog::COMMAND_CONTRACT,
            "host_commands": true,
            "skills": self.skill_rows(),
            "mcp_servers": self.config.mcp.iter().filter(|_| self.guard_available).map(|server| serde_json::json!({"name":server.name,"tools":server.tools,"protocol":catalog::MCP_PROTOCOL})).collect::<Vec<_>>(),
            "git": if self.guard_available && self.config.git_available() {
                let write = self.config.write && self.git_writes_available;
                let operations: Vec<_> = catalog::git_operations(write, true)
                    .into_iter()
                    .filter(|name| match *name {
                        "git_push" | "gh_pr_create" => self.config.write && self.host_authentication.as_ref().as_ref()
                            .is_some_and(|auth| auth.offers(name)),
                        "gh_pr_list" | "gh_pr_view" => self.host_authentication.as_ref().as_ref()
                            .is_some_and(|auth| auth.offers(name)),
                        _ => true,
                    })
                    .filter(|name| !matches!(*name, "gh_pr_create" | "gh_pr_list" | "gh_pr_view") || self.config.pinned_gh().is_some())
                    .collect();
                serde_json::json!({"write":write,"host":true,"operations":operations,"scope":catalog::GIT_SCOPE})
            } else {
                serde_json::json!("unavailable")
            },
        });
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
                    tools: vec![
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
                    ],
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
                data_bytes: WORKER_DATA_BYTES as u32,
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
        catalog["workspace_paths"] = serde_json::json!(grant.read_paths);
        catalog["write"] = serde_json::json!(self.guard_available);
        catalog["skills"] = serde_json::json!([]);
        catalog["mcp_servers"] = serde_json::json!([]);
        catalog["file_tools"] = serde_json::json!(if self.guard_available {
            catalog::FILE_TOOLS
        } else {
            ""
        });
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
        let catalog = serde_json::json!({
            "host_clock": {"unix_ms": now_ms(), "timezone": "UTC"},
            "workspace_paths": self.config.workspace_paths,
            "write": false,
            "file_tools": if self.guard_available { catalog::RESEARCH_FILE_TOOLS } else { "" },
            "commands": [],
            "skills": self.skill_rows(),
            "mcp_servers": self.config.mcp.iter().filter(|_| self.guard_available).map(|server| serde_json::json!({"name":server.name,"tools":server.tools,"protocol":catalog::RESEARCH_MCP_PROTOCOL})).collect::<Vec<_>>(),
            "tool_calls_remaining": tool_calls_remaining,
        });
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
        } else {
            self.enforcement_digest
        };
        let limits = if call.is_host() {
            ToolLimits {
                network: NetworkGrant::Host,
                runtime_ms: 600_000,
                cache_requested_bytes: 0,
                cache_bytes: 0,
                cache_candidate_bytes: 0,
                ..ToolLimits::default()
            }
        } else {
            ToolLimits::default()
        };
        EffectIntent {
            host: git::host::pin(&self.workspace, &call).ok(),
            host_command: match &call {
                ToolCall::CommandUnguarded { program, .. } => {
                    let path = std::env::var("PATH").unwrap_or_default();
                    let requested = self.command_executable(program).unwrap_or(program);
                    unguarded::pin(requested, &self.workspace, &path)
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
            target: crate::session::worker::EffectTarget::Workspace,
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
        if !unguarded::wrapper(program) && !unguarded::wrapper(&pin.executable) {
            return true;
        }
        let intent = intent.clone();
        let commands = self.config.commands.clone();
        tokio::task::spawn_blocking(move || {
            unguarded::wrapper_interpreters_admitted(&intent, &commands)
        })
        .await
        .unwrap_or(false)
    }

    pub(crate) fn worker_reservation_overhead(&self, call: &ToolCall) -> Duration {
        if discovery::possible(call)
            || call.facts().family == catalog::Family::File && call.facts().mutates
        {
            guard::RESERVATION_OVERHEAD
        } else {
            self.reservation_overhead(call)
        }
    }

    pub(crate) fn intent_reservation_overhead(&self, intent: &EffectIntent) -> Duration {
        match intent.target {
            crate::worker::EffectTarget::WorkerView { .. } => {
                self.worker_reservation_overhead(&intent.call)
            }
            crate::worker::EffectTarget::Workspace => self.reservation_overhead(&intent.call),
        }
    }

    pub(crate) fn reservation_overhead(&self, call: &ToolCall) -> Duration {
        if discovery::possible(call) {
            let exact = self
                .reads
                .as_ref()
                .is_some_and(|reads| discovery::exact_search(&reads.root, call).unwrap_or(false));
            if !exact {
                return guard::RESERVATION_OVERHEAD;
            }
        }
        match call.facts().site {
            Site::InProcess | Site::Unguarded | Site::Host => Duration::ZERO,
            Site::Guard => {
                guard::RESERVATION_OVERHEAD
                    + if self.worker.is_none() && matches!(call, ToolCall::Command { .. }) {
                        selection::PHASE_TIME
                    } else {
                        Duration::ZERO
                    }
            }
        }
    }

    pub(crate) async fn execute_concurrent_read(
        &self,
        intent: EffectIntent,
        cancellation: Cancellation,
    ) -> ToolObservation {
        Box::pin(self.execute_inner(intent, cancellation, ReadDispatch::InProcess)).await
    }

    pub(crate) async fn execute(
        &self,
        intent: EffectIntent,
        cancellation: Cancellation,
    ) -> ToolObservation {
        Box::pin(self.execute_inner(intent, cancellation, ReadDispatch::Automatic)).await
    }

    async fn execute_inner(
        &self,
        intent: EffectIntent,
        cancellation: Cancellation,
        reads: ReadDispatch,
    ) -> ToolObservation {
        if matches!(intent.call.facts().site, Site::Guard | Site::InProcess)
            && !self.guard_available
        {
            return unstarted_observation(intent, ToolError::ProtectionUnavailable);
        }
        if matches!(
            intent.target,
            crate::worker::EffectTarget::WorkerView { .. }
        ) && matches!(intent.call, ToolCall::Command { .. })
        {
            return ToolObservation {
                spill: None,
                enforcement_cause: None,
                limit_cause: None,
                intent,
                disposition: ToolDisposition::Denied,
                output: crate::tools::PRIVATE_IMAGE_REQUIRED.into(),
                attestation: None,
                approval_cause: None,
                cache_update: CacheUpdate::NotApplicable,
            };
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
        if intent.target != crate::session::worker::EffectTarget::Workspace
            || !self.config.allows(&intent.call)
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
                cache_update: crate::tools::CacheUpdate::NotApplicable,
            };
        }
        match intent.call.facts().site {
            Site::InProcess if intent.call.facts().mutates => {
                self.run_file(intent, cancellation).await
            }
            Site::InProcess => match reads {
                ReadDispatch::Automatic => self.run_read(intent, cancellation).await,
                ReadDispatch::InProcess => self.run_in_process(intent, cancellation).await,
            },
            Site::Host => {
                if let Err(error) = self.pinned_root(&intent) {
                    return unstarted_observation(intent, error);
                }
                git::host::run(
                    &self.config,
                    self.host_authentication.as_ref().as_ref(),
                    self.cache_state.as_deref(),
                    &self.workspace,
                    intent,
                    cancellation,
                )
                .await
            }
            Site::Unguarded => self.run_unguarded(intent, cancellation).await,
            Site::Guard => match guard::execute(self, &intent, cancellation).await {
                Ok(observation) => observation,
                Err(error) => {
                    let cache_update =
                        guard_error_cache_update(&intent.call, self.cache_state.is_some());
                    uncertain_observation(intent, error, cache_update)
                }
            },
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
        if !tokio::task::spawn_blocking(move || checked_pin.verify(&workspace))
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
        observation
    }

    fn pinned_root(
        &self,
        intent: &EffectIntent,
    ) -> Result<std::sync::Arc<cap_std::fs::Dir>, ToolError> {
        let root = self
            .reads
            .as_ref()
            .map(|reads| std::sync::Arc::clone(&reads.root))
            .ok_or(ToolError::ProtectionUnavailable)?;
        if intent.target != crate::session::worker::EffectTarget::Workspace
            || !intent.valid()
            || intent.policy_digest != self.config.digest()
            || intent.enforcement_digest
                != if fs::in_process_mutation(intent) {
                    FileReceipt::enforcement_digest()
                } else if intent.call.is_process() {
                    self.build_jobs_digest(intent)?
                } else {
                    self.enforcement_digest
                }
        {
            return Err(ToolError::Configuration);
        }
        if fs::identity(&root)? != (intent.workspace_device, intent.workspace_inode) {
            return Err(ToolError::ChangedInput);
        }
        fs::validate_workspace(self)?;
        Ok(root)
    }

    async fn run_read(&self, intent: EffectIntent, cancellation: Cancellation) -> ToolObservation {
        if !discovery::possible(&intent.call) {
            return self.run_in_process(intent, cancellation).await;
        }
        let root = match self.pinned_root(&intent) {
            Ok(root) => root,
            Err(error) => return unstarted_observation(intent, error),
        };
        match discovery::exact_search(&root, &intent.call) {
            Ok(false) => match guard::execute(self, &intent, cancellation).await {
                Ok(observation) => observation,
                Err(error) => uncertain_observation(intent, error, CacheUpdate::NotApplicable),
            },
            Ok(true) | Err(_) => self.run_in_process(intent, cancellation).await,
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
        let (runtime, read_intent) = (self.clone(), intent.clone());
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
            Ok(fs::inspect(
                &root,
                &read_intent.call,
                &runtime.config,
                &|path| hidden.covers(path),
                read_intent.limits.result_bytes as usize,
                std::time::Instant::now()
                    + Duration::from_millis(u64::from(read_intent.limits.runtime_ms)),
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

enum ReadDispatch {
    Automatic,
    InProcess,
}

#[derive(Clone)]
struct InProcessReads {
    skills: std::collections::BTreeMap<String, Option<std::sync::Arc<cap_std::fs::Dir>>>,
    root: std::sync::Arc<cap_std::fs::Dir>,
    slots: std::sync::Arc<tokio::sync::Semaphore>,
}

pub(crate) const PRIVATE_IMAGE_REQUIRED: &str =
    "Worker commands require an admitted private image; no operation was dispatched";
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

pub(crate) fn failure_stage(error: &ToolError) -> &'static str {
    match error {
        ToolError::GuardBootstrap(failure) => failure.failure_stage(),
        ToolError::GuardRejected(stage) => stage.failure_stage(),
        ToolError::Configuration
        | ToolError::HostPrefixConflict { .. }
        | ToolError::InactiveHostArchiveUnconfirmed
        | ToolError::InactiveHostArchiveConflict => "admission",
        ToolError::ChangedInput | ToolError::Path | ToolError::FilesystemDenied => "pinned input",
        ToolError::ProtectionUnavailable => "native protection",
        ToolError::Limit | ToolError::QueueFull => "resource limit",
        _ => "execution",
    }
}

pub(crate) fn failure_check(error: &ToolError) -> Option<&'static str> {
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
        failure_stage(error),
        failure_check(error),
        failure_detail(error),
    );
}

fn unstarted_observation(intent: EffectIntent, error: ToolError) -> ToolObservation {
    if matches!(error, ToolError::Cancelled) {
        let mut observation = cancelled_read(intent);
        observation.output = "File operation cancelled; nothing in the Workspace changed".into();
        return observation;
    }
    if matches!(error, ToolError::QueueFull) && discovery::possible(&intent.call) {
        return unattested_limit(
            intent,
            LimitCause::ReadCapacity,
            "Discovery capacity is busy; nothing was read",
        );
    }
    let resource_limit = matches!(&error, ToolError::Limit)
        && matches!(&intent.call, ToolCall::Command { .. })
        && intent.target == crate::session::worker::EffectTarget::Workspace;
    let enforcement_cause =
        matches!(&error, ToolError::GuardBootstrap(failure) if failure.sandbox_setup())
            .then_some(EnforcementCause::GuardBootstrap);
    let site = match intent.call.facts().site {
        Site::InProcess if intent.call.facts().mutates => "File",
        Site::InProcess => "Read",
        Site::Guard => "Guard",
        Site::Unguarded => "Unguarded",
        Site::Host => "host",
    };
    record_failure(site, &intent, &error);
    let mut output = if resource_limit {
        format!("{site} resource limit reached before dispatch; no operation was dispatched.")
    } else {
        match &error {
            ToolError::QueueFull => format!("{error}. No operation was dispatched."),
            _ => format!(
                "{site} {} unavailable before dispatch; no operation was dispatched.",
                failure_stage(&error)
            ),
        }
    };
    if intent.call.facts().site == Site::Guard
        && !resource_limit
        && !matches!(error, ToolError::QueueFull)
    {
        let cause = failure_check(&error).unwrap_or(match &error {
            ToolError::Operation => "Guard startup failed; exact cause was not reported",
            _ => failure_stage(&error),
        });
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
    ToolObservation {
        spill: None,
        enforcement_cause,
        limit_cause: resource_limit.then_some(LimitCause::Resource),
        output,
        intent,
        disposition: if resource_limit {
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
    let stage = failure_stage(&error);
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

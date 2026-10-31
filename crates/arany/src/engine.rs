#![cfg_attr(not(test), warn(clippy::too_many_lines))]

use crate::journal::{Journal, Subscription};
use crate::provider::{
    AgentPhase, ImageAttachment, PromptRole, Provider, ProviderRequest, RunInput, WorkspaceGuidance,
};
use crate::session::input::{InputError, WorkspaceInputs};
use crate::session::{
    AgentRole, AgentRunId, CollaborationPolicy, CompiledContext, Event, GoalBudget, GoalId,
    GoalPhase, GoalPlanContract, GoalStatus, GoalUsage, InstructionSource,
    MAX_CHILD_STEERING_LABEL_BYTES, MAX_STEERING_CONTEXT_BYTES, MAX_STEERING_LABEL_BYTES,
    MAX_STEERING_NOTE_BYTES, MAX_USER_MESSAGE_BYTES, RunConfig, RunId, RunView, SessionId,
    WorkspaceIdentity,
};
use crate::store::{AdmissionKind, SessionRunLock, StateRoot, Store, StoreError, WorkspaceRunLock};
use crate::telemetry::Telemetry;
use std::collections::HashMap;
use std::future::{Future, IntoFuture};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

mod command_environment;
mod command_tools;
mod compaction;
mod goal;
mod hints;
mod ledger;
mod lifecycle;
mod report;
mod resources;
pub use report::budget_refusal_summary;
mod rewind;
pub use rewind::{
    ConversationRewind, ConversationRewindOutcome, RewindBothResult, RewindFile, RewindIssue,
    RewindPreview, RewindRequest,
};
mod routing;
mod run_loop;
mod steering;
mod submission;
pub use crate::tools::Cancellation as RunCancellation;
pub use crate::tools::ToolGrant;
use hints::HintPublisher;
pub use hints::{DisplayUpdate, RunHints, RunPreview};
pub use ledger::Action;
use ledger::GoalLedger;
pub use lifecycle::{
    continue_session, create_session, fork_session, list_sessions, list_sessions_page,
    rename_session, resume_session, set_session_defaults, start_provider_setup_edit,
};
use lifecycle::{ensure_session_workspace, fork_with_store, lock_session};
use report::goal_report;
use routing::CallScope;
#[cfg(any(test, feature = "test-support"))]
pub(crate) use run_loop::MAX_DELEGATION_DEADLINE;
use run_loop::{LoopControls, RunIdentity, RunLoop};
pub use steering::{ChildSteeringTarget, RunSteering, SteeringRefused};

/// Checks bounded Workspace input before an attached draft becomes a saved Session and
/// returns the includes that will be attached only in part or by reference.
/// Run admission reads and validates the inputs again before disclosure.
pub fn validate_workspace_inputs(
    state: &StateRoot,
    workspace: &std::path::Path,
    launch_directory: Option<&std::path::Path>,
    include_paths: &[PathBuf],
) -> Result<Vec<crate::session::PartialInclude>, EngineError> {
    Ok(
        WorkspaceInputs::load(workspace, state, launch_directory, include_paths)?
            .includes
            .iter()
            .filter_map(crate::session::input::InputSnapshot::partial)
            .collect(),
    )
}

// GoalStarted or GoalResumed, GoalRunLinked, MessageAccepted, RunStarted and AgentSpawned.
const RUN_START_EVENTS: usize = 5;

const STORE_ACTION_OVERHEAD: std::time::Duration =
    std::time::Duration::from_millis(crate::session::STORE_ACTION_OVERHEAD_MS);

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("{path:?}: {source}")]
    InputPath {
        path: String,
        #[source]
        source: Box<EngineError>,
    },
    #[error("rewind is blocked; inspect the preview paths and uncertain publications")]
    RewindBlocked,
    #[error("rewind preview changed; preview again")]
    RewindPreviewChanged,
    #[error("rewind exceeds file, byte or explicit-path limits; restore with your VCS")]
    RewindTooLarge,
    #[error("restore the matching --profile preset to resume this Goal, or start a new Goal")]
    RosterSelectionMismatch,

    #[error(
        "Tool-enabled primary and child fallback chains must keep the same adapter and account"
    )]
    IncompatibleProviderChain,

    #[error("invalid Run request")]
    InvalidRequest,
    #[error("Session not found")]
    MissingSession,
    #[error("no Session for this Workspace")]
    NoSessionForWorkspace,
    #[error("too many Sessions for discovery; use --resume ID")]
    TooManySessions,
    #[error("Session has an active operation")]
    SessionBusy,
    #[error("Workspace has an active writing Goal")]
    WorkspaceBusy,
    #[error("Goal is not available for explicit resume")]
    GoalUnavailable,
    #[error(
        "Goal admission was not acknowledged; inspect Session history before retrying or explicitly resuming"
    )]
    GoalAdmissionUncertain,
    #[error("Goal budget is exhausted")]
    GoalBudgetExhausted,
    #[error("Goal budget refusal is uncommitted: {details}; inspect Session history before retrying", details = report::budget_refusal_details(.refusal))]
    GoalBudgetUncommitted {
        refusal: Box<crate::session::BudgetRefusal>,
        #[source]
        source: Box<EngineError>,
    },
    #[error("no exhausted Goal is available to continue")]
    GoalNotContinuable,
    #[error("Goal include paths are unknown; start a new Goal with explicit inputs")]
    GoalIncludesUnavailable,
    #[error("objective plus includes too large")]
    GoalInputTooLarge,
    #[error("Engine has an active operation")]
    Busy,
    #[error("Guard executable unavailable")]
    GuardUnavailable,
    #[error("Session is not idle")]
    SessionNotIdle,
    #[error(
        "Provider setup outcome is incomplete or uncertain; inspect history and start a new Session"
    )]
    ProviderSetupBlocked,
    #[error("Session has no committed Run boundary to fork")]
    ForkNotAtRunBoundary,
    #[error("conversation rewind exceeds the eight-link or Event limit; start a new Session")]
    ConversationRewindLimit,
    #[error("Session belongs to a different Workspace")]
    WorkspaceMismatch,
    #[error("Workspace input path is invalid")]
    InvalidWorkspacePath,
    #[error("Workspace input must be a regular file")]
    InputNotRegular,
    #[error("Workspace input exceeds its byte limit")]
    InputTooLarge,
    #[error("input is not UTF-8")]
    InputNotUtf8,
    #[error("explicit include does not exist")]
    MissingInclude,
    #[error(
        "too many explicit includes; a message takes at most {} @ files or directories",
        crate::session::input::MAX_INCLUDES
    )]
    TooManyIncludes,
    #[error("state directory overlaps the Workspace")]
    StateOverlap,
    #[error("Workspace input is unavailable")]
    WorkspaceUnavailable,
    #[error("invalid Session history")]
    InvalidHistory,
    #[error("compiled Provider context exceeds its byte limit")]
    ContextTooLarge,
    #[error("image input is not admitted for this custom Provider")]
    ImagesNotAdmitted,
    #[error("Session has no new successful conversation to compact")]
    CompactionInputEmpty,
    #[error("a PreCompact hook blocked compaction")]
    CompactionBlockedByHook,
    #[error("compaction input exceeds its limit")]
    CompactionInputTooLarge,
    #[error("Session is not idle at a committed Run boundary")]
    CompactionNotIdle,
    #[error("Run cancellation did not drain within its deadline")]
    CancellationDrainTimeout,
    #[error("Run coordinator failed")]
    CoordinatorFailed,
    #[error("Run was cancelled before admission")]
    CancelledBeforeStart,
    #[error(transparent)]
    PromptOverride(#[from] crate::provider::PromptOverrideError),
    #[error("state storage failed")]
    Store(#[from] StoreError),
    #[error(transparent)]
    Tool(#[from] crate::tools::ToolError),
}

impl From<InputError> for EngineError {
    fn from(error: InputError) -> Self {
        match error {
            InputError::AtPath { path, source } => Self::InputPath {
                path,
                source: Box::new(Self::from(*source)),
            },
            InputError::InvalidPath => Self::InvalidWorkspacePath,
            InputError::NotRegular => Self::InputNotRegular,
            InputError::UnsafeLinkCount => Self::WorkspaceUnavailable,
            InputError::TooLarge => Self::InputTooLarge,
            InputError::InvalidUtf8 => Self::InputNotUtf8,
            InputError::MissingInclude => Self::MissingInclude,
            InputError::TooManyIncludes => Self::TooManyIncludes,
            InputError::State(error) => Self::Store(error),
            InputError::StateOverlap => Self::StateOverlap,
            InputError::Include(error) => Self::Tool(error),
            InputError::Io(_) => Self::WorkspaceUnavailable,
        }
    }
}

pub struct RunRequest {
    pub skill: Option<String>,
    pub session_id: Option<SessionId>,
    pub title: Option<String>,
    /// Empty only for an explicit Goal resume, which runs the Goal's pinned objective.
    pub objective: String,
    pub images: Vec<ImageAttachment>,
    pub workspace: PathBuf,
    pub launch_directory: Option<PathBuf>,
    pub include_paths: Vec<PathBuf>,
    pub policy: CollaborationPolicy,
    pub goal: GoalRequest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalRequest {
    pub id: Option<crate::session::GoalId>,
    pub phase: Option<crate::session::GoalPhase>,
    pub budget: Option<crate::session::GoalBudget>,
    pub resource_settings: crate::tools::ToolResourceSettings,
    /// Resolved against the model's window at each Run's admission.
    pub context_window: crate::session::ContextWindow,
    /// Ignored for autonomous Goals, which finish only at a Goal stop condition.
    pub finish_after_run: bool,
    /// Set only by `autonomous`; a resumed Goal keeps the flag it was started with.
    pub autonomous: bool,
    /// Set only by `continue_from`: the exhausted Goal whose objective, phase and mode seed a new Goal.
    pub continues: Option<crate::session::GoalId>,
}

impl GoalRequest {
    #[must_use]
    pub fn new(budget: crate::session::GoalBudget, phase: crate::session::GoalPhase) -> Self {
        Self {
            id: None,
            phase: Some(phase),
            budget: Some(budget),
            resource_settings: crate::tools::ToolResourceSettings {
                build_jobs: None,
                build_cpu_cores: None,
                ..Default::default()
            },
            context_window: crate::session::ContextWindow::default(),
            finish_after_run: true,
            autonomous: false,
            continues: None,
        }
    }

    /// A new Goal seeded from the most recent exhausted Goal with this fresh budget; empty `RunRequest.objective`.
    #[must_use]
    pub fn continue_from(
        source: crate::session::GoalId,
        budget: crate::session::GoalBudget,
        resource_settings: crate::tools::ToolResourceSettings,
    ) -> Self {
        Self {
            id: None,
            phase: None,
            budget: Some(budget),
            resource_settings,
            context_window: crate::session::ContextWindow::default(),
            finish_after_run: true,
            autonomous: false,
            continues: Some(source),
        }
    }

    /// A new Goal that the Engine keeps running, Run after Run, until it is satisfied or stops.
    #[must_use]
    pub fn autonomous(
        budget: crate::session::GoalBudget,
        phase: crate::session::GoalPhase,
    ) -> Self {
        Self {
            autonomous: true,
            ..Self::new(budget, phase)
        }
    }

    #[must_use]
    pub fn resume(id: crate::session::GoalId, finish_after_run: bool) -> Self {
        Self {
            id: Some(id),
            phase: None,
            budget: None,
            resource_settings: crate::tools::ToolResourceSettings::default(),
            context_window: crate::session::ContextWindow::default(),
            finish_after_run,
            autonomous: false,
            continues: None,
        }
    }
}

impl Default for GoalRequest {
    fn default() -> Self {
        Self::new(
            crate::session::GoalBudget::default(),
            crate::session::GoalPhase::Implementation,
        )
    }
}

#[derive(Debug, PartialEq)]
pub struct RunOutcome {
    pub session_id: SessionId,
    pub goal_id: crate::session::GoalId,
    pub run: RunView,
}

/// Process-wide settings fixed when the Engine opens.
#[derive(Clone, Default)]
pub struct EngineConfig {
    pub telemetry: Telemetry,
    /// Launched by every Tool grant; a grant without one is refused at submission.
    pub guard_executable: Option<PathBuf>,
    pub worker_concurrency: Option<u8>,
}

/// What the caller holds for one submission; every Run of its Goal loop uses them.
#[derive(Clone, Default)]
pub struct RunControls {
    pub tools: ToolGrant,
    pub approvals: Option<crate::tools::ToolApprovals>,
    /// A pause request is sticky for the steering handle's lifetime, so each submission needs its own.
    pub steering: Option<RunSteering>,
    pub cancellation: RunCancellation,
    pub hints: Option<RunHints>,
}

/// The last committed Run of a submission.
#[derive(Debug)]
pub struct RunResult {
    pub run: RunOutcome,
    /// Why the submission's Goal stopped at its latest Run, as `SessionView::run_stop` projects
    /// the reopened history. A Run that could not settle is returned without a terminal status.
    pub stopped: Option<crate::session::RunStop>,
    /// The refusal of the next autonomous Run before its MessageAccepted; it has no journaled fact.
    pub continuation_error: Option<EngineError>,
}

pub type RunFuture<'a> = Pin<Box<dyn Future<Output = Result<RunResult, EngineError>> + Send + 'a>>;

/// One admitted submission. Awaiting the handle, or `result`, drives its Runs; dropping it first
/// abandons the open Run as an abrupt stop. The controls are the caller's, shared with the Engine.
pub struct RunHandle<'a> {
    pub cancel: RunCancellation,
    pub steer: Option<RunSteering>,
    pub approvals: Option<crate::tools::ToolApprovals>,
    /// Events the Engine appends from submission on; see `Subscription` for its gaps.
    pub events: Subscription,
    pub result: RunFuture<'a>,
}

impl<'a> IntoFuture for RunHandle<'a> {
    type Output = Result<RunResult, EngineError>;
    type IntoFuture = RunFuture<'a>;

    fn into_future(self) -> Self::IntoFuture {
        self.result
    }
}

struct HostSettings {
    digest: Option<[u8; 32]>,
    changed: bool,
}

struct ToolAdmission {
    config: crate::tools::config::Config,
    permissions: Option<crate::tools::PermissionSnapshot>,
    unguarded_rules: Option<crate::tools::unguarded_rules::ExactCommandRules>,
    guard_executable: PathBuf,
}

struct AdmittedSubmission {
    providers: crate::provider::AdmittedProviders,
    grant: ToolGrant,
    tools: Option<ToolAdmission>,
    approvals: Option<crate::tools::ToolApprovals>,
    steering: Option<RunSteering>,
}

impl AdmittedSubmission {
    fn cancel_if_paused(&self, cancellation: &RunCancellation) {
        if self
            .steering
            .as_ref()
            .is_some_and(RunSteering::pause_requested)
        {
            cancellation.cancel();
        }
    }

    fn permissions(&self) -> Option<&crate::tools::WorkspacePermissions> {
        match &self.grant {
            ToolGrant::Trusted(permissions) => Some(permissions),
            ToolGrant::None | ToolGrant::Configured => None,
        }
    }
}

struct OperationSlot<'a>(&'a AtomicBool);

impl Drop for OperationSlot<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct AdmittedTurn {
    session_id: SessionId,
    workspace_identity: WorkspaceIdentity,
    _lineage_locks: Vec<SessionRunLock>,
    history: Option<crate::session::SessionView>,
    goal: GoalAdmission,
    objective: String,
    budget: GoalBudget,
    provisional_workspace_lock: Option<WorkspaceRunLock>,
}

struct RootInput<'a> {
    objective: String,
    budget: GoalBudget,
    assignment_profiles: Vec<String>,
    context_window_tokens: Option<u32>,
    tool_context: Option<crate::tools::ToolContext>,
    inputs: &'a mut WorkspaceInputs,
    images: Vec<crate::provider::ProviderImage>,
    history: Vec<crate::provider::HistoryTurn>,
    summary: Option<String>,
    prompts: crate::provider::EffectivePrompts,
}

struct PreparedTurn {
    tools: Option<crate::tools::ToolRuntime>,
    tool_context: Option<crate::tools::ToolContext>,
    inputs: WorkspaceInputs,
    images: Vec<crate::provider::ProviderImage>,
    history: Vec<crate::provider::HistoryTurn>,
    summary: Option<String>,
    prompts: crate::provider::EffectivePrompts,
    config: RunConfig,
    context_window_tokens: Option<u32>,
    context_started: SystemTime,
    context_ended: SystemTime,
    provider_concurrency: u8,
}

struct RunOpening {
    skill: Option<String>,
    run_id: RunId,
    agent_run_id: AgentRunId,
    objective: String,
    images: Vec<ImageAttachment>,
    config: RunConfig,
}

struct StartedRun<'a> {
    session_id: SessionId,
    run_id: RunId,
    goal: &'a GoalAdmission,
    finish_after_run: bool,
    workspace_identity: WorkspaceIdentity,
}

struct RunTurn {
    outcome: RunOutcome,
    autonomous: bool,
    goal_status: GoalStatus,
}

impl RunTurn {
    fn continues(&self) -> bool {
        self.autonomous
            && self.goal_status == GoalStatus::Running
            && self.outcome.run.status == crate::session::RunStatus::Finished
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GoalEntry {
    New,
    Resume,
    Continue,
}

struct GoalAdmission {
    id: GoalId,
    objective: String,
    include_paths: Option<Vec<String>>,
    phase: GoalPhase,
    budget: GoalBudget,
    autonomous: bool,
    resource_settings: crate::tools::ToolResourceSettings,
    plan_contract: Option<GoalPlanContract>,
    entry: GoalEntry,
    usage: GoalUsage,
    continues: Option<GoalId>,
}

impl GoalAdmission {
    fn started_event(&self, objective: &str, workspace_identity: WorkspaceIdentity) -> Event {
        Event::GoalStarted {
            goal_id: self.id,
            objective: objective.to_owned(),
            include_paths: self.include_paths.clone(),
            workspace_identity,
            budget: self.budget,
            phase: self.phase,
            autonomous: self.autonomous,
            resource_settings: self.resource_settings,
            continues: self.continues,
        }
    }
}

/// One per process: it owns the Store thread until `close`, and runs one operation at a time.
pub struct Engine {
    state: StateRoot,
    journal: Journal,
    config: EngineConfig,
    busy: AtomicBool,
    host_epoch: Mutex<Option<(SessionId, uuid::Uuid)>>,
    host_permissions: Mutex<Option<HostSettings>>,
}

type WorkspaceLeases = HashMap<WorkspaceIdentity, GoalWorkspaceLock>;

#[derive(Default)]
struct SubmissionLeases {
    session: Option<(SessionId, SessionRunLock)>,
    workspaces: WorkspaceLeases,
}

struct GoalWorkspaceLock {
    goal_id: GoalId,
    _lock: WorkspaceRunLock,
}

impl Engine {
    pub fn revoke_host_approvals(&self) -> Result<(), EngineError> {
        self.clear_host_epoch();
        Ok(())
    }

    fn clear_host_epoch(&self) {
        *self.host_epoch.lock().expect("host epoch lock") = None;
    }

    pub fn readmit_host_permissions(&self) -> Result<(), EngineError> {
        let _slot = self.claim()?;
        self.clear_host_epoch();
        *self.host_permissions.lock().expect("host permissions lock") = Some(HostSettings {
            digest: None,
            changed: true,
        });
        let snapshot = crate::tools::load_permissions(&self.state)?;
        *self.host_permissions.lock().expect("host permissions lock") = Some(HostSettings {
            digest: snapshot.map(|snapshot| snapshot.digest),
            changed: false,
        });
        Ok(())
    }

    fn host_settings_match(&self, digest: Option<[u8; 32]>) -> bool {
        let mut pinned = self.host_permissions.lock().expect("host permissions lock");
        let settings = pinned.get_or_insert(HostSettings {
            digest,
            changed: false,
        });
        settings.changed |= settings.digest != digest;
        !settings.changed
    }

    fn host_epoch(&self, session: SessionId) -> uuid::Uuid {
        let mut binding = self.host_epoch.lock().expect("host epoch lock");
        if binding.as_ref().is_none_or(|(bound, _)| *bound != session) {
            *binding = Some((session, uuid::Uuid::now_v7()));
        }
        binding.as_ref().expect("bound epoch").1
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn record_tool_latency(&self) -> Arc<crate::test_support::latency::Recorder> {
        Arc::clone(
            self.journal
                .tool_latency
                .get_or_init(|| Arc::new(crate::test_support::latency::Recorder::default())),
        )
    }

    async fn lock_lineage(
        &self,
        view: &crate::session::SessionView,
    ) -> Result<Vec<SessionRunLock>, EngineError> {
        let mut source = view.lineage.map(|lineage| lineage.source_session_id);
        let mut locks = Vec::new();
        while let Some(id) = source {
            if locks.len() == 8 {
                return Err(StoreError::ReplayLimit.into());
            }
            let mut lock = self.lock_session(id)?;
            lock.keep();
            locks.push(lock);
            let ancestor = self
                .journal
                .store()
                .load_view(id)
                .await?
                .ok_or(EngineError::InvalidHistory)?;
            source = ancestor.lineage.map(|lineage| lineage.source_session_id);
        }
        Ok(locks)
    }

    fn lock_session(&self, session_id: SessionId) -> Result<SessionRunLock, EngineError> {
        lock_session(&self.state, session_id)
    }

    pub fn open(state: StateRoot, config: EngineConfig) -> Result<Self, EngineError> {
        if config.worker_concurrency.is_some_and(|n| {
            !(1..=crate::session::CollaborationPolicy::MAX_ACTIVE_CHILDREN).contains(&n)
        }) {
            return Err(EngineError::InvalidRequest);
        }
        let journal = Journal::new(Store::open(state.try_clone()?)?);
        Ok(Self {
            state,
            journal,
            config,
            busy: AtomicBool::new(false),
            host_epoch: Mutex::new(None),
            host_permissions: Mutex::new(None),
        })
    }

    pub async fn close(self) -> Result<(), EngineError> {
        self.journal.close().await?;
        Ok(())
    }

    /// Fails unless the State path is still private and still names the directory this Engine holds.
    pub fn readmit_state(&self) -> Result<(), EngineError> {
        Ok(self.state.readmit()?)
    }

    /// Events this Engine appends from now on, in commit order; see `Subscription` for its gaps.
    pub fn subscribe(&self) -> Subscription {
        self.journal.subscribe()
    }

    fn claim(&self) -> Result<OperationSlot<'_>, EngineError> {
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| OperationSlot(&self.busy))
            .map_err(|_| EngineError::Busy)
    }

    pub async fn create_session(
        &self,
        workspace: PathBuf,
        title: Option<String>,
    ) -> Result<SessionId, EngineError> {
        lifecycle::create_with_journal(&self.state, &self.journal, &workspace, title).await
    }

    pub async fn fork_session(
        &self,
        source_session_id: SessionId,
        workspace: PathBuf,
        title: Option<String>,
    ) -> Result<SessionId, EngineError> {
        fork_with_store(
            &self.state,
            self.journal.store(),
            source_session_id,
            &workspace,
            title,
        )
        .await
    }

    pub async fn fork_session_at(
        &self,
        source_session_id: SessionId,
        workspace: PathBuf,
        source_sequence: u64,
        title: Option<String>,
    ) -> Result<SessionId, EngineError> {
        let _slot = self.claim()?;
        lifecycle::validate_session_title(title.as_deref())?;
        let mut lock = self.lock_session(source_session_id)?;
        let view = self
            .journal
            .store()
            .load_view(source_session_id)
            .await?
            .ok_or(EngineError::MissingSession)?;
        if view.provider_setup_blocks_continuation() {
            return Err(EngineError::ProviderSetupBlocked);
        }
        lock.keep();
        let _ancestors = self.lock_lineage(&view).await?;
        let identity = WorkspaceInputs::admit_identity(&workspace, &self.state)?;
        ensure_session_workspace(&view, identity)?;
        let boundary = self
            .journal
            .store()
            .prepare_fork(source_session_id, Some(source_sequence))
            .await
            .map_err(rewind::fork_error)?;
        self.journal
            .store()
            .fork_at(boundary, title.unwrap_or_else(|| "Forked Session".into()))
            .await
            .map_err(rewind::fork_error)
    }
}

fn admit_providers(providers: &crate::provider::AdmittedProviders) -> Result<(), EngineError> {
    if !providers.identities().valid()
        || providers.roster.as_ref().is_some_and(|roster| {
            !roster.valid()
                || roster.snapshot.role_routes().as_ref() != Some(&providers.identities())
        })
    {
        return Err(EngineError::InvalidRequest);
    }
    if providers.roster.is_some() {
        let pinned_bytes = serde_json::to_vec(&(
            &providers.identities(),
            providers.roster.as_ref().map(|roster| &roster.snapshot),
        ))
        .map_err(|_| EngineError::InvalidRequest)?
        .len();
        // Remaining RunStarted metadata fits 16 KiB at the existing digest, history and Tool-receipt bounds.
        if pinned_bytes + 16 * 1024 > 64 * 1024 {
            return Err(EngineError::InvalidRequest);
        }
    }
    for chain in providers.values().into_iter().chain(
        providers
            .roster
            .iter()
            .flat_map(|roster| roster.profiles.iter().map(|(_, chain)| chain)),
    ) {
        for provider in chain {
            checked_identity(provider.as_ref())?;
        }
    }
    Ok(())
}

fn checked_identity(
    provider: &dyn Provider,
) -> Result<&crate::provider::ProviderIdentity, EngineError> {
    let identity = provider.identity();
    if identity.valid() {
        Ok(identity)
    } else {
        Err(EngineError::InvalidRequest)
    }
}

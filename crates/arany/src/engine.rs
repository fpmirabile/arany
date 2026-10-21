#![cfg_attr(not(test), warn(clippy::too_many_lines))]

use crate::journal::{Journal, Subscription};
use crate::provider::{
    AgentPhase, ImageAttachment, PromptRole, Provider, ProviderRequest, RunInput, WorkspaceGuidance,
};
use crate::session::input::{InputError, WorkspaceInputs};
use crate::session::{
    AgentRole, AgentRunId, CollaborationPolicy, CompiledContext, Event, GoalBudget, GoalId,
    GoalPhase, GoalPlanContract, GoalStatus, GoalUsage, InstructionSource,
    MAX_STEERING_CONTEXT_BYTES, MAX_STEERING_LABEL_BYTES, MAX_STEERING_NOTE_BYTES,
    MAX_USER_MESSAGE_BYTES, RunConfig, RunId, RunView, SessionId, WorkspaceIdentity,
};
use crate::store::{SessionRunLock, StateRoot, Store, StoreError, WorkspaceRunLock};
use crate::telemetry::Telemetry;
use std::collections::HashMap;
use std::future::{Future, IntoFuture};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

mod compaction;
mod hints;
mod ledger;
mod lifecycle;
mod report;
mod rewind;
pub use rewind::{
    ConversationRewind, ConversationRewindOutcome, RewindBothResult, RewindFile, RewindIssue,
    RewindPreview, RewindRequest,
};
mod routing;
mod run_loop;
#[cfg(test)]
mod tests;
pub use crate::tools::Cancellation as RunCancellation;
use hints::HintPublisher;
pub use hints::{DisplayUpdate, RunHints, RunPreview};
pub use ledger::Action;
use ledger::GoalLedger;
pub use lifecycle::{
    continue_session, create_session, fork_session, list_sessions, list_sessions_page,
    rename_session, resume_session, set_session_defaults,
};
use lifecycle::{ensure_session_workspace, fork_with_store, lock_session};
use report::goal_report;
use routing::CallScope;
use run_loop::{LoopControls, RunIdentity, RunLoop};

/// Checks bounded Workspace input before an attached draft becomes a saved Session.
/// Run admission reads and validates the inputs again before disclosure.
pub fn validate_workspace_inputs(
    state: &StateRoot,
    workspace: &std::path::Path,
    launch_directory: Option<&std::path::Path>,
    include_paths: &[PathBuf],
) -> Result<(), EngineError> {
    WorkspaceInputs::load(workspace, state, launch_directory, include_paths)?;
    Ok(())
}

const CHILD_OUTPUT_TOKEN_CAP: u32 = crate::provider::DEFAULT_OUTPUT_TOKEN_CAP;
// GoalStarted or GoalResumed, GoalRunLinked, MessageAccepted, RunStarted and AgentSpawned.
const RUN_START_EVENTS: usize = 5;

const STORE_ACTION_OVERHEAD: std::time::Duration =
    std::time::Duration::from_millis(crate::session::STORE_ACTION_OVERHEAD_MS);

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("rewind is blocked; inspect the preview paths and uncertain publications")]
    RewindBlocked,
    #[error("rewind preview changed or expired; preview again")]
    RewindPreviewExpired,
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
    #[error("no exhausted Goal is available to continue")]
    GoalNotContinuable,
    #[error("Engine has an active operation")]
    Busy,
    #[error("Guard executable unavailable")]
    GuardUnavailable,
    #[error("Session is not idle")]
    SessionNotIdle,
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
    #[error("too many explicit includes")]
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
    #[error("state storage failed")]
    Store(#[from] StoreError),
    #[error(transparent)]
    Tool(#[from] crate::tools::ToolError),
}

impl From<InputError> for EngineError {
    fn from(error: InputError) -> Self {
        match error {
            InputError::InvalidPath => Self::InvalidWorkspacePath,
            InputError::NotRegular => Self::InputNotRegular,
            InputError::UnsafeLinkCount => Self::WorkspaceUnavailable,
            InputError::TooLarge => Self::InputTooLarge,
            InputError::InvalidUtf8 => Self::InputNotUtf8,
            InputError::MissingInclude => Self::MissingInclude,
            InputError::TooManyIncludes => Self::TooManyIncludes,
            InputError::State(error) => Self::Store(error),
            InputError::StateOverlap => Self::StateOverlap,
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
            resource_settings: crate::tools::ToolResourceSettings::default(),
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

/// The Tools one submission may use.
#[derive(Clone, Default)]
pub enum ToolGrant {
    #[default]
    None,
    /// The State's private `tools.json`, loaded at submission.
    Configured,
    /// The trusted Workspace's built-in grant.
    Trusted(crate::tools::WorkspacePermissions),
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
    session_lock: SessionRunLock,
    _lineage_locks: Vec<SessionRunLock>,
    history: Option<crate::session::SessionView>,
    goal: GoalAdmission,
    objective: String,
    budget: GoalBudget,
    provisional_workspace_lock: Option<WorkspaceRunLock>,
}

struct RootInput<'a> {
    assignment_profiles: Vec<String>,
    tool_context: Option<crate::tools::ToolContext>,
    inputs: &'a mut WorkspaceInputs,
    images: Vec<crate::provider::ProviderImage>,
    history: Vec<crate::provider::HistoryTurn>,
    summary: Option<String>,
}

struct PreparedTurn {
    tools: Option<crate::tools::ToolRuntime>,
    tool_context: Option<crate::tools::ToolContext>,
    inputs: WorkspaceInputs,
    images: Vec<crate::provider::ProviderImage>,
    history: Vec<crate::provider::HistoryTurn>,
    summary: Option<String>,
    config: RunConfig,
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

/// The attached caller's handle on one submission's steering: at most one note waits for the
/// next primary call, and a pause request stops the Run at its next boundary.
#[derive(Clone, Default)]
pub struct RunSteering {
    state: Arc<Mutex<SteeringState>>,
    pause: Arc<AtomicBool>,
    pause_signal: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct SteeringState {
    waiting: Option<String>,
    run_bytes: usize,
    recorded: u64,
}

const NOTE_SEPARATOR: &str = "\n\n";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SteeringRefused {
    RunFull,
    Empty,
    TooLarge,
    Unavailable,
}

impl RunSteering {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Option<MutexGuard<'_, SteeringState>> {
        self.state.lock().ok()
    }

    /// Holds a note until the Engine records it or the caller withdraws it. A note offered while
    /// another waits is appended to it, so the Engine delivers one merged note. Recorded text plus
    /// one history label per note, and the waiting note, stay within this Run's byte bound.
    pub fn offer_note(&self, text: String) -> Result<(), SteeringRefused> {
        let mut state = self.lock().ok_or(SteeringRefused::Unavailable)?;
        if text.trim().is_empty() {
            return Err(SteeringRefused::Empty);
        }
        if text.len() + MAX_STEERING_LABEL_BYTES > MAX_STEERING_NOTE_BYTES {
            return Err(SteeringRefused::TooLarge);
        }
        let merged_len = state
            .waiting
            .as_ref()
            .map_or(0, |waiting| waiting.len() + NOTE_SEPARATOR.len())
            + text.len();
        if state.run_bytes + merged_len + MAX_STEERING_LABEL_BYTES > MAX_STEERING_NOTE_BYTES {
            return Err(SteeringRefused::RunFull);
        }
        match state.waiting.as_mut() {
            Some(waiting) => {
                waiting.push_str(NOTE_SEPARATOR);
                waiting.push_str(&text);
            }
            None => state.waiting = Some(text),
        }
        Ok(())
    }

    /// Removes the waiting note before the Engine took it and returns its full merged text.
    pub fn withdraw_note(&self) -> Option<String> {
        self.lock().and_then(|mut state| state.waiting.take())
    }

    pub fn delivered_notes(&self) -> u64 {
        self.lock().map_or(0, |state| state.recorded)
    }

    /// Returns true only for the first pause request.
    pub fn pause(&self) -> bool {
        let first = !self.pause.swap(true, Ordering::SeqCst);
        if first {
            self.pause_signal.notify_waiters();
        }
        first
    }

    pub(crate) fn pause_requested(&self) -> bool {
        self.pause.load(Ordering::SeqCst)
    }

    // Resolves once a pause is requested; the waiter registers before it reads the flag.
    pub(crate) async fn paused(&self) {
        let notified = self.pause_signal.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.pause_requested() {
            notified.await;
        }
    }

    pub(crate) fn begin_run(&self) {
        if let Some(mut state) = self.lock() {
            state.run_bytes = 0;
        }
    }

    pub(crate) fn take_note(&self) -> Option<String> {
        let mut state = self.lock()?;
        let fits = state.waiting.as_ref().is_some_and(|text| {
            state.run_bytes + text.len() + MAX_STEERING_LABEL_BYTES <= MAX_STEERING_NOTE_BYTES
        });
        if !fits {
            return None;
        }
        let text = state.waiting.take()?;
        state.run_bytes += text.len() + MAX_STEERING_LABEL_BYTES;
        Some(text)
    }

    pub(crate) fn note_recorded(&self) {
        if let Some(mut state) = self.lock() {
            state.recorded += 1;
        }
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
    phase: GoalPhase,
    budget: GoalBudget,
    autonomous: bool,
    resource_settings: crate::tools::ToolResourceSettings,
    plan_contract: Option<GoalPlanContract>,
    entry: GoalEntry,
    usage: GoalUsage,
    continues: Option<GoalId>,
}

fn goal_context_metadata_reservation(
    budget: &GoalBudget,
    plan_contract: Option<&GoalPlanContract>,
) -> Result<usize, EngineError> {
    let facts = crate::provider::GoalContextFacts::maximum();
    let serialized = serde_json::to_vec(&(budget, plan_contract, facts))
        .map_err(|_| EngineError::ContextTooLarge)?
        .len();
    serialized
        .checked_add(if plan_contract.is_none() {
            crate::session::MAX_GOAL_PLAN_CONTRACT_BYTES
        } else {
            0
        })
        .and_then(|bytes| bytes.checked_add(64))
        .ok_or(EngineError::ContextTooLarge)
}

// Locks a dead Guard's Git write left are reported, with their paths, before admission refuses
// to resume past its unsettled effect; nothing is moved or retried.
async fn report_stale_git_locks(
    view: &crate::SessionView,
    goal: crate::session::GoalId,
    tools: bool,
    workspace: &std::path::Path,
) -> Result<(), EngineError> {
    let Some(goal) = view.goal(goal) else {
        return Ok(());
    };
    let git_writes: Vec<_> = view
        .runs
        .iter()
        .filter(|r| goal.run_ids.contains(&r.id))
        .flat_map(|r| &r.tools)
        .filter(|t| t.intent.call.is_git() && t.intent.call.mutates() && !t.settled_without_stop())
        .map(|t| t.intent.id)
        .collect();
    if git_writes.is_empty() {
        return Ok(());
    }
    if !tools {
        return Err(EngineError::GoalUnavailable);
    }
    crate::tools::worker::recover_private(&git_writes).await?;
    for id in git_writes {
        let workspace = workspace.to_owned();
        tokio::task::spawn_blocking(move || crate::tools::report_git_locks(&workspace, id))
            .await
            .map_err(|_| crate::tools::ToolError::Operation)??;
    }
    Ok(())
}

fn resolve_goal_admission(
    request: &RunRequest,
    history: Option<&crate::session::SessionView>,
    entry: GoalEntry,
) -> Result<GoalAdmission, EngineError> {
    if let Some(id) = request.goal.id {
        let goal = history
            .and_then(|view| view.goal(id))
            .ok_or(EngineError::GoalUnavailable)?;
        // Only the most recent unfinished Goal is resumable; older paused Goals stay history.
        if goal.status == GoalStatus::Interrupted
            && history.and_then(crate::session::SessionView::interrupted_goal_id) != Some(id)
        {
            return Err(EngineError::GoalUnavailable);
        }
        if !request.objective.trim().is_empty()
            && !goal.run_ids.iter().any(|run_id| {
                history.is_some_and(|view| view.runs.iter().any(|run| run.id == *run_id))
            })
            && goal.objective != request.objective
        {
            return Err(EngineError::GoalUnavailable);
        }
        if goal.status == GoalStatus::Interrupted
            && history.is_none_or(|view| !goal.can_resume(view))
            || goal.active_run.is_some()
        {
            return Err(EngineError::GoalUnavailable);
        }
        let entry = match goal.status {
            GoalStatus::Running => GoalEntry::Continue,
            GoalStatus::Interrupted if entry == GoalEntry::Continue => GoalEntry::Continue,
            GoalStatus::Interrupted => GoalEntry::Resume,
            GoalStatus::Completed | GoalStatus::Failed => {
                return Err(EngineError::GoalUnavailable);
            }
        };
        let view = history.ok_or(EngineError::GoalUnavailable)?;
        Ok(GoalAdmission {
            id,
            objective: goal.objective.clone(),
            phase: goal.phase,
            budget: goal.budget,
            autonomous: goal.autonomous,
            resource_settings: goal.resource_settings,
            plan_contract: goal.plan_contract.clone(),
            entry,
            usage: goal.resume_usage(view),
            continues: None,
        })
    } else if let Some(source_id) = request.goal.continues {
        let source = history
            .and_then(crate::session::SessionView::continuable_goal)
            .filter(|source| source.id == source_id && request.objective.trim().is_empty())
            .ok_or(EngineError::GoalNotContinuable)?;
        Ok(GoalAdmission {
            id: GoalId::new(),
            objective: source.objective.clone(),
            phase: source.phase,
            budget: request.goal.budget.ok_or(EngineError::InvalidRequest)?,
            autonomous: source.autonomous,
            resource_settings: request.goal.resource_settings,
            plan_contract: None,
            entry: GoalEntry::New,
            usage: GoalUsage {
                runs: 0,
                provider_calls: 0,
                active_ms: 0,
                known_tokens: 0,
                token_usage_unknown: false,
            },
            continues: Some(source_id),
        })
    } else {
        // A paused Goal does not block a new one; the Session lock serializes live Runs.
        Ok(GoalAdmission {
            id: GoalId::new(),
            objective: request.objective.clone(),
            phase: request.goal.phase.ok_or(EngineError::InvalidRequest)?,
            budget: request.goal.budget.ok_or(EngineError::InvalidRequest)?,
            autonomous: request.goal.autonomous,
            resource_settings: request.goal.resource_settings,
            plan_contract: None,
            entry: GoalEntry::New,
            usage: GoalUsage {
                runs: 0,
                provider_calls: 0,
                active_ms: 0,
                known_tokens: 0,
                token_usage_unknown: false,
            },
            continues: None,
        })
    }
}

/// One per process: it owns the Store thread until `close`, and runs one operation at a time.
pub struct Engine {
    state: StateRoot,
    journal: Journal,
    config: EngineConfig,
    busy: AtomicBool,
}

type WorkspaceLeases = HashMap<WorkspaceIdentity, GoalWorkspaceLock>;

struct GoalWorkspaceLock {
    goal_id: GoalId,
    _lock: WorkspaceRunLock,
}

impl Engine {
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

    fn acquire_goal_workspace(
        &self,
        leases: &WorkspaceLeases,
        goal_id: GoalId,
        identity: WorkspaceIdentity,
    ) -> Result<Option<WorkspaceRunLock>, EngineError> {
        if let Some(held) = leases.get(&identity) {
            return if held.goal_id == goal_id {
                Ok(None)
            } else {
                Err(EngineError::WorkspaceBusy)
            };
        }
        let lock = self.state.lock_workspace(identity).map_err(|error| {
            if matches!(error, StoreError::WorkspaceBusy) {
                EngineError::WorkspaceBusy
            } else {
                EngineError::Store(error)
            }
        })?;
        Ok(Some(lock))
    }

    fn retain_goal_workspace(
        leases: &mut WorkspaceLeases,
        goal_id: GoalId,
        identity: WorkspaceIdentity,
        lock: Option<WorkspaceRunLock>,
    ) {
        if let Some(lock) = lock {
            leases.insert(
                identity,
                GoalWorkspaceLock {
                    goal_id,
                    _lock: lock,
                },
            );
        }
    }

    fn release_goal_workspace(
        leases: &mut WorkspaceLeases,
        goal_id: GoalId,
        identity: WorkspaceIdentity,
    ) {
        if leases
            .get(&identity)
            .is_some_and(|held| held.goal_id == goal_id)
        {
            leases.remove(&identity);
        }
    }

    async fn append_goal_lifecycle(
        &self,
        session_id: SessionId,
        ledger: Option<&GoalLedger>,
        event: Event,
    ) -> Result<u64, EngineError> {
        let appended = match ledger {
            Some(ledger) => {
                ledger
                    .append(&self.journal, session_id, event.clone())
                    .await
            }
            None => self
                .journal
                .append(session_id, event.clone())
                .await
                .map(|committed| committed.sequence)
                .map_err(EngineError::from),
        };
        match appended {
            Ok(sequence) => Ok(sequence),
            // The Session lock is held, so a commit that lost its acknowledgment is the last Event.
            Err(error) => match self.journal.store().load_session(session_id).await {
                Ok(events)
                    if events
                        .last()
                        .is_some_and(|envelope| envelope.event == event) =>
                {
                    Err(EngineError::GoalAdmissionUncertain)
                }
                Ok(_) => Err(error),
                Err(_) => Err(EngineError::GoalAdmissionUncertain),
            },
        }
    }

    async fn close_exhausted_goal(
        &self,
        session_id: SessionId,
        view: &crate::session::SessionView,
        goal: &GoalAdmission,
    ) -> Result<(), EngineError> {
        let goal_view = view.goal(goal.id).ok_or(EngineError::InvalidHistory)?;
        let run = goal_view
            .run_ids
            .iter()
            .rev()
            .find_map(|run_id| view.runs.iter().find(|run| run.id == *run_id))
            .ok_or(EngineError::InvalidHistory)?;
        let report = goal_report(view, run, goal_view);
        self.append_goal_lifecycle(
            session_id,
            None,
            Event::GoalFinished {
                goal_id: goal.id,
                status: GoalStatus::Failed,
                active_ms: goal.usage.active_ms,
                report,
                exhausted: true,
            },
        )
        .await?;
        Ok(())
    }

    pub fn open(state: StateRoot, config: EngineConfig) -> Result<Self, EngineError> {
        if config
            .worker_concurrency
            .is_some_and(|n| !(1..=8).contains(&n))
        {
            return Err(EngineError::InvalidRequest);
        }
        let journal = Journal::new(Store::open(state.try_clone()?)?);
        Ok(Self {
            state,
            journal,
            config,
            busy: AtomicBool::new(false),
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

    /// Admits one submission and returns its handle. It is refused with `Busy` while another
    /// operation holds this Engine, when a Provider chain is invalid, and when its Tool grant
    /// cannot be loaded. The per-role Provider chains are this submission's: the next one may bring
    /// others.
    pub fn submit(
        &self,
        providers: crate::provider::AdmittedProviders,
        request: RunRequest,
        controls: RunControls,
    ) -> Result<RunHandle<'_>, EngineError> {
        let slot = self.claim()?;
        admit_providers(&providers)?;
        let guard = || {
            self.config
                .guard_executable
                .clone()
                .ok_or(EngineError::GuardUnavailable)
        };
        let tools = match &controls.tools {
            ToolGrant::None => None,
            ToolGrant::Configured => {
                let guard = guard()?;
                let snapshot = crate::tools::ToolRuntime::load_permissions(&self.state)?;
                let unguarded_rules =
                    crate::tools::ToolRuntime::load_exact_command_rules(&self.state)?;
                Some(ToolAdmission {
                    config: crate::tools::config::Config::load(&self.state)?,
                    permissions: snapshot,
                    unguarded_rules,
                    guard_executable: guard,
                })
            }
            ToolGrant::Trusted(permissions) => {
                let guard = guard()?;
                let snapshot = crate::tools::ToolRuntime::load_permissions(&self.state)?;
                let unguarded_rules =
                    crate::tools::ToolRuntime::load_exact_command_rules(&self.state)?;
                Some(ToolAdmission {
                    config: permissions.config(&request.workspace)?,
                    permissions: snapshot,
                    unguarded_rules,
                    guard_executable: guard,
                })
            }
        };
        let RunControls {
            tools: grant,
            approvals,
            steering,
            cancellation,
            hints,
        } = controls;
        let submission = AdmittedSubmission {
            providers,
            grant,
            tools,
            approvals: approvals.clone(),
            steering: steering.clone(),
        };
        let events = self.journal.subscribe();
        let run_cancellation = cancellation.clone();
        let result = Box::pin(async move {
            let _slot = slot;
            let result = self
                .run_inner(&submission, request, run_cancellation, hints)
                .await;
            if let Err(error) = &result {
                crate::diagnostics::engine_failure(error);
            }
            result
        });
        Ok(RunHandle {
            cancel: cancellation,
            steer: steering,
            approvals,
            events,
            result,
        })
    }

    async fn run_inner(
        &self,
        submission: &AdmittedSubmission,
        request: RunRequest,
        cancellation: RunCancellation,
        hints: Option<RunHints>,
    ) -> Result<RunResult, EngineError> {
        let workspace = request.workspace.clone();
        let launch_directory = request.launch_directory.clone();
        let include_paths = request.include_paths.clone();
        let policy = request.policy;
        let mut leases = WorkspaceLeases::new();
        let entry = if request.goal.id.is_some() {
            GoalEntry::Resume
        } else {
            GoalEntry::New
        };
        // Boxed: keeps the large Run future off the caller stack in debug builds.
        let mut turn = Box::pin(self.run_turn(
            submission,
            request,
            cancellation.clone(),
            hints.clone(),
            entry,
            &mut leases,
        ))
        .await?;
        let mut continuation_error = None;
        while turn.continues() && !cancellation.is_cancelled() {
            let next = RunRequest {
                skill: None,
                session_id: Some(turn.outcome.session_id),
                title: None,
                objective: String::new(),
                images: Vec::new(),
                workspace: workspace.clone(),
                launch_directory: launch_directory.clone(),
                include_paths: include_paths.clone(),
                policy,
                goal: GoalRequest::resume(turn.outcome.goal_id, false),
            };
            // Settlement left the Goal open under the same admits_next_run check, so this cannot be refused for budget.
            turn = match Box::pin(self.run_turn(
                submission,
                next,
                cancellation.clone(),
                hints.clone(),
                GoalEntry::Continue,
                &mut leases,
            ))
            .await
            {
                Ok(next) if next.outcome.run.status.is_outcome() => next,
                // The earlier Finished Run keeps the answer; the projection below names the unsettled Run's stop.
                Ok(_) => break,
                // Cancellation before MessageAccepted admits no Run; the committed Finished Run keeps its outcome.
                Err(EngineError::CancelledBeforeStart) => break,
                // A refusal before MessageAccepted keeps the committed Finished Run with the failure;
                // after acceptance the error propagates, as for the first Run.
                Err(error) => {
                    if self.run_stop(&turn.outcome).await?.0.id != turn.outcome.run.id {
                        return Err(error);
                    }
                    crate::diagnostics::engine_failure(&error);
                    continuation_error = Some(error);
                    break;
                }
            };
        }
        let (latest, stopped) = self.run_stop(&turn.outcome).await?;
        let mut run = turn.outcome;
        // An unsettled Run is returned as history reopens it.
        if latest.id == run.run.id {
            run.run = latest;
        }
        Ok(RunResult {
            run,
            stopped,
            continuation_error,
        })
    }

    // Projected from the reopened history, so a live result and its replayed turn agree.
    async fn run_stop(
        &self,
        outcome: &RunOutcome,
    ) -> Result<(RunView, Option<crate::session::RunStop>), EngineError> {
        let view = self
            .journal
            .store()
            .load_view(outcome.session_id)
            .await?
            .ok_or(EngineError::InvalidHistory)?;
        let goal = view
            .goal(outcome.goal_id)
            .ok_or(EngineError::InvalidHistory)?;
        let latest = view
            .runs
            .iter()
            .rev()
            .find(|run| goal.run_ids.contains(&run.id))
            .ok_or(EngineError::InvalidHistory)?;
        Ok((latest.clone(), view.run_stop(latest)))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Keep the ordered Run lifecycle together"
    )]
    async fn run_turn(
        &self,
        submission: &AdmittedSubmission,
        request: RunRequest,
        cancellation: RunCancellation,
        hints: Option<RunHints>,
        entry: GoalEntry,
        leases: &mut WorkspaceLeases,
    ) -> Result<RunTurn, EngineError> {
        submission.cancel_if_paused(&cancellation);
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }

        let identity = checked_identity(submission.providers.primary[0].as_ref())?;
        validate_turn_providers(submission, &request)?;
        let admitted = self
            .admit_turn(
                submission,
                &request,
                identity,
                entry,
                hints.as_ref().is_some_and(RunHints::previews_enabled),
                leases,
            )
            .await?;
        let prepared = self.prepare_turn(submission, &request, identity, &admitted)?;
        let AdmittedTurn {
            session_id,
            workspace_identity,
            mut session_lock,
            _lineage_locks,
            history: _,
            goal,
            objective,
            budget,
            provisional_workspace_lock,
        } = admitted;
        let PreparedTurn {
            tools,
            tool_context,
            mut inputs,
            images,
            history,
            summary,
            config,
            context_started,
            context_ended,
            provider_concurrency,
        } = prepared;
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }

        if request.session_id.is_none() {
            self.start_session(
                &mut session_lock,
                session_id,
                request.title,
                workspace_identity,
            )
            .await?;
        }
        let run_id = RunId::new();
        let agent_run_id = AgentRunId::new();
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        let trace =
            self.config
                .telemetry
                .trace_run(session_id, goal.id, (context_started, context_ended));
        // Reduces the open state that appends validate; the closed view would reject this Run's links.
        let ledger = self.open_ledger(session_id, goal.id).await?;
        let publisher = hints.map(|hints| HintPublisher::new(hints, run_id));
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        // Subscribers follow the turn's Events and drain them when it ends; they never hold it back.
        let trace_tap = trace.as_ref().map(|_| self.journal.subscribe());
        let (end, _) = tokio::sync::watch::channel(());
        let ended = |mut ended: tokio::sync::watch::Receiver<()>| async move {
            let _ = ended.changed().await;
        };
        let trace = trace_follower(trace, trace_tap, ended(end.subscribe()), &ledger, goal.id);
        let started = StartedRun {
            session_id,
            run_id,
            goal: &goal,
            finish_after_run: request.goal.finish_after_run,
            workspace_identity,
        };
        let segment = end_turn(
            async {
                Self::retain_goal_workspace(
                    leases,
                    goal.id,
                    workspace_identity,
                    provisional_workspace_lock,
                );
                self.begin_goal(&started, &ledger, &objective, &cancellation)
                    .await?;
                self.open_run(
                    &ledger,
                    session_id,
                    RunOpening {
                        skill: request.skill,
                        run_id,
                        agent_run_id,
                        objective: objective.clone(),
                        images: request.images,
                        config,
                    },
                )
                .await?;
                let provider_request = root_request(
                    CallScope {
                        session_id,
                        run_id,
                        agent_run_id,
                    },
                    request.policy,
                    identity,
                    &goal,
                    objective,
                    budget,
                    RootInput {
                        assignment_profiles: submission
                            .providers
                            .roster
                            .as_ref()
                            .map_or_else(Vec::new, |roster| roster.snapshot.child_profiles.clone()),
                        tool_context,
                        inputs: &mut inputs,
                        images,
                        history,
                        summary,
                    },
                );
                if let Some(steering) = &submission.steering {
                    steering.begin_run();
                }
                self.drive_turn(
                    submission,
                    goal.id,
                    LoopControls {
                        policy: request.policy,
                        concurrency: provider_concurrency,
                        goal_phase: goal.phase,
                        cancellation,
                        tools: tools.as_ref(),
                        approvals: submission.approvals.as_ref(),
                        steering: submission.steering.clone(),
                    },
                    provider_request,
                    &ledger,
                    publisher.as_ref(),
                )
                .await?;
                self.settle_turn(started, &ledger, leases).await
            },
            end,
        );
        tokio::join!(segment, trace).0
    }

    async fn drive_turn(
        &self,
        submission: &AdmittedSubmission,
        goal_id: GoalId,
        controls: LoopControls<'_>,
        request: ProviderRequest,
        ledger: &GoalLedger,
        publisher: Option<&HintPublisher>,
    ) -> Result<(), EngineError> {
        // Boxed: the Run loop's future is large and would overflow a small caller stack.
        Box::pin(
            RunLoop::new(
                &self.journal,
                submission.providers.clone(),
                RunIdentity {
                    session_id: request.session_id,
                    run_id: request.run_id,
                    goal_id: Some(goal_id),
                    primary_id: request.agent_run_id,
                },
                controls,
                ledger,
                publisher,
            )
            .execute(request),
        )
        .await
    }

    async fn begin_goal(
        &self,
        started: &StartedRun<'_>,
        ledger: &GoalLedger,
        objective: &str,
        cancellation: &RunCancellation,
    ) -> Result<(), EngineError> {
        let StartedRun {
            session_id,
            run_id,
            goal,
            workspace_identity,
            ..
        } = *started;
        match goal.entry {
            GoalEntry::New => {
                self.append_goal_lifecycle(
                    session_id,
                    Some(ledger),
                    Event::GoalStarted {
                        goal_id: goal.id,
                        objective: objective.to_owned(),
                        workspace_identity,
                        budget: goal.budget,
                        phase: goal.phase,
                        autonomous: goal.autonomous,
                        resource_settings: goal.resource_settings,
                        continues: goal.continues,
                    },
                )
                .await?;
            }
            GoalEntry::Resume => {
                self.append_goal_lifecycle(
                    session_id,
                    Some(ledger),
                    Event::GoalResumed { goal_id: goal.id },
                )
                .await?;
            }
            GoalEntry::Continue => {}
        }
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        // A link is an admission acknowledgment: a lost acknowledgment may still have committed it.
        self.append_goal_lifecycle(
            session_id,
            Some(ledger),
            Event::GoalRunLinked {
                goal_id: goal.id,
                run_id,
                phase: goal.phase,
            },
        )
        .await?;
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        Ok(())
    }

    async fn open_run(
        &self,
        ledger: &GoalLedger,
        session_id: SessionId,
        opening: RunOpening,
    ) -> Result<(), EngineError> {
        let RunOpening {
            run_id,
            agent_run_id,
            objective,
            images,
            config,
            skill,
        } = opening;
        let accepted = Event::MessageAccepted {
            skill,
            run_id,
            text: objective,
            images,
        };
        ledger.append(&self.journal, session_id, accepted).await?;
        let profile = config
            .roster
            .as_ref()
            .map(|roster| roster.providers.primary.clone());
        let started = Event::RunStarted {
            run_id,
            config,
            active_deadline_ms: ledger.run_active_deadline()?,
        };
        ledger.append(&self.journal, session_id, started).await?;
        let spawned = Event::AgentSpawned {
            profile,
            assignment: None,
            definition: None,
            run_id,
            agent_run_id,
            role: AgentRole::Primary,
            ordinal: 0,
            objective: None,
            grant: None,
        };
        ledger.append(&self.journal, session_id, spawned).await?;
        Ok(())
    }

    async fn open_ledger(
        &self,
        session_id: SessionId,
        goal_id: GoalId,
    ) -> Result<GoalLedger, EngineError> {
        let view = self
            .journal
            .store()
            .load_open_view(session_id)
            .await?
            .ok_or(EngineError::InvalidHistory)?;
        Ok(GoalLedger::new(view, Some(goal_id)))
    }

    async fn start_session(
        &self,
        session_lock: &mut SessionRunLock,
        session_id: SessionId,
        title: Option<String>,
        workspace_identity: WorkspaceIdentity,
    ) -> Result<(), EngineError> {
        self.journal
            .append(
                session_id,
                Event::SessionStarted {
                    title: title.unwrap_or_else(|| "New Session".into()),
                    workspace_identity,
                },
            )
            .await?;
        session_lock.keep();
        Ok(())
    }

    async fn admit_turn(
        &self,
        submission: &AdmittedSubmission,
        request: &RunRequest,
        identity: &crate::provider::ProviderIdentity,
        entry: GoalEntry,
        previews: bool,
        leases: &WorkspaceLeases,
    ) -> Result<AdmittedTurn, EngineError> {
        self.validate_request(request)?;
        if submission.tools.is_some() && identity.custom_provenance().is_some() {
            return Err(EngineError::InvalidRequest);
        }
        if identity.custom_provenance().is_some() && !request.images.is_empty() {
            return Err(EngineError::ImagesNotAdmitted);
        }
        let workspace_identity = crate::tools::workspace_identity(&request.workspace)?;
        let session_id = request.session_id.unwrap_or_default();
        let mut session_lock = self.lock_session(session_id)?;
        let history = if request.session_id.is_some() {
            Some(
                self.journal
                    .store()
                    .load_view(session_id)
                    .await?
                    .ok_or(EngineError::MissingSession)?,
            )
        } else {
            None
        };
        if history.is_some() {
            session_lock.keep();
        }
        let _lineage_locks = match &history {
            Some(view) => self.lock_lineage(view).await?,
            None => Vec::new(),
        };
        if let Some(view) = &history {
            ensure_session_workspace(view, workspace_identity)?;
        }
        if entry == GoalEntry::Resume
            && let Some(view) = &history
            && let Some(goal) = request.goal.id
        {
            report_stale_git_locks(view, goal, submission.tools.is_some(), &request.workspace)
                .await?;
        }
        let goal = resolve_goal_admission(request, history.as_ref(), entry)?;
        if goal.entry == GoalEntry::Resume {
            let view = history.as_ref().ok_or(EngineError::InvalidHistory)?;
            let stored = view.goal(goal.id).ok_or(EngineError::InvalidHistory)?;
            let ids: Vec<_> = view
                .runs
                .iter()
                .filter(|r| stored.run_ids.contains(&r.id))
                .flat_map(|r| r.private_recovery_tools())
                .map(|t| t.intent.id)
                .collect();
            if !ids.is_empty() {
                if submission.tools.is_none() {
                    return Err(EngineError::GoalUnavailable);
                }
                crate::tools::worker::recover_private(&ids).await?;
            }
        }
        if entry == GoalEntry::Resume {
            let previous = history
                .as_ref()
                .and_then(|view| {
                    let stored = view.goal(goal.id)?;
                    view.runs
                        .iter()
                        .rev()
                        .filter(|run| stored.run_ids.contains(&run.id))
                        .find_map(|run| run.config.as_ref())
                })
                .and_then(|config| config.roster.as_ref());
            let selected = submission
                .providers
                .roster
                .as_ref()
                .map(|roster| &roster.snapshot);
            if previous.is_some_and(|roster| {
                Some(roster.selection_digest) != selected.map(|selected| selected.selection_digest)
            }) {
                return Err(EngineError::RosterSelectionMismatch);
            }
        }
        let objective = if request.objective.trim().is_empty()
            && (request.goal.id.is_some() || request.goal.continues.is_some())
        {
            goal.objective.clone()
        } else {
            request.objective.clone()
        };
        let budget = goal.budget.effective(goal.plan_contract.as_ref());
        if !budget.admits_next_run(goal.usage) {
            if goal.entry == GoalEntry::Resume {
                let view = history.as_ref().ok_or(EngineError::InvalidHistory)?;
                self.close_exhausted_goal(session_id, view, &goal).await?;
            }
            return Err(EngineError::GoalBudgetExhausted);
        }
        // Run start reserves the GoalStarted objective envelope, the MessageAccepted objective and image envelopes, the Goal lifecycle and the first action; each later action admits its own.
        let first_action = Action::ProviderCall {
            previews,
            steering: submission.steering.is_some(),
        };
        let event_slots = RUN_START_EVENTS
            + first_action.max_events_for_admitted(&submission.providers)
            + if submission.tools.is_some() {
                first_action.verdict_slots()
            } else {
                0
            }
            + usize::from(request.session_id.is_none());
        self.journal
            .admit_run_start(session_id, event_slots)
            .await?;
        let writes = match &submission.grant {
            ToolGrant::None => false,
            ToolGrant::Configured => submission
                .tools
                .as_ref()
                .is_some_and(|tools| tools.config.write),
            ToolGrant::Trusted(_) => true,
        };
        let provisional_workspace_lock =
            if goal.phase == crate::session::GoalPhase::Implementation && writes {
                self.acquire_goal_workspace(leases, goal.id, workspace_identity)?
            } else {
                None
            };
        Ok(AdmittedTurn {
            session_id,
            workspace_identity,
            session_lock,
            _lineage_locks,
            history,
            goal,
            objective,
            budget,
            provisional_workspace_lock,
        })
    }

    fn worker_policy(
        &self,
        runtime: Option<&crate::tools::ToolRuntime>,
        policy: CollaborationPolicy,
        concurrency: u8,
    ) -> Option<crate::session::worker::WorkerPolicy> {
        runtime
            .and_then(crate::tools::ToolRuntime::worker_policy)
            .map(|mut p| {
                p.concurrency = self
                    .config
                    .worker_concurrency
                    .unwrap_or(4)
                    .min(policy.max_children().max(1))
                    .min(concurrency);
                p
            })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "turn admission keeps pinned context and Skill validation together"
    )]
    fn prepare_turn(
        &self,
        submission: &AdmittedSubmission,
        request: &RunRequest,
        identity: &crate::provider::ProviderIdentity,
        admitted: &AdmittedTurn,
    ) -> Result<PreparedTurn, EngineError> {
        let (workspace_identity, history) = (admitted.workspace_identity, &admitted.history);
        let (goal, objective, budget) = (&admitted.goal, &admitted.objective, admitted.budget);
        if let Some(permissions) = submission.permissions() {
            permissions.verify(&request.workspace)?;
        }
        let mut tools = submission
            .tools
            .as_ref()
            .map(|tools| {
                crate::tools::ToolRuntime::admit(
                    tools.config.clone(),
                    tools.permissions.clone(),
                    tools.unguarded_rules.clone(),
                    &request.workspace,
                    &self.state,
                    tools.guard_executable.clone(),
                )
            })
            .transpose()?;
        if let Some(runtime) = &tools
            && runtime.workspace_identity() != workspace_identity
        {
            return Err(crate::tools::ToolError::ChangedInput.into());
        }
        if let (Some(permissions), Some(runtime)) = (submission.permissions(), &tools)
            && !permissions.matches_identity(runtime.workspace_identity())
        {
            return Err(crate::tools::ToolError::ChangedInput.into());
        }
        if request.skill.as_ref().is_some_and(|name| {
            !crate::tools::types::valid_name(name)
                || tools
                    .as_ref()
                    .is_none_or(|runtime| !runtime.allows_skill(name))
        }) {
            return Err(EngineError::InvalidRequest);
        }
        let tool_context = tools
            .as_ref()
            .map(crate::tools::ToolRuntime::context)
            .transpose()?;
        let inputs = match &tools {
            Some(runtime) => WorkspaceInputs::load_pinned(
                &request.workspace,
                &self.state,
                request.launch_directory.as_deref(),
                &request.include_paths,
                Some(runtime.workspace_identity()),
            )?,
            None => WorkspaceInputs::load(
                &request.workspace,
                &self.state,
                request.launch_directory.as_deref(),
                &request.include_paths,
            )?,
        };
        let root_identity = inputs.root_identity()?;
        if root_identity != workspace_identity {
            return Err(crate::tools::ToolError::ChangedInput.into());
        }
        if let Some(view) = &history {
            ensure_session_workspace(view, root_identity)?;
        }
        let context_started = SystemTime::now();
        let feedback_reserved = usize::from(submission.steering.is_some())
            * MAX_STEERING_CONTEXT_BYTES
            + tools
                .as_ref()
                .map_or(0, crate::tools::ToolRuntime::stop_feedback_reserve);
        let goal_metadata_reserved = goal_context_metadata_reservation(
            goal.plan_contract
                .as_ref()
                .map_or(&goal.budget, |plan| &plan.budget),
            goal.plan_contract.as_ref(),
        )?
        .checked_add(feedback_reserved)
        .ok_or(EngineError::ContextTooLarge)?;
        let context_window_tokens = submission.providers.context_window_tokens();
        let context = CompiledContext::compile(
            history.as_ref(),
            objective,
            &request.images,
            &inputs,
            request.policy,
            tools
                .as_ref()
                .map_or(0, |_| crate::tools::types::MIN_TOOL_CONTEXT_BYTES),
            goal_metadata_reserved,
            context_window_tokens,
        )
        .map_err(|_| EngineError::ContextTooLarge)?;
        if let Some(runtime) = &mut tools {
            runtime.set_context_limit(context.tool_context_budget_bytes)?;
        }
        if !context.images.is_empty()
            && submission.providers.agent_chains().iter().any(|chain| {
                chain
                    .iter()
                    .any(|provider| provider.identity().custom_provenance().is_some())
            })
        {
            return Err(EngineError::ImagesNotAdmitted);
        }
        let context_ended = SystemTime::now();
        let provider_concurrency = if budget.max_tokens.is_some() {
            1
        } else {
            submission.providers.agent_concurrency()
        };
        let routes = submission.providers.identities();
        let inherited = routes
            .values()
            .iter()
            .all(|chain| chain.len() == 1 && chain[0] == *identity);
        let config = RunConfig {
            supervisor: supervisor_policy(admitted.session_id, tools.is_some()),
            worker_policy: self.worker_policy(tools.as_ref(), request.policy, provider_concurrency),
            agent_definitions: tools.as_ref().and_then(|tools| tools.definitions.pin()),
            roster: submission
                .providers
                .roster
                .as_ref()
                .map(|roster| Box::new(roster.snapshot.clone())),
            providers: (!inherited).then_some(routes),
            provider: identity.profile.clone(),
            model: identity.model.clone(),
            effort: identity.effort,
            custom_profile_provenance: identity.custom_provenance().cloned(),
            saved_api_account_id: identity.saved_api_account_id(),
            chatgpt_provenance: identity.chatgpt_provenance().cloned(),
            output_token_bound: identity.limits.output_token_bound,
            context_window_tokens,
            policy: request.policy,
            output_token_cap: identity.limits.max_output_tokens,
            provider_concurrency,
            workspace_device: root_identity.device,
            workspace_inode: root_identity.inode,
            user_instruction_digest: inputs
                .user_instructions
                .as_ref()
                .map(|snapshot| snapshot.digest),
            instruction_sources: inputs
                .instructions
                .iter()
                .map(|snapshot| InstructionSource {
                    directory: snapshot.directory().to_owned(),
                    file: snapshot.instruction_file(),
                    digest: snapshot.digest,
                })
                .collect(),
            launch_directory: inputs.launch_directory.clone(),
            include_digests: inputs
                .includes
                .iter()
                .map(|snapshot| snapshot.digest)
                .collect(),
            history_run_ids: context.history_run_ids,
            excluded_history_runs: context.excluded_history_runs,
            context_usage: Some(context.context_usage),
            compaction_event_sequence: context.compaction_event_sequence,
            compaction_content_digest: context.compaction_content_digest,
            tool_policy: tools.as_ref().map(|tools| Box::new(tools.receipt())),
        };
        config.validate().map_err(|_| EngineError::InvalidRequest)?;
        if !crate::session::run_started_fits(&config) {
            return Err(EngineError::InputTooLarge);
        }
        Ok(PreparedTurn {
            tools,
            tool_context,
            inputs,
            images: context.images,
            history: context.history,
            summary: context.summary,
            config,
            context_started,
            context_ended,
            provider_concurrency,
        })
    }

    async fn settle_turn(
        &self,
        started: StartedRun<'_>,
        ledger: &GoalLedger,
        leases: &mut WorkspaceLeases,
    ) -> Result<RunTurn, EngineError> {
        let StartedRun {
            session_id,
            run_id,
            goal,
            finish_after_run,
            workspace_identity,
        } = started;
        let budget_stopped = ledger.budget_stopped();
        let (run, active_ms, terminal) = ledger.view(|view| -> Result<_, EngineError> {
            let run = view
                .runs
                .last()
                .filter(|run| run.id == run_id)
                .ok_or(EngineError::InvalidHistory)?
                .clone();
            let goal_view = view.goal(goal.id).ok_or(EngineError::InvalidHistory)?;
            // Only Run failure or a satisfied/exhausted Goal settles it; a cancelled Run leaves its Goal open to resume.
            let terminal = match run.status {
                crate::session::RunStatus::Finished if goal.autonomous => {
                    if goal_view.plan_contract.is_none() || goal_view.success_criterion_satisfied {
                        Some((GoalStatus::Completed, false))
                    } else if !goal_view.admits_next_run() {
                        // Decide the exhaustion before this segment closes, so the next admission cannot find it open.
                        Some((GoalStatus::Failed, true))
                    } else {
                        None
                    }
                }
                crate::session::RunStatus::Finished => {
                    finish_after_run.then_some((GoalStatus::Completed, false))
                }
                crate::session::RunStatus::Failed => Some((
                    GoalStatus::Failed,
                    budget_stopped && goal_view.budget_spent(),
                )),
                _ => None,
            }
            .map(|(status, exhausted)| (status, goal_report(view, &run, goal_view), exhausted));
            Ok((run, goal_view.active_ms, terminal))
        })??;
        let mut goal_status = GoalStatus::Running;
        if let Some((status, report, exhausted)) = terminal {
            self.journal
                .append(
                    session_id,
                    Event::GoalFinished {
                        goal_id: goal.id,
                        status,
                        active_ms,
                        report,
                        exhausted,
                    },
                )
                .await?;
            Self::release_goal_workspace(leases, goal.id, workspace_identity);
            goal_status = status;
        }
        // Only an unsettled primary leaves its Run open here; the Goal stays open as after a crash.
        if !run.status.is_outcome() && run.status != crate::session::RunStatus::Active {
            return Err(EngineError::InvalidHistory);
        }
        Ok(RunTurn {
            outcome: RunOutcome {
                session_id,
                goal_id: goal.id,
                run,
            },
            autonomous: goal.autonomous,
            goal_status,
        })
    }

    fn validate_request(&self, request: &RunRequest) -> Result<(), EngineError> {
        if (request.objective.trim().is_empty()
            && request.images.is_empty()
            && request.goal.id.is_none()
            && request.goal.continues.is_none())
            || request.objective.len() > MAX_USER_MESSAGE_BYTES
            || !crate::provider::valid_image_collection(&request.images)
            || (request.session_id.is_some() && request.title.is_some())
            || request
                .title
                .as_ref()
                .is_some_and(|title| title.is_empty() || title.len() > 128)
            || request.policy.validate().is_err()
            || !request.goal.resource_settings.valid()
            || match (&request.goal.id, &request.goal.budget) {
                (Some(_), Some(_)) | (None, None) => true,
                (Some(_), None) => false,
                (None, Some(budget)) => budget.validate().is_err(),
            }
            || (request.goal.id.is_some() && request.session_id.is_none())
            || (request.goal.autonomous && request.goal.id.is_some())
            || (request.goal.continues.is_some()
                && (request.goal.id.is_some()
                    || request.session_id.is_none()
                    || !request.images.is_empty()))
        {
            return Err(EngineError::InvalidRequest);
        }
        Ok(())
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

fn validate_turn_providers(
    submission: &AdmittedSubmission,
    request: &RunRequest,
) -> Result<(), EngineError> {
    let providers = &submission.providers;
    if !request.images.is_empty()
        && providers.agent_chains().iter().any(|chain| {
            chain
                .iter()
                .any(|candidate| candidate.identity().custom_provenance().is_some())
        })
    {
        return Err(EngineError::ImagesNotAdmitted);
    }
    if submission.tools.is_some()
        && providers.agent_chains().iter().any(|chain| {
            chain
                .iter()
                .any(|candidate| !candidate.identity().shares_account(chain[0].identity()))
        })
    {
        return Err(EngineError::IncompatibleProviderChain);
    }
    if submission.tools.is_some()
        && providers.agent_chains().iter().any(|chain| {
            chain
                .iter()
                .any(|candidate| candidate.identity().custom_provenance().is_some())
        })
    {
        return Err(EngineError::InvalidRequest);
    }
    Ok(())
}

fn root_request(
    scope: CallScope,
    policy: crate::session::CollaborationPolicy,
    identity: &crate::provider::ProviderIdentity,
    goal: &GoalAdmission,
    objective: String,
    budget: GoalBudget,
    input: RootInput<'_>,
) -> ProviderRequest {
    let CallScope {
        session_id,
        run_id,
        agent_run_id,
    } = scope;
    let tools = input.tool_context;
    let role = if tools.is_some() {
        PromptRole::Primary
    } else {
        PromptRole::ReadOnly
    };
    ProviderRequest {
        session_id,
        run_id,
        agent_run_id,
        phase: AgentPhase::RootPlan,
        collaboration: policy,
        model: identity.model.clone(),
        max_output_tokens: identity.limits.max_output_tokens,
        input: RunInput {
            assignment_profiles: input.assignment_profiles,
            images: input.images,
            goal_budget: (goal.autonomous || goal.plan_contract.is_some()).then_some(budget),
            pinned_budget: budget,
            goal_plan_contract: goal.plan_contract.clone(),
            workspace_guidance: input
                .inputs
                .user_instructions
                .take()
                .into_iter()
                .chain(std::mem::take(&mut input.inputs.instructions))
                .map(|snapshot| WorkspaceGuidance {
                    path: snapshot.path,
                    text: snapshot.content,
                })
                .collect(),
            includes: std::mem::take(&mut input.inputs.includes)
                .into_iter()
                .map(|snapshot| snapshot.model_include())
                .collect(),
            history: input.history,
            context_summary: input.summary,
            tools,
            ..RunInput::new(role, objective)
        },
    }
}

async fn end_turn(
    turn: impl Future<Output = Result<RunTurn, EngineError>>,
    end: tokio::sync::watch::Sender<()>,
) -> Result<RunTurn, EngineError> {
    let turn = turn.await;
    drop(end);
    turn
}

async fn trace_follower(
    trace: Option<crate::telemetry::RunTrace>,
    tap: Option<Subscription>,
    ended: impl Future<Output = ()>,
    ledger: &GoalLedger,
    goal_id: GoalId,
) {
    if let (Some(trace), Some(tap)) = (trace, tap) {
        let goal = || {
            ledger
                .view(|view| view.goal(goal_id).cloned())
                .ok()
                .flatten()
        };
        trace.follow(tap, ended, goal).await;
    }
}

fn checked_identity(
    provider: &dyn Provider,
) -> Result<&crate::provider::ProviderIdentity, EngineError> {
    let identity = provider.identity();
    let limits = &identity.limits;
    if identity.valid()
        && (limits.output_token_bound != crate::provider::OutputTokenBound::LocalAcceptanceOnly
            || limits.max_output_tokens <= crate::provider::DEFAULT_OUTPUT_TOKEN_CAP)
    {
        Ok(identity)
    } else {
        Err(EngineError::InvalidRequest)
    }
}

fn supervisor_policy(
    _session_id: SessionId,
    tools_enabled: bool,
) -> Option<crate::supervisor::SupervisorPolicy> {
    tools_enabled.then(|| {
        #[cfg(any(test, feature = "test-support"))]
        if crate::fault::FaultPlan::take_supervisor_off(_session_id) {
            return crate::supervisor::SupervisorPolicy::all_off();
        }
        crate::supervisor::SupervisorPolicy::default()
    })
}

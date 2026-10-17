use crate::journal::{Journal, Subscription};
use crate::provider::{
    AgentPhase, ImageAttachment, PromptRole, Provider, ProviderRequest, RunInput,
};
use crate::session::input::{InputError, WorkspaceInputs};
use crate::session::{
    AgentRole, AgentRunId, CollaborationPolicy, CompiledContext, Event, GoalBudget, GoalId,
    GoalPhase, GoalPlanContract, GoalStatus, GoalUsage, MAX_STEERING_CONTEXT_BYTES,
    MAX_STEERING_LABEL_BYTES, MAX_STEERING_NOTE_BYTES, MAX_USER_MESSAGE_BYTES, RunConfig, RunId,
    RunView, SessionId,
};
use crate::store::{SessionRunLock, StateRoot, Store, StoreError, WorkspaceRunLock};
use crate::telemetry::Telemetry;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

mod compaction;
mod ledger;
mod lifecycle;
mod progress;
mod report;
mod routing;
mod run_loop;
pub use crate::tools::Cancellation as RunCancellation;
pub use ledger::Action;
use ledger::GoalLedger;
pub use lifecycle::{
    continue_session, create_session, fork_session, list_sessions, rename_session, resume_session,
    set_session_defaults,
};
use lifecycle::{ensure_session_workspace, fork_with_store, lock_session};
use progress::ProgressPublisher;
pub use progress::{DisplayUpdate, RunPreview, RunProgress, RunUpdate};
use report::goal_report;
use run_loop::{RunControls, RunIdentity, RunLoop};

/// Checks bounded Workspace input before an attached draft becomes a saved Session.
/// Run admission reads and validates the inputs again before disclosure.
pub fn validate_workspace_inputs(
    state: &StateRoot,
    workspace: &std::path::Path,
    include_paths: &[PathBuf],
) -> Result<(), EngineError> {
    WorkspaceInputs::load(workspace, state, include_paths)?;
    Ok(())
}

const CHILD_OUTPUT_TOKEN_CAP: u32 = crate::provider::DEFAULT_OUTPUT_TOKEN_CAP;
/// Store admission and commit time reserved after each action's own deadline.
// GoalStarted or GoalResumed, GoalRunLinked, MessageAccepted, RunStarted and AgentSpawned.
const RUN_START_EVENTS: usize = 5;

const STORE_ACTION_OVERHEAD: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
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
    /// An autonomous Goal stopped after `run` committed; `reason` says why no further Run was admitted.
    #[error("Goal stopped after a committed Run: {reason}")]
    StoppedAfterRun {
        run: Box<RunOutcome>,
        reason: Box<EngineError>,
    },
    #[error("Session is not idle")]
    SessionNotIdle,
    #[error("Session has no committed Run boundary to fork")]
    ForkNotAtRunBoundary,
    #[error("Session belongs to a different Workspace")]
    WorkspaceMismatch,
    #[error("Workspace input path is invalid")]
    InvalidWorkspacePath,
    #[error("Workspace input must be a regular file")]
    InputNotRegular,
    #[error("Workspace input exceeds its byte limit")]
    InputTooLarge,
    #[error("Workspace input is not UTF-8")]
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
            InputError::StateOverlap => Self::StateOverlap,
            InputError::Io(_) => Self::WorkspaceUnavailable,
        }
    }
}

pub struct RunRequest {
    pub session_id: Option<SessionId>,
    pub title: Option<String>,
    /// Empty only for an explicit Goal resume, which runs the Goal's pinned objective.
    pub objective: String,
    pub images: Vec<ImageAttachment>,
    pub workspace: PathBuf,
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

struct AdmittedTurn {
    session_id: SessionId,
    workspace_identity: (u64, u64),
    session_lock: SessionRunLock,
    _lineage_locks: Vec<SessionRunLock>,
    history: Option<crate::session::SessionView>,
    goal: GoalAdmission,
    objective: String,
    budget: GoalBudget,
    provisional_workspace_lock: Option<WorkspaceRunLock>,
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

struct StartedRun<'a> {
    session_id: SessionId,
    run_id: RunId,
    goal: &'a GoalAdmission,
    finish_after_run: bool,
    workspace_identity: (u64, u64),
}

/// One admitted Run and the Goal state it left behind.
struct RunTurn {
    outcome: RunOutcome,
    autonomous: bool,
    goal_status: GoalStatus,
}

impl RunTurn {
    /// Settlement failed the Goal over this Finished Run, which leaves no admissible next Run.
    fn exhausted(&self) -> bool {
        self.goal_status == GoalStatus::Failed
            && self.outcome.run.status == crate::session::RunStatus::Finished
    }

    /// An autonomous Goal continues after an unsatisfied Run that it did not finish.
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
}

#[derive(Default)]
struct SteeringState {
    waiting: Option<String>,
    run_bytes: usize,
    recorded: u64,
}

/// Separates notes offered before the Engine takes them; the Engine delivers them as one note.
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
        !self.pause.swap(true, Ordering::SeqCst)
    }

    pub(crate) fn pause_requested(&self) -> bool {
        self.pause.load(Ordering::SeqCst)
    }

    pub(crate) fn begin_run(&self) {
        if let Some(mut state) = self.lock() {
            state.run_bytes = 0;
        }
    }

    /// Takes the waiting note only if it still fits this Run's quota. Taking reserves the quota,
    /// so an accepted offer always stays deliverable within this Run.
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

struct GoalAdmission {
    id: GoalId,
    objective: String,
    phase: GoalPhase,
    budget: GoalBudget,
    autonomous: bool,
    resource_settings: crate::tools::ToolResourceSettings,
    plan_contract: Option<GoalPlanContract>,
    is_new: bool,
    resume: bool,
    usage: GoalUsage,
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

#[cfg(all(test, unix))]
mod goal_workspace_lease_tests {
    use super::*;
    use crate::provider::{ProviderError, ProviderResponse};
    use std::time::Duration;

    struct NeverProvider;

    #[async_trait::async_trait]
    impl Provider for NeverProvider {
        fn identity(&self) -> &crate::provider::ProviderIdentity {
            static IDENTITY: std::sync::LazyLock<crate::provider::ProviderIdentity> =
                std::sync::LazyLock::new(|| {
                    crate::provider::ProviderIdentity::new(
                        "scripted",
                        "lease-test",
                        crate::provider::Provenance::native(),
                    )
                });
            &IDENTITY
        }

        async fn invoke(&self, _: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
            panic!("a dropped Goal admission must not call the Provider")
        }
    }

    #[tokio::test]
    async fn dropped_start_acknowledgment_releases_the_lease_once_the_goal_is_paused() {
        let temp = tempfile::tempdir().expect("temporary root");
        let state_path = temp.path().join("state-a");
        let second_state_path = temp.path().join("state-b");
        let account_root = temp.path().join("account");
        let state = StateRoot::admit(&state_path).expect("private State root");
        let second_state = StateRoot::admit(&second_state_path).expect("second private State root");
        let engine = Engine::open(state, NeverProvider).expect("Engine");
        let session_id = SessionId::new();
        let goal_id = GoalId::new();
        let workspace_identity = (7, 11);
        engine
            .journal
            .append(
                session_id,
                Event::SessionStarted {
                    title: "Goal lease acknowledgment".into(),
                    workspace_identity,
                },
            )
            .await
            .expect("Session start");

        let workspace_lock = engine
            .state
            .lock_workspace_in(&account_root, workspace_identity)
            .expect("provisional Workspace lock");
        let (acknowledgment_entered, release_acknowledgment) =
            crate::fault::FaultPlan::gate_goal_append_acknowledgment(session_id, |event| {
                matches!(event, Event::GoalStarted { .. } | Event::GoalResumed { .. })
            });
        let mut leases = WorkspaceLeases::new();
        Engine::retain_goal_workspace(
            &mut leases,
            goal_id,
            workspace_identity,
            Some(workspace_lock),
        );
        let mut admission = Box::pin(engine.append_goal_lifecycle(
            session_id,
            None,
            Event::GoalStarted {
                goal_id,
                objective: "Keep the Workspace lease through Store acknowledgment".into(),
                workspace_identity,
                budget: GoalBudget::default(),
                phase: GoalPhase::Implementation,
                autonomous: false,
                resource_settings: crate::tools::ToolResourceSettings::default(),
            },
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = &mut admission => panic!("append returned before its controlled acknowledgment: {result:?}"),
                result = acknowledgment_entered => result.expect("committed append reached acknowledgment gate"),
            }
        })
        .await
        .expect("Goal append acknowledgment gate");
        drop(admission);
        release_acknowledgment
            .send(())
            .expect("release Store acknowledgment");

        assert!(matches!(
            second_state.lock_workspace_in(&account_root, workspace_identity),
            Err(StoreError::WorkspaceBusy)
        ));
        // The call ends with its dropped future; the committed Goal is paused and no longer holds the writer lock.
        drop(leases);
        let view = engine
            .journal
            .store()
            .load_view(session_id)
            .await
            .expect("canonical Session view")
            .expect("Session exists");
        assert_eq!(view.interrupted_goal_id(), Some(goal_id));
        let writer = second_state
            .lock_workspace_in(&account_root, workspace_identity)
            .expect("paused Goal releases the Workspace writer lock");
        drop(writer);
        let events = engine
            .journal
            .store()
            .load_session(session_id)
            .await
            .expect("canonical Events");
        assert_eq!(events.len(), 2, "no Run or effect was admitted");
        engine.close().await.expect("close Engine");
    }

    #[tokio::test]
    async fn lost_run_link_acknowledgment_is_admission_uncertain() {
        let temp = tempfile::tempdir().expect("temporary root");
        let state_path = temp.path().join("state");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state = StateRoot::admit(&state_path).expect("private State root");
        let mut engine = Engine::open(state, NeverProvider).expect("Engine");
        let session_id = SessionId::new();
        let workspace_identity = crate::tools::workspace_identity(&workspace).expect("identity");
        engine
            .journal
            .append(
                session_id,
                Event::SessionStarted {
                    title: "Lost link acknowledgment".into(),
                    workspace_identity,
                },
            )
            .await
            .expect("Session start");
        let (link_entered, release_link) =
            crate::fault::FaultPlan::gate_goal_append_acknowledgment(session_id, |event| {
                matches!(event, Event::GoalRunLinked { .. })
            });
        let request = RunRequest {
            session_id: Some(session_id),
            title: None,
            objective: "Link the Run before its acknowledgment is lost".into(),
            images: Vec::new(),
            workspace: workspace.clone(),
            include_paths: Vec::new(),
            policy: CollaborationPolicy::Single,
            goal: GoalRequest::default(),
        };
        let mut run = Box::pin(engine.run(request));
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = &mut run => panic!("Run returned before its link was held: {}", result.is_ok()),
                result = link_entered => result.expect("link reached the acknowledgment gate"),
            }
        })
        .await
        .expect("link acknowledgment gate");
        drop(release_link);
        match run.await {
            Err(EngineError::GoalAdmissionUncertain) => {}
            Err(error) => panic!("a possibly committed link must be uncertain, not {error}"),
            Ok(_) => panic!("Run completed without an acknowledged link"),
        }
        engine.close().await.expect("close Engine");
    }

    const RESUMABLE_OBJECTIVE: &str = "Resume the paused Goal after a failed append";

    async fn paused_goal_session(
        engine: &mut Engine,
        workspace: &std::path::Path,
    ) -> (SessionId, GoalId) {
        let session_id = SessionId::new();
        let goal_id = GoalId::new();
        let workspace_identity = crate::tools::workspace_identity(workspace).expect("identity");
        engine
            .journal
            .append(
                session_id,
                Event::SessionStarted {
                    title: "Resumable Goal".into(),
                    workspace_identity,
                },
            )
            .await
            .expect("Session start");
        engine
            .journal
            .append(
                session_id,
                Event::GoalStarted {
                    goal_id,
                    objective: RESUMABLE_OBJECTIVE.into(),
                    workspace_identity,
                    budget: GoalBudget::default(),
                    phase: GoalPhase::Implementation,
                    autonomous: false,
                    resource_settings: crate::tools::ToolResourceSettings::default(),
                },
            )
            .await
            .expect("Goal start");
        (session_id, goal_id)
    }

    fn resume_request(
        session_id: SessionId,
        goal_id: GoalId,
        workspace: &std::path::Path,
    ) -> RunRequest {
        RunRequest {
            session_id: Some(session_id),
            title: None,
            objective: RESUMABLE_OBJECTIVE.into(),
            images: Vec::new(),
            workspace: workspace.to_path_buf(),
            include_paths: Vec::new(),
            policy: CollaborationPolicy::Single,
            goal: GoalRequest {
                id: Some(goal_id),
                phase: None,
                budget: None,
                resource_settings: crate::tools::ToolResourceSettings::default(),
                finish_after_run: true,
                autonomous: false,
            },
        }
    }

    #[tokio::test]
    async fn goal_resume_failure_before_commit_is_store_error() {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state = StateRoot::admit(&temp.path().join("state")).expect("private State root");
        let mut engine = Engine::open(state, NeverProvider).expect("Engine");
        let (session_id, goal_id) = paused_goal_session(&mut engine, &workspace).await;
        crate::fault::FaultPlan::fail_goal_append(session_id, |event| {
            matches!(event, Event::GoalResumed { .. })
        });
        match engine
            .run(resume_request(session_id, goal_id, &workspace))
            .await
        {
            Err(EngineError::Store(_)) => {}
            Err(error) => panic!("an uncommitted resume must report its Store error, not {error}"),
            Ok(_) => panic!("Run resumed without a committed GoalResumed"),
        }
        let events = engine
            .journal
            .store()
            .load_session(session_id)
            .await
            .expect("canonical Events");
        assert_eq!(events.len(), 2, "GoalResumed did not commit");
        engine.close().await.expect("close Engine");
    }

    #[tokio::test]
    async fn lost_goal_resume_acknowledgment_is_admission_uncertain() {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state = StateRoot::admit(&temp.path().join("state")).expect("private State root");
        let mut engine = Engine::open(state, NeverProvider).expect("Engine");
        let (session_id, goal_id) = paused_goal_session(&mut engine, &workspace).await;
        let (resume_entered, release_resume) =
            crate::fault::FaultPlan::gate_goal_append_acknowledgment(session_id, |event| {
                matches!(event, Event::GoalResumed { .. })
            });
        let mut run = Box::pin(engine.run(resume_request(session_id, goal_id, &workspace)));
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = &mut run => panic!("Run returned before its resume was held: {}", result.is_ok()),
                result = resume_entered => result.expect("resume reached the acknowledgment gate"),
            }
        })
        .await
        .expect("resume acknowledgment gate");
        drop(release_resume);
        match run.await {
            Err(EngineError::GoalAdmissionUncertain) => {}
            Err(error) => panic!("a possibly committed resume must be uncertain, not {error}"),
            Ok(_) => panic!("Run completed without an acknowledged resume"),
        }
        engine.close().await.expect("close Engine");
    }

    #[tokio::test]
    async fn goal_run_link_failure_before_commit_is_store_error() {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state = StateRoot::admit(&temp.path().join("state")).expect("private State root");
        let mut engine = Engine::open(state, NeverProvider).expect("Engine");
        let (session_id, goal_id) = paused_goal_session(&mut engine, &workspace).await;
        crate::fault::FaultPlan::fail_goal_append(session_id, |event| {
            matches!(event, Event::GoalRunLinked { .. })
        });
        match engine
            .run(resume_request(session_id, goal_id, &workspace))
            .await
        {
            Err(EngineError::Store(_)) => {}
            Err(error) => panic!("an uncommitted link must report its Store error, not {error}"),
            Ok(_) => panic!("Run completed without a committed link"),
        }
        engine.close().await.expect("close Engine");
    }
}

/// `continuation` admits the next Run of a live autonomous Goal, whose history projects as interrupted.
fn resolve_goal_admission(
    request: &RunRequest,
    history: Option<&crate::session::SessionView>,
    continuation: bool,
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
        let resume = match goal.status {
            GoalStatus::Running => false,
            GoalStatus::Interrupted => !continuation,
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
            is_new: false,
            resume,
            usage: goal.resume_usage(view),
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
            is_new: true,
            resume: false,
            usage: GoalUsage {
                runs: 0,
                provider_calls: 0,
                active_ms: 0,
                known_tokens: 0,
                token_usage_unknown: false,
            },
        })
    }
}

pub struct Engine {
    state: StateRoot,
    journal: Journal,
    providers: crate::provider::AdmittedProviders,
    telemetry: Telemetry,
    tools: Option<(crate::tools::config::Config, PathBuf)>,
    workspace_permissions: Option<crate::tools::WorkspacePermissions>,
    tool_approvals: Option<crate::tools::ToolApprovals>,
    steering: Option<RunSteering>,
}

/// Writer locks held by one call. A paused Goal holds none: the call drops them when it returns.
type WorkspaceLeases = HashMap<(u64, u64), GoalWorkspaceLock>;

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
        identity: (u64, u64),
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
        identity: (u64, u64),
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

    fn release_goal_workspace(leases: &mut WorkspaceLeases, goal_id: GoalId, identity: (u64, u64)) {
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

    /// Closes a paused Goal that explicit resume finds out of budget, so it is no longer resumable. It
    /// runs under the Session lock and acquires no Workspace lease; no Goal span exists yet to close.
    async fn close_exhausted_goal(
        &mut self,
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
            },
        )
        .await?;
        Ok(())
    }

    pub fn open(state: StateRoot, provider: impl Provider + 'static) -> Result<Self, EngineError> {
        Self::open_with_telemetry(state, provider, Telemetry::disabled())
    }

    pub fn open_with_telemetry(
        state: StateRoot,
        provider: impl Provider + 'static,
        telemetry: Telemetry,
    ) -> Result<Self, EngineError> {
        let journal = Journal::new(Store::open(state.try_clone()?)?);
        let provider: Arc<dyn Provider> = Arc::new(provider);
        Ok(Self {
            state,
            journal,
            providers: crate::provider::Providers::all(vec![provider]),
            telemetry,
            tools: None,
            workspace_permissions: None,
            tool_approvals: None,
            steering: None,
        })
    }

    pub fn set_providers(
        &mut self,
        providers: crate::provider::AdmittedProviders,
    ) -> Result<(), EngineError> {
        if !providers.identities().valid() {
            return Err(EngineError::InvalidRequest);
        }
        for chain in providers.values() {
            for provider in chain {
                checked_identity(provider.as_ref())?;
            }
        }
        self.providers = providers;
        Ok(())
    }

    pub async fn close(self) -> Result<(), EngineError> {
        self.journal.close().await?;
        Ok(())
    }

    /// Events this Engine appends from now on, in commit order; see `Subscription` for its gaps.
    pub fn subscribe(&self) -> Subscription {
        self.journal.subscribe()
    }

    pub fn enable_tools(&mut self, guard_executable: PathBuf) -> Result<(), EngineError> {
        let config = crate::tools::config::Config::load(&self.state)?;
        self.tools = Some((config, guard_executable));
        self.workspace_permissions = None;
        Ok(())
    }

    pub fn set_tool_approvals(&mut self, approvals: crate::tools::ToolApprovals) {
        self.tool_approvals = Some(approvals);
    }

    /// Installs the caller's handle. Its pause request is sticky for the handle's lifetime, so a
    /// caller creates one `RunSteering` per submission.
    pub fn set_steering(&mut self, steering: RunSteering) {
        self.steering = Some(steering);
    }

    pub fn enable_trusted_workspace(
        &mut self,
        permissions: crate::tools::WorkspacePermissions,
        workspace: &std::path::Path,
        guard_executable: PathBuf,
        approvals: crate::tools::ToolApprovals,
    ) -> Result<(), EngineError> {
        self.tools = Some((permissions.config(workspace)?, guard_executable));
        self.workspace_permissions = Some(permissions);
        self.tool_approvals = Some(approvals);
        Ok(())
    }

    pub async fn fork_session(
        &mut self,
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

    pub async fn run(&mut self, request: RunRequest) -> Result<RunOutcome, EngineError> {
        self.run_with_cancel(request, RunCancellation::new()).await
    }

    pub async fn run_with_cancel(
        &mut self,
        request: RunRequest,
        cancellation: RunCancellation,
    ) -> Result<RunOutcome, EngineError> {
        self.run_inner(request, cancellation, None)
            .await
            .inspect_err(crate::diagnostics::engine_failure)
    }

    pub async fn run_with_progress(
        &mut self,
        request: RunRequest,
        cancellation: RunCancellation,
        progress: RunProgress,
    ) -> Result<RunOutcome, EngineError> {
        self.run_inner(request, cancellation, Some(progress))
            .await
            .inspect_err(crate::diagnostics::engine_failure)
    }

    /// Runs one admitted Run, then keeps an autonomous Goal iterating on its pinned objective.
    async fn run_inner(
        &mut self,
        request: RunRequest,
        cancellation: RunCancellation,
        progress: Option<RunProgress>,
    ) -> Result<RunOutcome, EngineError> {
        let workspace = request.workspace.clone();
        let include_paths = request.include_paths.clone();
        let policy = request.policy;
        let mut leases = WorkspaceLeases::new();
        // Boxed: keeps the large Run future off the caller stack in debug builds.
        let mut turn = Box::pin(self.run_turn(
            request,
            cancellation.clone(),
            progress.clone(),
            false,
            &mut leases,
        ))
        .await?;
        while turn.continues() && !cancellation.is_cancelled() {
            let next = RunRequest {
                session_id: Some(turn.outcome.session_id),
                title: None,
                objective: String::new(),
                images: Vec::new(),
                workspace: workspace.clone(),
                include_paths: include_paths.clone(),
                policy,
                goal: GoalRequest::resume(turn.outcome.goal_id, false),
            };
            // Settlement left the Goal open under the same admits_next_run check, so this cannot be refused for budget.
            turn = match Box::pin(self.run_turn(
                next,
                cancellation.clone(),
                progress.clone(),
                true,
                &mut leases,
            ))
            .await
            {
                Ok(turn) => turn,
                // Cancellation before MessageAccepted admits no Run; the committed Finished Run keeps its outcome.
                Err(EngineError::CancelledBeforeStart) => break,
                // The earlier Finished Run stays committed; the caller gets it with the reason the loop stopped.
                Err(error) => {
                    return Err(EngineError::StoppedAfterRun {
                        run: Box::new(turn.outcome),
                        reason: Box::new(error),
                    });
                }
            };
        }
        if turn.exhausted() {
            return Err(EngineError::StoppedAfterRun {
                run: Box::new(turn.outcome),
                reason: Box::new(EngineError::GoalBudgetExhausted),
            });
        }
        Ok(turn.outcome)
    }

    async fn run_turn(
        &mut self,
        request: RunRequest,
        cancellation: RunCancellation,
        progress: Option<RunProgress>,
        continuation: bool,
        leases: &mut WorkspaceLeases,
    ) -> Result<RunTurn, EngineError> {
        self.cancel_if_paused(&cancellation);
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }

        let provider = Arc::clone(&self.providers.primary[0]);
        let identity = checked_identity(provider.as_ref())?;
        if !self.providers.identities().valid() {
            return Err(EngineError::InvalidRequest);
        }
        for chain in self.providers.values() {
            for candidate in chain {
                checked_identity(candidate.as_ref())?;
            }
        }
        if !request.images.is_empty()
            && self.providers.values().iter().take(2).any(|chain| {
                chain
                    .iter()
                    .any(|candidate| candidate.identity().custom_provenance().is_some())
            })
        {
            return Err(EngineError::ImagesNotAdmitted);
        }
        if self.tools.is_some()
            && [&self.providers.primary, &self.providers.child]
                .iter()
                .any(|chain| {
                    chain
                        .iter()
                        .any(|candidate| !candidate.identity().shares_account(chain[0].identity()))
                })
        {
            return Err(EngineError::IncompatibleProviderChain);
        }
        if self.tools.is_some()
            && self.providers.values().iter().take(2).any(|chain| {
                chain
                    .iter()
                    .any(|candidate| candidate.identity().custom_provenance().is_some())
            })
        {
            return Err(EngineError::InvalidRequest);
        }
        let previews = progress.as_ref().is_some_and(RunProgress::previews_enabled);
        let finish_after_run = request.goal.finish_after_run;
        let admitted = self
            .admit_turn(&request, identity, continuation, previews, leases)
            .await?;
        let prepared = self.prepare_turn(&request, identity, &admitted)?;
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
            inputs,
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
            self.journal
                .append(
                    session_id,
                    Event::SessionStarted {
                        title: request.title.unwrap_or_else(|| "New Session".into()),
                        workspace_identity,
                    },
                )
                .await?;
            session_lock.keep();
        }
        let run_id = RunId::new();
        let agent_run_id = AgentRunId::new();
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        let trace = self
            .telemetry
            .trace_run(session_id, goal.id, (context_started, context_ended));
        // Reduces the open state that appends validate; the closed view would reject this Run's links.
        let view = self
            .journal
            .store()
            .load_open_view(session_id)
            .await?
            .ok_or(EngineError::InvalidHistory)?;
        let ledger = GoalLedger::new(view, goal.id);
        let publisher = progress.map(|progress| ProgressPublisher::new(progress, run_id));
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        // Subscribers follow the turn's Events and drain them when it ends; they never hold it back.
        let progress_tap = publisher.as_ref().map(|_| self.journal.subscribe());
        let trace_tap = trace.as_ref().map(|_| self.journal.subscribe());
        let (end, _) = tokio::sync::watch::channel(());
        let ended = |mut ended: tokio::sync::watch::Receiver<()>| async move {
            let _ = ended.changed().await;
        };
        let follow_progress = {
            let ended = ended(end.subscribe());
            let (publisher, ledger) = (publisher.as_ref(), &ledger);
            async move {
                if let (Some(publisher), Some(tap)) = (publisher, progress_tap) {
                    publisher.follow(tap, ended, ledger).await;
                }
            }
        };
        let follow_trace = {
            let ended = ended(end.subscribe());
            let (ledger, goal_id) = (&ledger, goal.id);
            async move {
                if let (Some(trace), Some(tap)) = (trace, trace_tap) {
                    let goal = || {
                        ledger
                            .view(|view| view.goal(goal_id).cloned())
                            .ok()
                            .flatten()
                    };
                    trace.follow(tap, ended, goal).await;
                }
            }
        };
        let segment = async {
            let turn = async {
                Self::retain_goal_workspace(
                    leases,
                    goal.id,
                    workspace_identity,
                    provisional_workspace_lock,
                );
                if goal.is_new {
                    self.append_goal_lifecycle(
                        session_id,
                        Some(&ledger),
                        Event::GoalStarted {
                            goal_id: goal.id,
                            objective: objective.clone(),
                            workspace_identity,
                            budget: goal.budget,
                            phase: goal.phase,
                            autonomous: goal.autonomous,
                            resource_settings: goal.resource_settings,
                        },
                    )
                    .await?;
                } else if goal.resume {
                    self.append_goal_lifecycle(
                        session_id,
                        Some(&ledger),
                        Event::GoalResumed { goal_id: goal.id },
                    )
                    .await?;
                }
                if cancellation.is_cancelled() {
                    return Err(EngineError::CancelledBeforeStart);
                }
                // A link is an admission acknowledgment: a lost acknowledgment may still have committed it.
                self.append_goal_lifecycle(
                    session_id,
                    Some(&ledger),
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
                let accepted = Event::MessageAccepted {
                    run_id,
                    text: objective.clone(),
                    images: request.images,
                };
                ledger.append(&self.journal, session_id, accepted).await?;
                let started = Event::RunStarted {
                    run_id,
                    config,
                    active_deadline_ms: ledger.run_active_deadline()?,
                };
                ledger.append(&self.journal, session_id, started).await?;
                let spawned = Event::AgentSpawned {
                    run_id,
                    agent_run_id,
                    role: AgentRole::Primary,
                    ordinal: 0,
                    objective: None,
                    grant: None,
                };
                ledger.append(&self.journal, session_id, spawned).await?;
                let role = if tool_context.is_some() {
                    PromptRole::Primary
                } else {
                    PromptRole::ReadOnly
                };
                let provider_request = ProviderRequest {
                    session_id,
                    run_id,
                    agent_run_id,
                    phase: AgentPhase::RootPlan,
                    collaboration: request.policy,
                    model: identity.model.clone(),
                    max_output_tokens: identity.limits.max_output_tokens,
                    input: RunInput {
                        images,
                        goal_budget: Some(budget),
                        goal_plan_contract: goal.plan_contract.clone(),
                        workspace_guidance: inputs
                            .instructions
                            .map(|(_, snapshot)| snapshot.content),
                        includes: inputs
                            .includes
                            .into_iter()
                            .map(|snapshot| snapshot.model_include())
                            .collect(),
                        history,
                        context_summary: summary,
                        tools: tool_context,
                        ..RunInput::new(role, objective)
                    },
                };
                if let Some(steering) = &self.steering {
                    steering.begin_run();
                }
                // Boxed: the Run loop's future is large and would overflow a small caller stack.
                Box::pin(
                    RunLoop::new(
                        &self.journal,
                        self.providers.clone(),
                        RunIdentity {
                            session_id,
                            run_id,
                            goal_id: goal.id,
                            primary_id: agent_run_id,
                        },
                        RunControls {
                            policy: request.policy,
                            concurrency: provider_concurrency,
                            goal_phase: goal.phase,
                            cancellation,
                            tools: tools.as_ref(),
                            approvals: self.tool_approvals.as_ref(),
                            steering: self.steering.clone(),
                        },
                        &ledger,
                        publisher.as_ref(),
                    )
                    .execute(provider_request),
                )
                .await?;
                self.settle_turn(
                    StartedRun {
                        session_id,
                        run_id,
                        goal: &goal,
                        finish_after_run,
                        workspace_identity,
                    },
                    leases,
                )
                .await
            }
            .await;
            drop(end);
            turn
        };
        let (turn, (), ()) = tokio::join!(segment, follow_progress, follow_trace);
        turn
    }

    /// Resolves the Goal and takes the Session, lineage and Workspace admissions a Run needs.
    async fn admit_turn(
        &mut self,
        request: &RunRequest,
        identity: &crate::provider::ProviderIdentity,
        continuation: bool,
        previews: bool,
        leases: &WorkspaceLeases,
    ) -> Result<AdmittedTurn, EngineError> {
        self.validate_request(request)?;
        if self.tools.is_some() && identity.custom_provenance().is_some() {
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
            ensure_session_workspace(view, workspace_identity.0, workspace_identity.1)?;
        }
        let goal = resolve_goal_admission(request, history.as_ref(), continuation)?;
        let objective = if request.objective.trim().is_empty() && request.goal.id.is_some() {
            goal.objective.clone()
        } else {
            request.objective.clone()
        };
        let budget = goal.budget.effective(goal.plan_contract.as_ref());
        if !budget.admits_next_run(goal.usage) {
            if goal.resume {
                let view = history.as_ref().ok_or(EngineError::InvalidHistory)?;
                self.close_exhausted_goal(session_id, view, &goal).await?;
            }
            return Err(EngineError::GoalBudgetExhausted);
        }
        // Run start reserves the GoalStarted objective envelope, the MessageAccepted objective and image envelopes, the Goal lifecycle and the first action; each later action admits its own.
        let first_action = Action::ProviderCall {
            previews,
            steering: self.steering.is_some(),
        };
        let event_slots = RUN_START_EVENTS
            + first_action.max_events_for(&self.providers)
            + usize::from(request.session_id.is_none());
        self.journal
            .admit_run_start(session_id, event_slots)
            .await?;
        let provisional_workspace_lock = if goal.phase == crate::session::GoalPhase::Implementation
            && self.tools.as_ref().is_some_and(|(config, _)| config.write)
        {
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

    /// Admits the Tools, loads the pinned Workspace inputs and compiles the Run's context and config.
    fn prepare_turn(
        &self,
        request: &RunRequest,
        identity: &crate::provider::ProviderIdentity,
        admitted: &AdmittedTurn,
    ) -> Result<PreparedTurn, EngineError> {
        let workspace_identity = admitted.workspace_identity;
        let history = &admitted.history;
        let goal = &admitted.goal;
        let objective = &admitted.objective;
        let budget = admitted.budget;
        if let Some(permissions) = &self.workspace_permissions {
            permissions.verify(&request.workspace)?;
        }
        let mut tools = self
            .tools
            .as_ref()
            .map(|(config, executable)| {
                crate::tools::ToolRuntime::admit(
                    config.clone(),
                    &request.workspace,
                    &self.state,
                    executable.clone(),
                )
            })
            .transpose()?;
        if let Some(runtime) = &tools
            && runtime.workspace_identity() != workspace_identity
        {
            return Err(crate::tools::ToolError::ChangedInput.into());
        }
        if let (Some(permissions), Some(runtime)) = (&self.workspace_permissions, &tools)
            && !permissions.matches_identity(runtime.workspace_identity())
        {
            return Err(crate::tools::ToolError::ChangedInput.into());
        }
        let tool_context = tools
            .as_ref()
            .map(crate::tools::ToolRuntime::context)
            .transpose()?;
        let inputs = match &tools {
            Some(runtime) => WorkspaceInputs::load_pinned(
                &request.workspace,
                &self.state,
                &request.include_paths,
                Some(runtime.workspace_identity()),
            )?,
            None => WorkspaceInputs::load(&request.workspace, &self.state, &request.include_paths)?,
        };
        let (workspace_device, workspace_inode) = inputs.root_identity()?;
        if (workspace_device, workspace_inode) != workspace_identity {
            return Err(crate::tools::ToolError::ChangedInput.into());
        }
        if let Some(view) = &history {
            ensure_session_workspace(view, workspace_device, workspace_inode)?;
        }
        let context_started = SystemTime::now();
        let steering_reserved = if self.steering.is_some() {
            MAX_STEERING_CONTEXT_BYTES
        } else {
            0
        };
        let goal_metadata_reserved = goal_context_metadata_reservation(
            goal.plan_contract
                .as_ref()
                .map_or(&goal.budget, |plan| &plan.budget),
            goal.plan_contract.as_ref(),
        )?
        .checked_add(steering_reserved)
        .ok_or(EngineError::ContextTooLarge)?;
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
        )
        .map_err(|_| EngineError::ContextTooLarge)?;
        if let Some(runtime) = &mut tools {
            runtime.set_context_limit(context.tool_context_budget_bytes)?;
        }
        if identity.custom_provenance().is_some() && !context.images.is_empty() {
            return Err(EngineError::ImagesNotAdmitted);
        }
        let context_ended = SystemTime::now();
        let provider_concurrency = if budget.max_tokens.is_some() {
            1
        } else {
            identity.limits.concurrency.min(
                self.providers
                    .child
                    .iter()
                    .map(|provider| provider.identity().limits.concurrency)
                    .min()
                    .unwrap_or(1),
            )
        };
        let routes = self.providers.identities();
        let inherited = routes
            .values()
            .iter()
            .all(|chain| chain.len() == 1 && chain[0] == *identity);
        let config = RunConfig {
            providers: (!inherited).then_some(routes),
            provider: identity.profile.clone(),
            model: identity.model.clone(),
            effort: identity.effort,
            custom_profile_provenance: identity.custom_provenance().cloned(),
            saved_api_account_id: identity.saved_api_account_id(),
            chatgpt_provenance: identity.chatgpt_provenance().cloned(),
            output_token_bound: identity.limits.output_token_bound,
            policy: request.policy,
            output_token_cap: identity.limits.max_output_tokens,
            provider_concurrency,
            workspace_device,
            workspace_inode,
            instruction_digest: inputs
                .instructions
                .as_ref()
                .map(|(_, snapshot)| snapshot.digest),
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
            tool_policy: tools.as_ref().map(crate::tools::ToolRuntime::receipt),
        };
        config.validate().map_err(|_| EngineError::InvalidRequest)?;
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

    /// Closes the Goal when the Run's outcome settles it and reports the committed Run.
    async fn settle_turn(
        &self,
        started: StartedRun<'_>,
        leases: &mut WorkspaceLeases,
    ) -> Result<RunTurn, EngineError> {
        let StartedRun {
            session_id,
            run_id,
            goal,
            finish_after_run,
            workspace_identity,
        } = started;
        let view = self
            .journal
            .store()
            .load_view(session_id)
            .await?
            .ok_or(EngineError::InvalidHistory)?;
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
                    Some(GoalStatus::Completed)
                } else if !goal_view.admits_next_run() {
                    // Decide the exhaustion before this segment closes, so the next admission cannot find it open.
                    Some(GoalStatus::Failed)
                } else {
                    None
                }
            }
            crate::session::RunStatus::Finished => {
                finish_after_run.then_some(GoalStatus::Completed)
            }
            crate::session::RunStatus::Failed => Some(GoalStatus::Failed),
            _ => None,
        };
        let mut goal_status = GoalStatus::Running;
        if let Some(status) = terminal {
            let report = goal_report(&view, &run, goal_view);
            self.journal
                .append(
                    session_id,
                    Event::GoalFinished {
                        goal_id: goal.id,
                        status,
                        active_ms: goal_view.active_ms,
                        report,
                    },
                )
                .await?;
            Self::release_goal_workspace(leases, goal.id, workspace_identity);
            goal_status = status;
        }
        if !matches!(
            run.status,
            crate::session::RunStatus::Finished
                | crate::session::RunStatus::Failed
                | crate::session::RunStatus::Cancelled
        ) {
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

    /// A pause requested while no Run is open stops the Run before it is admitted.
    fn cancel_if_paused(&self, cancellation: &RunCancellation) {
        if self
            .steering
            .as_ref()
            .is_some_and(RunSteering::pause_requested)
        {
            cancellation.cancel();
        }
    }

    fn validate_request(&self, request: &RunRequest) -> Result<(), EngineError> {
        if (request.objective.trim().is_empty()
            && request.images.is_empty()
            && request.goal.id.is_none())
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
        {
            return Err(EngineError::InvalidRequest);
        }
        Ok(())
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

#[cfg(test)]
mod run_steering_tests {
    use super::*;

    #[test]
    fn run_steering_merges_waiting_notes_and_bounds_the_run() {
        let steering = RunSteering::new();
        assert_eq!(
            steering.offer_note(" \n".into()),
            Err(SteeringRefused::Empty)
        );
        assert_eq!(
            steering.offer_note("x".repeat(MAX_STEERING_NOTE_BYTES + 1)),
            Err(SteeringRefused::TooLarge)
        );
        // A note that can never fit with its label is TooLarge, not a full Run.
        assert_eq!(
            steering.offer_note("x".repeat(MAX_STEERING_NOTE_BYTES - MAX_STEERING_LABEL_BYTES + 1)),
            Err(SteeringRefused::TooLarge)
        );
        steering
            .offer_note("x".repeat(MAX_STEERING_NOTE_BYTES - MAX_STEERING_LABEL_BYTES))
            .expect("the largest single note fits an empty Run");
        assert!(steering.withdraw_note().is_some());
        steering.offer_note("first".into()).expect("first note");
        steering.offer_note("second".into()).expect("merged note");
        assert_eq!(
            steering.withdraw_note().as_deref(),
            Some("first\n\nsecond"),
            "withdrawal returns the whole merged text"
        );
        assert!(
            steering.withdraw_note().is_none(),
            "nothing is waiting twice"
        );

        steering.offer_note("one".into()).expect("note offered");
        steering.offer_note("two".into()).expect("note merged");
        assert_eq!(steering.take_note().as_deref(), Some("one\n\ntwo"));
        assert!(
            steering.withdraw_note().is_none(),
            "a note the Engine took cannot be withdrawn"
        );
        assert_eq!(steering.delivered_notes(), 0);
        steering.note_recorded();
        assert_eq!(steering.delivered_notes(), 1);

        let taken =
            "x".repeat(MAX_STEERING_NOTE_BYTES - "one\n\ntwo".len() - 2 * MAX_STEERING_LABEL_BYTES);
        steering
            .offer_note(taken.clone())
            .expect("fills the Run bound");
        assert_eq!(steering.take_note(), Some(taken));
        steering.note_recorded();
        assert_eq!(
            steering.offer_note("y".into()),
            Err(SteeringRefused::RunFull)
        );
        steering.begin_run();
        steering
            .offer_note("x".repeat(MAX_STEERING_NOTE_BYTES - MAX_STEERING_LABEL_BYTES))
            .expect("a new Run has its own byte quota");
        assert!(steering.take_note().is_some());
        steering.note_recorded();
        assert_eq!(
            steering.offer_note("y".into()),
            Err(SteeringRefused::RunFull)
        );

        assert!(steering.pause());
        assert!(!steering.pause(), "only the first pause request is new");
        assert!(steering.pause_requested());
    }

    #[test]
    fn run_steering_charges_a_label_per_note_and_refuses_the_next_tiny_note() {
        let steering = RunSteering::new();
        let mut taken = 0;
        while steering.offer_note("n".into()).is_ok() {
            assert!(steering.take_note().is_some());
            taken += 1;
        }
        assert_eq!(
            taken,
            MAX_STEERING_NOTE_BYTES / (1 + MAX_STEERING_LABEL_BYTES)
        );
        assert_eq!(
            steering.offer_note("n".into()),
            Err(SteeringRefused::RunFull),
            "the next one-byte note exceeds the bound with its label"
        );
    }

    #[test]
    fn run_steering_take_reserves_quota_and_poisoned_lock_fails_closed() {
        let steering = RunSteering::new();
        steering
            .offer_note("x".repeat(MAX_STEERING_NOTE_BYTES - MAX_STEERING_LABEL_BYTES))
            .expect("within quota");
        assert!(steering.take_note().is_some());
        assert_eq!(
            steering.offer_note("unrecorded takes still count".into()),
            Err(SteeringRefused::RunFull)
        );

        let poisoned = RunSteering::new();
        let handle = poisoned.clone();
        let _ = std::thread::spawn(move || {
            let _guard = handle.state.lock().expect("unpoisoned");
            panic!("poison the steering lock");
        })
        .join();
        assert_eq!(
            poisoned.offer_note("x".into()),
            Err(SteeringRefused::Unavailable)
        );
        assert!(poisoned.take_note().is_none());
        assert_eq!(poisoned.delivered_notes(), 0);
    }
}

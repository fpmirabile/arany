use crate::provider::{AgentPhase, ImageAttachment, Provider, ProviderRequest};
use crate::session::input::{InputError, WorkspaceInputs};
use crate::session::{
    AgentRole, AgentRunId, CollaborationPolicy, CompiledContext, Event, GoalBudget, GoalId,
    GoalPhase, GoalPlanContract, GoalReport, GoalStatus, GoalUsage, MAX_STEERING_CONTEXT_BYTES,
    MAX_USER_MESSAGE_BYTES as MAX_OBJECTIVE_BYTES, RunConfig, RunId, RunView, SessionId,
};
use crate::store::{SessionRunLock, StateRoot, Store, StoreError, WorkspaceRunLock};
use crate::telemetry::{Telemetry, TraceOutcome};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;
use tokio::sync::watch;

mod compaction;
mod lifecycle;
mod progress;
mod run_loop;
pub use lifecycle::{
    continue_session, create_session, fork_session, list_sessions, rename_session, resume_session,
    set_session_defaults,
};
use lifecycle::{ensure_session_workspace, fork_with_store, lock_session};
pub use progress::{DisplayUpdate, RunPreview, RunProgress, RunUpdate};
use progress::{ProgressPublisher, append_observed};
use run_loop::{GoalRunBudget, RunControls, RunIdentity, RunLoop};

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

const OUTPUT_TOKEN_CAP: u32 = 4096;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
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

#[derive(Clone)]
pub struct RunCancellation {
    sender: watch::Sender<bool>,
    receiver: watch::Receiver<bool>,
}

impl RunCancellation {
    #[must_use]
    pub fn new() -> Self {
        let (sender, receiver) = watch::channel(false);
        Self { sender, receiver }
    }

    /// Returns true only for the first cancellation request.
    pub fn cancel(&self) -> bool {
        !self.sender.send_replace(true)
    }

    fn is_cancelled(&self) -> bool {
        *self.receiver.borrow()
    }

    /// Waits for sticky cancellation; dropping the waiter does not undo started work.
    pub async fn cancelled(&mut self) {
        if self.is_cancelled() {
            return;
        }
        while self.receiver.changed().await.is_ok() {
            if *self.receiver.borrow_and_update() {
                return;
            }
        }
    }
}

impl Default for RunCancellation {
    fn default() -> Self {
        Self::new()
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
    run_notes: usize,
    run_bytes: usize,
    recorded: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SteeringRefused {
    NoteWaiting,
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

    /// Holds one note until the Engine records it or the caller withdraws it.
    pub fn offer_note(&self, text: String) -> Result<(), SteeringRefused> {
        let mut state = self.lock().ok_or(SteeringRefused::Unavailable)?;
        if state.waiting.is_some() {
            return Err(SteeringRefused::NoteWaiting);
        }
        if text.trim().is_empty() {
            return Err(SteeringRefused::Empty);
        }
        if text.len() > MAX_OBJECTIVE_BYTES {
            return Err(SteeringRefused::TooLarge);
        }
        if state.run_notes >= crate::session::MAX_GOAL_STEERING_NOTES_PER_RUN
            || state.run_bytes + text.len() > MAX_OBJECTIVE_BYTES
        {
            return Err(SteeringRefused::RunFull);
        }
        state.waiting = Some(text);
        Ok(())
    }

    /// Returns true when a waiting note was removed before the Engine took it.
    pub fn withdraw_note(&self) -> bool {
        self.lock()
            .is_some_and(|mut state| state.waiting.take().is_some())
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
            state.run_notes = 0;
            state.run_bytes = 0;
        }
    }

    /// Takes the waiting note only if it still fits this Run's quota. Taking reserves the quota,
    /// so an accepted offer always stays deliverable within this Run.
    pub(crate) fn take_note(&self) -> Option<String> {
        let mut state = self.lock()?;
        let fits = state.waiting.as_ref().is_some_and(|text| {
            state.run_notes < crate::session::MAX_GOAL_STEERING_NOTES_PER_RUN
                && state.run_bytes + text.len() <= MAX_OBJECTIVE_BYTES
        });
        if !fits {
            return None;
        }
        let text = state.waiting.take()?;
        state.run_notes += 1;
        state.run_bytes += text.len();
        Some(text)
    }

    pub(crate) fn note_recorded(&self) {
        if let Some(mut state) = self.lock() {
            state.recorded += 1;
        }
    }
}

struct ProviderSelection {
    name: String,
    model: String,
    effort: Option<crate::provider::Effort>,
    concurrency: u8,
    custom_profile_provenance: Option<crate::provider::CustomProfileProvenance>,
    saved_api_account_id: Option<uuid::Uuid>,
    chatgpt_provenance: Option<crate::provider::ChatGptProvenance>,
    output_token_bound: crate::provider::OutputTokenBound,
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
    uncertain_active_ms: u64,
    success_criterion_satisfied: bool,
    success_state: crate::session::GoalSuccessState,
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

fn goal_report(
    view: &crate::session::SessionView,
    run: &RunView,
    goal: &crate::session::GoalView,
) -> GoalReport {
    let raw_summary = run
        .primary()
        .and_then(|primary| primary.summary.as_deref())
        .filter(|summary| !summary.is_empty())
        .unwrap_or(match run.status {
            crate::session::RunStatus::Finished => "Goal completed.",
            crate::session::RunStatus::Failed => "Goal failed before completion.",
            _ => "Goal did not reach a terminal Run boundary.",
        });
    let mut report = GoalReport {
        summary: String::new(),
        changes: Vec::new(),
        evidence: Vec::new(),
        unresolved: Vec::new(),
    };
    let mut omitted_facts = false;
    let uncertain_calls = goal.unobserved_provider_calls.len();
    if uncertain_calls > 0 {
        push_goal_report_item(
            &mut report,
            GoalReportSection::Unresolved,
            &format!(
                "{} reserved Provider call(s) have no committed outcome; their usage remains unknown and charged.",
                uncertain_calls
            ),
            &mut omitted_facts,
        );
    }
    if goal.token_usage_unknown {
        push_goal_report_item(
            &mut report,
            GoalReportSection::Unresolved,
            "Provider token usage is incomplete.",
            &mut omitted_facts,
        );
    }
    if goal.uncertain_active_ms > 0 {
        push_goal_report_item(
            &mut report,
            GoalReportSection::Unresolved,
            &format!(
                "{} ms of active time were conservatively charged after interruption.",
                goal.uncertain_active_ms
            ),
            &mut omitted_facts,
        );
    }
    let mut cache_rejected = false;
    let mut cache_failed = false;
    let mut cache_uncertain = false;
    for task_run in view
        .runs
        .iter()
        .filter(|task_run| goal.run_ids.contains(&task_run.id))
    {
        for tool in &task_run.tools {
            if let Some(observation) = tool.observation.as_ref() {
                match observation.cache_update {
                    crate::tools::CacheUpdate::Rejected => cache_rejected = true,
                    crate::tools::CacheUpdate::Failed => cache_failed = true,
                    crate::tools::CacheUpdate::Uncertain => cache_uncertain = true,
                    crate::tools::CacheUpdate::NotApplicable
                    | crate::tools::CacheUpdate::NotAttempted
                    | crate::tools::CacheUpdate::Published => {}
                }
            }
        }
    }
    if cache_rejected {
        push_goal_report_item(
            &mut report,
            GoalReportSection::Unresolved,
            "One or more Commands completed, but the build cache update was rejected; the previous cache was kept.",
            &mut omitted_facts,
        );
    }
    if cache_failed {
        push_goal_report_item(
            &mut report,
            GoalReportSection::Unresolved,
            "One or more Commands completed, but the build cache update failed; the previous cache was kept.",
            &mut omitted_facts,
        );
    }
    if cache_uncertain {
        push_goal_report_item(
            &mut report,
            GoalReportSection::Unresolved,
            "One or more Commands have an uncertain build cache publication; inspect Session history before retrying.",
            &mut omitted_facts,
        );
    }
    // The serialized-size loop below is the only truncation; it starts from the full summary.
    let mut summary_bytes = raw_summary.len();
    let mut summary_notice_added = false;
    report.summary = raw_summary.to_owned();
    while !goal_report_fits_with_omission(&mut report) {
        summary_bytes = summary_bytes.saturating_sub(128);
        if summary_bytes < raw_summary.len() && !summary_notice_added {
            let summary = std::mem::take(&mut report.summary);
            push_goal_report_item(
                &mut report,
                GoalReportSection::Unresolved,
                "The primary summary was shortened to fit the durable Goal report.",
                &mut omitted_facts,
            );
            report.summary = summary;
            summary_notice_added = true;
        }
        report.summary = truncate_utf8(raw_summary, summary_bytes).to_owned();
        assert!(summary_bytes > 0 || goal_report_fits_with_omission(&mut report));
    }
    let protected_unresolved_count = report.unresolved.len();
    for task_run in view
        .runs
        .iter()
        .filter(|task_run| goal.run_ids.contains(&task_run.id))
    {
        for tool in &task_run.tools {
            let call = &tool.intent.call;
            let label = goal_report_call_label(call);
            let Some(observation) = tool.observation.as_ref() else {
                if matches!(
                    task_run.status,
                    crate::session::RunStatus::Cancelled
                        | crate::session::RunStatus::Failed
                        | crate::session::RunStatus::Interrupted
                ) {
                    push_goal_report_item(
                        &mut report,
                        GoalReportSection::Unresolved,
                        &format!("{label} has no committed Tool outcome."),
                        &mut omitted_facts,
                    );
                }
                continue;
            };
            match observation.disposition {
                crate::tools::ToolDisposition::Succeeded
                    if observation.attested_for(&tool.intent) => {}
                crate::tools::ToolDisposition::Denied
                | crate::tools::ToolDisposition::Failed
                | crate::tools::ToolDisposition::Conflict => push_goal_report_item(
                    &mut report,
                    GoalReportSection::Unresolved,
                    &format!("{label} failed; its effect was not recorded as successful."),
                    &mut omitted_facts,
                ),
                crate::tools::ToolDisposition::Limit => {
                    let timeout_limit = match call {
                        crate::tools::ToolCall::Command { .. } => {
                            let requested = goal
                                .plan_contract
                                .as_ref()
                                .and_then(|contract| contract.command_timeout_ms)
                                .unwrap_or(goal.resource_settings.default_command_timeout_ms);
                            (requested > goal.resource_settings.max_command_timeout_ms).then_some((
                                requested,
                                goal.resource_settings.max_command_timeout_ms,
                            ))
                        }
                        _ => None,
                    };
                    let fact = timeout_limit.map_or_else(
                        || format!("{label} failed; its effect was not recorded as successful."),
                        |(requested, maximum)| {
                            crate::tools::types::command_timeout_limit_message(requested, maximum)
                        },
                    );
                    push_goal_report_item(
                        &mut report,
                        GoalReportSection::Unresolved,
                        &fact,
                        &mut omitted_facts,
                    );
                }
                crate::tools::ToolDisposition::Uncertain
                | crate::tools::ToolDisposition::Cancelled => {
                    if observation.cache_update != crate::tools::CacheUpdate::Uncertain {
                        push_goal_report_item(
                            &mut report,
                            GoalReportSection::Unresolved,
                            &format!(
                                "{label} has an uncertain or cancelled outcome; inspect Workspace state before retrying."
                            ),
                            &mut omitted_facts,
                        );
                    }
                }
                crate::tools::ToolDisposition::Succeeded => push_goal_report_item(
                    &mut report,
                    GoalReportSection::Unresolved,
                    &format!("{label} reported success without a valid Guard attestation."),
                    &mut omitted_facts,
                ),
            }
        }
    }
    for task_run in view
        .runs
        .iter()
        .filter(|task_run| goal.run_ids.contains(&task_run.id))
    {
        for tool in &task_run.tools {
            let Some(observation) = tool.observation.as_ref() else {
                continue;
            };
            if observation.disposition != crate::tools::ToolDisposition::Succeeded
                || !observation.attested_for(&tool.intent)
            {
                continue;
            }
            let call = &tool.intent.call;
            match call {
                crate::tools::ToolCall::Write { path, .. }
                | crate::tools::ToolCall::Edit { path, .. } => {
                    push_goal_report_item(
                        &mut report,
                        GoalReportSection::Changes,
                        &format!("{} {path}.", call.label()),
                        &mut omitted_facts,
                    );
                }
                crate::tools::ToolCall::Mkdir { path } => {
                    push_goal_report_item(
                        &mut report,
                        GoalReportSection::Changes,
                        &format!("Created directory {path}."),
                        &mut omitted_facts,
                    );
                }
                crate::tools::ToolCall::ApplyChangeset { changeset, .. } => {
                    for change in &changeset.files {
                        let action = if change.after.is_none() {
                            "removed"
                        } else if change.before.is_none() {
                            "created"
                        } else {
                            "updated"
                        };
                        push_goal_report_item(
                            &mut report,
                            GoalReportSection::Changes,
                            &format!("Command {action} {}.", change.path),
                            &mut omitted_facts,
                        );
                    }
                }
                _ => {}
            }
            push_goal_report_item(
                &mut report,
                GoalReportSection::Evidence,
                &goal_report_evidence(call, &observation.output),
                &mut omitted_facts,
            );
        }
    }
    if omitted_facts {
        append_goal_report_omission(&mut report, protected_unresolved_count);
    }
    report
}

#[cfg(test)]
mod goal_report_tests {
    use super::*;
    use crate::session::{AgentStatus, AgentView, GoalPhase, GoalView, RunStatus, ToolView};
    use crate::tools::{
        CacheUpdate, EffectIntent, GuardReceipt, ToolCall, ToolDisposition, ToolLimits,
    };
    use uuid::Uuid;

    #[test]
    fn bounded_report_preserves_usage_uncertainty_ahead_of_many_tool_facts() {
        let session_id = SessionId::new();
        let goal_id = GoalId::new();
        let run_id = RunId::new();
        let command_intent = EffectIntent {
            id: Uuid::now_v7(),
            run_id,
            agent_run_id: AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: ToolCall::Command {
                program: "cargo".into(),
                args: vec!["check".into()],
                cwd: ".".into(),
            },
            limits: ToolLimits::default(),
            expires_at_ms: 1,
            use_count: 1,
        };
        let command_observation = crate::tools::ToolObservation {
            guard: Some(GuardReceipt {
                contract_version: 1,
                intent_digest: command_intent.digest(),
                enforcement_digest: command_intent.enforcement_digest,
                limits: command_intent.limits.clone(),
            }),
            intent: command_intent.clone(),
            disposition: ToolDisposition::Succeeded,
            output: serde_json::json!({
                "exit_code": 0,
                "stdout": "",
                "stderr": "",
                "changeset": {"files": []}
            })
            .to_string(),
            cache_update: CacheUpdate::Failed,
        };
        let mut tools = vec![ToolView {
            intent: command_intent,
            observation: Some(command_observation),
            review_call_index: None,
            finished_sequence: None,
        }];
        tools.extend((0..15).map(|index| {
            let intent = EffectIntent {
                id: Uuid::now_v7(),
                run_id,
                agent_run_id: AgentRunId::new(),
                policy_digest: [1; 32],
                enforcement_digest: [2; 32],
                workspace_device: 1,
                workspace_inode: 2,
                call: ToolCall::Read {
                    path: format!("src/{index}-{}", "x".repeat(480)),
                    offset: 0,
                    limit: 4096,
                },
                limits: ToolLimits::default(),
                expires_at_ms: 1,
                use_count: 1,
            };
            let observation = crate::tools::ToolObservation {
                guard: Some(GuardReceipt {
                    contract_version: 1,
                    intent_digest: intent.digest(),
                    enforcement_digest: intent.enforcement_digest,
                    limits: intent.limits.clone(),
                }),
                intent: intent.clone(),
                disposition: ToolDisposition::Succeeded,
                output: serde_json::json!({
                    "sha256": crate::tools::types::hex_digest(b"evidence")
                })
                .to_string(),
                cache_update: crate::tools::CacheUpdate::NotApplicable,
            };
            ToolView {
                intent,
                observation: Some(observation),
                review_call_index: None,
                finished_sequence: None,
            }
        }));
        let run = RunView {
            id: run_id,
            objective: "Report bounded evidence".into(),
            images: Vec::new(),
            config: None,
            agents: vec![AgentView {
                id: AgentRunId::new(),
                role: AgentRole::Primary,
                ordinal: 0,
                objective: None,
                summary: Some("A concise result".into()),
                result: Some("Done".into()),
                provider_calls: Vec::new(),
                status: AgentStatus::Finished,
            }],
            assistant_message: Some("Done".into()),
            status: RunStatus::Finished,
            accepted_sequence: 1,
            finished_sequence: Some(2),
            tools,
            tool_compaction_summary: None,
            tool_compaction_covered_count: 0,
            display: Vec::new(),
            steering_notes: Vec::new(),
        };
        let goal = GoalView {
            id: goal_id,
            objective: "Report bounded evidence".into(),
            workspace_identity: (1, 2),
            budget: GoalBudget::default(),
            phase: GoalPhase::Implementation,
            autonomous: false,
            resource_settings: crate::tools::ToolResourceSettings::default(),
            status: GoalStatus::Running,
            run_ids: vec![run_id],
            active_run: None,
            plan_contract: None,
            success_state: crate::session::GoalSuccessState::NoContract,
            success_criterion_satisfied: false,
            awaiting_plan_contract: false,
            active_ms: 0,
            uncertain_active_ms: 0,
            reserved_active_run: None,
            reserved_active_ms: None,
            compaction_call_reserved: None,
            uncertain_compaction_calls: 0,
            provider_calls: 1,
            unobserved_provider_calls: Vec::new(),
            known_tokens: 0,
            token_usage_unknown: true,
            report: None,
            started_sequence: 0,
            finished_sequence: None,
        };
        let view = crate::session::SessionView {
            id: session_id,
            title: "Report bounded evidence".into(),
            title_is_explicit: false,
            inherited_title: None,
            workspace_identity: Some((1, 2)),
            defaults: crate::session::SessionDefaults::default(),
            created_sequence: 0,
            last_sequence: 2,
            lineage: None,
            runs: vec![run.clone()],
            compactions: Vec::new(),
            goals: vec![goal.clone()],
        };

        let report = goal_report(&view, &run, &goal);
        assert!(report.evidence.len() < 17, "report retains its byte bound");
        assert!(
            report.evidence.len() > 1,
            "report keeps canonical tool evidence"
        );
        assert!(
            report
                .evidence
                .iter()
                .any(|item| item == "Command cargo exited 0."),
            "successful Command evidence survives a cache update failure"
        );
        assert!(report.unresolved.iter().any(|item| {
            item.contains("build cache update failed") && item.contains("previous cache was kept")
        }));
        assert!(
            report
                .unresolved
                .iter()
                .any(|item| item.contains("token usage is incomplete"))
        );
        assert!(
            serde_json::to_vec(&report).unwrap().len() <= crate::session::MAX_GOAL_REPORT_BYTES
        );

        let mut late_run = run.clone();
        let late_failed_command = late_run.tools.remove(0);
        let failed_cache_observation = late_failed_command
            .observation
            .as_ref()
            .expect("attested Command result");
        assert!(failed_cache_observation.valid());
        assert!(failed_cache_observation.attested_for(&late_failed_command.intent));
        late_run.tools.truncate(10);
        let mut late_rejected_command = late_failed_command.clone();
        late_run.tools.push(late_failed_command);
        late_rejected_command.intent.id = Uuid::now_v7();
        let rejected_digest = late_rejected_command.intent.digest();
        let rejected_observation = late_rejected_command
            .observation
            .as_mut()
            .expect("attested Command result");
        rejected_observation.intent = late_rejected_command.intent.clone();
        rejected_observation
            .guard
            .as_mut()
            .expect("Guard receipt")
            .intent_digest = rejected_digest;
        rejected_observation.cache_update = CacheUpdate::Rejected;
        assert!(rejected_observation.valid());
        assert!(rejected_observation.attested_for(&late_rejected_command.intent));
        late_run.tools.push(late_rejected_command);
        let uncertain_intent = EffectIntent {
            id: Uuid::now_v7(),
            run_id,
            agent_run_id: late_run.agents[0].id,
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: ToolCall::Command {
                program: "cargo".into(),
                args: vec!["test".into()],
                cwd: ".".into(),
            },
            limits: ToolLimits::default(),
            expires_at_ms: 1,
            use_count: 1,
        };
        let uncertain_observation = crate::tools::ToolObservation {
            guard: Some(GuardReceipt {
                contract_version: 1,
                intent_digest: uncertain_intent.digest(),
                enforcement_digest: uncertain_intent.enforcement_digest,
                limits: uncertain_intent.limits.clone(),
            }),
            intent: uncertain_intent.clone(),
            disposition: ToolDisposition::Uncertain,
            output: run.tools[0]
                .observation
                .as_ref()
                .expect("valid Command result")
                .output
                .clone(),
            cache_update: CacheUpdate::Uncertain,
        };
        assert!(uncertain_observation.valid());
        assert!(uncertain_observation.attested_for(&uncertain_intent));
        late_run.tools.push(ToolView {
            intent: uncertain_intent.clone(),
            observation: Some(uncertain_observation),
            review_call_index: None,
            finished_sequence: None,
        });
        assert_eq!(late_run.tools.len(), 13);
        late_run.agents[0].summary = Some("\u{1}".repeat(2_048));
        let mut late_view = view.clone();
        late_view.runs = vec![late_run.clone()];
        let late_report = goal_report(&late_view, &late_run, &goal);
        let late_report_bytes = serde_json::to_vec(&late_report).unwrap();
        assert!(
            late_report_bytes.len() > crate::session::MAX_GOAL_REPORT_BYTES * 3 / 4,
            "late cache outcomes exercise a near-capacity report"
        );
        assert!(late_report_bytes.len() <= crate::session::MAX_GOAL_REPORT_BYTES);
        assert!(late_report.summary.len() <= 2_048);
        assert!(
            late_report.summary.len() < 1_800,
            "JSON-escaped controls consume serialized report capacity"
        );
        assert!(
            late_report
                .summary
                .chars()
                .all(|character| character == '\u{1}')
        );
        late_report
            .validate()
            .expect("bounded report remains valid");
        assert!(late_report.unresolved.iter().any(|item| {
            item.contains("build cache update was rejected")
                && item.contains("previous cache was kept")
        }));
        assert!(late_report.unresolved.iter().any(|item| {
            item.contains("build cache update failed") && item.contains("previous cache was kept")
        }));
        assert!(late_report.unresolved.iter().any(|item| {
            item.contains("uncertain build cache publication") && item.contains("Session history")
        }));
        assert!(
            late_report
                .unresolved
                .iter()
                .any(|item| item == GOAL_REPORT_OMISSION),
            "omitted lower-priority facts remain visible: {late_report:?}"
        );
        assert!(
            late_report
                .unresolved
                .iter()
                .any(|item| { item.contains("primary summary was shortened") })
        );

        let mut byte_run = run.clone();
        byte_run.tools.truncate(13);
        let failed_intent = EffectIntent {
            id: Uuid::now_v7(),
            run_id,
            agent_run_id: byte_run.agents[0].id,
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: ToolCall::Read {
                path: "src/report-failure".into(),
                offset: 0,
                limit: 4096,
            },
            limits: ToolLimits::default(),
            expires_at_ms: 1,
            use_count: 1,
        };
        let failed_observation = crate::tools::ToolObservation {
            intent: failed_intent.clone(),
            disposition: ToolDisposition::Denied,
            output: String::new(),
            guard: None,
            cache_update: CacheUpdate::NotApplicable,
        };
        assert!(failed_observation.valid());
        assert!(!failed_observation.attested_for(&failed_intent));
        assert!(!failed_observation.stops_run());
        byte_run.tools.push(ToolView {
            observation: Some(failed_observation),
            intent: failed_intent,
            review_call_index: None,
            finished_sequence: None,
        });
        assert_eq!(byte_run.tools.len(), 14);
        byte_run.agents[0].summary = Some("x".repeat(1_800));
        let mut byte_view = view.clone();
        byte_view.runs = vec![byte_run.clone()];
        let byte_report = goal_report(&byte_view, &byte_run, &goal);
        byte_report
            .validate()
            .expect("byte-boundary Goal report remains valid");
        assert!(byte_report.unresolved.len() < 32);
        assert!(byte_report.unresolved.iter().any(|item| {
            item.contains("read src/report-failure failed")
                && item.contains("effect was not recorded as successful")
        }));
        assert!(
            byte_report
                .unresolved
                .iter()
                .any(|item| item == GOAL_REPORT_OMISSION)
        );
        assert!(byte_report.evidence.len() < 13);
        assert!(
            byte_report
                .evidence
                .iter()
                .any(|item| item == "Command cargo exited 0.")
        );
        assert!(
            serde_json::to_vec(&byte_report).unwrap().len()
                <= crate::session::MAX_GOAL_REPORT_BYTES
        );

        let mut saturation_runs = Vec::with_capacity(3);
        let mut saturation_run_ids = Vec::with_capacity(3);
        for (run_index, denied_count) in [10, 11, 12].into_iter().enumerate() {
            let run_id = RunId::new();
            let agent_run_id = AgentRunId::new();
            saturation_run_ids.push(run_id);
            let mut tools = Vec::with_capacity(denied_count + usize::from(run_index == 0));
            if run_index == 0 {
                let mut command = run.tools[0].clone();
                command.intent.id = Uuid::now_v7();
                command.intent.run_id = run_id;
                command.intent.agent_run_id = agent_run_id;
                let digest = command.intent.digest();
                let observation = command.observation.as_mut().expect("successful Command");
                observation.intent = command.intent.clone();
                observation
                    .guard
                    .as_mut()
                    .expect("Command Guard receipt")
                    .intent_digest = digest;
                observation.cache_update = CacheUpdate::NotApplicable;
                assert!(observation.valid());
                assert!(observation.attested_for(&command.intent));
                tools.push(command);
            }
            for failure_index in 0..denied_count {
                let intent = EffectIntent {
                    id: Uuid::now_v7(),
                    run_id,
                    agent_run_id,
                    policy_digest: [1; 32],
                    enforcement_digest: [2; 32],
                    workspace_device: 1,
                    workspace_inode: 2,
                    call: ToolCall::Read {
                        path: format!("src/{run_index}-{failure_index}"),
                        offset: 0,
                        limit: 4096,
                    },
                    limits: ToolLimits::default(),
                    expires_at_ms: 1,
                    use_count: 1,
                };
                let observation = crate::tools::ToolObservation {
                    intent: intent.clone(),
                    disposition: ToolDisposition::Denied,
                    output: String::new(),
                    guard: None,
                    cache_update: CacheUpdate::NotApplicable,
                };
                assert!(observation.valid());
                assert!(!observation.attested_for(&intent));
                tools.push(ToolView {
                    observation: Some(observation),
                    intent,
                    review_call_index: None,
                    finished_sequence: None,
                });
            }
            let mut saturation_run = run.clone();
            saturation_run.id = run_id;
            saturation_run.agents[0].id = agent_run_id;
            saturation_run.tools = tools;
            saturation_runs.push(saturation_run);
        }
        let mut saturation_goal = goal.clone();
        saturation_goal.run_ids = saturation_run_ids;
        let mut saturation_view = view.clone();
        saturation_view.runs = saturation_runs;
        assert_eq!(
            saturation_view
                .runs
                .iter()
                .flat_map(|task_run| &task_run.tools)
                .filter(|tool| {
                    tool.observation.as_ref().is_some_and(|observation| {
                        observation.disposition == ToolDisposition::Denied
                    })
                })
                .count(),
            33
        );
        let saturation_report =
            goal_report(&saturation_view, &saturation_view.runs[0], &saturation_goal);
        saturation_report
            .validate()
            .expect("item-boundary Goal report remains valid");
        assert_eq!(saturation_report.unresolved.len(), 32);
        assert!(
            saturation_report
                .unresolved
                .iter()
                .any(|item| item == GOAL_REPORT_OMISSION)
        );
        assert!(
            saturation_report
                .evidence
                .iter()
                .any(|item| item == "Command cargo exited 0.")
        );
        assert!(
            serde_json::to_vec(&saturation_report).unwrap().len()
                <= crate::session::MAX_GOAL_REPORT_BYTES
        );

        let mut timeout_intent = run.tools[0].intent.clone();
        timeout_intent.id = Uuid::now_v7();
        timeout_intent.limits.runtime_ms = 2_000;
        timeout_intent.limits.cache_bytes = 0;
        timeout_intent.limits.cache_candidate_bytes = 0;
        timeout_intent.limits.cache_transfer_ms = 0;
        let timeout_observation = crate::tools::ToolObservation {
            intent: timeout_intent.clone(),
            disposition: ToolDisposition::Limit,
            output: crate::tools::types::command_timeout_limit_message(2_000, 1_000),
            guard: None,
            cache_update: CacheUpdate::NotApplicable,
        };
        assert!(timeout_observation.valid());
        let mut timeout_run = run.clone();
        timeout_run.tools = vec![ToolView {
            intent: timeout_intent,
            observation: Some(timeout_observation),
            review_call_index: None,
            finished_sequence: None,
        }];
        let mut timeout_goal = goal.clone();
        timeout_goal.resource_settings.default_command_timeout_ms = 1_000;
        timeout_goal.resource_settings.max_command_timeout_ms = 1_000;
        timeout_goal.plan_contract = Some(crate::session::GoalPlanContract {
            mission: "Run the configured command within pinned limits".into(),
            paths: Vec::new(),
            commands: vec!["cargo".into()],
            resource_class: crate::tools::ToolResourceClass::Default,
            command_timeout_ms: Some(2_000),
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            success_criterion: crate::session::GoalSuccessCriterion::WorkspaceFileContent {
                path: "src/main.rs".into(),
                content: "expected".into(),
            },
            budget: GoalBudget::default(),
        });
        let mut timeout_view = view.clone();
        timeout_view.runs = vec![timeout_run.clone()];
        timeout_view.goals = vec![timeout_goal.clone()];
        let timeout_report = goal_report(&timeout_view, &timeout_run, &timeout_goal);
        assert!(timeout_report.unresolved.iter().any(|item| {
            item.contains("2000 ms")
                && item.contains("pinned max 1000 ms")
                && item.contains("Lower request or change /settings")
        }));
    }
}

#[cfg(all(test, unix))]
mod goal_workspace_lease_tests {
    use super::*;
    use crate::provider::{ProviderError, ProviderResponse};
    use std::time::Duration;

    struct NeverProvider;

    impl Provider for NeverProvider {
        fn profile_name(&self) -> &str {
            "scripted"
        }

        fn model_name(&self) -> &str {
            "lease-test"
        }

        fn max_concurrent_calls(&self) -> u8 {
            1
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
        let mut engine = Engine::open(state, NeverProvider).expect("Engine");
        let session_id = SessionId::new();
        let goal_id = GoalId::new();
        let workspace_identity = (7, 11);
        engine
            .store
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
        let (acknowledgment_entered, release_acknowledgment) = engine
            .store
            .gate_goal_append_acknowledgment(session_id, |event| {
                matches!(event, Event::GoalStarted { .. } | Event::GoalResumed { .. })
            })
            .await
            .expect("install acknowledgment gate");
        let mut leases = WorkspaceLeases::new();
        Engine::<NeverProvider>::retain_goal_workspace(
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
            .store
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
            .store
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
            .store
            .append(
                session_id,
                Event::SessionStarted {
                    title: "Lost link acknowledgment".into(),
                    workspace_identity,
                },
            )
            .await
            .expect("Session start");
        let (link_entered, release_link) = engine
            .store
            .gate_goal_append_acknowledgment(session_id, |event| {
                matches!(event, Event::GoalRunLinked { .. })
            })
            .await
            .expect("install acknowledgment gate");
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
        engine: &mut Engine<NeverProvider>,
        workspace: &std::path::Path,
    ) -> (SessionId, GoalId) {
        let session_id = SessionId::new();
        let goal_id = GoalId::new();
        let workspace_identity = crate::tools::workspace_identity(workspace).expect("identity");
        engine
            .store
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
            .store
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
        engine
            .store
            .fail_next_goal_append(session_id, |event| {
                matches!(event, Event::GoalResumed { .. })
            })
            .await
            .expect("install append failure");
        match engine
            .run(resume_request(session_id, goal_id, &workspace))
            .await
        {
            Err(EngineError::Store(_)) => {}
            Err(error) => panic!("an uncommitted resume must report its Store error, not {error}"),
            Ok(_) => panic!("Run resumed without a committed GoalResumed"),
        }
        let events = engine
            .store
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
        let (resume_entered, release_resume) = engine
            .store
            .gate_goal_append_acknowledgment(session_id, |event| {
                matches!(event, Event::GoalResumed { .. })
            })
            .await
            .expect("install acknowledgment gate");
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
        engine
            .store
            .fail_next_goal_append(session_id, |event| {
                matches!(event, Event::GoalRunLinked { .. })
            })
            .await
            .expect("install append failure");
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

#[derive(Clone, Copy)]
enum GoalReportSection {
    Changes,
    Evidence,
    Unresolved,
}

const GOAL_REPORT_OMISSION: &str =
    "Some lower-priority Goal facts were omitted to fit the report limits.";

fn push_goal_report_item(
    report: &mut GoalReport,
    section: GoalReportSection,
    text: &str,
    omitted_facts: &mut bool,
) {
    let count = match section {
        GoalReportSection::Changes => report.changes.len(),
        GoalReportSection::Evidence => report.evidence.len(),
        GoalReportSection::Unresolved => report.unresolved.len(),
    };
    if count >= 32 {
        *omitted_facts = true;
        return;
    }
    let item = truncate_utf8(text, 512).to_owned();
    match section {
        GoalReportSection::Changes => report.changes.push(item),
        GoalReportSection::Evidence => report.evidence.push(item),
        GoalReportSection::Unresolved => report.unresolved.push(item),
    }
    if serde_json::to_vec(report).map_or(true, |bytes| {
        bytes.len() > crate::session::MAX_GOAL_REPORT_BYTES
    }) {
        match section {
            GoalReportSection::Changes => report.changes.pop(),
            GoalReportSection::Evidence => report.evidence.pop(),
            GoalReportSection::Unresolved => report.unresolved.pop(),
        };
        *omitted_facts = true;
    }
}

fn append_goal_report_omission(report: &mut GoalReport, protected_unresolved_count: usize) {
    while !goal_report_fits_with_omission(report) {
        if report.unresolved.len() >= 32 && report.unresolved.len() > protected_unresolved_count {
            report.unresolved.pop();
            continue;
        }
        if report.evidence.pop().is_some() || report.changes.pop().is_some() {
            continue;
        }
        if report.unresolved.len() > protected_unresolved_count {
            report.unresolved.pop();
            continue;
        }
        if !report.summary.is_empty() {
            let summary_bytes = report.summary.len().saturating_sub(128);
            report.summary = truncate_utf8(&report.summary, summary_bytes).to_owned();
            continue;
        }
        unreachable!("priority Goal report facts and omission notice fit their bounds");
    }
    report.unresolved.push(GOAL_REPORT_OMISSION.to_owned());
}

fn goal_report_fits_with_omission(report: &mut GoalReport) -> bool {
    if report.unresolved.len() >= 32 {
        return false;
    }
    report.unresolved.push(GOAL_REPORT_OMISSION.to_owned());
    let fits = serde_json::to_vec(report)
        .is_ok_and(|bytes| bytes.len() <= crate::session::MAX_GOAL_REPORT_BYTES);
    report.unresolved.pop();
    fits
}

fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn goal_report_call_label(call: &crate::tools::ToolCall) -> String {
    match call {
        crate::tools::ToolCall::Read { path, .. }
        | crate::tools::ToolCall::List { path }
        | crate::tools::ToolCall::Search { path, .. }
        | crate::tools::ToolCall::Mkdir { path }
        | crate::tools::ToolCall::Write { path, .. }
        | crate::tools::ToolCall::Edit { path, .. } => format!("{} {path}", call.label()),
        crate::tools::ToolCall::Command { program, .. } => format!("Command {program}"),
        crate::tools::ToolCall::ApplyChangeset { .. } => "Command changeset publication".into(),
        crate::tools::ToolCall::Skill { name, .. } => format!("Skill {name}"),
        crate::tools::ToolCall::McpList { server } => format!("MCP catalog {server}"),
        crate::tools::ToolCall::McpCall { server, tool, .. } => format!("MCP {server}.{tool}"),
    }
}

fn goal_report_evidence(call: &crate::tools::ToolCall, output: &str) -> String {
    match call {
        crate::tools::ToolCall::Command { program, .. } => {
            serde_json::from_str::<crate::tools::types::CommandResult>(output)
                .ok()
                .filter(crate::tools::types::CommandResult::valid)
                .map_or_else(
                    || format!("Command {program} returned an attested result."),
                    |result| {
                        format!(
                            "Command {program} exited {}.",
                            result
                                .exit_code
                                .map_or_else(|| "without a code".into(), |code| code.to_string())
                        )
                    },
                )
        }
        crate::tools::ToolCall::ApplyChangeset { changeset, .. } => {
            format!(
                "Guard attested publication of {} command file change(s).",
                changeset.files.len()
            )
        }
        crate::tools::ToolCall::Read { path, .. } => {
            serde_json::from_str::<serde_json::Value>(output)
                .ok()
                .and_then(|value| {
                    value
                        .get("sha256")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .map_or_else(
                    || format!("Guard attested a read of {path}."),
                    |digest| format!("Guard attested a read of {path} (SHA-256 {digest})."),
                )
        }
        crate::tools::ToolCall::McpCall { server, tool, .. } => {
            format!("Guard attested MCP call {server}.{tool}.")
        }
        _ => format!("Guard attested {}.", goal_report_call_label(call)),
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
            uncertain_active_ms: goal.uncertain_active_ms,
            success_criterion_satisfied: goal.success_criterion_satisfied,
            success_state: goal.success_state.clone(),
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
            uncertain_active_ms: 0,
            success_criterion_satisfied: false,
            success_state: crate::session::GoalSuccessState::NoContract,
        })
    }
}

pub struct Engine<P: Provider> {
    state: StateRoot,
    store: Store,
    provider: Arc<P>,
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

impl<P: Provider + 'static> Engine<P> {
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
                .store
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
        &mut self,
        session_id: SessionId,
        progress: Option<&ProgressPublisher>,
        event: Event,
    ) -> Result<u64, EngineError> {
        match append_observed(&self.store, progress, None, session_id, event.clone()).await {
            Ok(sequence) => Ok(sequence),
            // The Session lock is held, so a commit that lost its acknowledgment is the last Event.
            Err(error) => match self.store.load_session(session_id).await {
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

    pub fn open(state: StateRoot, provider: P) -> Result<Self, EngineError> {
        Self::open_with_telemetry(state, provider, Telemetry::disabled())
    }

    pub fn open_with_telemetry(
        state: StateRoot,
        provider: P,
        telemetry: Telemetry,
    ) -> Result<Self, EngineError> {
        let store = Store::open(state.try_clone()?)?;
        Ok(Self {
            state,
            store,
            provider: Arc::new(provider),
            telemetry,
            tools: None,
            workspace_permissions: None,
            tool_approvals: None,
            steering: None,
        })
    }

    pub async fn close(self) -> Result<(), EngineError> {
        self.store.close().await?;
        Ok(())
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
            &self.store,
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
        let selection = self.select_provider()?;
        self.validate_request(&request)?;
        if self.tools.is_some() && selection.custom_profile_provenance.is_some() {
            return Err(EngineError::InvalidRequest);
        }
        if selection.custom_profile_provenance.is_some() && !request.images.is_empty() {
            return Err(EngineError::ImagesNotAdmitted);
        }
        let workspace_identity = crate::tools::workspace_identity(&request.workspace)?;
        let session_id = request.session_id.unwrap_or_default();
        let mut session_lock = self.lock_session(session_id)?;
        let history = if request.session_id.is_some() {
            Some(
                self.store
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
        let goal = resolve_goal_admission(&request, history.as_ref(), continuation)?;
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
        // Run start covers the Goal lifecycle, the first action's envelope and the image message envelope; each later action admits its own.
        let previews = progress.as_ref().is_some_and(RunProgress::previews_enabled);
        let event_slots =
            13 + if previews {
                crate::session::MAX_DISPLAY_CHUNKS
            } else {
                0
            } + usize::from(request.session_id.is_none());
        self.store.admit_run_start(session_id, event_slots).await?;
        let provisional_workspace_lock = if goal.phase == crate::session::GoalPhase::Implementation
            && self.tools.as_ref().is_some_and(|(config, _)| config.write)
        {
            self.acquire_goal_workspace(leases, goal.id, workspace_identity)?
        } else {
            None
        };
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
            &objective,
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
        if selection.custom_profile_provenance.is_some() && !context.images.is_empty() {
            return Err(EngineError::ImagesNotAdmitted);
        }
        let context_ended = SystemTime::now();
        let provider_concurrency = if budget.max_tokens.is_some() {
            1
        } else {
            selection.concurrency
        };
        let compaction_provider_name = selection.name.clone();
        let compaction_chatgpt_provenance = selection.chatgpt_provenance.clone();
        let compaction_output_token_bound = selection.output_token_bound;
        let config = RunConfig {
            provider: selection.name,
            model: selection.model.clone(),
            effort: selection.effort,
            custom_profile_provenance: selection.custom_profile_provenance,
            saved_api_account_id: selection.saved_api_account_id,
            chatgpt_provenance: selection.chatgpt_provenance,
            output_token_bound: selection.output_token_bound,
            policy: request.policy,
            output_token_cap: OUTPUT_TOKEN_CAP,
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

        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }

        if request.session_id.is_none() {
            self.store
                .append(
                    session_id,
                    Event::SessionStarted {
                        title: request.title.unwrap_or_else(|| "New Session".into()),
                        workspace_identity: (workspace_device, workspace_inode),
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
        let trace_provider = crate::telemetry::TraceProvider::from_profile(&config.provider);
        let goal_trace = self
            .telemetry
            .begin_goal(session_id, goal.id, context_started);
        let publisher = if let Some(progress) = progress {
            // Reduces the open state that appends validate; the closed view would reject this Run's links.
            let view = self
                .store
                .load_open_view(session_id)
                .await?
                .ok_or(EngineError::InvalidHistory)?;
            Some(ProgressPublisher::new(view, progress, run_id))
        } else {
            None
        };
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        Self::retain_goal_workspace(
            leases,
            goal.id,
            workspace_identity,
            provisional_workspace_lock,
        );
        if goal.is_new {
            let sequence = self
                .append_goal_lifecycle(
                    session_id,
                    publisher.as_ref(),
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
            goal_trace.event_committed(sequence, crate::telemetry::TraceEventKind::GoalStarted);
        } else if goal.resume {
            let sequence = self
                .append_goal_lifecycle(
                    session_id,
                    publisher.as_ref(),
                    Event::GoalResumed { goal_id: goal.id },
                )
                .await?;
            goal_trace.event_committed(sequence, crate::telemetry::TraceEventKind::GoalResumed);
        }
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        // A link is an admission acknowledgment: a lost acknowledgment may still have committed it.
        let linked_sequence = self
            .append_goal_lifecycle(
                session_id,
                publisher.as_ref(),
                Event::GoalRunLinked {
                    goal_id: goal.id,
                    run_id,
                    phase: goal.phase,
                },
            )
            .await?;
        goal_trace.event_committed(
            linked_sequence,
            crate::telemetry::TraceEventKind::GoalRunLinked,
        );
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        let accepted_sequence = append_observed(
            &self.store,
            publisher.as_ref(),
            None,
            session_id,
            Event::MessageAccepted {
                run_id,
                text: objective.clone(),
                images: request.images,
            },
        )
        .await?;
        let started_sequence = append_observed(
            &self.store,
            publisher.as_ref(),
            None,
            session_id,
            Event::RunStarted { run_id, config },
        )
        .await?;
        let trace = self.telemetry.begin_goal_run(
            session_id,
            run_id,
            trace_provider,
            &goal_trace,
            Some((context_started, context_ended)),
        );
        trace.event_committed(
            accepted_sequence,
            crate::telemetry::TraceEventKind::MessageAccepted,
        );
        trace.event_committed(
            started_sequence,
            crate::telemetry::TraceEventKind::RunStarted,
        );
        append_observed(
            &self.store,
            publisher.as_ref(),
            Some(&trace),
            session_id,
            Event::AgentSpawned {
                run_id,
                agent_run_id,
                role: AgentRole::Primary,
                ordinal: 0,
                objective: None,
            },
        )
        .await?;

        let provider_request = ProviderRequest {
            run_id,
            agent_run_id,
            phase: AgentPhase::RootPlan,
            collaboration: request.policy,
            model: selection.model,
            instructions: inputs.instructions.map(|(_, snapshot)| snapshot.content),
            objective,
            images: context.images,
            includes: inputs
                .includes
                .into_iter()
                .map(|snapshot| snapshot.model_include())
                .collect(),
            history: context.history,
            context_summary: context.summary,
            goal_budget: Some(budget),
            goal_plan_contract: goal.plan_contract.clone(),
            goal_context_facts: None,
            child_results: Vec::new(),
            steering_notes: Vec::new(),
            max_output_tokens: OUTPUT_TOKEN_CAP,
            tools: tool_context,
        };
        if let Some(steering) = &self.steering {
            steering.begin_run();
        }
        let goal_run_budget = Arc::new(GoalRunBudget::new(
            &goal,
            budget.max_provider_calls,
            budget.max_tokens,
        ));
        RunLoop::new(
            &self.store,
            Arc::clone(&self.provider),
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
                provider_name: compaction_provider_name,
                chatgpt_provenance: compaction_chatgpt_provenance,
                output_token_bound: compaction_output_token_bound,
                cancellation,
                tools: tools.as_ref(),
                approvals: self.tool_approvals.as_ref(),
                goal_budget: Arc::clone(&goal_run_budget),
                steering: self.steering.clone(),
            },
            publisher.as_ref(),
            &trace,
        )
        .execute(provider_request)
        .await?;
        drop(publisher);
        let view = self
            .store
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
            crate::session::RunStatus::Finished => request
                .goal
                .finish_after_run
                .then_some(GoalStatus::Completed),
            crate::session::RunStatus::Failed => Some(GoalStatus::Failed),
            _ => None,
        };
        let mut goal_status = GoalStatus::Running;
        if let Some(status) = terminal {
            let report = goal_report(&view, &run, goal_view);
            append_observed(
                &self.store,
                None,
                Some(&trace),
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
        let trace_outcome = match run.status {
            crate::session::RunStatus::Finished => TraceOutcome::Finished,
            crate::session::RunStatus::Failed => TraceOutcome::Failed,
            crate::session::RunStatus::Cancelled => TraceOutcome::Cancelled,
            _ => return Err(EngineError::InvalidHistory),
        };
        trace.finish(trace_outcome);
        goal_trace.finish(goal_status, trace_outcome, goal_view);
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
            || request.objective.len() > MAX_OBJECTIVE_BYTES
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

    fn select_provider(&self) -> Result<ProviderSelection, EngineError> {
        let selection = ProviderSelection {
            name: self.provider.profile_name().to_owned(),
            model: self.provider.model_name().to_owned(),
            effort: self.provider.reasoning_effort(),
            concurrency: self.provider.max_concurrent_calls(),
            custom_profile_provenance: self.provider.custom_profile_provenance(),
            saved_api_account_id: self.provider.saved_api_account_id(),
            chatgpt_provenance: self.provider.chatgpt_provenance(),
            output_token_bound: self.provider.output_token_bound(),
        };
        if !(crate::provider::ProviderIdentity {
            profile: &selection.name,
            model: &selection.model,
            effort: selection.effort,
            concurrency: selection.concurrency,
            custom_profile_provenance: selection.custom_profile_provenance.as_ref(),
            saved_api_account_id: selection.saved_api_account_id,
            chatgpt_provenance: selection.chatgpt_provenance.as_ref(),
            output_token_bound: selection.output_token_bound,
        })
        .valid()
        {
            return Err(EngineError::InvalidRequest);
        }
        Ok(selection)
    }
}

#[cfg(test)]
mod run_steering_tests {
    use super::*;

    #[test]
    fn run_steering_quota_and_withdraw_race() {
        let steering = RunSteering::new();
        assert_eq!(
            steering.offer_note(" \n".into()),
            Err(SteeringRefused::Empty)
        );
        assert_eq!(
            steering.offer_note("x".repeat(MAX_OBJECTIVE_BYTES + 1)),
            Err(SteeringRefused::TooLarge)
        );
        steering.offer_note("first".into()).expect("first note");
        assert_eq!(
            steering.offer_note("second".into()),
            Err(SteeringRefused::NoteWaiting)
        );
        assert!(steering.withdraw_note());
        assert!(!steering.withdraw_note(), "nothing is waiting twice");

        steering.offer_note("taken".into()).expect("note offered");
        assert_eq!(steering.take_note().as_deref(), Some("taken"));
        assert!(
            !steering.withdraw_note(),
            "a note the Engine took cannot be withdrawn"
        );
        assert_eq!(steering.delivered_notes(), 0);
        steering.note_recorded();
        assert_eq!(steering.delivered_notes(), 1);

        for _ in 0..3 {
            steering.offer_note("n".into()).expect("within quota");
            assert!(steering.take_note().is_some());
            steering.note_recorded();
        }
        assert_eq!(
            steering.offer_note("fifth".into()),
            Err(SteeringRefused::RunFull)
        );

        steering.begin_run();
        steering
            .offer_note("x".repeat(MAX_OBJECTIVE_BYTES))
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
    fn run_steering_take_reserves_quota_and_poisoned_lock_fails_closed() {
        let steering = RunSteering::new();
        for _ in 0..crate::session::MAX_GOAL_STEERING_NOTES_PER_RUN {
            steering.offer_note("n".into()).expect("within quota");
            assert!(steering.take_note().is_some());
        }
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

use crate::provider::{AgentPhase, ImageAttachment, Provider, ProviderRequest};
use crate::session::input::{InputError, WorkspaceInputs};
use crate::session::{
    AgentRole, AgentRunId, CollaborationPolicy, CompiledContext, Event, GoalBudget, GoalId,
    GoalPhase, GoalPlanContract, GoalReport, GoalStatus, GoalUsage,
    MAX_USER_MESSAGE_BYTES as MAX_OBJECTIVE_BYTES, RunConfig, RunId, RunView, SessionId,
};
use crate::store::{SessionRunLock, StateRoot, Store, StoreError, WorkspaceRunLock};
use crate::telemetry::{Telemetry, TraceOutcome};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
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
    #[error("Session compaction limit reached; start a new Session")]
    CompactionLimit,
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
    plan_contract: Option<GoalPlanContract>,
    is_new: bool,
    resume: bool,
    calls_used: u32,
    known_tokens: u64,
    token_usage_unknown: bool,
    success_criterion_satisfied: bool,
    observations_before_run: Vec<crate::tools::ToolObservation>,
    active_ms: u64,
    runs_used: u8,
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
            crate::session::RunStatus::Cancelled => "Goal was cancelled.",
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
                | crate::tools::ToolDisposition::Conflict
                | crate::tools::ToolDisposition::Limit => push_goal_report_item(
                    &mut report,
                    GoalReportSection::Unresolved,
                    &format!("{label} failed; its effect was not recorded as successful."),
                    &mut omitted_facts,
                ),
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
            display: Vec::new(),
        };
        let goal = GoalView {
            id: goal_id,
            objective: "Report bounded evidence".into(),
            workspace_identity: (1, 2),
            budget: GoalBudget::default(),
            phase: GoalPhase::Implementation,
            autonomous: false,
            status: GoalStatus::Running,
            run_ids: vec![run_id],
            active_run: None,
            plan_contract: None,
            success_criterion_satisfied: false,
            awaiting_plan_contract: false,
            active_ms: 0,
            uncertain_active_ms: 0,
            active_time_reservation: None,
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
        });
        assert_eq!(late_run.tools.len(), crate::tools::MAX_TOOL_CALLS - 3);
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
        });
        assert_eq!(byte_run.tools.len(), crate::tools::MAX_TOOL_CALLS - 2);
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
                });
            }
            assert!(tools.len() <= crate::tools::MAX_TOOL_CALLS);
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
    async fn goal_workspace_lease_survives_dropped_start_acknowledgment() {
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
            .gate_goal_append_acknowledgment(session_id)
            .await
            .expect("install acknowledgment gate");
        let mut admission = Box::pin(engine.append_goal_lifecycle(
            goal_id,
            workspace_identity,
            Some(workspace_lock),
            session_id,
            None,
            Event::GoalStarted {
                goal_id,
                objective: "Keep the Workspace lease through Store acknowledgment".into(),
                workspace_identity,
                budget: GoalBudget::default(),
                phase: GoalPhase::Implementation,
                autonomous: false,
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

        let view = engine
            .store
            .load_view(session_id)
            .await
            .expect("canonical Session view")
            .expect("Session exists");
        assert!(view.active_goal().is_some_and(|goal| goal.id == goal_id));
        assert!(matches!(
            second_state.lock_workspace_in(&account_root, workspace_identity),
            Err(StoreError::WorkspaceBusy)
        ));
        let events = engine
            .store
            .load_session(session_id)
            .await
            .expect("canonical Events");
        assert_eq!(events.len(), 2, "no Run or effect was admitted");
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
            .and_then(|view| view.goals.last())
            .filter(|goal| goal.id == id)
            .ok_or(EngineError::GoalUnavailable)?;
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
            GoalStatus::Completed | GoalStatus::Failed | GoalStatus::Cancelled => {
                return Err(EngineError::GoalUnavailable);
            }
        };
        let active_ms = goal
            .active_ms
            .checked_add(if goal.status == GoalStatus::Interrupted {
                goal.active_time_reservation
                    .map_or(0, |(_, reserved_ms)| reserved_ms)
            } else {
                0
            })
            .ok_or(EngineError::GoalBudgetExhausted)?;
        let linked_run_is_unaccepted = history.is_some_and(|view| {
            goal.run_ids
                .last()
                .is_some_and(|run_id| !view.runs.iter().any(|run| run.id == *run_id))
        });
        Ok(GoalAdmission {
            id,
            objective: goal.objective.clone(),
            phase: goal.phase,
            budget: goal.budget.clone(),
            autonomous: goal.autonomous,
            plan_contract: goal.plan_contract.clone(),
            is_new: false,
            resume,
            calls_used: goal.provider_calls,
            known_tokens: goal.known_tokens,
            success_criterion_satisfied: goal.success_criterion_satisfied,
            observations_before_run: history
                .into_iter()
                .flat_map(|view| view.runs.iter())
                .filter(|run| goal.run_ids.contains(&run.id))
                .flat_map(|run| run.tools.iter())
                .filter_map(|tool| tool.observation.clone())
                .collect(),
            token_usage_unknown: goal.token_usage_unknown
                || (goal
                    .budget
                    .effective(goal.plan_contract.as_ref())
                    .max_tokens
                    .is_some()
                    && !goal.unobserved_provider_calls.is_empty()),
            active_ms,
            runs_used: u8::try_from(
                goal.run_ids
                    .len()
                    .saturating_sub(usize::from(linked_run_is_unaccepted)),
            )
            .unwrap_or(u8::MAX),
        })
    } else {
        if history
            .and_then(crate::session::SessionView::active_goal)
            .is_some()
        {
            return Err(EngineError::SessionBusy);
        }
        Ok(GoalAdmission {
            id: GoalId::new(),
            objective: request.objective.clone(),
            phase: request.goal.phase.ok_or(EngineError::InvalidRequest)?,
            budget: request
                .goal
                .budget
                .clone()
                .ok_or(EngineError::InvalidRequest)?,
            autonomous: request.goal.autonomous,
            plan_contract: None,
            is_new: true,
            resume: false,
            calls_used: 0,
            known_tokens: 0,
            token_usage_unknown: false,
            success_criterion_satisfied: false,
            observations_before_run: Vec::new(),
            active_ms: 0,
            runs_used: 0,
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
    workspace_locks: HashMap<(u64, u64), GoalWorkspaceLock>,
}

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
        &mut self,
        goal_id: GoalId,
        identity: (u64, u64),
    ) -> Result<Option<WorkspaceRunLock>, EngineError> {
        if let Some(held) = self.workspace_locks.get(&identity) {
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
        &mut self,
        goal_id: GoalId,
        identity: (u64, u64),
        lock: Option<WorkspaceRunLock>,
    ) {
        if let Some(lock) = lock {
            self.workspace_locks.insert(
                identity,
                GoalWorkspaceLock {
                    goal_id,
                    _lock: lock,
                },
            );
        }
    }

    fn release_goal_workspace(&mut self, goal_id: GoalId, identity: (u64, u64)) {
        if self
            .workspace_locks
            .get(&identity)
            .is_some_and(|held| held.goal_id == goal_id)
        {
            self.workspace_locks.remove(&identity);
        }
    }

    async fn append_goal_lifecycle(
        &mut self,
        goal_id: GoalId,
        workspace_identity: (u64, u64),
        workspace_lock: Option<WorkspaceRunLock>,
        session_id: SessionId,
        progress: Option<&ProgressPublisher>,
        event: Event,
    ) -> Result<u64, EngineError> {
        self.retain_goal_workspace(goal_id, workspace_identity, workspace_lock);
        match append_observed(&self.store, progress, None, session_id, event).await {
            Ok(sequence) => Ok(sequence),
            Err(error) => match self.store.load_view(session_id).await {
                Ok(Some(view)) if view.active_goal().is_some() => {
                    Err(EngineError::GoalAdmissionUncertain)
                }
                Ok(_) => {
                    self.release_goal_workspace(goal_id, workspace_identity);
                    Err(error)
                }
                Err(_) => Err(EngineError::GoalAdmissionUncertain),
            },
        }
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
            workspace_locks: HashMap::new(),
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
        let mut turn = self
            .run_turn(request, cancellation.clone(), progress.clone(), false)
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
            turn = match self
                .run_turn(next, cancellation.clone(), progress.clone(), true)
                .await
            {
                Ok(turn) => turn,
                // Cancellation before MessageAccepted admits no Run; the committed Finished Run keeps its outcome.
                Err(EngineError::CancelledBeforeStart) => break,
                Err(error) => return Err(error),
            };
        }
        if turn.exhausted() {
            return Err(EngineError::GoalBudgetExhausted);
        }
        Ok(turn.outcome)
    }

    async fn run_turn(
        &mut self,
        request: RunRequest,
        cancellation: RunCancellation,
        progress: Option<RunProgress>,
        continuation: bool,
    ) -> Result<RunTurn, EngineError> {
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
        if !budget.admits_next_run(GoalUsage {
            runs: usize::from(goal.runs_used),
            provider_calls: goal.calls_used,
            active_ms: goal.active_ms,
            known_tokens: goal.known_tokens,
            token_usage_unknown: goal.token_usage_unknown,
        }) {
            return Err(EngineError::GoalBudgetExhausted);
        }
        let token_limit = budget.max_tokens;
        let active_remaining_ms = budget.max_active_ms.saturating_sub(goal.active_ms);
        let mut event_budget = goal.budget.clone();
        event_budget.max_provider_calls = budget.max_provider_calls.saturating_sub(goal.calls_used);
        event_budget.max_runs = budget.max_runs.saturating_sub(goal.runs_used);
        event_budget.max_active_ms = active_remaining_ms;
        event_budget.max_tokens = token_limit;
        let event_slots = event_budget
            .event_slots(
                request.policy.max_children(),
                self.tools.is_some(),
                progress.as_ref().is_some_and(RunProgress::previews_enabled),
            )
            .ok_or(EngineError::InvalidRequest)?;
        self.store
            .admit_operation(
                session_id,
                event_slots.saturating_add(usize::from(request.session_id.is_none())),
            )
            .await?;
        let provisional_workspace_lock = if goal.phase == crate::session::GoalPhase::Implementation
            && self.tools.as_ref().is_some_and(|(config, _)| config.write)
        {
            self.acquire_goal_workspace(goal.id, workspace_identity)?
        } else {
            None
        };
        if let Some(permissions) = &self.workspace_permissions {
            permissions.verify(&request.workspace)?;
        }
        let tools = self
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
        let context = CompiledContext::compile(
            history.as_ref(),
            &objective,
            &request.images,
            &inputs,
            request.policy,
            tools
                .as_ref()
                .map_or(0, |_| crate::tools::MAX_TOOL_CONTEXT_BYTES),
        )
        .map_err(|_| EngineError::ContextTooLarge)?;
        if selection.custom_profile_provenance.is_some() && !context.images.is_empty() {
            return Err(EngineError::ImagesNotAdmitted);
        }
        let context_ended = SystemTime::now();
        let provider_concurrency = if token_limit.is_some() {
            1
        } else {
            selection.concurrency
        };
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
        if goal.is_new {
            let sequence = self
                .append_goal_lifecycle(
                    goal.id,
                    workspace_identity,
                    provisional_workspace_lock,
                    session_id,
                    publisher.as_ref(),
                    Event::GoalStarted {
                        goal_id: goal.id,
                        objective: objective.clone(),
                        workspace_identity,
                        budget: goal.budget.clone(),
                        phase: goal.phase,
                        autonomous: goal.autonomous,
                    },
                )
                .await?;
            goal_trace.event_committed(sequence, crate::telemetry::TraceEventKind::GoalStarted);
        } else if goal.resume {
            let sequence = self
                .append_goal_lifecycle(
                    goal.id,
                    workspace_identity,
                    provisional_workspace_lock,
                    session_id,
                    publisher.as_ref(),
                    Event::GoalResumed { goal_id: goal.id },
                )
                .await?;
            goal_trace.event_committed(sequence, crate::telemetry::TraceEventKind::GoalResumed);
        } else {
            self.retain_goal_workspace(goal.id, workspace_identity, provisional_workspace_lock);
        }
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        let linked_sequence = append_observed(
            &self.store,
            publisher.as_ref(),
            None,
            session_id,
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
        let active_segment_ms = active_remaining_ms.min(crate::session::MAX_GOAL_RUN_ACTIVE_MS);
        let active_reserved_sequence = append_observed(
            &self.store,
            publisher.as_ref(),
            None,
            session_id,
            Event::GoalActiveTimeReserved {
                goal_id: goal.id,
                run_id,
                reserved_ms: active_segment_ms,
            },
        )
        .await?;
        goal_trace.event_committed(
            active_reserved_sequence,
            crate::telemetry::TraceEventKind::GoalActiveTimeReserved,
        );
        let active_started = tokio::time::Instant::now();
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
            goal_budget: Some(budget.clone()),
            goal_plan_contract: goal.plan_contract.clone(),
            child_results: Vec::new(),
            max_output_tokens: OUTPUT_TOKEN_CAP,
            tools: tool_context,
        };
        let goal_run_budget = Arc::new(GoalRunBudget::new(
            &goal,
            &budget,
            active_segment_ms,
            active_started,
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
                cancellation,
                tools: tools.as_ref(),
                approvals: self.tool_approvals.as_ref(),
                goal_budget: Arc::clone(&goal_run_budget),
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
        let reserved_active_ms = view
            .goal(goal.id)
            .and_then(|goal| goal.active_time_reservation)
            .filter(|(reserved_run, _)| *reserved_run == run_id)
            .map(|(_, reserved_ms)| reserved_ms)
            .ok_or(EngineError::InvalidHistory)?;
        let active_ms = goal_run_budget.elapsed_active_ms().min(reserved_active_ms);
        append_observed(
            &self.store,
            None,
            Some(&trace),
            session_id,
            Event::GoalActiveTimeSettled {
                goal_id: goal.id,
                run_id,
                active_ms,
            },
        )
        .await?;
        // Settlement only moves the reservation into active time, so the settled Goal needs no second replay.
        let mut goal_view = view
            .goal(goal.id)
            .ok_or(EngineError::InvalidHistory)?
            .clone();
        goal_view.active_ms += active_ms;
        // An autonomous Goal ends only at its own stop conditions; a cancelled Run leaves it open to resume.
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
            crate::session::RunStatus::Cancelled => {
                (!goal.autonomous).then_some(GoalStatus::Cancelled)
            }
            _ => None,
        };
        let mut goal_status = GoalStatus::Running;
        if let Some(status) = terminal {
            let report = goal_report(&view, &run, &goal_view);
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
            self.release_goal_workspace(goal.id, workspace_identity);
            goal_status = status;
        }
        let trace_outcome = match run.status {
            crate::session::RunStatus::Finished => TraceOutcome::Finished,
            crate::session::RunStatus::Failed => TraceOutcome::Failed,
            crate::session::RunStatus::Cancelled => TraceOutcome::Cancelled,
            _ => return Err(EngineError::InvalidHistory),
        };
        trace.finish(trace_outcome);
        goal_trace.finish(
            goal_status,
            goal_view.provider_calls,
            goal_view.known_tokens,
            goal_view.token_usage_unknown,
            goal_view.active_ms,
            goal_view.uncertain_active_ms,
        );
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
            && request.goal.id.is_none())
            || request.objective.len() > MAX_OBJECTIVE_BYTES
            || !crate::provider::valid_image_collection(&request.images)
            || (request.session_id.is_some() && request.title.is_some())
            || request
                .title
                .as_ref()
                .is_some_and(|title| title.is_empty() || title.len() > 128)
            || request.policy.validate().is_err()
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

use super::hints::HintPublisher;
use super::ledger::{Action, ActiveReservationLimit, Admission, GoalLedger, duration_ms};
use super::{EngineError, RunCancellation, RunSteering, STORE_ACTION_OVERHEAD};
use crate::journal::Journal;
use crate::provider::{
    AgentPhase, Answer, MAX_REPORTED_INPUT_TOKENS, PreviewText, Provider, ProviderFailureClass,
    ProviderFailureReason, ProviderOutcome, ProviderRequest, ProviderResponse, RateLimitSlot,
    RateLimits, ResponsePreview, Usage,
};
use crate::session::{
    AgentDisposition, AgentRunId, CollaborationPolicy, DisplayCategory, DisplayChunk, Event,
    GoalPhase, MAX_ASSISTANT_MESSAGE_BYTES as MAX_PRIMARY_RESULT_BYTES, MAX_DISPLAY_CHUNK_BYTES,
    MAX_DISPLAY_CHUNKS, MAX_DISPLAY_COMMENTARY_ITEMS, MAX_DISPLAY_TEXT_BYTES, ProposalRefusal,
    ProposalRefusalCause, ProviderCallDisposition, ProviderCallRecord, RunDisposition, RunFailure,
    RunId, SessionId,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

mod children;
#[cfg(any(test, feature = "test-support"))]
pub(crate) use children::MAX_DELEGATION_DEADLINE;
pub(super) mod step;
mod stop;
mod tool_compaction;
mod worker;

use step::Grant;
pub(super) use step::hooks::LifecycleHook;
pub(super) use stop::HookRefusal;
use stop::SizingRefusal;
use stop::{PrimaryEnd, RunFacts, RunProgress, RunRefusal, Settlement, StepEnd, StopCause};

const TOOL_CLEANUP_OVERHEAD: Duration = Duration::from_secs(10);
const TOOL_REVIEW_DEADLINE: Duration = Duration::from_secs(20);

pub(super) enum CallResult {
    Response(ProviderResponse),
    Failed(CallFailure),
    Cancelled,
}

pub(super) enum CallFailure {
    Timeout,
    Unavailable {
        reason: Option<ProviderFailureReason>,
    },
    Rejected(Option<ProviderFailureReason>),
    InvalidResponse(Option<ProviderFailureReason>),
    OutputLimit,
    TaskPanic,
}

enum PrimaryStop {
    Continue,
    Finish,
}

struct PrimaryAction {
    phase: AgentPhase,
    budget_action: crate::session::BudgetAction,
}

enum PrimaryFlow {
    Continue,
    Settled,
    End(PrimaryEnd, usize),
}

struct PrimaryCall {
    result: CallResult,
    validity: Result<(), ProposalRefusalCause>,
    facts: RunFacts,
    budget_action: crate::session::BudgetAction,
}

struct DisplayCheckpoint {
    call_index: u32,
    phase: AgentPhase,
    chunks: usize,
    answer_offset: usize,
    commentary_offsets: Vec<usize>,
    remaining_commentary_bytes: usize,
    remaining_commentary_items: usize,
    limited: bool,
    limit_recorded: bool,
}

struct ActiveSegment {
    started: Instant,
    reserved_ms: u64,
    limit: ActiveReservationLimit,
}

impl ActiveSegment {
    fn deadline(&self) -> Instant {
        self.started + Duration::from_millis(self.reserved_ms)
    }

    fn active_ms(&self, sampled_at: Instant) -> u64 {
        duration_ms(sampled_at.saturating_duration_since(self.started))
    }
}

struct ActivePause {
    segment: ActiveSegment,
    paused_at: Instant,
    activity_started: Duration,
}

impl DisplayCheckpoint {
    fn filter(&self, text: &PreviewText) -> PreviewText {
        let mut remaining = self.remaining_commentary_bytes;
        let mut commentary = Vec::new();
        let mut limited =
            text.commentary_limited || text.commentary.len() > self.remaining_commentary_items;
        for item in text.commentary.iter().take(self.remaining_commentary_items) {
            let mut end = item.len().min(remaining);
            while !item.is_char_boundary(end) {
                end -= 1;
            }
            limited |= end < item.len();
            commentary.push(item[..end].to_owned());
            remaining -= end;
        }
        PreviewText {
            answer: text.answer.clone(),
            commentary,
            commentary_limited: limited,
            tentative: text.tentative,
        }
    }
}

pub(super) struct RunLoop<'a> {
    journal: &'a Journal,
    routing: super::routing::CallRouter<'a>,
    session_id: SessionId,
    run_id: RunId,
    goal_id: Option<crate::session::GoalId>,
    primary_id: AgentRunId,
    controls: LoopControls<'a>,
    ledger: &'a GoalLedger,
    supervisor: std::sync::Mutex<crate::supervisor::SupervisorState>,
    hints: Option<&'a HintPublisher>,
    definitions: std::sync::OnceLock<
        std::collections::BTreeMap<String, crate::tools::definitions::AgentDefinition>,
    >,
    active_time_floor: std::sync::atomic::AtomicBool,
    window_pressure_handled: std::sync::atomic::AtomicU64,
    approval_withdrawals:
        std::sync::Mutex<std::collections::HashMap<uuid::Uuid, crate::tools::ApprovalWithdrawal>>,
}

pub(super) struct LoopControls<'a> {
    pub(super) host_settings: &'a std::sync::Mutex<Option<super::HostSettings>>,
    pub(super) policy: CollaborationPolicy,
    pub(super) concurrency: u8,
    pub(super) goal_phase: GoalPhase,
    pub(super) cancellation: RunCancellation,
    pub(super) tools: Option<&'a crate::tools::ToolRuntime>,
    pub(super) approvals: Option<&'a crate::tools::ToolApprovals>,
    pub(super) steering: Option<RunSteering>,
}

pub(super) struct RunIdentity {
    pub(super) session_id: SessionId,
    pub(super) run_id: RunId,
    pub(super) goal_id: Option<crate::session::GoalId>,
    pub(super) primary_id: AgentRunId,
}

impl Drop for RunLoop<'_> {
    fn drop(&mut self) {
        crate::fault::dispose_latency(&self.journal.tool_latency, self.run_id);
        if let Some(steering) = &self.controls.steering {
            steering.end_run(self.session_id, self.run_id);
        }
    }
}

fn place_steering_note(input: &mut crate::provider::RunInput, text: String) {
    if let Some(feedback) = continuation_feedback(input) {
        feedback.steering_notes.push(text);
    } else if let Some(step) = input
        .tools
        .as_mut()
        .and_then(|tools| tools.steps.last_mut())
    {
        step.steering_notes.push(text);
    } else {
        input.steering_notes.push(text);
    }
}

impl<'a> RunLoop<'a> {
    pub(super) fn new(
        journal: &'a Journal,
        providers: crate::provider::AdmittedProviders,
        identity: RunIdentity,
        controls: LoopControls<'a>,
        ledger: &'a GoalLedger,
        hints: Option<&'a HintPublisher>,
    ) -> Result<Self, EngineError> {
        let supervisor = ledger.view(crate::supervisor::SupervisorState::from_committed)?;
        Ok(Self {
            routing: super::routing::CallRouter {
                providers,
                journal,
                ledger,
                publisher: hints,
                steering: controls.steering.clone(),
            },
            supervisor: std::sync::Mutex::new(supervisor),
            definitions: std::sync::OnceLock::new(),
            active_time_floor: std::sync::atomic::AtomicBool::new(false),
            window_pressure_handled: std::sync::atomic::AtomicU64::new(0),
            approval_withdrawals: Default::default(),
            journal,
            session_id: identity.session_id,
            run_id: identity.run_id,
            goal_id: identity.goal_id,
            primary_id: identity.primary_id,
            controls,
            ledger,
            hints,
        })
    }

    // Only a fork boundary hook runs without a Goal.
    pub(super) fn goal(&self) -> Result<crate::session::GoalId, EngineError> {
        self.goal_id.ok_or(EngineError::InvalidHistory)
    }

    fn ledger(&self) -> &GoalLedger {
        self.ledger
    }

    fn defer_approval_withdrawal(
        &self,
        intent_id: uuid::Uuid,
        withdrawal: crate::tools::ApprovalWithdrawal,
    ) -> Result<(), EngineError> {
        self.approval_withdrawals
            .lock()
            .map_err(|_| EngineError::CoordinatorFailed)?
            .insert(intent_id, withdrawal);
        Ok(())
    }

    fn commit_approval_withdrawal(&self, intent_id: uuid::Uuid) -> Result<(), EngineError> {
        if let Some(withdrawal) = self
            .approval_withdrawals
            .lock()
            .map_err(|_| EngineError::CoordinatorFailed)?
            .remove(&intent_id)
        {
            withdrawal.commit();
        }
        Ok(())
    }

    fn settle_result(&self, result: &CallResult) -> Result<(), EngineError> {
        let (input, output) = call_usage(result);
        self.ledger().settle(input, output)
    }

    fn append(&self, event: Event) -> step::BoxedStep<'_, ()> {
        Box::pin(self.append_inner(event))
    }

    async fn append_inner(&self, event: Event) -> Result<(), EngineError> {
        let writer = self.journal.writer().await;
        let fact = matches!(
            event,
            Event::AgentSpawned { .. } | Event::AgentFinished { .. } | Event::RunFinished { .. }
        )
        .then(|| event.clone());
        let committed = self
            .ledger
            .append_committed_in(&writer, self.session_id, event)
            .await?;
        self.record_supervisor_verdict(&writer, &committed).await?;
        if let (Some(steering), Some(fact)) = (&self.controls.steering, fact) {
            match fact {
                Event::AgentSpawned {
                    run_id,
                    agent_run_id,
                    role: crate::session::AgentRole::Child,
                    ..
                } => {
                    steering.register_child(super::ChildSteeringTarget {
                        session_id: self.session_id,
                        run_id,
                        agent_run_id,
                    });
                }
                Event::AgentFinished {
                    run_id,
                    agent_run_id,
                    ..
                } => {
                    steering.close_child(super::ChildSteeringTarget {
                        session_id: self.session_id,
                        run_id,
                        agent_run_id,
                    });
                }
                Event::RunFinished { run_id, .. } => steering.end_run(self.session_id, run_id),
                _ => {}
            }
        }
        Ok(())
    }

    async fn record_supervisor_verdict(
        &self,
        writer: &crate::journal::JournalWriter<'_>,
        committed: &crate::session::EventEnvelope,
    ) -> Result<(), EngineError> {
        let decision = self.ledger.view(|view| {
            self.supervisor
                .lock()
                .map_err(|_| EngineError::CoordinatorFailed)
                .map(|mut state| state.decide_verdict(view, committed))
        })??;
        match decision {
            crate::supervisor::SupervisorDecision::Continue => {}
            crate::supervisor::SupervisorDecision::Record {
                run_id,
                agent_run_id,
                verdict,
            } => {
                self.ledger
                    .append_committed_in(
                        writer,
                        self.session_id,
                        Event::SupervisorVerdictRecorded {
                            run_id,
                            agent_run_id,
                            verdict,
                        },
                    )
                    .await?;
            }
        }
        Ok(())
    }

    fn reserve_active_segment(
        &self,
        action_bound: Duration,
        action: crate::session::BudgetAction,
        actor: AgentRunId,
    ) -> Result<Admission<ActiveSegment>, EngineError> {
        Ok(self
            .ledger()
            .reserve_active(duration_ms(action_bound), action, Some(actor))?
            .map(|(started, reserved_ms, limit)| ActiveSegment {
                started,
                reserved_ms,
                limit,
            }))
    }

    fn settle_active_segment(&self, segment: ActiveSegment) -> Result<(), EngineError> {
        let sampled_at = Instant::now();
        self.settle_active_segment_at(segment, sampled_at, Some(sampled_at))
    }

    fn settle_active_segment_at(
        &self,
        segment: ActiveSegment,
        sampled_at: Instant,
        active_origin: Option<Instant>,
    ) -> Result<(), EngineError> {
        self.ledger()
            .settle_active(segment.active_ms(sampled_at), active_origin)
    }

    fn reserve_active_pause(&self) -> Result<Admission<ActivePause>, EngineError> {
        Ok(self
            .reserve_active_segment(
                Duration::ZERO,
                crate::session::BudgetAction::Tool,
                self.primary_id,
            )?
            .map(|segment| ActivePause {
                segment,
                paused_at: Instant::now(),
                activity_started: self.ledger().active_elapsed(),
            }))
    }

    fn settle_active_pause(
        &self,
        pause: ActivePause,
        resume_at: Option<Instant>,
    ) -> Result<(), EngineError> {
        let active_during = self
            .ledger()
            .active_elapsed()
            .saturating_sub(pause.activity_started);
        let sampled_at = pause.paused_at + active_during;
        let resume_at =
            resume_at.or_else(|| self.ledger().activity_in_progress().then(Instant::now));
        self.settle_active_segment_at(pause.segment, sampled_at, resume_at)
    }

    async fn reserve_provider_call(
        &self,
        agent_run_id: AgentRunId,
        action: Action,
        pending_ms: u64,
    ) -> Result<Admission<()>, EngineError> {
        let Admission::Reserved(reservation) = self.ledger().reserve(action, agent_run_id).await?
        else {
            return Ok(Admission::Exhausted);
        };
        // A review runs inside the Tool step's admission, which already counts its slots.
        if !matches!(action, Action::Review) {
            self.journal
                .admit(
                    self.session_id,
                    action.max_events_for_admitted(&self.routing.providers)
                        + self.ledger().verdicts(action),
                    action.tool_observations(),
                )
                .await?;
        }
        self.append(Event::GoalProviderCallReserved {
            goal_id: self.goal()?,
            run_id: self.run_id,
            agent_run_id,
            active_ceiling_ms: self.ledger().active_ceiling(pending_ms)?,
        })
        .await?;
        reservation.commit()?;
        Ok(Admission::Reserved(()))
    }

    fn primary_call(&self) -> Action {
        Action::ProviderCall {
            previews: self.hints.is_some_and(HintPublisher::previews_enabled),
            steering: self.controls.steering.is_some(),
        }
    }

    async fn record_steering_note(&self, request: &mut ProviderRequest) -> Result<(), EngineError> {
        let Some(steering) = &self.controls.steering else {
            return Ok(());
        };
        // A cancelled call never dispatches, so its note must not be journaled as delivered.
        if self.controls.cancellation.is_cancelled() {
            return Ok(());
        }
        let Some(text) = steering.take_note() else {
            return Ok(());
        };
        self.append(Event::GoalSteeringNoteRecorded {
            goal_id: self.goal()?,
            run_id: self.run_id,
            text: text.clone(),
        })
        .await?;
        steering.note_recorded();
        place_steering_note(&mut request.input, text);
        Ok(())
    }

    // The profile an unassigned child resolves to; spawns and their proposals must agree on it.
    pub(super) fn default_child_profile(&self) -> Option<String> {
        self.routing
            .providers
            .roster
            .as_ref()
            .map(|roster| roster.snapshot.providers.child.clone())
    }

    pub(super) fn pause_requested(&self) -> bool {
        self.controls
            .steering
            .as_ref()
            .is_some_and(RunSteering::pause_requested)
    }

    pub(super) fn pause_at_boundary(&self) -> bool {
        let requested = self.pause_requested();
        if requested {
            self.controls.cancellation.cancel();
        }
        requested
    }

    pub(super) async fn execute(
        &self,
        mut root_request: ProviderRequest,
    ) -> Result<(), EngineError> {
        match self.execute_inner(&mut root_request).await? {
            RunProgress::Ready(()) => Ok(()),
            RunProgress::Stopped(RunRefusal::Budget) => Err(EngineError::GoalBudgetExhausted),
            RunProgress::Stopped(RunRefusal::Capacity) => {
                Err(crate::tools::ToolError::Limit.into())
            }
        }
    }

    async fn execute_inner(
        &self,
        root_request: &mut ProviderRequest,
    ) -> Result<RunProgress<()>, EngineError> {
        let end = match self.initialize_definitions(root_request).await? {
            RunProgress::Ready(step::DefinitionInitialization::Ready) => None,
            RunProgress::Ready(step::DefinitionInitialization::Stopped)
                if self.pause_at_boundary() =>
            {
                Some(PrimaryEnd::Stopped(StopCause::Paused))
            }
            RunProgress::Ready(step::DefinitionInitialization::Stopped)
                if self.take_active_time_floor() =>
            {
                Some(PrimaryEnd::Stopped(StopCause::ActiveTime))
            }
            RunProgress::Ready(step::DefinitionInitialization::Stopped) => {
                Some(PrimaryEnd::Failed(RunFailure::DefinitionInvalid))
            }
            RunProgress::Stopped(RunRefusal::Capacity) => {
                return Ok(RunProgress::Stopped(RunRefusal::Capacity));
            }
            RunProgress::Stopped(RunRefusal::Budget) => {
                Some(PrimaryEnd::Stopped(StopCause::ActiveTime))
            }
        };
        if let Some(end) = end {
            return self.settle_primary(end).await.map(RunProgress::Ready);
        }
        let end = match self.initialize_skill(root_request).await? {
            RunProgress::Ready(step::SkillInitialization::Ready) => {
                return self.drive_primary(root_request).await;
            }
            // A stopped read carries no cause: the observed facts decide, as for a refused reservation.
            RunProgress::Ready(step::SkillInitialization::Stopped) => {
                PrimaryEnd::Stopped(StopCause::Budget)
            }
            RunProgress::Ready(step::SkillInitialization::Failed(cause)) => {
                PrimaryEnd::Failed(cause)
            }
            RunProgress::Stopped(RunRefusal::Capacity) => {
                return Ok(RunProgress::Stopped(RunRefusal::Capacity));
            }
            RunProgress::Stopped(RunRefusal::Budget) => PrimaryEnd::Stopped(StopCause::ActiveTime),
        };
        if matches!(end, PrimaryEnd::Failed(RunFailure::ToolContextFull)) {
            self.record_context_full(root_request, 0);
        }
        self.settle_primary(end).await.map(RunProgress::Ready)
    }

    fn drive_primary<'b>(
        &'b self,
        root_request: &'b mut ProviderRequest,
    ) -> step::BoxedStep<'b, RunProgress<()>> {
        Box::pin(self.drive_primary_inner(root_request))
    }

    async fn drive_primary_inner(
        &self,
        root_request: &mut ProviderRequest,
    ) -> Result<RunProgress<()>, EngineError> {
        let (end, pending_turn_bytes) = loop {
            let phase = root_request.phase;
            if self.pause_at_boundary() {
                break (PrimaryEnd::Stopped(StopCause::Paused), 0);
            }
            if self.controls.cancellation.is_cancelled() {
                break (PrimaryEnd::Stopped(StopCause::Cancelled), 0);
            }
            match self.maintain_primary_context(root_request, None).await? {
                tool_compaction::PrimaryContext::Ready => {}
                tool_compaction::PrimaryContext::Stop(cause) => {
                    break (PrimaryEnd::Stopped(cause), 0);
                }
            }
            let call = match self.record_primary_call(root_request).await? {
                Admission::Reserved(call) => call,
                Admission::Exhausted => break (PrimaryEnd::Stopped(StopCause::Budget), 0),
            };
            let decision =
                decide_primary_call(PrimarySite::Loop, call.result, call.validity, &call.facts);
            match self
                .apply_primary_decision(root_request, decision, phase, call.budget_action)
                .await?
            {
                RunProgress::Ready(PrimaryFlow::Continue) => {}
                RunProgress::Ready(PrimaryFlow::Settled) => return Ok(RunProgress::Ready(())),
                RunProgress::Ready(PrimaryFlow::End(end, pending)) => break (end, pending),
                RunProgress::Stopped(cause) => return Ok(RunProgress::Stopped(cause)),
            }
        };
        if matches!(end, PrimaryEnd::Stopped(StopCause::ContextFull)) {
            self.record_context_full(root_request, pending_turn_bytes);
        }
        self.settle_primary(end).await.map(RunProgress::Ready)
    }

    async fn record_primary_call(
        &self,
        root_request: &mut ProviderRequest,
    ) -> Result<Admission<PrimaryCall>, EngineError> {
        let phase = root_request.phase;
        let limits = RateLimitSlot::default();
        let (first, active_segment) =
            match self.invoke_primary(root_request, limits.clone()).await? {
                Admission::Reserved(result) => result,
                Admission::Exhausted => return Ok(Admission::Exhausted),
            };
        root_request.input.outcome_correction = false;
        self.settle_active_segment(active_segment)?;
        let validity = self.validate_primary_outcome(&first, phase == AgentPhase::RootPlan)?;
        let facts = RunFacts::observe(self)?;
        let record = call_record(
            phase,
            &first,
            primary_admitted(PrimarySite::Loop, &first, validity, &facts),
            self.default_child_profile(),
            validity
                .err()
                .and_then(|cause| proposal_refusal(&first, cause)),
            limits.take(),
        );
        self.append(Event::ProviderCallRecorded {
            run_id: self.run_id,
            agent_run_id: self.primary_id,
            record,
        })
        .await?;
        // A cancellation or active-time expiry while the record commits still stops this call.
        let facts = RunFacts::observe(self)?;
        let budget_action = match &first {
            CallResult::Response(response) => match response.outcome {
                ProviderOutcome::Tools(_) => crate::session::BudgetAction::Tool,
                ProviderOutcome::Delegate(_) => crate::session::BudgetAction::Delegation,
                ProviderOutcome::Answer(_)
                | ProviderOutcome::Plan(_)
                | ProviderOutcome::Review(_) => crate::session::BudgetAction::Provider,
            },
            CallResult::Failed(_) | CallResult::Cancelled => crate::session::BudgetAction::Provider,
        };
        Ok(Admission::Reserved(PrimaryCall {
            result: first,
            validity,
            facts,
            budget_action,
        }))
    }

    async fn apply_primary_decision(
        &self,
        root_request: &mut ProviderRequest,
        decision: PrimaryDecision,
        phase: AgentPhase,
        budget_action: crate::session::BudgetAction,
    ) -> Result<RunProgress<PrimaryFlow>, EngineError> {
        let flow = match decision {
            PrimaryDecision::Plan(plan, turn) => {
                self.record_plan_step(root_request, *plan, turn).await?;
                PrimaryFlow::Continue
            }
            PrimaryDecision::Finish(answer, turn) => {
                self.apply_primary_answer(root_request, answer, turn)
                    .await?
            }
            PrimaryDecision::Delegate(delegation, turn) => {
                if self.pause_at_boundary() {
                    PrimaryFlow::End(PrimaryEnd::Stopped(StopCause::Paused), 0)
                } else {
                    return Ok(self
                        .run_children(root_request, delegation.children, delegation.agent, turn)
                        .await?
                        .map(|progress| match progress {
                            children::DelegationProgress::Continue => PrimaryFlow::Continue,
                            children::DelegationProgress::Settled => PrimaryFlow::Settled,
                        }));
                }
            }
            PrimaryDecision::ToolStep(requests, turn) => {
                self.apply_primary_tools(
                    root_request,
                    requests,
                    turn,
                    PrimaryAction {
                        phase,
                        budget_action,
                    },
                )
                .await?
            }
            PrimaryDecision::CorrectOutcome => {
                root_request.input.outcome_correction = true;
                PrimaryFlow::Continue
            }
            PrimaryDecision::RefusedTool(cause, requests, turn) => {
                self.apply_primary_refused_tool(root_request, requests, turn, cause)
                    .await?
            }
            PrimaryDecision::Refused(cause) => {
                if self.primary_refusal()?.1 {
                    PrimaryFlow::End(PrimaryEnd::Stopped(StopCause::Refused(cause)), 0)
                } else {
                    PrimaryFlow::Continue
                }
            }
            PrimaryDecision::Stop(cause) => {
                if matches!(cause, StopCause::Budget | StopCause::ActiveTime) {
                    self.ledger()
                        .capture_budget_refusal(budget_action, Some(self.primary_id))?;
                }
                PrimaryFlow::End(PrimaryEnd::Stopped(cause), 0)
            }
            PrimaryDecision::Fail(failure) => PrimaryFlow::End(PrimaryEnd::Failed(failure), 0),
            PrimaryDecision::Defer => return Err(EngineError::CoordinatorFailed),
        };
        Ok(RunProgress::Ready(flow))
    }

    async fn apply_primary_answer(
        &self,
        root_request: &mut ProviderRequest,
        answer: Answer,
        turn: crate::provider::ProviderTurn,
    ) -> Result<PrimaryFlow, EngineError> {
        let pending_turn_bytes = turn.len();
        match self.stop_primary(root_request, &answer, turn).await? {
            Ok(PrimaryStop::Continue) => {}
            Ok(PrimaryStop::Finish) => {
                return Ok(PrimaryFlow::End(
                    PrimaryEnd::Finished(answer),
                    pending_turn_bytes,
                ));
            }
            Err(refusal) => {
                return Ok(PrimaryFlow::End(
                    PrimaryEnd::Stopped(refusal.into()),
                    pending_turn_bytes,
                ));
            }
        }
        Ok(PrimaryFlow::Continue)
    }

    async fn apply_primary_tools(
        &self,
        root_request: &mut ProviderRequest,
        requests: Vec<crate::ToolRequest>,
        turn: crate::provider::ProviderTurn,
        action: PrimaryAction,
    ) -> Result<PrimaryFlow, EngineError> {
        let pending_turn_bytes = turn.len();
        if let Some(end) = self
            .primary_tools(
                root_request,
                requests,
                turn,
                action.phase == AgentPhase::RootPlan,
            )
            .await?
        {
            if let PrimaryEnd::Stopped(cause) = &end
                && matches!(cause, StopCause::Budget | StopCause::ActiveTime)
            {
                self.ledger()
                    .capture_budget_refusal(action.budget_action, Some(self.primary_id))?;
            }
            return Ok(PrimaryFlow::End(end, pending_turn_bytes));
        }

        Ok(PrimaryFlow::Continue)
    }

    async fn apply_primary_refused_tool(
        &self,
        root_request: &mut ProviderRequest,
        requests: Vec<crate::ToolRequest>,
        turn: crate::provider::ProviderTurn,
        cause: ProposalRefusalCause,
    ) -> Result<PrimaryFlow, EngineError> {
        let pending_turn_bytes = turn.len();
        match self.record_refused_tool_step(root_request, requests, turn)? {
            tool_compaction::ContextCapacity::Fits => {}
            tool_compaction::ContextCapacity::Full => {
                return Ok(PrimaryFlow::End(
                    PrimaryEnd::Stopped(StopCause::ContextFull),
                    pending_turn_bytes,
                ));
            }
        }
        if self.primary_refusal()?.1 {
            return Ok(PrimaryFlow::End(
                PrimaryEnd::Stopped(StopCause::Refused(cause)),
                pending_turn_bytes,
            ));
        }

        Ok(PrimaryFlow::Continue)
    }

    fn record_refused_tool_step(
        &self,
        request: &mut ProviderRequest,
        requests: Vec<crate::ToolRequest>,
        turn: crate::provider::ProviderTurn,
    ) -> Result<tool_compaction::ContextCapacity, EngineError> {
        let (Some(context), Some(runtime)) = (request.input.tools.as_mut(), self.controls.tools)
        else {
            return Ok(tool_compaction::ContextCapacity::Fits);
        };
        if requests.is_empty() {
            return Ok(tool_compaction::ContextCapacity::Fits);
        }
        let contract = self.ledger().plan_contract()?;
        let mut refusal_index = self.ledger().view(|view| {
            requests.iter().position(|item| {
                self.check_tool(view, &item.call, contract.as_ref())
                    .is_err()
            })
        })?;
        if refusal_index.is_none() {
            for (index, item) in requests.iter().enumerate() {
                if self.primary_lease_allows(&item.call)?.is_err() {
                    refusal_index = Some(index);
                    break;
                }
            }
        }
        let step = crate::tools::ToolStep {
            narrowed_observations: Default::default(),
            refusal_index,
            refusal: Some(
                self.primary_refusal()?
                    .0
                    .ok_or(EngineError::InvalidHistory)?,
            ),
            cleared: false,
            plan: None,
            turn,
            requests,
            observations: Vec::new(),
            facts: Some(crate::provider::GoalContextFacts::maximum()),
            steering_notes: Vec::new(),
            calls_remaining: None,
            hook_feedback: Default::default(),
        };
        if matches!(
            tool_compaction::context_capacity(
                context.with_step_len(&step),
                runtime.context_limit_bytes(),
                0
            ),
            tool_compaction::ContextCapacity::Full
        ) {
            return Ok(tool_compaction::ContextCapacity::Full);
        }
        context.steps.push(step);
        Ok(tool_compaction::ContextCapacity::Fits)
    }

    fn record_context_full(&self, request: &ProviderRequest, pending_turn_bytes: usize) {
        crate::provider::diagnostics::tool_context_full(
            request,
            request.input.context_window_tokens,
            self.controls
                .tools
                .map_or(0, crate::tools::ToolRuntime::context_limit_bytes),
            pending_turn_bytes,
        );
    }

    fn validate_primary_outcome(
        &self,
        first: &CallResult,
        planning: bool,
    ) -> Result<Result<(), ProposalRefusalCause>, EngineError> {
        let CallResult::Response(response) = first else {
            return Ok(Ok(()));
        };
        match &response.outcome {
            ProviderOutcome::Answer(answer) => Ok(self.well_formed_answer(answer, planning)),
            ProviderOutcome::Delegate(delegation) => {
                if !planning && !self.wave_settled()? {
                    Ok(Err(ProposalRefusalCause::Phase))
                } else {
                    self.well_formed_delegation(&delegation.children, delegation.agent.as_deref())
                }
            }
            ProviderOutcome::Tools(requests) => self.well_formed_primary_tools(requests),
            ProviderOutcome::Plan(plan) => self.check_plan(plan),
            ProviderOutcome::Review(_) => Ok(Ok(())),
        }
    }

    fn primary_tools<'b>(
        &'b self,
        root_request: &'b mut ProviderRequest,
        requests: Vec<crate::ToolRequest>,
        turn: crate::provider::ProviderTurn,
        planning: bool,
    ) -> step::BoxedStep<'b, Option<PrimaryEnd>> {
        Box::pin(self.primary_tools_inner(root_request, requests, turn, planning))
    }

    async fn primary_tools_inner(
        &self,
        root_request: &mut ProviderRequest,
        requests: Vec<crate::ToolRequest>,
        turn: crate::provider::ProviderTurn,
        planning: bool,
    ) -> Result<Option<PrimaryEnd>, EngineError> {
        if self.pause_at_boundary() {
            return Ok(Some(PrimaryEnd::Stopped(StopCause::Paused)));
        }
        let Some(request) = self.begin_tool_step(requests).await? else {
            return Ok(Some(PrimaryEnd::Failed(RunFailure::PlanAdmission)));
        };
        let grant = Grant::Primary {
            request: &mut *root_request,
            review_allowed: planning && self.ledger().admits(Action::Review),
        };
        match Box::pin(self.execute_step(grant, request, turn)).await? {
            StepEnd::Continue => {}
            StepEnd::Stopped(cause) => return Ok(Some(PrimaryEnd::Stopped(cause))),
        }
        Box::pin(self.verify_workers()).await?;
        Ok(None)
    }

    async fn stop_primary(
        &self,
        root: &mut ProviderRequest,
        answer: &Answer,
        turn: crate::provider::ProviderTurn,
    ) -> Result<Result<PrimaryStop, SizingRefusal>, EngineError> {
        let fits = self
            .controls
            .tools
            .zip(root.input.tools.as_ref())
            .is_none_or(|(runtime, tools)| {
                tools.has_stop_room(&turn, runtime.context_limit_bytes())
            });
        let feedback = match self
            .lifecycle_hooks(LifecycleHook {
                agent: self.primary_id,
                event: crate::tools::permissions::HookEvent::Stop,
                subject: "",
                message: &answer.text,
                deadline: None,
                feedback_fits: fits,
            })
            .await?
        {
            Ok(Some(feedback)) => feedback,
            Ok(None) => return Ok(Ok(PrimaryStop::Finish)),
            Err(refusal) => return Ok(Err(refusal.into())),
        };
        let after_step = root.input.tools.as_mut().map_or(0, |tools| {
            tools.add_stop_feedback(&turn, &feedback);
            tools.steps.len()
        });
        root.input
            .stop_hook_feedback
            .push(crate::provider::StopHookFeedback {
                after_step,
                turn,
                text: feedback,
                facts: None,
                steering_notes: Vec::new(),
            });
        Ok(Ok(PrimaryStop::Continue))
    }

    fn primary_refusal(&self) -> Result<(Option<ProposalRefusal>, bool), EngineError> {
        self.ledger().view(|view| {
            let agent = view
                .runs
                .last()
                .ok_or(EngineError::InvalidHistory)?
                .agents
                .iter()
                .find(|agent| agent.id == self.primary_id)
                .ok_or(EngineError::InvalidHistory)?;
            Ok((
                agent
                    .provider_calls
                    .last()
                    .and_then(|call| call.refusal.clone()),
                agent.refusal_limit_reached(),
            ))
        })?
    }

    async fn primary_context(&self, root: &mut ProviderRequest) -> Result<(), EngineError> {
        root.input.last_answer_refused = self.primary_refusal()?.0;
        self.record_steering_note(root).await?;
        let facts = self.ledger().context_facts()?;
        if let Some(feedback) = continuation_feedback(&mut root.input) {
            feedback.facts = Some(facts);
        } else {
            match root
                .input
                .tools
                .as_mut()
                .and_then(|tools| tools.steps.last_mut())
            {
                Some(step) => step.facts = Some(facts),
                None => root.input.goal_context_facts = Some(facts),
            }
        }
        Ok(())
    }

    fn invoke_primary<'b>(
        &'b self,
        root: &'b mut ProviderRequest,
        limits: RateLimitSlot,
    ) -> step::BoxedStep<'b, Admission<(CallResult, ActiveSegment)>> {
        Box::pin(self.invoke_primary_inner(root, limits))
    }

    async fn invoke_primary_inner(
        &self,
        root: &mut ProviderRequest,
        limits: RateLimitSlot,
    ) -> Result<Admission<(CallResult, ActiveSegment)>, EngineError> {
        let provider = self.routing.bind(
            crate::provider::ProviderRole::Primary,
            self.primary_id,
            &mut root.model,
            &mut root.max_output_tokens,
        )?;
        let Admission::Reserved(active_segment) = self.reserve_active_segment(
            provider.identity().limits.deadline,
            crate::session::BudgetAction::Provider,
            self.primary_id,
        )?
        else {
            return Ok(Admission::Exhausted);
        };
        let deadline = active_segment
            .deadline()
            .checked_sub(STORE_ACTION_OVERHEAD)
            .unwrap_or(active_segment.started);
        let Admission::Reserved(()) = self
            .reserve_provider_call(self.primary_id, self.primary_call(), 0)
            .await?
        else {
            self.settle_active_segment(active_segment)?;
            return Ok(Admission::Exhausted);
        };
        self.primary_context(root).await?;
        let request = root.clone();
        let context = self
            .hints
            .map(|hints| hints.preview_context(self.ledger, self.primary_id))
            .transpose()?
            .flatten();
        let Some(context) = context else {
            let result = self
                .routing
                .invoke(
                    request,
                    deadline,
                    self.controls.cancellation.clone(),
                    None,
                    limits,
                )
                .await?;
            self.settle_result(&result)?;
            return Ok(Admission::Reserved((result, active_segment)));
        };
        let call_index = context.call_index;
        let (preview, mut receiver) = ResponsePreview::new();
        let mut checkpoint = DisplayCheckpoint {
            call_index,
            phase: request.phase,
            chunks: context.chunks,
            answer_offset: 0,
            commentary_offsets: vec![0; MAX_DISPLAY_COMMENTARY_ITEMS],
            remaining_commentary_bytes: MAX_DISPLAY_TEXT_BYTES - context.commentary_bytes,
            remaining_commentary_items: MAX_DISPLAY_COMMENTARY_ITEMS - context.commentary_items,
            limited: false,
            limit_recorded: context.limit_recorded,
        };
        let future = self.routing.invoke(
            request,
            deadline,
            self.controls.cancellation.clone(),
            Some(preview),
            limits,
        );
        tokio::pin!(future);
        let mut observing = true;
        let result = loop {
            tokio::select! {
                result = &mut future => break result?,
                changed = receiver.changed(), if observing => {
                    if changed.is_err() { observing = false; continue; }
                    let text = checkpoint.filter(&receiver.borrow_and_update());
                    self.checkpoint(&text, &mut checkpoint, false).await?;
                    self.hints.expect("preview owner").publish_preview(self.primary_id, call_index, Some(Arc::new(text)));
                }
            }
        };
        let mut shown = (**receiver.borrow()).clone();
        let answered = matches!(&result, CallResult::Response(response) if matches!(response.outcome, ProviderOutcome::Answer(_)));
        let called = matches!(&result, CallResult::Response(_)) && !answered;
        if (called || shown.tentative && !answered) && !shown.answer.is_empty() {
            shown.commentary.push(std::mem::take(&mut shown.answer));
        }
        let text = checkpoint.filter(&shown);
        let result = match result {
            CallResult::Response(response)
                if !text.answer.is_empty()
                    && !matches!(&response.outcome,
                ProviderOutcome::Answer(answer) if answer.text.starts_with(&text.answer)) =>
            {
                CallResult::Failed(CallFailure::InvalidResponse(Some(
                    ProviderFailureReason::ResponseContract,
                )))
            }
            result => result,
        };
        self.settle_result(&result)?;
        if !matches!(
            result,
            CallResult::Failed(
                CallFailure::InvalidResponse(_)
                    | CallFailure::Rejected(_)
                    | CallFailure::OutputLimit
            )
        ) {
            self.checkpoint(&text, &mut checkpoint, true).await?;
        }
        self.hints
            .expect("preview owner")
            .publish_preview(self.primary_id, call_index, None);
        Ok(Admission::Reserved((result, active_segment)))
    }

    async fn checkpoint(
        &self,
        text: &PreviewText,
        state: &mut DisplayCheckpoint,
        tail: bool,
    ) -> Result<(), EngineError> {
        state.limited |= text.commentary_limited;
        for (category, item, body) in
            std::iter::once((DisplayCategory::Answer, 0, text.answer.as_str())).chain(
                text.commentary
                    .iter()
                    .enumerate()
                    .map(|(index, text)| (DisplayCategory::Commentary, index, text.as_str())),
            )
        {
            let answer_reserve = MAX_DISPLAY_TEXT_BYTES
                .saturating_sub(state.answer_offset)
                .div_ceil(MAX_DISPLAY_CHUNK_BYTES - 3);
            let offset = if category == DisplayCategory::Answer {
                &mut state.answer_offset
            } else {
                &mut state.commentary_offsets[item]
            };
            while (category != DisplayCategory::Answer || !text.tentative)
                && body.len().saturating_sub(*offset) >= MAX_DISPLAY_CHUNK_BYTES
                || (tail || category == DisplayCategory::Commentary) && *offset < body.len()
            {
                if category == DisplayCategory::Commentary
                    && state.chunks
                        >= MAX_DISPLAY_CHUNKS - answer_reserve - usize::from(!state.limit_recorded)
                {
                    state.limited = true;
                    break;
                }
                if state.chunks == MAX_DISPLAY_CHUNKS {
                    return Err(EngineError::CoordinatorFailed);
                }
                let mut end = body.len().min(*offset + MAX_DISPLAY_CHUNK_BYTES);
                while !body.is_char_boundary(end) {
                    end -= 1;
                }
                self.append(Event::AssistantDisplayRecorded {
                    run_id: self.run_id,
                    agent_run_id: self.primary_id,
                    chunk: DisplayChunk {
                        call_index: state.call_index,
                        phase: state.phase,
                        category,
                        item: item as u8,
                        offset: *offset as u32,
                        text: body[*offset..end].to_owned(),
                    },
                })
                .await?;
                *offset = end;
                state.chunks += 1;
            }
        }
        if state.limited && !state.limit_recorded {
            self.append(Event::AssistantDisplayRecorded {
                run_id: self.run_id,
                agent_run_id: self.primary_id,
                chunk: DisplayChunk {
                    call_index: state.call_index,
                    phase: state.phase,
                    category: DisplayCategory::Limit,
                    item: 0,
                    offset: 0,
                    text: crate::session::DISPLAY_LIMIT_TEXT.to_owned(),
                },
            })
            .await?;
            state.chunks += 1;
            state.limit_recorded = true;
        }
        Ok(())
    }

    fn well_formed_answer(
        &self,
        answer: &Answer,
        planning: bool,
    ) -> Result<(), ProposalRefusalCause> {
        if planning && matches!(self.controls.policy, CollaborationPolicy::Team { .. }) {
            Err(ProposalRefusalCause::Phase)
        } else if !valid_answer(answer, MAX_PRIMARY_RESULT_BYTES) {
            Err(ProposalRefusalCause::AnswerInvalid)
        } else {
            Ok(())
        }
    }

    fn well_formed_primary_tools(
        &self,
        requests: &[crate::tools::ToolRequest],
    ) -> Result<Result<(), ProposalRefusalCause>, EngineError> {
        if let Err(cause) = self.well_formed_tools(requests)? {
            return Ok(Err(cause));
        }
        for request in requests {
            if let Err(cause) = self.primary_lease_allows(&request.call)? {
                return Ok(Err(cause));
            }
        }
        Ok(Ok(()))
    }

    fn well_formed_tools(
        &self,
        requests: &[crate::tools::ToolRequest],
    ) -> Result<Result<(), ProposalRefusalCause>, EngineError> {
        if requests.is_empty()
            || requests.len() > usize::from(crate::session::MAX_STEP_TOOL_CALLS)
            || requests.iter().enumerate().any(|(index, request)| {
                requests[..index].iter().any(|prior| prior.id == request.id)
            })
        {
            return Ok(Err(ProposalRefusalCause::InvalidArguments));
        }
        let contract = self.ledger().plan_contract()?;
        self.ledger().view(|view| {
            requests
                .iter()
                .try_for_each(|request| self.check_tool(view, &request.call, contract.as_ref()))
        })
    }

    fn model_may_propose(&self, call: &crate::tools::ToolCall) -> bool {
        match call {
            crate::tools::ToolCall::Skill { name, .. } => self
                .controls
                .tools
                .is_some_and(|tools| !tools.hides_skill_from_model(name)),
            crate::tools::ToolCall::ParseAgentDefinition { .. }
            | crate::tools::ToolCall::List { .. }
            | crate::tools::ToolCall::Read { .. }
            | crate::tools::ToolCall::Search { .. }
            | crate::tools::ToolCall::Mkdir { .. }
            | crate::tools::ToolCall::Write { .. }
            | crate::tools::ToolCall::Edit { .. }
            | crate::tools::ToolCall::Delete { .. }
            | crate::tools::ToolCall::Move { .. }
            | crate::tools::ToolCall::MultiEdit { .. }
            | crate::tools::ToolCall::Command { .. }
            | crate::tools::ToolCall::CommandUnguarded { .. }
            | crate::tools::ToolCall::ApplyChangeset { .. }
            | crate::tools::ToolCall::McpList { .. }
            | crate::tools::ToolCall::McpCall { .. }
            | crate::tools::ToolCall::GitStatus { .. }
            | crate::tools::ToolCall::GitDiff { .. }
            | crate::tools::ToolCall::GitLog { .. }
            | crate::tools::ToolCall::GitCommit { .. }
            | crate::tools::ToolCall::GitBranch { .. }
            | crate::tools::ToolCall::GitSwitch { .. }
            | crate::tools::ToolCall::GitPush { .. }
            | crate::tools::ToolCall::GhPrCreate { .. }
            | crate::tools::ToolCall::GhPrList { .. }
            | crate::tools::ToolCall::GhPrView { .. }
            | crate::tools::ToolCall::Glob { .. } => true,
        }
    }

    fn check_tool(
        &self,
        view: &crate::session::SessionView,
        call: &crate::tools::ToolCall,
        contract: Option<&crate::session::GoalPlanContract>,
    ) -> Result<(), ProposalRefusalCause> {
        use ProposalRefusalCause::*;
        if self.controls.tools.is_none()
            || matches!(
                call,
                crate::tools::ToolCall::ApplyChangeset { .. }
                    | crate::tools::ToolCall::ParseAgentDefinition { .. }
            )
        {
            return Err(NotOffered);
        }
        if !self.model_may_propose(call) {
            return Err(NotOffered);
        }
        if !serde_json::to_vec(call).is_ok_and(|bytes| bytes.len() <= call.argument_limit()) {
            return Err(ArgumentBytes);
        }
        if call
            .path_argument()
            .is_some_and(|(path, root)| !crate::validate::valid_relative(path, root))
        {
            return Err(InvalidPath);
        }
        if !call.valid() {
            return Err(InvalidArguments);
        }
        crate::session::validate_primary_tool_scope(view, self.run_id, call)?;
        if let Some(contract) = contract
            && !self
                .controls
                .tools
                .is_some_and(|tools| tools.allows_goal_contract(contract))
        {
            return Err(PlanOutsideGrant);
        }
        Ok(())
    }

    fn check_plan(
        &self,
        plan: &crate::provider::PlanRequest,
    ) -> Result<Result<(), ProposalRefusalCause>, EngineError> {
        use ProposalRefusalCause::*;
        let existing = self.ledger().plan_contract()?;
        let budget = self.ledger().effective_budget()?;
        let settings = self.ledger().requested_command_resources()?.2;
        let contract = &plan.contract;
        let cause = if self.controls.tools.is_none() {
            Some(NotOffered)
        } else if !serde_json::to_vec(contract)
            .is_ok_and(|bytes| bytes.len() <= crate::tools::types::MAX_ORDINARY_TOOL_ARGUMENT_BYTES)
        {
            Some(ArgumentBytes)
        } else if existing.is_some()
            || contract.validate().is_err()
            || contract.resource_class == crate::tools::ToolResourceClass::Build
                && settings.build_jobs.is_none()
        {
            Some(PlanInvalid)
        } else if !contract.budget.is_within(&budget) {
            Some(PlanBudgetOutsideGoal)
        } else if !self
            .controls
            .tools
            .is_some_and(|tools| tools.allows_goal_contract(contract))
        {
            Some(PlanOutsideGrant)
        } else if !self.ledger().can_narrow(contract) {
            Some(PlanBudgetSpent)
        } else {
            None
        };
        Ok(cause.map_or(Ok(()), Err))
    }

    async fn record_plan_step(
        &self,
        root: &mut ProviderRequest,
        plan: crate::provider::PlanRequest,
        turn: crate::provider::ProviderTurn,
    ) -> Result<(), EngineError> {
        self.append(Event::GoalPlanContractRecorded {
            goal_id: self.goal()?,
            run_id: self.run_id,
            contract: plan.contract.clone(),
        })
        .await?;
        let tools = root
            .input
            .tools
            .as_mut()
            .ok_or(EngineError::InvalidRequest)?;
        tools.steps.push(crate::tools::ToolStep {
            narrowed_observations: Default::default(),
            refusal_index: None,
            refusal: None,
            plan: Some(plan),
            turn,
            requests: Vec::new(),
            observations: Vec::new(),
            facts: None,
            steering_notes: Vec::new(),
            calls_remaining: None,
            cleared: false,
            hook_feedback: Default::default(),
        });

        Ok(())
    }

    pub(super) async fn begin_tool_step(
        &self,
        requests: Vec<crate::tools::ToolRequest>,
    ) -> Result<Option<Vec<crate::tools::ToolRequest>>, EngineError> {
        Ok((!requests.is_empty()
            && requests.len() <= usize::from(crate::session::MAX_STEP_TOOL_CALLS))
        .then_some(requests))
    }

    // The only primary terminal writer: AgentFinished, MessageCommitted for an answer, RunFinished.
    async fn settle_primary(&self, end: PrimaryEnd) -> Result<(), EngineError> {
        let (agent, run, failure, answer) = match stop::settle(end, &RunFacts::observe(self)?) {
            Settlement::Unknown => return Err(crate::tools::ToolError::Uncertain.into()),
            // No terminal Event: the submission reports the stop its reopened Run projects.
            Settlement::Unsettleable => return Ok(()),
            Settlement::Finish(answer) => (
                AgentDisposition::Finished,
                RunDisposition::Finished,
                None,
                Some(answer),
            ),
            Settlement::Fail { failure, exhausted } => {
                if exhausted {
                    if failure != RunFailure::CriterionUnmet {
                        self.ledger().capture_budget_refusal(
                            crate::session::BudgetAction::Provider,
                            Some(self.primary_id),
                        )?;
                    }
                    self.ledger().mark_budget_stopped();
                }
                (
                    AgentDisposition::Failed,
                    RunDisposition::Failed,
                    Some(failure),
                    None,
                )
            }
            Settlement::Cancel => (
                AgentDisposition::Cancelled,
                RunDisposition::Cancelled,
                None,
                None,
            ),
            Settlement::HookStop => (
                AgentDisposition::StoppedByHook,
                RunDisposition::StoppedByHook,
                None,
                None,
            ),
        };
        self.append(Event::AgentFinished {
            worker: None,
            run_id: self.run_id,
            agent_run_id: self.primary_id,
            disposition: agent,
            summary: answer
                .as_ref()
                .map(|answer| crate::session::answer_summary(&answer.text)),
            result: answer.as_ref().map(|answer| answer.text.clone()),
        })
        .await?;
        if let Some(answer) = answer {
            self.append(Event::MessageCommitted {
                run_id: self.run_id,
                text: answer.text,
            })
            .await?;
        }
        self.append(Event::RunFinished {
            run_id: self.run_id,
            disposition: run,
            failure,
            active_ms: self.ledger().run_active_ms()?,
        })
        .await?;
        Ok(())
    }

    fn take_active_time_floor(&self) -> bool {
        self.active_time_floor
            .swap(false, std::sync::atomic::Ordering::Relaxed)
    }

    fn hook_requested_stop(&self) -> Result<bool, EngineError> {
        self.ledger().view(|view| {
            view.runs.iter().any(|run| {
                run.id == self.run_id && run.hooks_settled() && run.hook_stop_reason().is_some()
            })
        })
    }

    fn uncertain_hook(&self) -> Result<bool, EngineError> {
        self.ledger().view(|view| {
            view.runs
                .last()
                .is_some_and(|run| self.uncertain_hook_in(run))
        })
    }

    fn uncertain_hook_in(&self, run: &crate::session::RunView) -> bool {
        run.tools
            .iter()
            .chain(run.agents.iter().flat_map(|agent| &agent.tools))
            .flat_map(|tool| &tool.hooks)
            .chain(&run.lifecycle_hooks)
            .any(|hook| {
                hook.outcome.as_ref().is_some_and(|outcome| {
                    outcome.observation.disposition == crate::tools::ToolDisposition::Uncertain
                })
            })
    }

    fn actor_hook_requested_stop(&self, agent: AgentRunId) -> Result<bool, EngineError> {
        self.ledger().view(|view| {
            view.runs.iter().any(|run| {
                run.id == self.run_id && run.hooks_settled() && run.actor_stopped_by_hook(agent)
            })
        })
    }
}

// A failure before a decoded outcome also returns its retry cause and any server Retry-After.
pub(super) async fn invoke_bounded_with_preview(
    provider: &dyn Provider,
    request: ProviderRequest,
    run_deadline: Instant,
    mut cancellation: RunCancellation,
    preview: Option<ResponsePreview>,
    limits: RateLimitSlot,
    receipt: crate::fault::LatencyReceipt<'_>,
) -> (CallResult, Option<super::routing::Transient>) {
    if cancellation.is_cancelled() {
        return (CallResult::Cancelled, None);
    }
    let remaining = run_deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return (CallResult::Failed(CallFailure::Timeout), None);
    }
    let timeout = provider.identity().limits.deadline.min(remaining);
    let output_cap = request.max_output_tokens;
    let output_bound = provider.identity().limits.output_token_bound;
    let invocation = provider.invoke_observed(request, preview, limits);
    let result = tokio::select! {
        biased;
        () = cancellation.cancelled() => return (CallResult::Cancelled, None),
        result = tokio::time::timeout(timeout, invocation) => result,
    };
    receipt.received(&result);
    let transient = match &result {
        Ok(Err(error)) => error
            .retry_cause()
            .map(|(cause, retry_after)| super::routing::Transient { cause, retry_after }),
        Err(_) => {
            provider
                .identity()
                .ceiling_retry_cause()
                .map(|cause| super::routing::Transient {
                    cause,
                    retry_after: None,
                })
        }
        _ => None,
    };
    let result = match result {
        Ok(Ok(response))
            if response
                .usage
                .input_tokens
                .is_some_and(|count| count > MAX_REPORTED_INPUT_TOKENS)
                || !response.usage.cache_consistent() =>
        {
            CallResult::Failed(CallFailure::InvalidResponse(Some(
                ProviderFailureReason::ResponseContract,
            )))
        }
        Ok(Ok(response))
            if !output_bound.admits_reported_output(response.usage.output_tokens, output_cap) =>
        {
            CallResult::Failed(CallFailure::OutputLimit)
        }
        Ok(Ok(response))
            if response.response_id.as_ref().is_some_and(|id| {
                id.is_empty() || id.len() > 128 || !id.bytes().all(|byte| byte.is_ascii_graphic())
            }) =>
        {
            CallResult::Failed(CallFailure::InvalidResponse(Some(
                ProviderFailureReason::ResponseContract,
            )))
        }
        Ok(Ok(response))
            if response.wire_provenance.is_some_and(|provenance| {
                !provenance.valid_for(
                    response.response_id.as_deref(),
                    response.usage.input_tokens,
                    response.usage.output_tokens,
                )
            }) =>
        {
            CallResult::Failed(CallFailure::InvalidResponse(Some(
                ProviderFailureReason::ResponseContract,
            )))
        }
        Ok(Ok(response)) => CallResult::Response(response),
        Ok(Err(error)) => match error.failure_class() {
            ProviderFailureClass::Unavailable => CallResult::Failed(CallFailure::Unavailable {
                reason: error.failure_reason(),
            }),
            ProviderFailureClass::Rejected => {
                CallResult::Failed(CallFailure::Rejected(error.failure_reason()))
            }
            ProviderFailureClass::InvalidOutcome => {
                CallResult::Failed(CallFailure::InvalidResponse(error.failure_reason()))
            }
        },
        Err(_) => CallResult::Failed(CallFailure::Timeout),
    };
    (result, transient)
}

fn call_usage(result: &CallResult) -> (Option<u32>, Option<u32>) {
    match result {
        CallResult::Response(response) => {
            (response.usage.input_tokens, response.usage.output_tokens)
        }
        CallResult::Failed(_) | CallResult::Cancelled => (None, None),
    }
}

fn proposal_refusal(result: &CallResult, cause: ProposalRefusalCause) -> Option<ProposalRefusal> {
    let CallResult::Response(response) = result else {
        return None;
    };
    let operation = match &response.outcome {
        ProviderOutcome::Plan(_) => "arany_plan".to_owned(),
        ProviderOutcome::Answer(_) => "answer".to_owned(),
        ProviderOutcome::Review(_) => "review".to_owned(),
        ProviderOutcome::Delegate(_) => "delegate".to_owned(),
        ProviderOutcome::Tools(requests) => format!("arany_{}", requests.first()?.call.label()),
    };
    Some(ProposalRefusal { operation, cause })
}

fn call_record(
    phase: AgentPhase,
    result: &CallResult,
    valid_response: bool,
    default_profile: Option<String>,
    refusal: Option<ProposalRefusal>,
    rate_limits: Option<RateLimits>,
) -> ProviderCallRecord {
    let disposition = match result {
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Answer(_) | ProviderOutcome::Review(_),
            ..
        }) if valid_response => ProviderCallDisposition::Finished,
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Delegate(_),
            ..
        }) if valid_response => ProviderCallDisposition::Delegated,
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Tools(_),
            ..
        }) if valid_response => ProviderCallDisposition::ToolRequested,
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Plan(_),
            ..
        }) if valid_response => ProviderCallDisposition::PlanRecorded,
        CallResult::Response(_) if refusal.is_some() => ProviderCallDisposition::Rejected,
        CallResult::Response(_) => ProviderCallDisposition::InvalidResponse,
        CallResult::Failed(failure) => match failure {
            CallFailure::Timeout => ProviderCallDisposition::TimedOut,
            CallFailure::Unavailable { .. } => ProviderCallDisposition::Unavailable,
            CallFailure::Rejected(_) => ProviderCallDisposition::Rejected,
            CallFailure::InvalidResponse(_) => ProviderCallDisposition::InvalidResponse,
            CallFailure::OutputLimit => ProviderCallDisposition::OutputLimit,
            CallFailure::TaskPanic => ProviderCallDisposition::TaskPanic,
        },
        CallResult::Cancelled => ProviderCallDisposition::Cancelled,
    };
    let (response_id, usage, wire_provenance) = match result {
        CallResult::Response(response) => (
            response.response_id.clone(),
            response.usage,
            response.wire_provenance,
        ),
        CallResult::Failed(_) | CallResult::Cancelled => (None, Usage::default(), None),
    };
    let refused = refusal.is_some();
    let mut record = ProviderCallRecord {
        rate_limits,
        refusal,
        delegation: if disposition == ProviderCallDisposition::Delegated {
            match result {
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Delegate(d),
                    ..
                }) => Some(
                    d.children
                        .iter()
                        .map(|child| {
                            child.editor.as_ref().map_or_else(
                                || crate::session::worker::ChildAssignment::Researcher {
                                    objective: child.objective.clone(),
                                    definition: d.agent.clone(),
                                    profile: child
                                        .profile
                                        .clone()
                                        .or_else(|| default_profile.clone()),
                                },
                                |assignment| crate::session::worker::ChildAssignment::Editor {
                                    assignment: assignment.clone(),
                                },
                            )
                        })
                        .collect(),
                ),
                _ => None,
            }
        } else {
            None
        },
        phase,
        disposition,
        requested_tools: match result {
            CallResult::Response(ProviderResponse {
                outcome: ProviderOutcome::Tools(requests),
                ..
            }) if disposition == ProviderCallDisposition::ToolRequested => {
                u8::try_from(requests.len()).expect("a valid step carries bounded Tool requests")
            }
            _ => 0,
        },
        response_id,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        wire_provenance,
        failure_reason: if refused {
            Some(ProviderFailureReason::ProposalRefused)
        } else {
            match result {
                CallResult::Failed(
                    CallFailure::Unavailable { reason, .. }
                    | CallFailure::Rejected(reason)
                    | CallFailure::InvalidResponse(reason),
                ) => *reason,
                _ => None,
            }
        },
    };
    if record.rate_limits.is_some()
        && !serde_json::to_vec(&record)
            .is_ok_and(|bytes| bytes.len() <= crate::store::MAX_EVENT_BYTES)
    {
        record.rate_limits = None;
    }
    record
}

// The Stop feedback directly before the next call owns that call's facts and notes.
fn continuation_feedback(
    input: &mut crate::provider::RunInput,
) -> Option<&mut crate::provider::StopHookFeedback> {
    let steps = input.tools.as_ref().map_or(0, |tools| tools.steps.len());
    input
        .stop_hook_feedback
        .last_mut()
        .filter(|feedback| feedback.after_step == steps)
}

fn valid_answer(answer: &Answer, result_limit: usize) -> bool {
    !answer.text.trim().is_empty()
        && answer.text.len() <= result_limit
        && Event::finished_payload_size(&crate::session::answer_summary(&answer.text), &answer.text)
            .is_ok_and(|bytes| bytes <= crate::store::MAX_EVENT_BYTES)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrimarySite {
    Loop,
    // A primary call joined inside a worker wave; `queue_stopped` once the wave is ending.
    Wave { queue_stopped: bool },
}

#[must_use]
#[derive(Debug)]
enum PrimaryDecision {
    Plan(
        Box<crate::provider::PlanRequest>,
        crate::provider::ProviderTurn,
    ),
    Finish(Answer, crate::provider::ProviderTurn),
    Delegate(crate::provider::Delegate, crate::provider::ProviderTurn),
    ToolStep(Vec<crate::ToolRequest>, crate::provider::ProviderTurn),
    // The refusal is journaled; the loop continues unless the refusal streak is full.
    Refused(ProposalRefusalCause),
    RefusedTool(
        ProposalRefusalCause,
        Vec<crate::ToolRequest>,
        crate::provider::ProviderTurn,
    ),
    CorrectOutcome,
    // A wave proposal waits for the workers to settle.
    Defer,
    Stop(StopCause),
    Fail(RunFailure),
}

// Whether the call is journaled as an accepted proposal.
fn primary_admitted(
    site: PrimarySite,
    result: &CallResult,
    validity: Result<(), ProposalRefusalCause>,
    facts: &RunFacts,
) -> bool {
    let CallResult::Response(response) = result else {
        return false;
    };
    validity.is_ok()
        && facts.tokens_within_limit
        && match site {
            PrimarySite::Loop => match &response.outcome {
                ProviderOutcome::Delegate(_) => facts.tokens_open,
                ProviderOutcome::Tools(_) => facts.tool_admitted(),
                ProviderOutcome::Plan(_)
                | ProviderOutcome::Answer(_)
                | ProviderOutcome::Review(_) => true,
            },
            PrimarySite::Wave { queue_stopped } => !queue_stopped,
        }
}

// A wave response the workers must settle before; the record carries its deferral reason.
fn wave_deferred(result: &CallResult, admitted: bool, facts: &RunFacts) -> bool {
    matches!(result, CallResult::Response(_))
        && !admitted
        && facts.tokens_within_limit
        && !facts.cancel_requested
}

fn decide_primary_call(
    site: PrimarySite,
    result: CallResult,
    validity: Result<(), ProposalRefusalCause>,
    facts: &RunFacts,
) -> PrimaryDecision {
    let admitted = primary_admitted(site, &result, validity, facts);
    match site {
        PrimarySite::Loop => {
            if facts.cancel_requested || matches!(result, CallResult::Cancelled) {
                return PrimaryDecision::Stop(StopCause::Cancelled);
            }
            if facts.active_time_exhausted {
                return PrimaryDecision::Stop(StopCause::ActiveTime);
            }
            if matches!(
                result,
                CallResult::Failed(CallFailure::InvalidResponse(Some(
                    ProviderFailureReason::OutcomeContract
                )))
            ) && facts.outcome_correction_pending
            {
                if let Some(cause) = facts.observed() {
                    return PrimaryDecision::Stop(cause);
                }
                return if facts.tool_admitted() {
                    PrimaryDecision::CorrectOutcome
                } else {
                    PrimaryDecision::Stop(StopCause::Budget)
                };
            }
            let CallResult::Response(ProviderResponse { outcome, turn, .. }) = result else {
                return PrimaryDecision::Fail(RunFailure::ProviderCall);
            };
            match (validity, outcome) {
                // An empty Tool list names no operation to refuse.
                (Err(_), ProviderOutcome::Tools(requests)) if requests.is_empty() => {
                    PrimaryDecision::Fail(RunFailure::ProviderCall)
                }
                (Err(cause), ProviderOutcome::Tools(requests)) => {
                    PrimaryDecision::RefusedTool(cause, requests, turn)
                }
                (Err(cause), _) => PrimaryDecision::Refused(cause),
                (Ok(()), _) if !admitted => PrimaryDecision::Stop(StopCause::Budget),
                (Ok(()), ProviderOutcome::Plan(plan)) => PrimaryDecision::Plan(plan, turn),
                (Ok(()), ProviderOutcome::Answer(answer)) => PrimaryDecision::Finish(answer, turn),
                (Ok(()), ProviderOutcome::Delegate(delegation)) => {
                    PrimaryDecision::Delegate(delegation, turn)
                }
                (Ok(()), ProviderOutcome::Tools(requests)) => {
                    PrimaryDecision::ToolStep(requests, turn)
                }
                (Ok(()), ProviderOutcome::Review(_)) => {
                    PrimaryDecision::Fail(RunFailure::ProviderCall)
                }
            }
        }
        PrimarySite::Wave { .. } if wave_deferred(&result, admitted, facts) => {
            PrimaryDecision::Defer
        }
        PrimarySite::Wave { .. } => match result {
            CallResult::Response(ProviderResponse {
                outcome: ProviderOutcome::Tools(requests),
                turn,
                ..
            }) if admitted => PrimaryDecision::ToolStep(requests, turn),
            CallResult::Response(_) | CallResult::Failed(_) | CallResult::Cancelled
                if facts.cancel_requested =>
            {
                PrimaryDecision::Stop(StopCause::Cancelled)
            }
            // The token budget refused the next action.
            CallResult::Response(_) if !facts.tokens_within_limit => {
                PrimaryDecision::Stop(StopCause::Budget)
            }
            CallResult::Response(_) | CallResult::Failed(_) | CallResult::Cancelled => {
                PrimaryDecision::Stop(StopCause::ToolStopped)
            }
        },
    }
}

#[cfg(test)]
mod decision_tests {
    use super::*;
    use crate::provider::ProviderTurn;

    #[derive(Debug, PartialEq)]
    enum Kind {
        Plan,
        Finish,
        Delegate,
        ToolStep,
        Refused(ProposalRefusalCause),
        RefusedTool(ProposalRefusalCause),
        CorrectOutcome,
        Defer,
        Stop(StopCause),
        Fail(RunFailure),
    }

    // (site, call, validity, facts, decision, journaled as admitted)
    type Row = (
        PrimarySite,
        CallResult,
        Result<(), ProposalRefusalCause>,
        RunFacts,
        Kind,
        bool,
    );

    fn kind(decision: &PrimaryDecision) -> Kind {
        match decision {
            PrimaryDecision::Plan(..) => Kind::Plan,
            PrimaryDecision::Finish(..) => Kind::Finish,
            PrimaryDecision::Delegate(..) => Kind::Delegate,
            PrimaryDecision::ToolStep(..) => Kind::ToolStep,
            PrimaryDecision::Refused(cause) => Kind::Refused(*cause),
            PrimaryDecision::RefusedTool(cause, ..) => Kind::RefusedTool(*cause),
            PrimaryDecision::CorrectOutcome => Kind::CorrectOutcome,
            PrimaryDecision::Defer => Kind::Defer,
            PrimaryDecision::Stop(cause) => Kind::Stop(*cause),
            PrimaryDecision::Fail(failure) => Kind::Fail(*failure),
        }
    }

    fn response(outcome: ProviderOutcome) -> CallResult {
        CallResult::Response(ProviderResponse {
            outcome,
            turn: ProviderTurn::new("test", "[{}]", "absent-secret", 4096).expect("minimal turn"),
            response_id: None,
            usage: Usage::default(),
            wire_provenance: None,
        })
    }

    fn answer() -> CallResult {
        response(ProviderOutcome::Answer(Answer {
            text: "done".into(),
        }))
    }

    #[test]
    fn optional_plan_limits_cannot_overflow_a_delegated_call_record() {
        let mut result = response(ProviderOutcome::Delegate(crate::provider::Delegate {
            agent: None,
            children: (0..8)
                .map(|_| crate::provider::ChildAssignment {
                    objective: "\0".repeat(1300),
                    profile: None,
                    editor: None,
                })
                .collect(),
        }));
        let sample = crate::test_support::routing::plan_sample(1000);
        let make_record = |result: &CallResult, limits| {
            call_record(AgentPhase::RootPlan, result, true, None, None, limits)
        };
        let small = make_record(&result, Some(sample.clone()));
        assert_eq!(small.rate_limits, Some(sample.clone()));
        let base_size = serde_json::to_vec(&make_record(&result, None))
            .unwrap()
            .len();
        let CallResult::Response(response) = &mut result else {
            unreachable!()
        };
        let ProviderOutcome::Delegate(delegation) = &mut response.outcome else {
            unreachable!()
        };
        let mut remaining = crate::store::MAX_EVENT_BYTES - base_size;
        for child in &mut delegation.children {
            let added = remaining.min(2048 - child.objective.len());
            child.objective.push_str(&"x".repeat(added));
            remaining -= added;
        }
        assert_eq!(remaining, 0);
        let record = make_record(&result, Some(sample.clone()));
        assert!(record.rate_limits.is_none());
        let mut event = Event::ProviderCallRecorded {
            run_id: RunId::new(),
            agent_run_id: AgentRunId::new(),
            record,
        };
        assert_eq!(
            event.payload().unwrap().len(),
            crate::store::MAX_EVENT_BYTES
        );
        let Event::ProviderCallRecorded { record, .. } = &mut event else {
            unreachable!()
        };
        record.rate_limits = Some(sample);
        assert!(
            event.payload().is_err(),
            "replay still rejects oversized records"
        );
    }

    #[test]
    fn invalid_outcome_failure_reason_is_recorded() {
        let result = CallResult::Failed(CallFailure::InvalidResponse(Some(
            ProviderFailureReason::OutcomeDecode,
        )));
        let record = call_record(AgentPhase::RootPlan, &result, false, None, None, None);
        assert_eq!(record.disposition, ProviderCallDisposition::InvalidResponse);
        assert_eq!(
            record.failure_reason,
            Some(ProviderFailureReason::OutcomeDecode)
        );
    }

    fn tools(count: usize) -> CallResult {
        response(ProviderOutcome::Tools(
            (0..count)
                .map(|index| crate::ToolRequest {
                    id: crate::provider::ToolUseId::new(format!("call-{index}")).expect("id"),
                    call: crate::tools::ToolCall::List { path: ".".into() },
                })
                .collect(),
        ))
    }

    fn plan() -> CallResult {
        response(ProviderOutcome::Plan(Box::new(
            crate::provider::PlanRequest {
                id: crate::provider::ToolUseId::new("plan").expect("id"),
                contract: crate::session::GoalPlanContract {
                    mission: "m".into(),
                    paths: vec![".".into()],
                    commands: Vec::new(),
                    resource_class: crate::tools::ToolResourceClass::Default,
                    command_timeout_ms: None,
                    skills: Vec::new(),
                    mcp_tools: Vec::new(),
                    success_criterion: crate::session::GoalSuccessCriterion::WorkspaceFileContent {
                        path: "a".into(),
                        content: "b".into(),
                    },
                    budget: crate::session::GoalBudget {
                        max_provider_calls: None,
                        max_tokens: None,
                        max_active_ms: None,
                        max_runs: 1,
                    },
                },
            },
        )))
    }

    #[test]
    fn primary_calls_decide_by_site_outcome_and_facts() {
        let open = RunFacts::ready().with(|facts| {
            facts.settleable = false;
            facts.workers_settled = false;
        });
        let cancel = open.with(|facts| facts.cancel_requested = true);
        let lapsed = open.with(|facts| facts.active_time_exhausted = true);
        let over_tokens = open.with(|facts| facts.tokens_within_limit = false);
        let wave = PrimarySite::Wave {
            queue_stopped: false,
        };
        let stopped_wave = PrimarySite::Wave {
            queue_stopped: true,
        };
        let loop_ = PrimarySite::Loop;
        let delegate = || {
            response(ProviderOutcome::Delegate(crate::provider::Delegate {
                agent: None,
                children: Vec::new(),
            }))
        };
        let review = || {
            response(ProviderOutcome::Review(
                crate::provider::ReviewVerdict::Approve,
            ))
        };
        let timeout = || CallResult::Failed(CallFailure::Timeout);
        let ok = Ok(());
        let refused = Err(ProposalRefusalCause::AnswerInvalid);
        let lease = Err(ProposalRefusalCause::LeaseOverlap);
        let malformed = || {
            CallResult::Failed(CallFailure::InvalidResponse(Some(
                ProviderFailureReason::OutcomeContract,
            )))
        };
        let corrective = open.with(|facts| facts.outcome_correction_pending = true);
        let rows: Vec<Row> = vec![
            (
                loop_,
                malformed(),
                ok,
                corrective,
                Kind::CorrectOutcome,
                false,
            ),
            (
                loop_,
                malformed(),
                ok,
                open,
                Kind::Fail(RunFailure::ProviderCall),
                false,
            ),
            (
                loop_,
                malformed(),
                ok,
                corrective.with(|facts| facts.calls_left = false),
                Kind::Stop(StopCause::Budget),
                false,
            ),
            (
                loop_,
                malformed(),
                ok,
                corrective.with(|facts| facts.tokens_open = false),
                Kind::Stop(StopCause::Budget),
                false,
            ),
            (
                loop_,
                malformed(),
                ok,
                corrective.with(|facts| facts.uncertain = true),
                Kind::Stop(StopCause::Uncertain),
                false,
            ),
            (
                loop_,
                malformed(),
                ok,
                corrective.with(|facts| facts.cancel_requested = true),
                Kind::Stop(StopCause::Cancelled),
                false,
            ),
            (loop_, answer(), ok, open, Kind::Finish, true),
            (
                loop_,
                answer(),
                ok,
                cancel,
                Kind::Stop(StopCause::Cancelled),
                true,
            ),
            (
                loop_,
                CallResult::Cancelled,
                ok,
                open,
                Kind::Stop(StopCause::Cancelled),
                false,
            ),
            (
                loop_,
                answer(),
                ok,
                lapsed,
                Kind::Stop(StopCause::ActiveTime),
                true,
            ),
            (
                loop_,
                answer(),
                ok,
                open.with(|facts| {
                    facts.cancel_requested = true;
                    facts.active_time_exhausted = true;
                }),
                Kind::Stop(StopCause::Cancelled),
                true,
            ),
            (
                loop_,
                answer(),
                refused,
                open,
                Kind::Refused(ProposalRefusalCause::AnswerInvalid),
                false,
            ),
            (
                loop_,
                answer(),
                refused,
                lapsed,
                Kind::Stop(StopCause::ActiveTime),
                false,
            ),
            (
                loop_,
                answer(),
                ok,
                over_tokens,
                Kind::Stop(StopCause::Budget),
                false,
            ),
            (
                loop_,
                delegate(),
                ok,
                open.with(|facts| facts.tokens_open = false),
                Kind::Stop(StopCause::Budget),
                false,
            ),
            (
                loop_,
                delegate(),
                ok,
                open.with(|facts| facts.calls_left = false),
                Kind::Delegate,
                true,
            ),
            (
                loop_,
                tools(1),
                ok,
                open.with(|facts| facts.calls_left = false),
                Kind::Stop(StopCause::Budget),
                false,
            ),
            (loop_, tools(1), ok, open, Kind::ToolStep, true),
            (
                loop_,
                tools(1),
                Err(ProposalRefusalCause::InvalidPath),
                open,
                Kind::RefusedTool(ProposalRefusalCause::InvalidPath),
                false,
            ),
            (
                loop_,
                tools(0),
                Err(ProposalRefusalCause::InvalidArguments),
                open,
                Kind::Fail(RunFailure::ProviderCall),
                false,
            ),
            (loop_, plan(), ok, open, Kind::Plan, true),
            (
                loop_,
                review(),
                ok,
                open,
                Kind::Fail(RunFailure::ProviderCall),
                true,
            ),
            (
                loop_,
                review(),
                ok,
                over_tokens,
                Kind::Stop(StopCause::Budget),
                false,
            ),
            (
                loop_,
                timeout(),
                ok,
                open,
                Kind::Fail(RunFailure::ProviderCall),
                false,
            ),
            (
                loop_,
                timeout(),
                ok,
                lapsed,
                Kind::Stop(StopCause::ActiveTime),
                false,
            ),
            (wave, tools(1), ok, open, Kind::ToolStep, true),
            (wave, tools(1), ok, lapsed, Kind::ToolStep, true),
            (
                wave,
                tools(1),
                ok,
                open.with(|facts| facts.calls_left = false),
                Kind::ToolStep,
                true,
            ),
            (stopped_wave, tools(1), ok, open, Kind::Defer, false),
            (wave, tools(1), lease, open, Kind::Defer, false),
            (
                wave,
                answer(),
                Err(ProposalRefusalCause::Phase),
                open,
                Kind::Defer,
                false,
            ),
            (
                wave,
                answer(),
                Err(ProposalRefusalCause::Phase),
                cancel,
                Kind::Stop(StopCause::Cancelled),
                false,
            ),
            (
                wave,
                tools(1),
                ok,
                over_tokens,
                Kind::Stop(StopCause::Budget),
                false,
            ),
            (
                wave,
                tools(1),
                ok,
                open.with(|facts| {
                    facts.tokens_within_limit = false;
                    facts.cancel_requested = true;
                }),
                Kind::Stop(StopCause::Cancelled),
                false,
            ),
            (
                wave,
                timeout(),
                ok,
                over_tokens,
                Kind::Stop(StopCause::ToolStopped),
                false,
            ),
            (
                wave,
                timeout(),
                ok,
                open,
                Kind::Stop(StopCause::ToolStopped),
                false,
            ),
            (
                wave,
                timeout(),
                ok,
                cancel,
                Kind::Stop(StopCause::Cancelled),
                false,
            ),
            (
                wave,
                CallResult::Cancelled,
                ok,
                open,
                Kind::Stop(StopCause::ToolStopped),
                false,
            ),
        ];
        for (index, (site, result, validity, facts, expected, admitted)) in
            rows.into_iter().enumerate()
        {
            assert_eq!(
                primary_admitted(site, &result, validity, &facts),
                admitted,
                "row {index}: {site:?} {facts:?}"
            );
            assert_eq!(
                kind(&decide_primary_call(site, result, validity, &facts)),
                expected,
                "row {index}: {site:?} {facts:?}"
            );
        }
    }
}

#[cfg(test)]
mod call_bound_tests {
    use super::*;
    use crate::provider::{
        AgentPhase, Answer, ChatGptAdmission, ChatGptProvenance, ChatGptProvider,
        CompactionRequest, CompactionResponse, Effort, OutputTokenBound, PromptRole, Provider,
        ProviderError, ProviderIdentity, ProviderRequest, ProviderTurn, RunInput, Usage,
    };
    use crate::session::{AgentRunId, CollaborationPolicy, RunId, SessionId};

    struct Slow {
        identity: ProviderIdentity,
        delay: Duration,
    }

    #[async_trait::async_trait]
    impl Provider for Slow {
        fn identity(&self) -> &ProviderIdentity {
            &self.identity
        }

        async fn invoke_observed(
            &self,
            request: ProviderRequest,
            _preview: Option<ResponsePreview>,
            limits: RateLimitSlot,
        ) -> Result<ProviderResponse, ProviderError> {
            if self.identity.chatgpt_provenance().is_some() {
                limits.record(crate::test_support::routing::plan_sample(1234));
            }
            self.invoke(request).await
        }

        async fn invoke(&self, _: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
            tokio::time::sleep(self.delay).await;
            Ok(ProviderResponse {
                outcome: ProviderOutcome::Answer(Answer {
                    text: "Synthetic answer".into(),
                }),
                turn: ProviderTurn::scripted(),
                response_id: None,
                // A reasoning-heavy total: well above the cap, which the route checks on visible output.
                usage: Usage {
                    input_tokens: Some(20),
                    output_tokens: Some(9_000),
                    ..Usage::default()
                },
                wire_provenance: None,
            })
        }

        async fn compact(&self, _: CompactionRequest) -> Result<CompactionResponse, ProviderError> {
            Err(ProviderError::Rejected)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_chatgpt_ceiling_cuts_a_slow_call_retryably_and_only_local_bounds_admit_large_totals()
     {
        let chatgpt = ChatGptProvider::from_checked_account(
            "model-a".into(),
            Effort::High,
            "synthetic-subscription-token".into(),
            ChatGptProvenance {
                account_id: uuid::Uuid::now_v7(),
                evidence_fingerprint: [7; 32],
                admission: ChatGptAdmission::AccountConsent,
            },
        )
        .expect("synthetic account")
        .identity()
        .clone();
        let native = ProviderIdentity::new("openai", "model-a", crate::Provenance::native());
        assert_eq!(
            native.limits.output_token_bound,
            OutputTokenBound::ProviderEnforced
        );
        let ceiling = chatgpt.limits.deadline;
        for (label, identity, delay, accepted) in [
            (
                "ChatGPT within the ceiling",
                &chatgpt,
                ceiling - Duration::from_secs(1),
                true,
            ),
            (
                "ChatGPT past the ceiling",
                &chatgpt,
                ceiling + Duration::from_secs(1),
                false,
            ),
            ("provider-enforced cap", &native, Duration::ZERO, false),
        ] {
            let provider = Slow {
                identity: identity.clone(),
                delay,
            };
            let request = ProviderRequest {
                session_id: SessionId::new(),
                run_id: RunId::new(),
                agent_run_id: AgentRunId::new(),
                phase: AgentPhase::RootPlan,
                collaboration: CollaborationPolicy::Single,
                model: "model-a".into(),
                max_output_tokens: identity.limits.max_output_tokens,
                input: RunInput::new(PromptRole::ReadOnly, "Synthetic objective".into()),
            };
            let limits = RateLimitSlot::default();
            let started = Instant::now();
            let (result, transient) = invoke_bounded_with_preview(
                &provider,
                request,
                started + Duration::from_secs(1200),
                RunCancellation::new(),
                None,
                limits.clone(),
                crate::fault::LatencyReceipt::default(),
            )
            .await;
            assert_eq!(
                limits.take(),
                identity
                    .chatgpt_provenance()
                    .map(|_| crate::test_support::routing::plan_sample(1234))
            );
            // A ChatGPT ceiling cut is a stream cut; the router decides whether a retry still fits.
            assert_eq!(
                transient.map(|transient| transient.cause),
                (delay > ceiling).then_some(crate::provider::RetryCause::StreamCut),
                "{label}"
            );
            match result {
                CallResult::Response(response) => {
                    assert!(accepted, "{label}");
                    assert_eq!(response.usage.output_tokens, Some(9_000), "{label}");
                }
                CallResult::Failed(CallFailure::Timeout) => {
                    assert!(!accepted && delay > ceiling, "{label}");
                    assert_eq!(started.elapsed(), ceiling, "{label}");
                }
                CallResult::Failed(CallFailure::OutputLimit) => {
                    assert!(!accepted && delay.is_zero(), "{label}");
                }
                _ => panic!("{label}: unexpected call result"),
            }
        }
    }
}

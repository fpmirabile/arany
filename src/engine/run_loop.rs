use super::ledger::{Action, GoalLedger, duration_ms};
use super::progress::{ProgressPublisher, RunScope, append_scoped};
use super::{EngineError, RunCancellation, RunSteering, STORE_ACTION_OVERHEAD};
use crate::provider::{
    AgentPhase, Finish, MAX_REPORTED_INPUT_TOKENS, PreviewText, Provider, ProviderFailureClass,
    ProviderFailureReason, ProviderOutcome, ProviderRequest, ProviderResponse, ResponsePreview,
    Usage,
};
use crate::session::{
    AgentDisposition, AgentRunId, CollaborationPolicy, DisplayCategory, DisplayChunk, Event,
    GoalPhase, MAX_ASSISTANT_MESSAGE_BYTES as MAX_PRIMARY_RESULT_BYTES, MAX_DISPLAY_CHUNK_BYTES,
    MAX_DISPLAY_CHUNKS, MAX_DISPLAY_COMMENTARY_ITEMS, MAX_DISPLAY_TEXT_BYTES,
    ProviderCallDisposition, ProviderCallRecord, RunDisposition, RunId, SessionId,
};
use crate::store::Store;
use crate::telemetry::{AgentTrace, ProviderTraceOutcome, RunTrace, TraceFailure, TraceOutcome};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

mod children;
mod step;
mod tool_compaction;

use step::Grant;

const MAX_FINISH_SUMMARY_BYTES: usize = 2 * 1024;
const TOOL_CLEANUP_OVERHEAD: Duration = Duration::from_secs(10);
/// Wall-clock bound of one Tool-review call, excluding its Store overhead.
const TOOL_REVIEW_DEADLINE: Duration = Duration::from_secs(20);

pub(super) enum CallResult {
    Response(ProviderResponse),
    Failed(CallFailure),
    Cancelled,
}

pub(super) enum CallFailure {
    Timeout,
    Unavailable(Option<ProviderFailureReason>),
    Rejected(Option<ProviderFailureReason>),
    InvalidResponse(Option<ProviderFailureReason>),
    OutputLimit,
    TaskPanic,
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
        }
    }
}

pub(super) struct RunLoop<'a> {
    store: &'a Store,
    routing: super::routing::CallRouter<'a>,
    session_id: SessionId,
    run_id: RunId,
    goal_id: crate::session::GoalId,
    primary_id: AgentRunId,
    controls: RunControls<'a>,
    ledger: &'a GoalLedger,
    progress: Option<&'a ProgressPublisher>,
    trace: &'a RunTrace,
}

pub(super) struct RunControls<'a> {
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
    pub(super) goal_id: crate::session::GoalId,
    pub(super) primary_id: AgentRunId,
}

impl<'a> RunLoop<'a> {
    pub(super) fn new(
        store: &'a Store,
        providers: crate::provider::AdmittedProviders,
        identity: RunIdentity,
        controls: RunControls<'a>,
        ledger: &'a GoalLedger,
        progress: Option<&'a ProgressPublisher>,
        trace: &'a RunTrace,
    ) -> Self {
        Self {
            routing: super::routing::CallRouter {
                providers,
                store,
                ledger,
                publisher: progress,
            },
            store,
            session_id: identity.session_id,
            run_id: identity.run_id,
            goal_id: identity.goal_id,
            primary_id: identity.primary_id,
            controls,
            ledger,
            progress,
            trace,
        }
    }

    fn ledger(&self) -> &GoalLedger {
        self.ledger
    }

    fn scope(&self) -> RunScope<'_> {
        RunScope {
            ledger: self.ledger,
            publisher: self.progress,
        }
    }

    fn settle_result(&self, result: &CallResult) {
        let (input, output) = call_trace_usage(result);
        self.ledger().settle(input, output);
    }

    async fn append(&self, event: Event) -> Result<(), EngineError> {
        let _ = append_scoped(
            self.store,
            self.scope(),
            Some(self.trace),
            self.session_id,
            event,
        )
        .await?;
        Ok(())
    }

    /// Closes the primary after its loop stopped early. Exactly one exit applies: an uncertain
    /// Tool, an open reservation or an unacknowledged action returns an error and leaves the Run
    /// for recovery; a cancellation, including a pause, cancels the primary; otherwise the Goal
    /// budget ran out and the primary fails.
    async fn settle_stopped_primary(&self, primary: AgentTrace) -> Result<(), EngineError> {
        let view = self
            .store
            .load_open_view(self.session_id)
            .await?
            .ok_or(EngineError::InvalidHistory)?;
        let goal = view.goal(self.goal_id).ok_or(EngineError::InvalidHistory)?;
        let run = view
            .runs
            .iter()
            .find(|run| run.id == self.run_id)
            .ok_or(EngineError::InvalidHistory)?;
        let primary_active = run.agents.iter().any(|agent| {
            agent.id == self.primary_id && agent.status == crate::session::AgentStatus::Active
        });
        let other_agents_closed = run.agents.iter().all(|agent| {
            agent.id == self.primary_id || agent.status != crate::session::AgentStatus::Active
        });
        let has_uncertain_tool = run.tools.iter().any(|tool| {
            tool.observation.as_ref().is_some_and(|observation| {
                observation.disposition == crate::tools::ToolDisposition::Uncertain
            })
        });
        let action_cancelled = run.tools.iter().any(|tool| {
            tool.observation.as_ref().is_some_and(|observation| {
                observation.disposition == crate::tools::ToolDisposition::Cancelled
            })
        });
        let actions_acknowledged = run.tools.iter().all(|tool| {
            tool.observation.as_ref().is_some_and(|observation| {
                observation.disposition != crate::tools::ToolDisposition::Uncertain
            })
        });
        if has_uncertain_tool {
            return Err(crate::tools::ToolError::Uncertain.into());
        }
        if goal.status != crate::session::GoalStatus::Running
            || goal.active_run != Some(self.run_id)
            || self.ledger().active_reserved()
            || !goal.unobserved_provider_calls.is_empty()
            || goal.compaction_call_reserved.is_some()
            || run.status != crate::session::RunStatus::Active
            || !primary_active
            || !other_agents_closed
        {
            return Err(EngineError::GoalBudgetExhausted);
        }
        let cancelled = self.controls.cancellation.is_cancelled() || action_cancelled;
        if !cancelled && !actions_acknowledged {
            return Err(EngineError::GoalBudgetExhausted);
        }
        if cancelled {
            self.cancel_primary(primary).await
        } else {
            self.fail_primary(primary).await
        }
    }

    fn reserve_active_segment(&self, action_bound: Duration) -> Result<ActiveSegment, EngineError> {
        let (started, reserved_ms) = self.ledger().reserve_active(duration_ms(action_bound))?;
        Ok(ActiveSegment {
            started,
            reserved_ms,
        })
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

    fn reserve_active_pause(&self) -> Result<ActivePause, EngineError> {
        let segment = self.reserve_active_segment(Duration::ZERO)?;
        Ok(ActivePause {
            segment,
            paused_at: Instant::now(),
        })
    }

    fn settle_active_pause(
        &self,
        pause: ActivePause,
        resume_at: Option<Instant>,
    ) -> Result<(), EngineError> {
        self.settle_active_segment_at(pause.segment, pause.paused_at, resume_at)
    }

    /// `pending_ms` bounds an action still to run once this call settles, such as a reviewed Tool.
    async fn reserve_provider_call(
        &self,
        agent_run_id: AgentRunId,
        action: Action,
        pending_ms: u64,
    ) -> Result<(), EngineError> {
        let reservation = self.ledger().reserve(action).await?;
        self.store
            .admit_operation(
                self.session_id,
                action.max_events_for(&self.routing.providers),
                action.tool_observations(),
            )
            .await?;
        self.append(Event::GoalProviderCallReserved {
            goal_id: self.goal_id,
            run_id: self.run_id,
            agent_run_id,
            active_ceiling_ms: self.ledger().active_ceiling(pending_ms)?,
        })
        .await?;
        reservation.commit()
    }

    fn primary_call(&self) -> Action {
        Action::ProviderCall {
            previews: self
                .progress
                .is_some_and(ProgressPublisher::previews_enabled),
            steering: self.controls.steering.is_some(),
        }
    }

    /// Journals the waiting steering note inside the open primary reservation and adds it to this call,
    /// after the step whose results it accompanies.
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
            goal_id: self.goal_id,
            run_id: self.run_id,
            text: text.clone(),
        })
        .await?;
        steering.note_recorded();
        match request
            .input
            .tools
            .as_mut()
            .and_then(|tools| tools.steps.last_mut())
        {
            Some(step) => step.steering_notes.push(text),
            None => request.input.steering_notes.push(text),
        }
        Ok(())
    }

    /// Whether the caller asked to pause; unlike `pause_at_boundary` it never cancels in-flight calls.
    pub(super) fn pause_requested(&self) -> bool {
        self.controls
            .steering
            .as_ref()
            .is_some_and(RunSteering::pause_requested)
    }

    /// Converts a requested pause into this Run's cancellation; callers stop at their boundary.
    pub(super) fn pause_at_boundary(&self) -> bool {
        let requested = self.pause_requested();
        if requested {
            self.controls.cancellation.cancel();
        }
        requested
    }

    /// Records an admitted Plan contract and narrows this Run to it; `false` means the Run must fail.
    /// The request's own goal block stays as the Run began, so every later request extends the last.
    pub(super) async fn admit_plan_contract(
        &self,
        contract: crate::session::GoalPlanContract,
    ) -> Result<bool, EngineError> {
        if !self.ledger().can_narrow(&contract) {
            return Ok(false);
        }
        self.store
            .admit_operation(
                self.session_id,
                Action::Tool.remaining(0, &self.routing.providers),
                Action::Tool.tool_observations(),
            )
            .await?;
        self.append(Event::GoalPlanContractRecorded {
            goal_id: self.goal_id,
            run_id: self.run_id,
            contract,
        })
        .await?;
        Ok(true)
    }

    pub(super) async fn execute(
        &self,
        mut root_request: ProviderRequest,
    ) -> Result<(), EngineError> {
        let primary = self.trace.begin_primary(self.primary_id);
        let mut first_call = true;
        loop {
            if self.pause_at_boundary() {
                return self.settle_stopped_primary(primary).await;
            }
            let call_provider = self
                .routing
                .provider(crate::provider::ProviderRole::Primary, self.primary_id)?;
            root_request.model = call_provider.identity().model.clone();
            let first_trace = first_call.then(|| primary.begin_first_provider());
            let next_trace = (!first_call)
                .then(|| primary.begin_provider(AgentPhase::RootPlan, call_provider.identity()));
            first_call = false;
            let (first, active_segment) = match self.invoke_primary(&mut root_request).await {
                Ok(result) => result,
                Err(EngineError::GoalBudgetExhausted) => {
                    if let Some(first_trace) = first_trace {
                        first_trace.finish_refused(&primary, &root_request.model);
                    }
                    if let Some(next_trace) = next_trace {
                        next_trace.finish_refused();
                    }
                    return self.settle_stopped_primary(primary).await;
                }
                Err(error) => return Err(error),
            };
            let (input_tokens, output_tokens) = call_trace_usage(&first);
            let valid_response = self.ledger().tokens_within_limit()
                && match &first {
                    CallResult::Response(ProviderResponse {
                        outcome: ProviderOutcome::Finish(finish),
                        ..
                    }) => {
                        !matches!(self.controls.policy, CollaborationPolicy::Team { .. })
                            && valid_finish(finish, MAX_PRIMARY_RESULT_BYTES)
                    }
                    CallResult::Response(ProviderResponse {
                        outcome: ProviderOutcome::Delegate(delegation),
                        ..
                    }) => self.valid_delegation(&delegation.children),
                    CallResult::Response(ProviderResponse {
                        outcome: ProviderOutcome::Tools(requests),
                        ..
                    }) => self.valid_tools(requests),
                    _ => false,
                };
            if let Some(first_trace) = first_trace {
                first_trace.finish(
                    &primary,
                    &root_request.model,
                    matches!(
                        &first,
                        CallResult::Response(ProviderResponse {
                            outcome: ProviderOutcome::Delegate(_),
                            ..
                        })
                    ),
                    call_trace_result(&first, valid_response),
                    input_tokens,
                    output_tokens,
                );
            }
            if let Some(next_trace) = next_trace {
                next_trace.finish_with_usage(
                    call_trace_result(&first, valid_response),
                    input_tokens,
                    output_tokens,
                );
            }
            self.append(Event::ProviderCallRecorded {
                run_id: self.run_id,
                agent_run_id: self.primary_id,
                record: call_record(AgentPhase::RootPlan, &first, valid_response),
            })
            .await?;
            self.settle_active_segment(active_segment)?;
            if self.controls.cancellation.is_cancelled() {
                return self.settle_stopped_primary(primary).await;
            }
            if matches!(&first, CallResult::Cancelled) {
                return self.cancel_primary(primary).await;
            }
            if self.ledger().active_time_exhausted() {
                return self.settle_stopped_primary(primary).await;
            }
            match first {
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Finish(finish),
                    ..
                }) if valid_response => return self.finish_primary(finish, primary).await,
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Delegate(delegation),
                    turn,
                    ..
                }) if valid_response => {
                    if self.pause_at_boundary() {
                        return self.settle_stopped_primary(primary).await;
                    }
                    return self
                        .run_children(&mut root_request, delegation.children, turn, primary)
                        .await;
                }
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Tools(requests),
                    turn,
                    ..
                }) if valid_response => {
                    if self.pause_at_boundary() {
                        return self.settle_stopped_primary(primary).await;
                    }
                    let Some(request) = self.begin_tool_step(requests).await? else {
                        return self.fail_primary(primary).await;
                    };
                    let grant = Grant::Primary {
                        request: &mut root_request,
                        review_allowed: self.ledger().admits(Action::Review),
                        trace: &primary,
                    };
                    let continuation = self.execute_step(grant, request, turn).await;
                    match continuation {
                        Ok(true) => {}
                        Ok(false) if self.controls.cancellation.is_cancelled() => {
                            return self.settle_stopped_primary(primary).await;
                        }
                        Ok(false) if self.ledger().active_time_exhausted() => {
                            return self.settle_stopped_primary(primary).await;
                        }
                        Ok(false) => return self.fail_primary(primary).await,
                        Err(EngineError::GoalBudgetExhausted)
                            if self.controls.cancellation.is_cancelled() =>
                        {
                            return self.settle_stopped_primary(primary).await;
                        }
                        Err(EngineError::GoalBudgetExhausted) => {
                            return self.settle_stopped_primary(primary).await;
                        }
                        Err(error) => return Err(error),
                    }
                }
                CallResult::Cancelled => return self.cancel_primary(primary).await,
                _ => return self.fail_primary(primary).await,
            }
        }
    }

    /// Each primary call carries every steering note this Run has recorded so far, in order.
    async fn invoke_primary(
        &self,
        root: &mut ProviderRequest,
    ) -> Result<(CallResult, ActiveSegment), EngineError> {
        let provider = self
            .routing
            .provider(crate::provider::ProviderRole::Primary, self.primary_id)?;
        root.model = provider.identity().model.clone();
        root.max_output_tokens = root
            .max_output_tokens
            .min(provider.identity().limits.max_output_tokens);
        let active_segment = self.reserve_active_segment(provider.identity().limits.deadline)?;
        let deadline = active_segment
            .deadline()
            .checked_sub(STORE_ACTION_OVERHEAD)
            .unwrap_or(active_segment.started);
        if let Err(error) = self
            .reserve_provider_call(self.primary_id, self.primary_call(), 0)
            .await
        {
            if matches!(error, EngineError::GoalBudgetExhausted) {
                self.settle_active_segment(active_segment)?;
            }
            return Err(error);
        }
        self.record_steering_note(root).await?;
        let facts = self.ledger().context_facts()?;
        match root
            .input
            .tools
            .as_mut()
            .and_then(|tools| tools.steps.last_mut())
        {
            Some(step) => step.facts = Some(facts),
            None => root.input.goal_context_facts = Some(facts),
        }
        let request = root.clone();
        let context = self
            .progress
            .map(|progress| progress.preview_context(self.ledger, self.primary_id))
            .transpose()?
            .flatten();
        let Some(context) = context else {
            let result = self
                .routing
                .invoke(request, deadline, self.controls.cancellation.clone(), None)
                .await?;
            self.settle_result(&result);
            return Ok((result, active_segment));
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
                    self.progress.expect("preview owner").publish_preview(self.primary_id, call_index, Some(Arc::new(text)));
                }
            }
        };
        let text = checkpoint.filter(&receiver.borrow());
        let result = match result {
            CallResult::Response(response)
                if !text.answer.is_empty()
                    && !matches!(&response.outcome,
                ProviderOutcome::Finish(finish) if finish.result.starts_with(&text.answer)) =>
            {
                CallResult::Failed(CallFailure::InvalidResponse(Some(
                    ProviderFailureReason::ResponseContract,
                )))
            }
            result => result,
        };
        self.settle_result(&result);
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
        self.progress
            .expect("preview owner")
            .publish_preview(self.primary_id, call_index, None);
        Ok((result, active_segment))
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
            while body.len().saturating_sub(*offset) >= MAX_DISPLAY_CHUNK_BYTES
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

    /// The Engine accepts exactly one Tool request per step.
    fn valid_tools(&self, requests: &[crate::tools::ToolRequest]) -> bool {
        matches!(requests, [request] if self.valid_tool(&request.call, &request.plan_contract))
    }

    fn valid_tool(
        &self,
        call: &crate::tools::ToolCall,
        plan_contract: &Option<Box<crate::session::GoalPlanContract>>,
    ) -> bool {
        let (Ok(existing), Ok(budget)) = (
            self.ledger().plan_contract(),
            self.ledger().effective_budget(),
        ) else {
            return false;
        };
        // Restating the recorded contract is a no-op; only a different one is a widening attempt.
        let plan_contract = if existing.as_ref() == plan_contract.as_deref() {
            &None
        } else {
            plan_contract
        };
        let plan_and_call_fit = plan_contract.as_ref().is_none_or(|contract| {
            serde_json::to_vec(&(call, contract))
                .is_ok_and(|bytes| bytes.len() <= crate::tools::types::MAX_TOOL_ARGUMENT_BYTES)
        });
        let plan_valid = if let Some(existing) = &existing {
            plan_contract.is_none()
                && existing.allows_call(call)
                && self
                    .controls
                    .tools
                    .is_some_and(|tools| tools.allows_goal_contract(existing))
        } else if let Some(contract) = plan_contract {
            contract.budget.is_within(&budget)
                && contract.validate().is_ok()
                && contract.allows_call(call)
                && self
                    .controls
                    .tools
                    .is_some_and(|tools| tools.allows_goal_contract(contract))
        } else {
            call.is_goal_plan_prelude()
        };
        let contract_budget_available = plan_contract
            .as_ref()
            .is_none_or(|contract| self.ledger().can_narrow(contract));
        self.controls.tools.is_some()
            && call.valid()
            && (self.controls.goal_phase == GoalPhase::Implementation
                || call.is_goal_plan_prelude())
            && plan_and_call_fit
            && plan_valid
            && contract_budget_available
            && self.ledger().admits(Action::Tool)
            && !matches!(call, crate::tools::ToolCall::ApplyChangeset { .. })
    }

    /// Admits the request's Plan contract and returns the step's one request; `None` means the
    /// Run must fail.
    pub(super) async fn begin_tool_step(
        &self,
        requests: Vec<crate::tools::ToolRequest>,
    ) -> Result<Option<crate::tools::ToolRequest>, EngineError> {
        let Ok([mut request]) = <[_; 1]>::try_from(requests) else {
            return Ok(None);
        };
        if let Some(contract) = request.plan_contract.take()
            && self.ledger().plan_contract()?.as_ref() != Some(&*contract)
            && !self.admit_plan_contract(*contract).await?
        {
            return Ok(None);
        }
        Ok(Some(request))
    }

    async fn finish_primary(&self, finish: Finish, primary: AgentTrace) -> Result<(), EngineError> {
        if self.controls.cancellation.is_cancelled() {
            return self.cancel_primary(primary).await;
        }
        if !self.ledger().success_criterion_satisfied() && !self.ledger().allows_next_run() {
            return self.fail_primary(primary).await;
        }
        self.append(Event::AgentFinished {
            run_id: self.run_id,
            agent_run_id: self.primary_id,
            disposition: AgentDisposition::Finished,
            summary: Some(finish.summary),
            result: Some(finish.result.clone()),
        })
        .await?;
        self.append(Event::MessageCommitted {
            run_id: self.run_id,
            text: finish.result,
        })
        .await?;
        self.append(Event::RunFinished {
            run_id: self.run_id,
            disposition: RunDisposition::Finished,
            active_ms: self.ledger().run_active_ms()?,
        })
        .await?;
        primary.finish(TraceOutcome::Finished);
        Ok(())
    }

    async fn fail_primary(&self, primary: AgentTrace) -> Result<(), EngineError> {
        if self.controls.cancellation.is_cancelled() {
            return self.cancel_primary(primary).await;
        }
        self.append(Event::AgentFinished {
            run_id: self.run_id,
            agent_run_id: self.primary_id,
            disposition: AgentDisposition::Failed,
            summary: None,
            result: None,
        })
        .await?;
        self.append(Event::RunFinished {
            run_id: self.run_id,
            disposition: RunDisposition::Failed,
            active_ms: self.ledger().run_active_ms()?,
        })
        .await?;
        primary.finish(TraceOutcome::Failed);
        Ok(())
    }

    async fn cancel_primary(&self, primary: AgentTrace) -> Result<(), EngineError> {
        self.append(Event::AgentFinished {
            run_id: self.run_id,
            agent_run_id: self.primary_id,
            disposition: AgentDisposition::Cancelled,
            summary: None,
            result: None,
        })
        .await?;
        self.append(Event::RunFinished {
            run_id: self.run_id,
            disposition: RunDisposition::Cancelled,
            active_ms: self.ledger().run_active_ms()?,
        })
        .await?;
        primary.finish(TraceOutcome::Cancelled);
        Ok(())
    }
}

pub(super) async fn invoke_bounded_with_preview(
    provider: &dyn Provider,
    request: ProviderRequest,
    run_deadline: Instant,
    mut cancellation: RunCancellation,
    preview: Option<ResponsePreview>,
) -> CallResult {
    if cancellation.is_cancelled() {
        return CallResult::Cancelled;
    }
    let remaining = run_deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return CallResult::Failed(CallFailure::Timeout);
    }
    let timeout = provider.identity().limits.deadline.min(remaining);
    let output_cap = request.max_output_tokens;
    let invocation = async {
        match preview {
            Some(preview) => provider.invoke_with_preview(request, preview).await,
            None => provider.invoke(request).await,
        }
    };
    tokio::select! {
        biased;
        () = cancellation.cancelled() => CallResult::Cancelled,
        result = tokio::time::timeout(timeout, invocation) => match result {
            Ok(Ok(response))
                if response
                    .usage
                    .input_tokens
                    .is_some_and(|count| count > MAX_REPORTED_INPUT_TOKENS)
                    || !response.usage.cache_consistent() =>
            {
                CallResult::Failed(CallFailure::InvalidResponse(None))
            }
            Ok(Ok(response)) if response.usage.output_tokens.is_some_and(|count| count > output_cap) => {
                CallResult::Failed(CallFailure::OutputLimit)
            }
            Ok(Ok(response)) if response.response_id.as_ref().is_some_and(|id| {
                id.is_empty() || id.len() > 128 || !id.bytes().all(|byte| byte.is_ascii_graphic())
            }) => CallResult::Failed(CallFailure::InvalidResponse(None)),
            Ok(Ok(response)) if response.wire_provenance.is_some_and(|provenance| {
                !provenance.valid_for(
                    response.response_id.as_deref(),
                    response.usage.input_tokens,
                    response.usage.output_tokens,
                )
            }) => CallResult::Failed(CallFailure::InvalidResponse(None)),
            Ok(Ok(response)) => CallResult::Response(response),
            Ok(Err(error)) => match error.failure_class() {
                ProviderFailureClass::Unavailable => CallResult::Failed(CallFailure::Unavailable(error.failure_reason())),
                ProviderFailureClass::Rejected => CallResult::Failed(CallFailure::Rejected(error.failure_reason())),
                ProviderFailureClass::InvalidOutcome => {
                    CallResult::Failed(CallFailure::InvalidResponse(error.failure_reason()))
                }
            },
            Err(_) => CallResult::Failed(CallFailure::Timeout),
        }
    }
}

fn call_trace_result(result: &CallResult, valid_response: bool) -> ProviderTraceOutcome {
    match result {
        CallResult::Response(_) if valid_response => ProviderTraceOutcome::Finished,
        CallResult::Response(_) => ProviderTraceOutcome::Failed(TraceFailure::InvalidResponse),
        CallResult::Failed(failure) => ProviderTraceOutcome::Failed(match failure {
            CallFailure::Timeout => TraceFailure::Timeout,
            CallFailure::Unavailable(_) => TraceFailure::Unavailable,
            CallFailure::Rejected(_) => TraceFailure::Rejected,
            CallFailure::InvalidResponse(_) => TraceFailure::InvalidResponse,
            CallFailure::OutputLimit => TraceFailure::OutputLimit,
            CallFailure::TaskPanic => TraceFailure::TaskPanic,
        }),
        CallResult::Cancelled => ProviderTraceOutcome::Cancelled,
    }
}

fn call_trace_usage(result: &CallResult) -> (Option<u32>, Option<u32>) {
    match result {
        CallResult::Response(response) => {
            (response.usage.input_tokens, response.usage.output_tokens)
        }
        CallResult::Failed(_) | CallResult::Cancelled => (None, None),
    }
}

fn call_record(phase: AgentPhase, result: &CallResult, valid_response: bool) -> ProviderCallRecord {
    let disposition = match result {
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Finish(_),
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
        CallResult::Response(_) => ProviderCallDisposition::InvalidResponse,
        CallResult::Failed(failure) => match failure {
            CallFailure::Timeout => ProviderCallDisposition::TimedOut,
            CallFailure::Unavailable(_) => ProviderCallDisposition::Unavailable,
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
    ProviderCallRecord {
        phase,
        disposition,
        requested_tools: match result {
            CallResult::Response(ProviderResponse {
                outcome: ProviderOutcome::Tools(requests),
                ..
            }) if disposition == ProviderCallDisposition::ToolRequested => {
                u8::try_from(requests.len()).expect("a valid step carries one Tool request")
            }
            _ => 0,
        },
        response_id,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        wire_provenance,
        failure_reason: match result {
            CallResult::Failed(
                CallFailure::Unavailable(reason)
                | CallFailure::Rejected(reason)
                | CallFailure::InvalidResponse(reason),
            ) => *reason,
            _ => None,
        },
    }
}

fn valid_finish(finish: &Finish, result_limit: usize) -> bool {
    !finish.summary.is_empty()
        && finish.summary.len() <= MAX_FINISH_SUMMARY_BYTES
        && !finish.result.is_empty()
        && finish.result.len() <= result_limit
        && Event::finished_payload_size(&finish.summary, &finish.result)
            .is_ok_and(|bytes| bytes <= crate::store::MAX_EVENT_BYTES)
}

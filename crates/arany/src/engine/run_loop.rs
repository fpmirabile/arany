use super::hints::HintPublisher;
use super::ledger::{Action, GoalLedger, duration_ms};
use super::{EngineError, RunCancellation, RunSteering, STORE_ACTION_OVERHEAD};
use crate::journal::Journal;
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
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

mod children;
pub(super) mod step;
mod tool_compaction;
mod worker;

use step::Grant;

const MAX_FINISH_SUMMARY_BYTES: usize = 2 * 1024;
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
        fallback: bool,
    },
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
    journal: &'a Journal,
    routing: super::routing::CallRouter<'a>,
    session_id: SessionId,
    run_id: RunId,
    goal_id: Option<crate::session::GoalId>,
    primary_id: AgentRunId,
    controls: LoopControls<'a>,
    ledger: &'a GoalLedger,
    hints: Option<&'a HintPublisher>,
    worker_data_bytes: std::sync::Mutex<std::collections::HashMap<AgentRunId, u32>>,
    worker_requests:
        std::sync::Mutex<std::collections::HashMap<AgentRunId, crate::worker::EditorAssignment>>,
    worker: tokio::sync::Mutex<std::collections::HashMap<AgentRunId, crate::tools::worker::View>>,
    handover: std::sync::Mutex<std::collections::HashMap<AgentRunId, std::sync::Arc<[u8]>>>,
    definitions: std::sync::OnceLock<
        std::collections::BTreeMap<String, crate::tools::definitions::AgentDefinition>,
    >,
}

pub(super) struct LoopControls<'a> {
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

impl<'a> RunLoop<'a> {
    pub(super) fn new(
        journal: &'a Journal,
        providers: crate::provider::AdmittedProviders,
        identity: RunIdentity,
        controls: LoopControls<'a>,
        ledger: &'a GoalLedger,
        hints: Option<&'a HintPublisher>,
    ) -> Self {
        Self {
            routing: super::routing::CallRouter {
                providers,
                journal,
                ledger,
                publisher: hints,
            },
            worker_data_bytes: Default::default(),
            worker_requests: Default::default(),
            worker: Default::default(),
            handover: Default::default(),
            definitions: std::sync::OnceLock::new(),
            journal,
            session_id: identity.session_id,
            run_id: identity.run_id,
            goal_id: identity.goal_id,
            primary_id: identity.primary_id,
            controls,
            ledger,
            hints,
        }
    }

    // Only a fork boundary hook runs without a Goal.
    pub(super) fn goal(&self) -> Result<crate::session::GoalId, EngineError> {
        self.goal_id.ok_or(EngineError::InvalidHistory)
    }

    fn ledger(&self) -> &GoalLedger {
        self.ledger
    }

    fn settle_result(&self, result: &CallResult) -> Result<(), EngineError> {
        let (input, output) = call_usage(result);
        self.ledger().settle(input, output)
    }

    fn append(&self, event: Event) -> step::BoxedStep<'_, ()> {
        Box::pin(self.append_inner(event))
    }

    async fn append_inner(&self, event: Event) -> Result<(), EngineError> {
        self.ledger
            .append(self.journal, self.session_id, event)
            .await?;
        Ok(())
    }

    async fn settle_stopped_primary(&self) -> Result<(), EngineError> {
        let active_reserved = self.ledger().active_reserved();
        let (
            goal_open,
            has_uncertain_tool,
            action_cancelled,
            actions_acknowledged,
            run_active,
            primary_active,
            other_agents_closed,
        ) = self.ledger().view(|view| -> Result<_, EngineError> {
            let goal = view.goal(self.goal()?).ok_or(EngineError::InvalidHistory)?;
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
            let has_uncertain_tool = run.lifecycle_hooks.iter().any(|hook| {
                hook.outcome.as_ref().is_some_and(|outcome| {
                    outcome.observation.stops_run()
                        && outcome.observation.disposition
                            != crate::tools::ToolDisposition::Cancelled
                })
            }) || run.tools.iter().any(|tool| {
                tool.observation
                    .iter()
                    .chain(tool.hooks.iter().filter_map(|hook| {
                        hook.outcome.as_ref().map(|outcome| &outcome.observation)
                    }))
                    .any(|observation| {
                        observation.disposition == crate::tools::ToolDisposition::Uncertain
                    })
            });
            let action_cancelled = run.lifecycle_hooks.iter().any(|hook| {
                hook.outcome.as_ref().is_some_and(|outcome| {
                    outcome.observation.disposition == crate::tools::ToolDisposition::Cancelled
                })
            }) || run.tools.iter().any(|tool| {
                tool.observation.as_ref().is_some_and(|observation| {
                    observation.disposition == crate::tools::ToolDisposition::Cancelled
                })
            });
            let actions_acknowledged = run.tools.iter().all(|tool| {
                tool.observation.as_ref().is_some_and(|observation| {
                    observation.disposition != crate::tools::ToolDisposition::Uncertain
                })
            });
            let goal_open = goal.status == crate::session::GoalStatus::Running
                && goal.active_run == Some(self.run_id)
                && goal.unobserved_provider_calls.is_empty()
                && goal.compaction_call_reserved.is_none();
            Ok((
                goal_open,
                has_uncertain_tool,
                action_cancelled,
                actions_acknowledged,
                run.status == crate::session::RunStatus::Active,
                primary_active,
                other_agents_closed,
            ))
        })??;
        if has_uncertain_tool {
            return Err(crate::tools::ToolError::Uncertain.into());
        }
        if !goal_open || active_reserved || !run_active || !primary_active || !other_agents_closed {
            return Err(EngineError::GoalBudgetExhausted);
        }
        let cancelled = self.controls.cancellation.is_cancelled() || action_cancelled;
        if !cancelled && !actions_acknowledged {
            return Err(EngineError::GoalBudgetExhausted);
        }
        if cancelled {
            self.cancel_primary().await
        } else if self.hook_requested_stop()? {
            self.finish_hook_stop().await
        } else {
            self.fail_for_budget().await
        }
    }

    // Journals `exhausted` only when settlement agrees the Goal admits no next Run.
    async fn fail_for_budget(&self) -> Result<(), EngineError> {
        self.ledger().mark_budget_stopped();
        self.fail_primary().await
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
        let sampled_at = if self.ledger().providers_active() {
            Instant::now()
        } else {
            pause.paused_at
        };
        self.settle_active_segment_at(pause.segment, sampled_at, resume_at)
    }

    async fn reserve_provider_call(
        &self,
        agent_run_id: AgentRunId,
        action: Action,
        pending_ms: u64,
    ) -> Result<(), EngineError> {
        let reservation = self.ledger().reserve(action).await?;
        // A review runs inside the Tool step's admission, which already counts its slots.
        if !matches!(action, Action::Review) {
            self.journal
                .admit(
                    self.session_id,
                    action.max_events_for_admitted(&self.routing.providers),
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
        reservation.commit()
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
        if let Some(feedback) = continuation_feedback(&mut request.input) {
            feedback.steering_notes.push(text);
        } else {
            match request
                .input
                .tools
                .as_mut()
                .and_then(|tools| tools.steps.last_mut())
            {
                Some(step) => step.steering_notes.push(text),
                None => request.input.steering_notes.push(text),
            }
        }
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

    pub(super) async fn admit_plan_contract(
        &self,
        contract: crate::session::GoalPlanContract,
    ) -> Result<bool, EngineError> {
        if !self.ledger().can_narrow(&contract) {
            return Ok(false);
        }
        self.journal
            .admit(
                self.session_id,
                Action::Tool.remaining(0, &self.routing.providers),
                Action::Tool.tool_observations(),
            )
            .await?;
        self.append(Event::GoalPlanContractRecorded {
            goal_id: self.goal()?,
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
        match self.initialize_definitions(&mut root_request).await {
            Ok(true) => {}
            Ok(false) => return self.settle_definition_stop().await,
            Err(EngineError::GoalBudgetExhausted) => return self.settle_stopped_primary().await,
            Err(error) => return Err(error),
        }
        self.drive_primary(&mut root_request).await
    }

    pub(super) fn drive_primary<'b>(
        &'b self,
        root_request: &'b mut ProviderRequest,
    ) -> step::BoxedStep<'b, ()> {
        Box::pin(self.drive_primary_inner(root_request))
    }

    async fn drive_primary_inner(
        &self,
        root_request: &mut ProviderRequest,
    ) -> Result<(), EngineError> {
        loop {
            let phase = root_request.phase;
            let planning = phase == AgentPhase::RootPlan;
            if self.pause_at_boundary() {
                return self.settle_stopped_primary().await;
            }
            let (first, active_segment) = match self.invoke_primary(root_request).await {
                Ok(result) => result,
                Err(EngineError::GoalBudgetExhausted) => {
                    return self.settle_stopped_primary().await;
                }
                Err(error) => return Err(error),
            };
            let (well_formed, budget_admits) = match &first {
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Finish(finish),
                    ..
                }) => (
                    (!planning
                        || !matches!(self.controls.policy, CollaborationPolicy::Team { .. }))
                        && valid_finish(finish, MAX_PRIMARY_RESULT_BYTES),
                    true,
                ),
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Delegate(delegation),
                    ..
                }) => (
                    (planning || self.wave_settled()?)
                        && self.well_formed_delegation(
                            &delegation.children,
                            delegation.agent.as_deref(),
                        ),
                    self.ledger().can_start_provider_call(),
                ),
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Tools(requests),
                    ..
                }) => (
                    self.well_formed_tools(requests)
                        && requests.iter().all(|r| self.primary_lease_allows(&r.call)),
                    self.ledger().admits(Action::Tool),
                ),
                _ => (false, true),
            };
            let tokens_ok = self.ledger().tokens_within_limit();
            let valid_response = tokens_ok && well_formed && budget_admits;
            let budget_refused = well_formed && !(tokens_ok && budget_admits);
            self.append(Event::ProviderCallRecorded {
                run_id: self.run_id,
                agent_run_id: self.primary_id,
                record: call_record(phase, &first, valid_response, self.default_child_profile()),
            })
            .await?;
            self.settle_active_segment(active_segment)?;
            if self.controls.cancellation.is_cancelled() {
                return self.settle_stopped_primary().await;
            }
            if matches!(&first, CallResult::Cancelled) {
                return self.cancel_primary().await;
            }
            if self.ledger().active_time_exhausted() {
                return self.settle_stopped_primary().await;
            }
            match first {
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Finish(finish),
                    turn,
                    ..
                }) if valid_response => {
                    match self.stop_primary(root_request, &finish, turn).await {
                        Ok(true) => continue,
                        Ok(false) => return self.finish_primary(finish).await,
                        Err(EngineError::GoalBudgetExhausted) => {
                            return self.settle_stopped_primary().await;
                        }
                        Err(EngineError::ContextTooLarge) => return self.fail_primary().await,
                        Err(error) => return Err(error),
                    }
                }
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Delegate(delegation),
                    turn,
                    ..
                }) if valid_response => {
                    if self.pause_at_boundary() {
                        return self.settle_stopped_primary().await;
                    }
                    if self
                        .run_children(root_request, delegation.children, delegation.agent, turn)
                        .await?
                    {
                        continue;
                    }
                    return Ok(());
                }
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Tools(requests),
                    turn,
                    ..
                }) if valid_response => {
                    if self
                        .primary_tools(root_request, requests, turn, planning)
                        .await?
                    {
                        continue;
                    }
                    return Ok(());
                }
                CallResult::Cancelled => return self.cancel_primary().await,
                CallResult::Response(_) if budget_refused => return self.fail_for_budget().await,
                _ => return self.fail_primary().await,
            }
        }
    }

    fn primary_tools<'b>(
        &'b self,
        root_request: &'b mut ProviderRequest,
        requests: Vec<crate::ToolRequest>,
        turn: crate::provider::ProviderTurn,
        planning: bool,
    ) -> step::BoxedStep<'b, bool> {
        Box::pin(self.primary_tools_inner(root_request, requests, turn, planning))
    }

    async fn primary_tools_inner(
        &self,
        root_request: &mut ProviderRequest,
        requests: Vec<crate::ToolRequest>,
        turn: crate::provider::ProviderTurn,
        planning: bool,
    ) -> Result<bool, EngineError> {
        if self.pause_at_boundary() {
            return self.settle_stopped_primary().await.map(|()| false);
        }
        let Some(request) = self.begin_tool_step(requests).await? else {
            return self.fail_primary().await.map(|()| false);
        };
        let grant = Grant::Primary {
            request: &mut *root_request,
            review_allowed: planning && self.ledger().admits(Action::Review),
        };
        match Box::pin(self.execute_step(grant, request, turn)).await {
            Ok(true) => {}
            Ok(false) if self.controls.cancellation.is_cancelled() => {
                return self.settle_stopped_primary().await.map(|()| false);
            }
            Ok(false) if self.ledger().active_time_exhausted() => {
                return self.settle_stopped_primary().await.map(|()| false);
            }
            Ok(false) => return self.fail_primary().await.map(|()| false),
            Err(EngineError::GoalBudgetExhausted) => {
                return self.settle_stopped_primary().await.map(|()| false);
            }
            Err(error) => return Err(error),
        }
        Box::pin(self.verify_workers()).await?;
        Ok(true)
    }

    async fn stop_primary(
        &self,
        root: &mut ProviderRequest,
        finish: &Finish,
        turn: crate::provider::ProviderTurn,
    ) -> Result<bool, EngineError> {
        let fits = self
            .controls
            .tools
            .zip(root.input.tools.as_ref())
            .is_none_or(|(runtime, tools)| {
                tools.has_stop_room(&turn, runtime.context_limit_bytes())
            });
        let Some(feedback) = self
            .lifecycle_hooks(
                self.primary_id,
                crate::tools::permissions::HookEvent::Stop,
                "",
                &finish.result,
                None,
                fits,
            )
            .await?
        else {
            return Ok(false);
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
        Ok(true)
    }

    async fn primary_context(&self, root: &mut ProviderRequest) -> Result<(), EngineError> {
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
    ) -> step::BoxedStep<'b, (CallResult, ActiveSegment)> {
        Box::pin(self.invoke_primary_inner(root))
    }

    async fn invoke_primary_inner(
        &self,
        root: &mut ProviderRequest,
    ) -> Result<(CallResult, ActiveSegment), EngineError> {
        let provider = self.routing.bind(
            crate::provider::ProviderRole::Primary,
            self.primary_id,
            &mut root.model,
            &mut root.max_output_tokens,
        )?;
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
                .invoke(request, deadline, self.controls.cancellation.clone(), None)
                .await?;
            self.settle_result(&result)?;
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
                    self.hints.expect("preview owner").publish_preview(self.primary_id, call_index, Some(Arc::new(text)));
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

    fn well_formed_tools(&self, requests: &[crate::tools::ToolRequest]) -> bool {
        matches!(requests, [request] if self.well_formed_tool(&request.call, &request.plan_contract))
    }

    fn well_formed_tool(
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
            && !matches!(
                call,
                crate::tools::ToolCall::ApplyChangeset { .. }
                    | crate::tools::ToolCall::ParseAgentDefinition { .. }
                    | crate::tools::ToolCall::PrepareWorker { .. }
            )
    }

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

    async fn finish_primary(&self, finish: Finish) -> Result<(), EngineError> {
        if self.controls.cancellation.is_cancelled() {
            return self.cancel_primary().await;
        }
        if self.hook_requested_stop()? {
            return self.finish_hook_stop().await;
        }
        let workers_verified = self.ledger().view(|v| {
            v.runs.last().is_some_and(|r| {
                r.agents
                    .iter()
                    .filter_map(|a| a.assignment.as_ref())
                    .all(|w| w.settled())
            })
        })?;
        if !workers_verified {
            return self.fail_primary().await;
        }
        if !self.ledger().success_criterion_satisfied() && !self.ledger().allows_next_run() {
            // Only an autonomous Goal runs out of Runs; a single-Run Goal with an unmet plan simply fails.
            return if self.ledger().autonomous() {
                self.fail_for_budget().await
            } else {
                self.fail_primary().await
            };
        }
        self.append(Event::AgentFinished {
            worker: None,
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
        Ok(())
    }

    async fn settle_definition_stop(&self) -> Result<(), EngineError> {
        let stopped = self.pause_at_boundary()
            || self.controls.cancellation.is_cancelled()
            || self.ledger().active_time_exhausted()
            || self.ledger().view(|view| {
                view.runs.iter().any(|run| {
                    run.id == self.run_id
                        && run.tools.iter().any(|tool| {
                            tool.observation.as_ref().is_some_and(|observation| {
                                matches!(
                                    observation.disposition,
                                    crate::tools::ToolDisposition::Uncertain
                                        | crate::tools::ToolDisposition::Cancelled
                                )
                            })
                        })
                })
            })?;
        match definition_stop(stopped, !self.ledger().can_start_provider_call()) {
            DefinitionStop::Settle => self.settle_stopped_primary().await,
            DefinitionStop::Budget => self.fail_for_budget().await,
            DefinitionStop::Fail => self.fail_primary().await,
        }
    }

    async fn fail_primary(&self) -> Result<(), EngineError> {
        if self.controls.cancellation.is_cancelled() {
            return self.cancel_primary().await;
        }
        if self.hook_requested_stop()? {
            return self.finish_hook_stop().await;
        }
        self.append(Event::AgentFinished {
            worker: None,
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
        Ok(())
    }

    fn hook_requested_stop(&self) -> Result<bool, EngineError> {
        self.ledger().view(|view| {
            view.runs.iter().any(|run| {
                run.id == self.run_id && run.hooks_settled() && run.hook_stop_reason().is_some()
            })
        })
    }

    async fn finish_hook_stop(&self) -> Result<(), EngineError> {
        self.append(Event::AgentFinished {
            worker: None,
            run_id: self.run_id,
            agent_run_id: self.primary_id,
            disposition: AgentDisposition::StoppedByHook,
            summary: None,
            result: None,
        })
        .await?;
        self.append(Event::RunFinished {
            run_id: self.run_id,
            disposition: RunDisposition::StoppedByHook,
            active_ms: self.ledger().run_active_ms()?,
        })
        .await?;
        Ok(())
    }

    async fn cancel_primary(&self) -> Result<(), EngineError> {
        self.append(Event::AgentFinished {
            worker: None,
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
                ProviderFailureClass::Unavailable => CallResult::Failed(CallFailure::Unavailable {
                    reason: error.failure_reason(),
                    fallback: error.allows_fallback(),
                }),
                ProviderFailureClass::Rejected => CallResult::Failed(CallFailure::Rejected(error.failure_reason())),
                ProviderFailureClass::InvalidOutcome => {
                    CallResult::Failed(CallFailure::InvalidResponse(error.failure_reason()))
                }
            },
            Err(_) => CallResult::Failed(CallFailure::Timeout),
        }
    }
}

fn call_usage(result: &CallResult) -> (Option<u32>, Option<u32>) {
    match result {
        CallResult::Response(response) => {
            (response.usage.input_tokens, response.usage.output_tokens)
        }
        CallResult::Failed(_) | CallResult::Cancelled => (None, None),
    }
}

fn call_record(
    phase: AgentPhase,
    result: &CallResult,
    valid_response: bool,
    default_profile: Option<String>,
) -> ProviderCallRecord {
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
    ProviderCallRecord {
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
                CallFailure::Unavailable { reason, .. }
                | CallFailure::Rejected(reason)
                | CallFailure::InvalidResponse(reason),
            ) => *reason,
            _ => None,
        },
    }
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

fn valid_finish(finish: &Finish, result_limit: usize) -> bool {
    !finish.summary.is_empty()
        && finish.summary.len() <= MAX_FINISH_SUMMARY_BYTES
        && !finish.result.is_empty()
        && finish.result.len() <= result_limit
        && Event::finished_payload_size(&finish.summary, &finish.result)
            .is_ok_and(|bytes| bytes <= crate::store::MAX_EVENT_BYTES)
}

#[derive(Debug, Eq, PartialEq)]
enum DefinitionStop {
    Settle,
    Budget,
    Fail,
}

// Interruptions (cancel, pause, active time, Uncertain or Cancelled parse) settle honestly; only a plain refusal fails.
fn definition_stop(interrupted: bool, provider_budget_closed: bool) -> DefinitionStop {
    if interrupted {
        DefinitionStop::Settle
    } else if provider_budget_closed {
        DefinitionStop::Budget
    } else {
        DefinitionStop::Fail
    }
}

#[cfg(test)]
mod definition_stop_tests {
    use super::*;

    #[test]
    fn parser_stops_settle_interruptions_and_fail_budget_or_refusal_distinctly() {
        assert_eq!(definition_stop(true, true), DefinitionStop::Settle);
        assert_eq!(definition_stop(true, false), DefinitionStop::Settle);
        assert_eq!(definition_stop(false, true), DefinitionStop::Budget);
        assert_eq!(definition_stop(false, false), DefinitionStop::Fail);
    }
}

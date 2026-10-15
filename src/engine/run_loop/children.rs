use super::{
    CallFailure, CallResult, MAX_PRIMARY_RESULT_BYTES, RunLoop, call_record, call_trace_result,
    call_trace_usage, invoke_bounded, valid_finish,
};
use crate::engine::{EngineError, OUTPUT_TOKEN_CAP};
use crate::provider::{
    AgentPhase, ChildResult, Finish, Provider, ProviderOutcome, ProviderRequest, ProviderResponse,
};
use crate::session::{AgentDisposition, AgentRole, AgentRunId, CollaborationPolicy, Event};
use crate::telemetry::{AgentTrace, ProviderTrace, TraceOutcome};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::{Id, JoinError, JoinSet};
use tokio::time::Instant;

const MAX_CHILD_OBJECTIVE_BYTES: usize = 2 * 1024;
const MAX_CHILD_RESULT_BYTES: usize = 16 * 1024;
const CANCELLATION_DRAIN: Duration = Duration::from_secs(2);

struct ChildWork {
    agent_run_id: AgentRunId,
    objective: String,
    trace: Option<AgentTrace>,
}

struct ChildQueue {
    pending: VecDeque<usize>,
    calls: JoinSet<CallResult>,
    active: HashMap<Id, ActiveChild>,
    parallel: usize,
}

struct ActiveChild {
    index: usize,
    provider_trace: ProviderTrace,
}

enum AbortReason {
    Failed,
    Budget,
    Cancelled,
}

impl ChildQueue {
    fn new(count: usize, parallel: usize) -> Self {
        Self {
            pending: (0..count).collect(),
            calls: JoinSet::new(),
            active: HashMap::new(),
            parallel,
        }
    }

    /// Drains aborted calls and returns the failure that caused the abort; a drain error is
    /// recorded as a diagnostic instead of replacing that failure.
    async fn abort_with(&mut self, error: EngineError) -> EngineError {
        let _ = self
            .abort_and_drain()
            .await
            .inspect_err(crate::diagnostics::engine_failure);
        error
    }

    async fn abort_and_drain(&mut self) -> Result<(), EngineError> {
        self.calls.abort_all();
        let deadline = Instant::now() + CANCELLATION_DRAIN;
        while !self.calls.is_empty() {
            let joined = tokio::time::timeout_at(deadline, self.calls.join_next_with_id())
                .await
                .map_err(|_| EngineError::CancellationDrainTimeout)?
                .ok_or(EngineError::CoordinatorFailed)?;
            let (task_id, result) = classify_join(joined);
            self.close_provider_span(task_id, &result, valid_child_response(&result));
        }
        self.active.clear();
        Ok(())
    }

    fn close_provider_span(
        &mut self,
        task_id: Id,
        result: &CallResult,
        valid_response: bool,
    ) -> Option<usize> {
        let active = self.active.remove(&task_id)?;
        let (input_tokens, output_tokens) = call_trace_usage(result);
        active.provider_trace.finish_with_usage(
            call_trace_result(result, valid_response),
            input_tokens,
            output_tokens,
        );
        Some(active.index)
    }
}

impl<P: Provider + 'static> RunLoop<'_, P> {
    pub(super) fn valid_delegation(&self, children: &[String]) -> bool {
        self.controls.goal_budget.can_start_provider_call()
            && !matches!(self.controls.policy, CollaborationPolicy::Single)
            && !children.is_empty()
            && children.len() <= usize::from(self.controls.policy.max_children())
            && children.iter().all(|objective| {
                !objective.trim().is_empty() && objective.len() <= MAX_CHILD_OBJECTIVE_BYTES
            })
    }

    pub(super) async fn run_children(
        &self,
        root_request: &mut ProviderRequest,
        objectives: Vec<String>,
        primary: AgentTrace,
    ) -> Result<(), EngineError> {
        if self
            .controls
            .goal_budget
            .remaining_calls()
            .is_some_and(|remaining| {
                remaining < u32::try_from(objectives.len().saturating_add(1)).unwrap_or(u32::MAX)
            })
        {
            return self.fail_known_budget_exhaustion(primary).await;
        }
        let parallel = self.controls.goal_budget.provider_concurrency(
            usize::from(self.controls.concurrency)
                .min(usize::from(self.controls.policy.max_children())),
        );
        let waves = objectives.len().div_ceil(parallel.max(1));
        let active_bound =
            super::CALL_DEADLINE.saturating_mul(u32::try_from(waves).unwrap_or(u32::MAX));
        let active_event_tail = 10_usize.saturating_add(objectives.len().saturating_mul(3));
        let active_segment = match self
            .reserve_active_segment_with_tail(active_bound, active_event_tail)
            .await
        {
            Ok(segment) => segment,
            Err(EngineError::GoalBudgetExhausted) => {
                return self.fail_known_budget_exhaustion(primary).await;
            }
            Err(error) => return Err(error),
        };
        let mut children = Vec::with_capacity(objectives.len());
        for (index, objective) in objectives.into_iter().enumerate() {
            if self.controls.cancellation.is_cancelled() {
                break;
            }
            let agent_run_id = AgentRunId::new();
            self.append(Event::AgentSpawned {
                run_id: self.run_id,
                agent_run_id,
                role: AgentRole::Child,
                ordinal: index as u8 + 1,
                objective: Some(objective.clone()),
            })
            .await?;
            children.push(ChildWork {
                agent_run_id,
                objective,
                trace: Some(primary.begin_child(agent_run_id)),
            });
        }
        if self.controls.cancellation.is_cancelled() {
            self.settle_active_segment(active_segment).await?;
            for mut child in children {
                self.finish_child(child.agent_run_id, AgentDisposition::Cancelled, None, false)
                    .await?;
                child
                    .trace
                    .take()
                    .expect("child trace exists")
                    .finish(TraceOutcome::Cancelled);
            }
            return self.fail_known_budget_exhaustion(primary).await;
        }

        let mut results = vec![None; children.len()];
        let mut queue = ChildQueue::new(children.len(), parallel);
        let deadline = active_segment.deadline();
        if let Err(error) = self
            .fill_slots(&mut queue, &children, root_request, deadline)
            .await
        {
            return Err(queue.abort_with(error).await);
        }
        let mut abort_reason = (!queue.pending.is_empty()
            && !self.controls.goal_budget.can_start_provider_call())
        .then_some(AbortReason::Budget);
        let mut drain_deadline = None;

        while !queue.active.is_empty() {
            let joined = match drain_deadline {
                Some(limit) => tokio::time::timeout_at(limit, queue.calls.join_next_with_id())
                    .await
                    .map_err(|_| EngineError::CancellationDrainTimeout)?,
                None => queue.calls.join_next_with_id().await,
            }
            .ok_or(EngineError::CoordinatorFailed)?;
            let (task_id, result) = classify_join(joined);
            let valid_response =
                valid_child_response(&result) && self.controls.goal_budget.tokens_within_limit();
            let (disposition, finish) = match &result {
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Finish(finish),
                    ..
                }) if valid_response => (AgentDisposition::Finished, Some(finish)),
                CallResult::Cancelled => (AgentDisposition::Cancelled, None),
                _ => (AgentDisposition::Failed, None),
            };
            let Some(index) = queue.close_provider_span(task_id, &result, valid_response) else {
                return Err(queue.abort_with(EngineError::CoordinatorFailed).await);
            };
            if let Err(error) = self
                .append(Event::ProviderCallRecorded {
                    run_id: self.run_id,
                    agent_run_id: children[index].agent_run_id,
                    record: call_record(AgentPhase::ChildWork, &result, valid_response),
                })
                .await
            {
                return Err(queue.abort_with(error).await);
            }
            if let Err(error) = self
                .finish_child(children[index].agent_run_id, disposition, finish, true)
                .await
            {
                return Err(queue.abort_with(error).await);
            }
            children[index]
                .trace
                .take()
                .expect("child trace exists")
                .finish(disposition_trace_outcome(disposition));
            if let CallResult::Response(ProviderResponse {
                outcome: ProviderOutcome::Finish(finish),
                ..
            }) = result
                && disposition == AgentDisposition::Finished
            {
                results[index] = Some(ChildResult {
                    objective: children[index].objective.clone(),
                    summary: finish.summary,
                    result: finish.result,
                });
            }
            if abort_reason.is_none() {
                abort_reason = if self.controls.cancellation.is_cancelled()
                    || disposition == AgentDisposition::Cancelled
                {
                    Some(AbortReason::Cancelled)
                } else if disposition == AgentDisposition::Failed {
                    Some(AbortReason::Failed)
                } else {
                    None
                };
            }
            if abort_reason.is_some() && drain_deadline.is_none() {
                queue.calls.abort_all();
                drain_deadline = Some(Instant::now() + CANCELLATION_DRAIN);
            }
            if abort_reason.is_none()
                && let Err(error) = self
                    .fill_slots(&mut queue, &children, root_request, deadline)
                    .await
            {
                return Err(queue.abort_with(error).await);
            }
        }

        self.settle_active_segment(active_segment).await?;

        if self.controls.cancellation.is_cancelled() {
            abort_reason = Some(AbortReason::Cancelled);
        }
        if abort_reason.is_none() && self.controls.goal_budget.active_time_exhausted() {
            abort_reason = Some(AbortReason::Budget);
        }
        if abort_reason.is_none() && !queue.pending.is_empty() {
            abort_reason = Some(AbortReason::Failed);
        }
        if let Some(reason) = abort_reason {
            for index in queue.pending {
                self.finish_child(
                    children[index].agent_run_id,
                    AgentDisposition::Cancelled,
                    None,
                    false,
                )
                .await?;
                children[index]
                    .trace
                    .take()
                    .expect("pending child trace exists")
                    .finish(TraceOutcome::Cancelled);
            }
            return match reason {
                AbortReason::Failed if self.controls.goal_budget.active_time_exhausted() => {
                    self.fail_known_budget_exhaustion(primary).await
                }
                AbortReason::Failed => self.fail_primary(primary).await,
                AbortReason::Budget => self.fail_known_budget_exhaustion(primary).await,
                AbortReason::Cancelled => self.fail_known_budget_exhaustion(primary).await,
            };
        }
        if !self.controls.goal_budget.can_start_provider_call() {
            return self.fail_known_budget_exhaustion(primary).await;
        }
        root_request.phase = AgentPhase::RootSynthesis;
        root_request.child_results = results
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or(EngineError::CoordinatorFailed)?;
        loop {
            let synthesis_trace =
                primary.begin_provider(AgentPhase::RootSynthesis, &root_request.model);
            let (synthesis, active_segment) = match self.invoke_primary(root_request.clone()).await
            {
                Ok(result) => result,
                Err(EngineError::GoalBudgetExhausted) => {
                    synthesis_trace.finish_refused();
                    return self.fail_known_budget_exhaustion(primary).await;
                }
                Err(error) => return Err(error),
            };
            let (input_tokens, output_tokens) = call_trace_usage(&synthesis);
            let valid_response = self.controls.goal_budget.tokens_within_limit()
                && (matches!(
                    &synthesis,
                    CallResult::Response(ProviderResponse {
                        outcome: ProviderOutcome::Finish(finish),
                        ..
                    }) if valid_finish(finish, MAX_PRIMARY_RESULT_BYTES)
                ) || matches!(&synthesis, CallResult::Response(ProviderResponse { outcome: ProviderOutcome::Tool { call, plan_contract }, .. }) if self.valid_tool(root_request, call, plan_contract)));
            synthesis_trace.finish_with_usage(
                call_trace_result(&synthesis, valid_response),
                input_tokens,
                output_tokens,
            );
            self.append(Event::ProviderCallRecorded {
                run_id: self.run_id,
                agent_run_id: self.primary_id,
                record: call_record(AgentPhase::RootSynthesis, &synthesis, valid_response),
            })
            .await?;
            self.settle_active_segment(active_segment).await?;
            if self.controls.cancellation.is_cancelled() {
                return self.fail_known_budget_exhaustion(primary).await;
            }
            if matches!(&synthesis, CallResult::Cancelled) {
                return self.cancel_primary(primary).await;
            }
            if self.controls.goal_budget.active_time_exhausted() {
                return self.fail_known_budget_exhaustion(primary).await;
            }
            match synthesis {
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Finish(finish),
                    ..
                }) if valid_response => return self.finish_primary(finish, primary).await,
                CallResult::Response(ProviderResponse {
                    outcome:
                        ProviderOutcome::Tool {
                            call,
                            plan_contract,
                        },
                    ..
                }) if valid_response => {
                    if let Some(contract) = plan_contract
                        && !self.admit_plan_contract(root_request, *contract).await?
                    {
                        return self.fail_primary(primary).await;
                    }
                    match self.execute_tool(root_request, call, false, &primary).await {
                        Ok(true) => {}
                        Ok(false) if self.controls.cancellation.is_cancelled() => {
                            return self.fail_known_budget_exhaustion(primary).await;
                        }
                        Ok(false) if self.controls.goal_budget.active_time_exhausted() => {
                            return self.fail_known_budget_exhaustion(primary).await;
                        }
                        Ok(false) => return self.fail_primary(primary).await,
                        Err(EngineError::GoalBudgetExhausted)
                            if self.controls.cancellation.is_cancelled() =>
                        {
                            return self.fail_known_budget_exhaustion(primary).await;
                        }
                        Err(EngineError::GoalBudgetExhausted) => {
                            return self.fail_known_budget_exhaustion(primary).await;
                        }
                        Err(error) => return Err(error),
                    }
                }
                CallResult::Cancelled => return self.cancel_primary(primary).await,
                _ => return self.fail_primary(primary).await,
            }
        }
    }

    async fn fill_slots(
        &self,
        queue: &mut ChildQueue,
        children: &[ChildWork],
        root_request: &ProviderRequest,
        deadline: Instant,
    ) -> Result<(), EngineError> {
        while queue.active.len()
            < self
                .controls
                .goal_budget
                .provider_concurrency(queue.parallel)
            && !self.controls.cancellation.is_cancelled()
            && self.controls.goal_budget.can_start_provider_call()
        {
            let Some(index) = queue.pending.pop_front() else {
                break;
            };
            let provider = Arc::clone(&self.provider);
            let cancellation = self.controls.cancellation.clone();
            let request = ProviderRequest {
                run_id: self.run_id,
                agent_run_id: children[index].agent_run_id,
                phase: AgentPhase::ChildWork,
                collaboration: CollaborationPolicy::Single,
                model: root_request.model.clone(),
                instructions: root_request.instructions.clone(),
                objective: children[index].objective.clone(),
                images: root_request
                    .images
                    .iter()
                    .filter(|value| value.origin == crate::provider::ImageOrigin::Objective)
                    .cloned()
                    .collect(),
                includes: root_request.includes.clone(),
                history: Vec::new(),
                context_summary: None,
                goal_budget: root_request.goal_budget,
                goal_plan_contract: None,
                goal_context_facts: None,
                child_results: Vec::new(),
                max_output_tokens: OUTPUT_TOKEN_CAP,
                tools: None,
            };
            self.reserve_provider_call(children[index].agent_run_id)
                .await?;
            let provider_trace = children[index]
                .trace
                .as_ref()
                .expect("child trace exists")
                .begin_provider(AgentPhase::ChildWork, &request.model);
            let goal_budget = Arc::clone(&self.controls.goal_budget);
            let handle = queue.calls.spawn(async move {
                let result =
                    invoke_bounded(provider.as_ref(), request, deadline, cancellation).await;
                goal_budget.account_result(&result);
                result
            });
            queue.active.insert(
                handle.id(),
                ActiveChild {
                    index,
                    provider_trace,
                },
            );
        }
        Ok(())
    }

    async fn finish_child(
        &self,
        agent_run_id: AgentRunId,
        disposition: AgentDisposition,
        finish: Option<&Finish>,
        call_reserved: bool,
    ) -> Result<(), EngineError> {
        self.append(Event::AgentFinished {
            run_id: self.run_id,
            agent_run_id,
            disposition,
            summary: finish.map(|value| value.summary.clone()),
            result: finish.map(|value| value.result.clone()),
        })
        .await?;
        if call_reserved {
            self.controls.goal_budget.child_call_finished()?;
        }
        Ok(())
    }
}

fn classify_join(joined: Result<(Id, CallResult), JoinError>) -> (Id, CallResult) {
    match joined {
        Ok((task_id, result)) => (task_id, result),
        Err(error) => (
            error.id(),
            if error.is_cancelled() {
                CallResult::Cancelled
            } else {
                CallResult::Failed(CallFailure::TaskPanic)
            },
        ),
    }
}

fn disposition_trace_outcome(disposition: AgentDisposition) -> TraceOutcome {
    match disposition {
        AgentDisposition::Finished => TraceOutcome::Finished,
        AgentDisposition::Failed => TraceOutcome::Failed,
        AgentDisposition::Cancelled => TraceOutcome::Cancelled,
    }
}

fn valid_child_response(result: &CallResult) -> bool {
    matches!(
        result,
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Finish(finish),
            ..
        }) if valid_finish(finish, MAX_CHILD_RESULT_BYTES)
    )
}

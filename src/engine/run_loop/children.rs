use super::{
    CallFailure, CallResult, MAX_PRIMARY_RESULT_BYTES, RunLoop, TOOL_CLEANUP_OVERHEAD,
    TOOL_PREPARATION_OVERHEAD, call_record, call_trace_result, call_trace_usage, duration_ms,
    invoke_bounded, valid_finish,
};
use crate::engine::{CHILD_OUTPUT_TOKEN_CAP, EngineError, RunCancellation, STORE_ACTION_OVERHEAD};
use crate::provider::{
    AgentPhase, ChildResult, Finish, Provider, ProviderOutcome, ProviderRequest, ProviderResponse,
};
use crate::session::{
    AgentDisposition, AgentRole, AgentRunId, ChildGrant, CollaborationPolicy, Event,
    MAX_CHILD_TOOL_CALLS,
};
use crate::telemetry::{AgentTrace, ProviderTrace, TraceOutcome};
use crate::tools::{
    CacheUpdate, GuardReceipt, ToolCall, ToolContext, ToolDisposition, ToolObservation,
};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::task::{Id, JoinError, JoinSet};
use tokio::time::Instant;

const MAX_CHILD_OBJECTIVE_BYTES: usize = 2 * 1024;
const MAX_CHILD_RESULT_BYTES: usize = 16 * 1024;
const CANCELLATION_DRAIN: Duration = Duration::from_secs(2);
/// Wall-clock bound of one delegation, with or without read-only Tools.
const MAX_DELEGATION_DEADLINE: Duration = Duration::from_secs(20 * 60);
/// Free context kept for the next child Tool intent.
const CHILD_INTENT_RESERVE_BYTES: usize = 4 * 1024;
/// A full 4 KiB Read page plus metadata; with less room a child is offered only Finish.
const MIN_CHILD_RESULT_BYTES: usize = 4096 + 512;

struct ChildWork {
    agent_run_id: AgentRunId,
    objective: String,
    /// Read-only research context; `None` for a `NoTools` child.
    tools: Option<ToolContext>,
    tool_calls_remaining: usize,
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
    Paused,
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
            return self.settle_stopped_primary(primary).await;
        }
        let research = self.controls.tools.is_some();
        let tool_allowance = if research {
            self.controls
                .goal_budget
                .remaining_calls()
                .map_or(MAX_CHILD_TOOL_CALLS, |remaining| {
                    // Each child keeps one call for its report, the primary one for synthesis; researchers share half the rest.
                    let usable = usize::try_from(remaining)
                        .unwrap_or(usize::MAX)
                        .saturating_sub(objectives.len() + 1);
                    (usable / 2 / objectives.len()).min(MAX_CHILD_TOOL_CALLS)
                })
        } else {
            0
        };
        let research_template = self
            .controls
            .tools
            .map(|runtime| runtime.research_context(tool_allowance))
            .transpose()?;
        let grant = if research {
            ChildGrant::ReadOnly
        } else {
            ChildGrant::NoTools
        };
        let parallel = self.controls.goal_budget.provider_concurrency(
            usize::from(self.controls.concurrency)
                .min(usize::from(self.controls.policy.max_children())),
        );
        let waves = objectives.len().div_ceil(parallel.max(1));
        let active_bound = if research {
            MAX_DELEGATION_DEADLINE
        } else {
            self.provider
                .call_deadline()
                .saturating_mul(u32::try_from(waves).unwrap_or(u32::MAX))
                .min(MAX_DELEGATION_DEADLINE)
        };
        // Covers every child's calls, Tools and terminal facts plus the largest later admission, so a refusal precedes AgentSpawned.
        let active_event_tail = if research {
            objectives.len() * (4 * MAX_CHILD_TOOL_CALLS + 4) + 10
        } else {
            4 * objectives.len() + 10
        };
        let active_segment = match self
            .reserve_active_segment_with_tail(active_bound, active_event_tail)
            .await
        {
            Ok(segment) => segment,
            Err(EngineError::GoalBudgetExhausted) => {
                return self.settle_stopped_primary(primary).await;
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
                grant: Some(grant),
            })
            .await?;
            children.push(ChildWork {
                agent_run_id,
                objective,
                tools: research_template.clone(),
                tool_calls_remaining: tool_allowance,
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
            return self.settle_stopped_primary(primary).await;
        }

        let mut results = vec![None; children.len()];
        let mut queue = ChildQueue::new(children.len(), parallel);
        let deadline = active_segment.deadline();
        if let Err(error) = self
            .fill_slots(&mut queue, &mut children, root_request, deadline)
            .await
        {
            return Err(queue.abort_with(error).await);
        }
        let mut abort_reason = self.unstarted_abort(&queue);
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
            let Some(index) = queue.active.get(&task_id).map(|active| active.index) else {
                return Err(queue.abort_with(EngineError::CoordinatorFailed).await);
            };
            let valid_response = self.controls.goal_budget.tokens_within_limit()
                && match &result {
                    CallResult::Response(ProviderResponse {
                        outcome: ProviderOutcome::Finish(finish),
                        ..
                    }) => valid_finish(finish, MAX_CHILD_RESULT_BYTES),
                    CallResult::Response(ProviderResponse {
                        outcome:
                            ProviderOutcome::Tool {
                                call,
                                plan_contract,
                            },
                        ..
                    }) => self.valid_child_tool(&children[index], call, plan_contract.is_some()),
                    _ => false,
                };
            if queue
                .close_provider_span(task_id, &result, valid_response)
                .is_none()
            {
                return Err(queue.abort_with(EngineError::CoordinatorFailed).await);
            }
            let agent_run_id = children[index].agent_run_id;
            if let Err(error) = self
                .append(Event::ProviderCallRecorded {
                    run_id: self.run_id,
                    agent_run_id,
                    record: call_record(AgentPhase::ChildWork, &result, valid_response),
                })
                .await
            {
                return Err(queue.abort_with(error).await);
            }
            // `None` means the child continues with another step.
            let step_disposition = match result {
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Finish(finish),
                    ..
                }) if valid_response => {
                    if let Err(error) = self
                        .finish_child(
                            agent_run_id,
                            AgentDisposition::Finished,
                            Some(&finish),
                            true,
                        )
                        .await
                    {
                        return Err(queue.abort_with(error).await);
                    }
                    results[index] = Some(ChildResult {
                        objective: children[index].objective.clone(),
                        summary: finish.summary,
                        result: finish.result,
                    });
                    Some(AgentDisposition::Finished)
                }
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Tool { call, .. },
                    ..
                }) if valid_response => {
                    if let Err(error) = self.controls.goal_budget.child_call_finished() {
                        return Err(queue.abort_with(error).await);
                    }
                    // An exhausted Goal budget stops the child before its Tool starts, like a pause.
                    let budget_spent = !self.controls.goal_budget.can_start_provider_call()
                        || self.controls.goal_budget.remaining_calls() == Some(0);
                    if budget_spent && abort_reason.is_none() {
                        abort_reason = Some(AbortReason::Budget);
                    }
                    let skipped = self.controls.cancellation.is_cancelled()
                        || self.pause_requested()
                        || abort_reason.is_some();
                    let continues = if skipped {
                        false
                    } else {
                        // Boxed: the Tool future is large and would overflow the caller's stack.
                        match Box::pin(self.execute_child_tool(
                            &mut children[index],
                            call,
                            deadline,
                        ))
                        .await
                        {
                            Ok(continues) => continues,
                            Err(error) => return Err(queue.abort_with(error).await),
                        }
                    };
                    if continues {
                        children[index].tool_calls_remaining =
                            children[index].tool_calls_remaining.saturating_sub(1);
                        queue.pending.push_back(index);
                        None
                    } else {
                        let disposition = if skipped || self.controls.cancellation.is_cancelled() {
                            AgentDisposition::Cancelled
                        } else {
                            AgentDisposition::Failed
                        };
                        if let Err(error) = self
                            .finish_child(agent_run_id, disposition, None, false)
                            .await
                        {
                            return Err(queue.abort_with(error).await);
                        }
                        Some(disposition)
                    }
                }
                other => {
                    let disposition = if matches!(other, CallResult::Cancelled) {
                        AgentDisposition::Cancelled
                    } else {
                        AgentDisposition::Failed
                    };
                    if let Err(error) = self
                        .finish_child(agent_run_id, disposition, None, true)
                        .await
                    {
                        return Err(queue.abort_with(error).await);
                    }
                    Some(disposition)
                }
            };
            if let Some(disposition) = step_disposition {
                children[index]
                    .trace
                    .take()
                    .expect("child trace exists")
                    .finish(disposition_trace_outcome(disposition));
            }
            if abort_reason.is_none() {
                abort_reason = if self.controls.cancellation.is_cancelled() {
                    Some(AbortReason::Cancelled)
                } else if self.pause_requested() {
                    Some(AbortReason::Paused)
                } else if step_disposition == Some(AgentDisposition::Failed) {
                    Some(AbortReason::Failed)
                } else if step_disposition == Some(AgentDisposition::Cancelled) {
                    Some(AbortReason::Cancelled)
                } else {
                    None
                };
            }
            // A pause lets in-flight child calls finish and be recorded; every other abort drains them.
            if abort_reason.is_some()
                && !matches!(abort_reason, Some(AbortReason::Paused))
                && drain_deadline.is_none()
            {
                queue.calls.abort_all();
                drain_deadline = Some(Instant::now() + CANCELLATION_DRAIN);
            }
            if abort_reason.is_none() {
                if let Err(error) = self
                    .fill_slots(&mut queue, &mut children, root_request, deadline)
                    .await
                {
                    return Err(queue.abort_with(error).await);
                }
                abort_reason = self.unstarted_abort(&queue);
            }
        }

        self.settle_active_segment(active_segment).await?;
        if matches!(abort_reason, Some(AbortReason::Paused)) {
            self.pause_at_boundary();
        }

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
                    self.settle_stopped_primary(primary).await
                }
                AbortReason::Failed => self.fail_primary(primary).await,
                AbortReason::Budget | AbortReason::Cancelled | AbortReason::Paused => {
                    self.settle_stopped_primary(primary).await
                }
            };
        }
        if !self.controls.goal_budget.can_start_provider_call() {
            return self.settle_stopped_primary(primary).await;
        }
        root_request.phase = AgentPhase::RootSynthesis;
        root_request.child_results = results
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or(EngineError::CoordinatorFailed)?;
        loop {
            if self.pause_at_boundary() {
                return self.settle_stopped_primary(primary).await;
            }
            let synthesis_trace =
                primary.begin_provider(AgentPhase::RootSynthesis, &root_request.model);
            let (synthesis, active_segment) = match self.invoke_primary(root_request).await {
                Ok(result) => result,
                Err(EngineError::GoalBudgetExhausted) => {
                    synthesis_trace.finish_refused();
                    return self.settle_stopped_primary(primary).await;
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
                return self.settle_stopped_primary(primary).await;
            }
            if matches!(&synthesis, CallResult::Cancelled) {
                return self.cancel_primary(primary).await;
            }
            if self.controls.goal_budget.active_time_exhausted() {
                return self.settle_stopped_primary(primary).await;
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
                    if self.pause_at_boundary() {
                        return self.settle_stopped_primary(primary).await;
                    }
                    if let Some(contract) = plan_contract
                        && !self.admit_plan_contract(root_request, *contract).await?
                    {
                        return self.fail_primary(primary).await;
                    }
                    match self.execute_tool(root_request, call, false, &primary).await {
                        Ok(true) => {}
                        Ok(false) if self.controls.cancellation.is_cancelled() => {
                            return self.settle_stopped_primary(primary).await;
                        }
                        Ok(false) if self.controls.goal_budget.active_time_exhausted() => {
                            return self.settle_stopped_primary(primary).await;
                        }
                        Ok(false) => return self.fail_primary(primary).await,
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

    /// Why children still queued after a fill will not start: a pause first, then an exhausted call budget.
    fn unstarted_abort(&self, queue: &ChildQueue) -> Option<AbortReason> {
        if queue.pending.is_empty() {
            None
        } else if self.pause_requested() {
            Some(AbortReason::Paused)
        } else if !self.controls.goal_budget.can_start_provider_call() {
            Some(AbortReason::Budget)
        } else {
            None
        }
    }

    async fn fill_slots(
        &self,
        queue: &mut ChildQueue,
        children: &mut [ChildWork],
        root_request: &ProviderRequest,
        deadline: Instant,
    ) -> Result<(), EngineError> {
        while queue.active.len()
            < self
                .controls
                .goal_budget
                .provider_concurrency(queue.parallel)
            && !self.controls.cancellation.is_cancelled()
            && !self.pause_requested()
            && self.controls.goal_budget.can_start_provider_call()
        {
            let Some(index) = queue.pending.pop_front() else {
                break;
            };
            let child = &mut children[index];
            if let Some(tools) = child.tools.as_mut() {
                let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
                let room = tools.serialized_len().is_ok_and(|bytes| {
                    runtime
                        .result_capacity(bytes + CHILD_INTENT_RESERVE_BYTES, 0)
                        .is_some_and(|capacity| capacity >= MIN_CHILD_RESULT_BYTES)
                });
                if !room {
                    child.tool_calls_remaining = 0;
                }
                tools.catalog = runtime
                    .research_context(child.tool_calls_remaining)?
                    .catalog;
            }
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
                goal_plan_contract: children[index]
                    .tools
                    .is_some()
                    .then(|| root_request.goal_plan_contract.clone())
                    .flatten(),
                goal_context_facts: None,
                child_results: Vec::new(),
                steering_notes: Vec::new(),
                max_output_tokens: CHILD_OUTPUT_TOKEN_CAP.min(root_request.max_output_tokens),
                tools: children[index].tools.clone(),
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

    fn valid_child_tool(
        &self,
        child: &ChildWork,
        call: &ToolCall,
        has_plan_contract: bool,
    ) -> bool {
        !has_plan_contract
            && child.tools.is_some()
            && child.tool_calls_remaining > 0
            && call.valid()
            && call.is_child_research()
    }

    /// Runs one read-only child Tool inside the delegation's active segment; `false` stops the child.
    async fn execute_child_tool(
        &self,
        child: &mut ChildWork,
        call: ToolCall,
        deadline: Instant,
    ) -> Result<bool, EngineError> {
        // Copy the id first so the mutable borrow of `child.tools` below stays disjoint.
        let agent_run_id = child.agent_run_id;
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        let tools = child.tools.as_mut().ok_or(EngineError::CoordinatorFailed)?;
        let mut intent = runtime.intent(self.run_id, agent_run_id, call);
        let available = deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(
                TOOL_PREPARATION_OVERHEAD + TOOL_CLEANUP_OVERHEAD + STORE_ACTION_OVERHEAD,
            );
        // Half for the Guard capacity wait (it ends at `expires_at_ms`), half for the run itself.
        let runtime_ms = intent
            .limits
            .runtime_ms
            .min(u32::try_from(duration_ms(available) / 2).unwrap_or(u32::MAX));
        if runtime_ms == 0 {
            return Ok(false);
        }
        intent.limits.runtime_ms = runtime_ms;
        intent.expires_at_ms = crate::tools::now_ms().saturating_add(u64::from(runtime_ms));
        let mut reserved = tools.clone();
        reserved.observations.push(ToolObservation {
            intent: intent.clone(),
            disposition: ToolDisposition::Succeeded,
            output: String::new(),
            guard: Some(GuardReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                enforcement_digest: intent.enforcement_digest,
                limits: intent.limits.clone(),
            }),
            cache_update: CacheUpdate::NotApplicable,
        });
        let overhead = serde_json::to_vec(&reserved)
            .map_err(|_| EngineError::InvalidRequest)?
            .len();
        let Some(capacity) = runtime.result_capacity(overhead, 0) else {
            return Ok(false);
        };
        intent.limits.result_bytes =
            capacity.min(crate::tools::types::MAX_TOOL_RESULT_BYTES) as u32;
        let pending = self
            .controls
            .goal_budget
            .in_flight_children
            .load(Ordering::Acquire);
        self.store
            .admit_operation(
                self.session_id,
                12_usize.saturating_add(
                    usize::try_from(pending)
                        .unwrap_or(usize::MAX)
                        .saturating_mul(2),
                ),
            )
            .await?;
        self.append(Event::ToolStarted {
            run_id: self.run_id,
            agent_run_id,
            intent: intent.clone(),
        })
        .await?;
        let observation = if self.controls.goal_budget.allows_tool(&intent.call) {
            let worker_cancellation = RunCancellation::new();
            let mut run_cancellation = self.controls.cancellation.clone();
            let execution = runtime.execute(intent, worker_cancellation.clone());
            tokio::pin!(execution);
            tokio::select! {
                biased;
                () = run_cancellation.cancelled() => {
                    worker_cancellation.cancel();
                    execution.await
                }
                observation = &mut execution => observation,
            }
        } else {
            ToolObservation {
                intent,
                disposition: ToolDisposition::Denied,
                output: "The effect is outside the active Goal plan; no operation was dispatched"
                    .into(),
                guard: None,
                cache_update: CacheUpdate::NotApplicable,
            }
        };
        self.append(Event::ToolFinished {
            run_id: self.run_id,
            agent_run_id,
            observation: observation.clone(),
        })
        .await?;
        let continues = !observation.stops_run() && !self.controls.cancellation.is_cancelled();
        tools.observations.push(observation);
        Ok(continues
            && tools
                .serialized_len()
                .is_ok_and(|bytes| bytes <= runtime.context_limit_bytes()))
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

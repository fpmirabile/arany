use super::{
    Action, ActiveSegment, CallFailure, CallResult, Grant, MAX_PRIMARY_RESULT_BYTES, RunLoop,
    call_record, valid_finish,
};
use crate::engine::{CHILD_OUTPUT_TOKEN_CAP, EngineError};
use crate::provider::{
    AgentPhase, ChildResult, Finish, PromptRole, ProviderOutcome, ProviderRequest,
    ProviderResponse, ProviderTurn, RunInput,
};
use crate::session::{
    AgentDisposition, AgentRole, AgentRunId, ChildGrant, CollaborationPolicy, Event,
    MAX_CHILD_TOOL_CALLS,
};
use crate::tools::{ToolContext, ToolRequest, ToolStep};
use std::collections::{HashMap, VecDeque};
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

pub(super) struct ChildWork {
    pub(super) agent_run_id: AgentRunId,
    objective: String,
    /// Read-only research context; `None` for a `NoTools` child.
    pub(super) tools: Option<ToolContext>,
    pub(super) tool_calls_remaining: usize,
}

struct Spawned {
    children: Vec<ChildWork>,
    parallel: usize,
    active_segment: ActiveSegment,
}

/// Where the children stand once no call is in flight.
struct Drained {
    results: Vec<Option<ChildResult>>,
    abort_reason: Option<AbortReason>,
    pending: VecDeque<usize>,
}

struct ChildQueue {
    pending: VecDeque<usize>,
    calls: JoinSet<crate::engine::routing::AttemptResult>,
    active: HashMap<Id, ActiveChild>,
    parallel: usize,
}

struct ActiveChild {
    index: usize,
    request: ProviderRequest,
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
            let _ = tokio::time::timeout_at(deadline, self.calls.join_next_with_id())
                .await
                .map_err(|_| EngineError::CancellationDrainTimeout)?
                .ok_or(EngineError::CoordinatorFailed)?;
        }
        self.active.clear();
        Ok(())
    }
}

impl RunLoop<'_> {
    pub(super) fn valid_delegation(&self, children: &[String]) -> bool {
        self.ledger().can_start_provider_call()
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
        delegate_turn: ProviderTurn,
    ) -> Result<(), EngineError> {
        let Some(Spawned {
            mut children,
            parallel,
            active_segment,
        }) = self.spawn_children(objectives).await?
        else {
            return self.settle_stopped_primary().await;
        };
        let deadline = active_segment.deadline();
        let Drained {
            results,
            mut abort_reason,
            pending,
        } = self
            .drive_children(&mut children, root_request, parallel, deadline)
            .await?;
        self.settle_active_segment(active_segment)?;
        if matches!(abort_reason, Some(AbortReason::Paused)) {
            self.pause_at_boundary();
        }

        if self.controls.cancellation.is_cancelled() {
            abort_reason = Some(AbortReason::Cancelled);
        }
        if abort_reason.is_none() && self.ledger().active_time_exhausted() {
            abort_reason = Some(AbortReason::Budget);
        }
        if abort_reason.is_none() && !pending.is_empty() {
            abort_reason = Some(AbortReason::Failed);
        }
        if let Some(reason) = abort_reason {
            for index in pending {
                self.finish_child(
                    children[index].agent_run_id,
                    AgentDisposition::Cancelled,
                    None,
                    false,
                )
                .await?;
            }
            return match reason {
                AbortReason::Failed if self.ledger().active_time_exhausted() => {
                    self.settle_stopped_primary().await
                }
                AbortReason::Failed => self.fail_primary().await,
                AbortReason::Budget | AbortReason::Cancelled | AbortReason::Paused => {
                    self.settle_stopped_primary().await
                }
            };
        }
        self.synthesize(root_request, results, delegate_turn).await
    }

    /// Reserves the delegation and journals each child; `None` means the primary must stop.
    async fn spawn_children(
        &self,
        objectives: Vec<String>,
    ) -> Result<Option<Spawned>, EngineError> {
        let research = self.controls.tools.is_some();
        let delegation = Action::Delegation {
            children: objectives.len(),
            research,
        };
        if !self.ledger().admits(delegation) {
            return Ok(None);
        }
        let tool_allowance = if research {
            self.ledger()
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
        let parallel = self.ledger().provider_concurrency(
            usize::from(self.controls.concurrency)
                .min(usize::from(self.controls.policy.max_children())),
        );
        let waves = objectives.len().div_ceil(parallel.max(1));
        let active_bound = delegation_active_bound(
            research,
            self.routing.providers.child[0].identity().limits.deadline,
            waves,
        );
        // Covers every child's calls, Tools and terminal facts plus the largest later admission, so a refusal precedes AgentSpawned.
        self.journal
            .admit(
                self.session_id,
                delegation.max_events_for(&self.routing.providers),
                delegation.tool_observations(),
            )
            .await?;
        let active_segment = match self.reserve_active_segment(active_bound) {
            Ok(segment) => segment,
            Err(EngineError::GoalBudgetExhausted) => return Ok(None),
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
            });
        }
        if self.controls.cancellation.is_cancelled() {
            self.settle_active_segment(active_segment)?;
            for child in children {
                self.finish_child(child.agent_run_id, AgentDisposition::Cancelled, None, false)
                    .await?;
            }
            return Ok(None);
        }
        Ok(Some(Spawned {
            children,
            parallel,
            active_segment,
        }))
    }

    /// Runs the children's calls and Tool steps in bounded slots until each one finished or the
    /// delegation aborted.
    async fn drive_children(
        &self,
        children: &mut [ChildWork],
        root_request: &ProviderRequest,
        parallel: usize,
        deadline: Instant,
    ) -> Result<Drained, EngineError> {
        let mut results = vec![None; children.len()];
        let mut queue = ChildQueue::new(children.len(), parallel);
        if let Err(error) = self
            .fill_slots(&mut queue, children, root_request, deadline)
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
            if result.eligible
                && abort_reason.is_none()
                && !self.controls.cancellation.is_cancelled()
                && !self.pause_requested()
                && Instant::now() < deadline
            {
                let retried: Result<bool, EngineError> = async {
                    let role = crate::provider::ProviderRole::Child;
                    let active = queue
                        .active
                        .get(&task_id)
                        .ok_or(EngineError::CoordinatorFailed)?;
                    let provider = self.routing.provider(role, active.request.agent_run_id)?;
                    if self
                        .routing
                        .switch(
                            (self.session_id, self.run_id, active.request.agent_run_id),
                            role,
                            provider.identity(),
                            Some(&active.request),
                        )
                        .await?
                    {
                        let mut active = queue
                            .active
                            .remove(&task_id)
                            .ok_or(EngineError::CoordinatorFailed)?;
                        let provider = self.routing.provider(role, active.request.agent_run_id)?;
                        active.request.model = provider.identity().model.clone();
                        active.request.max_output_tokens = active
                            .request
                            .max_output_tokens
                            .min(provider.identity().limits.max_output_tokens);
                        let request = active.request.clone();
                        let cancellation = self.controls.cancellation.clone();
                        let handle = queue.calls.spawn(async move {
                            crate::engine::routing::invoke_attempt(
                                provider.as_ref(),
                                request,
                                deadline,
                                cancellation,
                                Some(crate::provider::ResponsePreview::new().0),
                            )
                            .await
                        });
                        queue.active.insert(handle.id(), active);
                        return Ok(true);
                    }
                    Ok(false)
                }
                .await;
                match retried {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(error) => return Err(queue.abort_with(error).await),
                }
            }
            queue.active.remove(&task_id);
            let result = result.result;
            self.settle_result(&result)?;
            let valid_response = self.ledger().tokens_within_limit()
                && match &result {
                    CallResult::Response(ProviderResponse {
                        outcome: ProviderOutcome::Finish(finish),
                        ..
                    }) => valid_finish(finish, MAX_CHILD_RESULT_BYTES),
                    CallResult::Response(ProviderResponse {
                        outcome: ProviderOutcome::Tools(requests),
                        ..
                    }) => self.valid_child_tools(&children[index], requests),
                    _ => false,
                };
            let step_disposition = match self
                .settle_child_call(
                    &mut children[index],
                    &mut results[index],
                    result,
                    valid_response,
                    &mut abort_reason,
                    deadline,
                )
                .await
            {
                Ok(None) => {
                    queue.pending.push_back(index);
                    None
                }
                Ok(Some(disposition)) => Some(disposition),
                Err(error) => return Err(queue.abort_with(error).await),
            };
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
                    .fill_slots(&mut queue, children, root_request, deadline)
                    .await
                {
                    return Err(queue.abort_with(error).await);
                }
                abort_reason = self.unstarted_abort(&queue);
            }
        }
        Ok(Drained {
            results,
            abort_reason,
            pending: queue.pending,
        })
    }

    /// Journals one child's finished call, then finishes the child or runs its Tool step; `None`
    /// means the child continues with another call.
    async fn settle_child_call(
        &self,
        child: &mut ChildWork,
        result_slot: &mut Option<ChildResult>,
        result: CallResult,
        valid_response: bool,
        abort_reason: &mut Option<AbortReason>,
        deadline: Instant,
    ) -> Result<Option<AgentDisposition>, EngineError> {
        let agent_run_id = child.agent_run_id;
        self.append(Event::ProviderCallRecorded {
            run_id: self.run_id,
            agent_run_id,
            record: call_record(AgentPhase::ChildWork, &result, valid_response),
        })
        .await?;
        let disposition = match result {
            CallResult::Response(ProviderResponse {
                outcome: ProviderOutcome::Finish(finish),
                ..
            }) if valid_response => {
                self.finish_child(
                    agent_run_id,
                    AgentDisposition::Finished,
                    Some(&finish),
                    true,
                )
                .await?;
                *result_slot = Some(ChildResult {
                    objective: child.objective.clone(),
                    summary: finish.summary,
                    result: finish.result,
                });
                AgentDisposition::Finished
            }
            CallResult::Response(ProviderResponse {
                outcome: ProviderOutcome::Tools(mut requests),
                turn,
                ..
            }) if valid_response => {
                self.ledger().child_call_finished()?;
                // An exhausted Goal budget stops the child before its Tool starts, like a pause.
                let budget_spent = !self.ledger().can_start_provider_call()
                    || self.ledger().remaining_calls() == Some(0);
                if budget_spent && abort_reason.is_none() {
                    *abort_reason = Some(AbortReason::Budget);
                }
                let skipped = self.controls.cancellation.is_cancelled()
                    || self.pause_requested()
                    || abort_reason.is_some();
                let continues = if skipped {
                    false
                } else {
                    let request = requests.pop().ok_or(EngineError::CoordinatorFailed)?;
                    // Boxed: the Tool future is large and would overflow the caller's stack.
                    Box::pin(self.execute_step(
                        Grant::Researcher {
                            child: &mut *child,
                            deadline,
                        },
                        request,
                        turn,
                    ))
                    .await?
                };
                if continues {
                    child.tool_calls_remaining = child.tool_calls_remaining.saturating_sub(1);
                    return Ok(None);
                }
                let disposition = if skipped || self.controls.cancellation.is_cancelled() {
                    AgentDisposition::Cancelled
                } else {
                    AgentDisposition::Failed
                };
                self.finish_child(agent_run_id, disposition, None, false)
                    .await?;
                disposition
            }
            other => {
                let disposition = if matches!(other, CallResult::Cancelled) {
                    AgentDisposition::Cancelled
                } else {
                    AgentDisposition::Failed
                };
                self.finish_child(agent_run_id, disposition, None, true)
                    .await?;
                disposition
            }
        };
        Ok(Some(disposition))
    }

    /// Asks the primary to synthesize the children's results; it may continue with Tool steps.
    async fn synthesize(
        &self,
        root_request: &mut ProviderRequest,
        results: Vec<Option<ChildResult>>,
        delegate_turn: ProviderTurn,
    ) -> Result<(), EngineError> {
        if !self.ledger().can_start_provider_call() {
            return self.settle_stopped_primary().await;
        }
        root_request.phase = AgentPhase::RootSynthesis;
        root_request.input.child_results = results
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or(EngineError::CoordinatorFailed)?;
        if let Some(tools) = root_request.input.tools.as_mut() {
            tools.steps.push(ToolStep {
                turn: delegate_turn,
                requests: Vec::new(),
                observations: Vec::new(),
                facts: None,
                steering_notes: Vec::new(),
                calls_remaining: None,
            });
        }
        loop {
            if self.pause_at_boundary() {
                return self.settle_stopped_primary().await;
            }
            let (synthesis, active_segment) = match self.invoke_primary(root_request).await {
                Ok(result) => result,
                Err(EngineError::GoalBudgetExhausted) => {
                    return self.settle_stopped_primary().await;
                }
                Err(error) => return Err(error),
            };
            let valid_response = self.ledger().tokens_within_limit()
                && (matches!(
                    &synthesis,
                    CallResult::Response(ProviderResponse {
                        outcome: ProviderOutcome::Finish(finish),
                        ..
                    }) if valid_finish(finish, MAX_PRIMARY_RESULT_BYTES)
                ) || matches!(&synthesis, CallResult::Response(ProviderResponse { outcome: ProviderOutcome::Tools(requests), .. }) if self.valid_tools(requests)));
            self.append(Event::ProviderCallRecorded {
                run_id: self.run_id,
                agent_run_id: self.primary_id,
                record: call_record(AgentPhase::RootSynthesis, &synthesis, valid_response),
            })
            .await?;
            self.settle_active_segment(active_segment)?;
            if self.controls.cancellation.is_cancelled() {
                return self.settle_stopped_primary().await;
            }
            if matches!(&synthesis, CallResult::Cancelled) {
                return self.cancel_primary().await;
            }
            if self.ledger().active_time_exhausted() {
                return self.settle_stopped_primary().await;
            }
            match synthesis {
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Finish(finish),
                    ..
                }) if valid_response => return self.finish_primary(finish).await,
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Tools(requests),
                    turn,
                    ..
                }) if valid_response => {
                    if self.pause_at_boundary() {
                        return self.settle_stopped_primary().await;
                    }
                    let Some(request) = self.begin_tool_step(requests).await? else {
                        return self.fail_primary().await;
                    };
                    let grant = Grant::Primary {
                        request: &mut *root_request,
                        review_allowed: false,
                    };
                    match self.execute_step(grant, request, turn).await {
                        Ok(true) => {}
                        Ok(false) if self.controls.cancellation.is_cancelled() => {
                            return self.settle_stopped_primary().await;
                        }
                        Ok(false) if self.ledger().active_time_exhausted() => {
                            return self.settle_stopped_primary().await;
                        }
                        Ok(false) => return self.fail_primary().await,
                        Err(EngineError::GoalBudgetExhausted) => {
                            return self.settle_stopped_primary().await;
                        }
                        Err(error) => return Err(error),
                    }
                }
                CallResult::Cancelled => return self.cancel_primary().await,
                _ => return self.fail_primary().await,
            }
        }
    }

    /// Why children still queued after a fill will not start: a pause first, then an exhausted call budget.
    fn unstarted_abort(&self, queue: &ChildQueue) -> Option<AbortReason> {
        if queue.pending.is_empty() {
            None
        } else if self.pause_requested() {
            Some(AbortReason::Paused)
        } else if !self.ledger().can_start_provider_call() {
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
        while queue.active.len() < self.ledger().provider_concurrency(queue.parallel)
            && !self.controls.cancellation.is_cancelled()
            && !self.pause_requested()
            && self.ledger().can_start_provider_call()
        {
            let Some(index) = queue.pending.pop_front() else {
                break;
            };
            let child = &mut children[index];
            if let Some(tools) = child.tools.as_mut() {
                let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
                let room = runtime
                    .result_capacity(tools.rendered_len() + CHILD_INTENT_RESERVE_BYTES, 0)
                    .is_some_and(|capacity| capacity >= MIN_CHILD_RESULT_BYTES);
                if !room {
                    child.tool_calls_remaining = 0;
                }
                // The catalog is the request head and stays as first sent; later calls carry the
                // remaining budget in their newest step.
                match tools.steps.last_mut() {
                    Some(step) => step.calls_remaining = Some(child.tool_calls_remaining),
                    None => {
                        tools.catalog = runtime
                            .research_context(child.tool_calls_remaining)?
                            .catalog;
                    }
                }
            }
            let provider = self.routing.provider(
                crate::provider::ProviderRole::Child,
                children[index].agent_run_id,
            )?;
            let cancellation = self.controls.cancellation.clone();
            let researcher = children[index].tools.is_some();
            let root = &root_request.input;
            let request = ProviderRequest {
                session_id: self.session_id,
                run_id: self.run_id,
                agent_run_id: children[index].agent_run_id,
                phase: AgentPhase::ChildWork,
                collaboration: CollaborationPolicy::Single,
                model: provider.identity().model.clone(),
                max_output_tokens: CHILD_OUTPUT_TOKEN_CAP
                    .min(provider.identity().limits.max_output_tokens),
                input: RunInput {
                    images: root
                        .images
                        .iter()
                        .filter(|value| value.origin == crate::provider::ImageOrigin::Objective)
                        .cloned()
                        .collect(),
                    goal_budget: Some(self.ledger().effective_budget()?),
                    goal_plan_contract: if researcher {
                        self.ledger().plan_contract()?
                    } else {
                        None
                    },
                    workspace_guidance: root.workspace_guidance.clone(),
                    includes: root.includes.clone(),
                    tools: children[index].tools.clone(),
                    ..RunInput::new(
                        if researcher {
                            PromptRole::Researcher
                        } else {
                            PromptRole::ReadOnly
                        },
                        children[index].objective.clone(),
                    )
                },
            };
            self.reserve_provider_call(
                children[index].agent_run_id,
                Action::Child {
                    in_flight: self.ledger().in_flight_children(),
                    research: researcher,
                },
                0,
            )
            .await?;
            let preview = (self.routing.providers.child.len() > 1)
                .then(|| crate::provider::ResponsePreview::new().0);
            let attempt_request = request.clone();
            let handle = queue.calls.spawn(async move {
                crate::engine::routing::invoke_attempt(
                    provider.as_ref(),
                    attempt_request,
                    deadline,
                    cancellation,
                    preview,
                )
                .await
            });
            queue
                .active
                .insert(handle.id(), ActiveChild { index, request });
        }
        Ok(())
    }

    fn valid_child_tools(&self, child: &ChildWork, requests: &[ToolRequest]) -> bool {
        matches!(requests, [request]
            if request.plan_contract.is_none()
                && child.tools.is_some()
                && child.tool_calls_remaining > 0
                && request.call.valid()
                && request.call.is_child_research())
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
            self.ledger().child_call_finished()?;
        }
        Ok(())
    }
}

fn classify_join(
    joined: Result<(Id, crate::engine::routing::AttemptResult), JoinError>,
) -> (Id, crate::engine::routing::AttemptResult) {
    match joined {
        Ok((task_id, result)) => (task_id, result),
        Err(error) => (
            error.id(),
            crate::engine::routing::AttemptResult {
                eligible: false,
                result: if error.is_cancelled() {
                    CallResult::Cancelled
                } else {
                    CallResult::Failed(CallFailure::TaskPanic)
                },
            },
        ),
    }
}

/// Researcher delegations use the fixed bound; a tool-free delegation runs one call per wave.
fn delegation_active_bound(research: bool, call_deadline: Duration, waves: usize) -> Duration {
    if research {
        MAX_DELEGATION_DEADLINE
    } else {
        call_deadline
            .saturating_mul(u32::try_from(waves).unwrap_or(u32::MAX))
            .min(MAX_DELEGATION_DEADLINE)
    }
}

#[cfg(test)]
mod delegation_bound_tests {
    use super::*;

    #[test]
    fn tool_free_delegation_is_capped_at_the_delegation_deadline() {
        let call = Duration::from_secs(600);
        assert_eq!(
            delegation_active_bound(false, call, 1),
            Duration::from_secs(600)
        );
        assert_eq!(
            delegation_active_bound(false, call, 2),
            MAX_DELEGATION_DEADLINE
        );
        assert_eq!(
            delegation_active_bound(false, call, 4),
            MAX_DELEGATION_DEADLINE
        );
        assert_eq!(
            delegation_active_bound(true, Duration::from_secs(120), 1),
            MAX_DELEGATION_DEADLINE
        );
        assert_eq!(MAX_DELEGATION_DEADLINE, Duration::from_secs(20 * 60));
    }
}

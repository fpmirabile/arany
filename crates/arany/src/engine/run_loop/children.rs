use super::{
    Action, ActiveSegment, CallFailure, CallResult, Grant, RunLoop, call_record, valid_finish,
};
use crate::engine::routing::CallScope;
use crate::engine::{CHILD_OUTPUT_TOKEN_CAP, EngineError};
use crate::provider::{
    AgentPhase, ChildAssignment, ChildResult, Finish, PromptRole, ProviderOutcome, ProviderRequest,
    ProviderResponse, ProviderTurn, RunInput,
};
use crate::session::{
    AgentDisposition, AgentRole, AgentRunId, ChildGrant, CollaborationPolicy, Event,
    MAX_CHILD_TOOL_CALLS, MAX_CHILD_TURNS,
};
use crate::tools::{ToolCall, ToolContext, ToolRequest, ToolStep};
use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use tokio::task::{Id, JoinError, JoinSet};
use tokio::time::Instant;

const MAX_CHILD_RESULT_BYTES: usize = 16 * 1024;
const CANCELLATION_DRAIN: Duration = Duration::from_secs(2);
const MAX_DELEGATION_DEADLINE: Duration = Duration::from_secs(20 * 60);
const CHILD_INTENT_RESERVE_BYTES: usize = 4 * 1024;
// Keep enough room for a full Read page and its metadata before offering child Tools.
const MIN_CHILD_RESULT_BYTES: usize = 4096 + 512;

pub(super) struct ChildWork {
    pub(super) agent_run_id: AgentRunId,
    pub(super) objective: String,
    pub(super) definition: Option<crate::tools::definitions::AgentDefinition>,
    pub(super) turns_remaining: u8,
    pub(super) editor: bool,
    pub(super) stop_hook_feedback: Vec<crate::provider::StopHookFeedback>,
    pub(super) tools: Option<ToolContext>,
    pub(super) tool_calls_remaining: usize,
}

struct Spawned {
    children: Vec<ChildWork>,
    parallel: usize,
    active_segment: Option<ActiveSegment>,
}

struct Drained {
    abort_reason: Option<AbortReason>,
    pending: VecDeque<usize>,
}

struct ChildQueue {
    pending: VecDeque<usize>,
    calls: JoinSet<crate::engine::routing::AttemptResult>,
    active: HashMap<Id, ActiveChild>,
    parallel: usize,
    unspawned: VecDeque<ChildAssignment>,
    definition: Option<String>,
    tool_allowance: usize,
    primary_step: Option<(Vec<ToolRequest>, ProviderTurn)>,
    primary_pending: bool,
    deferred_at: Option<usize>,
    refusal: Option<&'static str>,
    budget_stopped: bool,
}

struct ActiveChild {
    index: Option<usize>,
    request: ProviderRequest,
}

enum AbortReason {
    Failed,
    Hook,
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
            unspawned: VecDeque::new(),
            definition: None,
            tool_allowance: 0,
            primary_step: None,
            primary_pending: false,
            deferred_at: None,
            refusal: None,
            budget_stopped: false,
        }
    }

    // Preserve the original abort cause if draining also fails.
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
    pub(super) fn well_formed_delegation(
        &self,
        children: &[ChildAssignment],
        agent: Option<&str>,
    ) -> bool {
        if children.iter().any(|child| child.editor.is_some()) {
            return self.valid_editor_delegation(children, agent);
        }
        self.wave_settled().unwrap_or(false)
            && !matches!(self.controls.policy, CollaborationPolicy::Single)
            && crate::provider::valid_assignments(children, agent)
            && self
                .ledger()
                .view(|v| {
                    v.runs.last().is_some_and(|r| {
                        let attempts: usize = r
                            .primary()
                            .into_iter()
                            .flat_map(|a| &a.provider_calls)
                            .filter_map(|c| c.delegation.as_ref())
                            .map(Vec::len)
                            .sum();
                        attempts + children.len()
                            <= usize::from(self.controls.policy.max_children())
                    })
                })
                .unwrap_or(false)
            && children
                .iter()
                .all(|a| self.valid_research_assignment(a, agent))
    }

    pub(super) fn valid_research_assignment(
        &self,
        assignment: &ChildAssignment,
        agent: Option<&str>,
    ) -> bool {
        agent.is_none_or(|name| {
            self.definitions
                .get()
                .and_then(|r| r.get(name))
                .is_some_and(|d| !d.editor())
        }) && assignment.profile.as_ref().is_none_or(|name| {
            self.routing
                .providers
                .roster
                .as_ref()
                .is_some_and(|r| r.snapshot.child_profiles.contains(name))
        })
    }

    pub(super) fn run_children<'a>(
        &'a self,
        root_request: &'a mut ProviderRequest,
        objectives: Vec<ChildAssignment>,
        agent: Option<String>,
        delegate_turn: ProviderTurn,
    ) -> super::step::BoxedStep<'a, bool> {
        Box::pin(self.run_children_inner(root_request, objectives, agent, delegate_turn))
    }

    async fn run_children_inner(
        &self,
        root_request: &mut ProviderRequest,
        objectives: Vec<ChildAssignment>,
        agent: Option<String>,
        delegate_turn: ProviderTurn,
    ) -> Result<bool, EngineError> {
        let count = objectives.len();
        let Some(Spawned {
            mut children,
            parallel,
            mut active_segment,
        }) = self
            .spawn_children(count, objectives.iter().any(|a| a.editor.is_some()))
            .await?
        else {
            return self.settle_stopped_primary().await.map(|()| false);
        };
        let deadline = active_segment
            .as_ref()
            .expect("delegation segment")
            .deadline();
        let eager = objectives.iter().all(|a| a.editor.is_none());
        let mut queue = ChildQueue::new(0, parallel);
        queue.unspawned = objectives.into();
        queue.definition = agent;
        queue.tool_allowance =
            self.ledger()
                .remaining_calls()
                .map_or(MAX_CHILD_TOOL_CALLS, |remaining| {
                    (usize::try_from(remaining)
                        .unwrap_or(usize::MAX)
                        .saturating_sub(count + 1)
                        / 2
                        / count)
                        .min(MAX_CHILD_TOOL_CALLS)
                });
        if eager
            && self
                .spawn_researchers(
                    &mut queue,
                    &mut children,
                    root_request,
                    &mut active_segment,
                    deadline,
                )
                .await?
        {
            return self.settle_stopped_primary().await.map(|()| false);
        }
        if let Some(tools) = root_request.input.tools.as_mut() {
            tools.steps.push(ToolStep {
                turn: delegate_turn.clone(),
                requests: Vec::new(),
                observations: Vec::new(),
                facts: None,
                steering_notes: Vec::new(),
                calls_remaining: None,
                hook_feedback: Default::default(),
            });
        }
        let driven = self
            .drive_children(
                &mut children,
                root_request,
                &mut queue,
                &mut active_segment,
                deadline,
            )
            .await;
        let drained = match driven {
            Ok(drained) => drained,
            Err(error) => return Err(queue.abort_with(error).await),
        };
        self.settle_delegation(
            root_request,
            children,
            active_segment,
            drained,
            queue.primary_step.take(),
            deadline,
        )
        .await
    }

    fn settle_delegation<'b>(
        &'b self,
        root_request: &'b mut ProviderRequest,
        children: Vec<ChildWork>,
        active_segment: Option<ActiveSegment>,
        drained: Drained,
        primary_step: Option<(Vec<ToolRequest>, ProviderTurn)>,
        deadline: Instant,
    ) -> super::step::BoxedStep<'b, bool> {
        Box::pin(self.settle_delegation_inner(
            root_request,
            children,
            active_segment,
            drained,
            primary_step,
            deadline,
        ))
    }

    async fn settle_delegation_inner(
        &self,
        root_request: &mut ProviderRequest,
        mut children: Vec<ChildWork>,
        mut active_segment: Option<ActiveSegment>,
        drained: Drained,
        primary_step: Option<(Vec<ToolRequest>, ProviderTurn)>,
        deadline: Instant,
    ) -> Result<bool, EngineError> {
        let Drained {
            mut abort_reason,
            pending,
        } = drained;
        if let Some(segment) = active_segment.take() {
            self.settle_active_segment(segment)?;
        }
        if matches!(abort_reason, Some(AbortReason::Paused)) {
            self.pause_at_boundary();
        }

        if self.controls.cancellation.is_cancelled() {
            abort_reason = Some(AbortReason::Cancelled);
        }
        if abort_reason.is_none() && self.ledger().active_time_exhausted() {
            abort_reason = Some(AbortReason::Budget);
        }
        if abort_reason.is_none() && self.worker_stopped()? {
            abort_reason = Some(AbortReason::Budget);
        }
        if abort_reason.is_none() && !pending.is_empty() {
            abort_reason = Some(AbortReason::Failed);
        }
        if let Some(reason) = abort_reason {
            if children.iter().any(|c| c.editor) {
                self.observe_worker_stop().await?;
            }
            for index in pending {
                if children[index].editor {
                    let completed = self
                        .complete_worker(
                            &mut children[index],
                            None,
                            deadline,
                            !matches!(reason, AbortReason::Budget),
                        )
                        .await?;
                    self.settle_worker_handover(root_request, &children[index], completed)
                        .await?;
                } else {
                    self.finish_child(
                        children[index].agent_run_id,
                        AgentDisposition::Cancelled,
                        None,
                        false,
                    )
                    .await?;
                }
            }
            if self.uncertain_hook()? {
                return self.settle_stopped_primary().await.map(|()| false);
            }
            return match reason {
                AbortReason::Failed if self.ledger().active_time_exhausted() => {
                    self.settle_stopped_primary().await
                }
                AbortReason::Failed => self.fail_primary().await,
                AbortReason::Hook => self.finish_hook_stop().await,
                AbortReason::Budget | AbortReason::Cancelled | AbortReason::Paused => {
                    self.settle_stopped_primary().await
                }
            }
            .map(|()| false);
        }
        if let Some((requests, turn)) = primary_step {
            let Some(request) = self.begin_tool_step(requests).await? else {
                return self.fail_primary().await.map(|()| false);
            };
            if !Box::pin(self.execute_step(
                Grant::Primary {
                    request: root_request,
                    review_allowed: true,
                },
                request,
                turn,
            ))
            .await?
            {
                return self.fail_primary().await.map(|()| false);
            }
            self.verify_workers().await?;
        }
        self.begin_synthesis(root_request).await
    }

    fn settled_agents(&self) -> Result<usize, EngineError> {
        self.ledger().view(|v| {
            v.runs.last().map_or(0, |r| {
                r.agents
                    .iter()
                    .skip(1)
                    .filter(|a| a.status != crate::session::AgentStatus::Active)
                    .count()
            })
        })
    }

    async fn spawn_researchers(
        &self,
        queue: &mut ChildQueue,
        children: &mut Vec<ChildWork>,
        root_request: &mut ProviderRequest,
        segment: &mut Option<ActiveSegment>,
        deadline: Instant,
    ) -> Result<bool, EngineError> {
        while !queue.unspawned.is_empty() && !self.controls.cancellation.is_cancelled() {
            let child = match self
                .spawn_next_child(queue, root_request, segment, deadline)
                .await
            {
                Ok(Some(child)) => child,
                Ok(None) => break,
                Err(error) => return Err(queue.abort_with(error).await),
            };
            queue.pending.push_back(children.len());
            children.push(child);
        }
        if !self.controls.cancellation.is_cancelled() {
            return Ok(false);
        }
        if let Some(segment) = segment.take() {
            self.settle_active_segment(segment)?;
        }
        for child in children.iter() {
            self.finish_child(child.agent_run_id, AgentDisposition::Cancelled, None, false)
                .await?;
        }
        Ok(true)
    }

    async fn spawn_children(
        &self,
        count: usize,
        workers: bool,
    ) -> Result<Option<Spawned>, EngineError> {
        let research = self.controls.tools.is_some();
        let action = Action::Delegation {
            children: count,
            research,
        };
        if !self.ledger().admits(action) {
            return Ok(None);
        }
        let capacity = if workers {
            Action::Worker { chunks: 0 }
        } else {
            action
        };
        self.journal
            .admit(
                self.session_id,
                capacity.max_events_for_admitted(&self.routing.providers),
                capacity.tool_observations(),
            )
            .await?;
        let parallel = self.ledger().provider_concurrency(
            usize::from(self.controls.concurrency)
                .min(usize::from(self.controls.policy.max_children()))
                .min(self.ledger().view(|v| {
                    v.runs
                        .last()
                        .and_then(|r| r.config.as_ref())
                        .and_then(|c| c.worker_policy.as_ref())
                        .map_or(8, |p| usize::from(p.concurrency))
                })?),
        );
        let bound = delegation_active_bound(
            research,
            self.routing
                .providers
                .child_chains()
                .into_iter()
                .flatten()
                .map(|p| p.identity().limits.deadline)
                .max()
                .expect("child chain"),
            count.div_ceil(parallel.max(1)),
        );
        let segment = match self.reserve_active_segment(bound) {
            Ok(s) => s,
            Err(EngineError::GoalBudgetExhausted) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(Some(Spawned {
            children: Vec::with_capacity(count),
            parallel,
            active_segment: Some(segment),
        }))
    }

    async fn spawn_next_child(
        &self,
        queue: &mut ChildQueue,
        root: &mut ProviderRequest,
        segments: &mut Option<ActiveSegment>,
        deadline: Instant,
    ) -> Result<Option<ChildWork>, EngineError> {
        let Some(assignment) = queue.unspawned.pop_front() else {
            return Ok(None);
        };
        if let Some(editor) = assignment.editor {
            if let Some(s) = segments.take() {
                self.settle_active_segment(s)?;
            }
            let mut child = self.begin_worker(root, editor).await?;
            if let Some(child) = &mut child {
                child.tool_calls_remaining = child.tool_calls_remaining.min(queue.tool_allowance);
            }
            *segments = match self
                .reserve_active_segment(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(segment) => Some(segment),
                Err(EngineError::GoalBudgetExhausted) => {
                    queue.budget_stopped = true;
                    None
                }
                Err(error) => return Err(error),
            };
            return Ok(child);
        }
        let definition = queue
            .definition
            .as_ref()
            .and_then(|n| self.definitions.get()?.get(n))
            .cloned();
        let id = AgentRunId::new();
        let research = self.controls.tools.is_some();
        self.append(Event::AgentSpawned {
            run_id: self.run_id,
            agent_run_id: id,
            role: AgentRole::Child,
            ordinal: self
                .ledger()
                .view(|v| v.runs.last().expect("Run").agents.len())?
                .try_into()
                .map_err(|_| EngineError::CoordinatorFailed)?,
            objective: Some(assignment.objective.clone()),
            definition: queue.definition.clone(),
            profile: assignment.profile.or_else(|| self.default_child_profile()),
            grant: Some(if research {
                ChildGrant::ReadOnly
            } else {
                ChildGrant::NoTools
            }),
            assignment: None,
        })
        .await?;
        Ok(Some(ChildWork {
            agent_run_id: id,
            objective: assignment.objective,
            turns_remaining: definition.as_ref().map_or(MAX_CHILD_TURNS, |d| d.max_turns),
            definition,
            editor: false,
            stop_hook_feedback: Vec::new(),
            tools: self
                .controls
                .tools
                .map(|r| r.research_context(queue.tool_allowance))
                .transpose()?,
            tool_calls_remaining: queue.tool_allowance,
        }))
    }

    fn drive_children<'a>(
        &'a self,
        children: &'a mut Vec<ChildWork>,
        root_request: &'a mut ProviderRequest,
        queue: &'a mut ChildQueue,
        segments: &'a mut Option<ActiveSegment>,
        deadline: Instant,
    ) -> super::step::BoxedStep<'a, Drained> {
        Box::pin(self.drive_children_inner(children, root_request, queue, segments, deadline))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Keep child retry and settlement ordering together"
    )]
    async fn drive_children_inner(
        &self,
        children: &mut Vec<ChildWork>,
        root_request: &mut ProviderRequest,
        queue: &mut ChildQueue,
        segments: &mut Option<ActiveSegment>,
        deadline: Instant,
    ) -> Result<Drained, EngineError> {
        if let Err(error) = self
            .fill_slots(queue, children, root_request, segments, deadline)
            .await
        {
            return Err(queue.abort_with(error).await);
        }
        let mut abort_reason = self.unstarted_abort(queue);
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
                    let role = if index.is_none() {
                        crate::provider::ProviderRole::Primary
                    } else {
                        crate::provider::ProviderRole::Child
                    };
                    let active = queue
                        .active
                        .get(&task_id)
                        .ok_or(EngineError::CoordinatorFailed)?;
                    let provider = self.routing.provider(role, active.request.agent_run_id)?;
                    if self
                        .routing
                        .switch(
                            CallScope {
                                session_id: self.session_id,
                                run_id: self.run_id,
                                agent_run_id: active.request.agent_run_id,
                            },
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
                        let provider = self.routing.bind(
                            role,
                            active.request.agent_run_id,
                            &mut active.request.model,
                            &mut active.request.max_output_tokens,
                        )?;
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
            if index.is_none() {
                let valid = self.ledger().tokens_within_limit()
                    && matches!(&result, CallResult::Response(ProviderResponse { outcome: ProviderOutcome::Tools(requests), .. }) if self.well_formed_tools(requests) && requests.iter().all(|r| r.call.is_process() || self.primary_lease_allows(&r.call)));
                self.append(Event::ProviderCallRecorded {
                    run_id: self.run_id,
                    agent_run_id: self.primary_id,
                    record: call_record(AgentPhase::RootSynthesis, &result, valid, None),
                })
                .await?;
                match result {
                    CallResult::Response(ProviderResponse {
                        outcome: ProviderOutcome::Tools(requests),
                        turn,
                        ..
                    }) if valid => {
                        let process = requests.iter().any(|r| r.call.is_process());
                        if process {
                            queue.primary_step = Some((requests, turn));
                        } else {
                            if let Some(segment) = segments.take() {
                                self.settle_active_segment(segment)?;
                            }
                            let request = self.begin_tool_step(requests).await?;
                            let continues = if let Some(request) = request {
                                match Box::pin(self.execute_step(
                                    Grant::Primary {
                                        request: root_request,
                                        review_allowed: false,
                                    },
                                    request,
                                    turn,
                                ))
                                .await
                                {
                                    Ok(continues) => continues,
                                    Err(
                                        EngineError::GoalBudgetExhausted
                                        | EngineError::Tool(crate::tools::ToolError::Limit),
                                    ) => {
                                        queue.budget_stopped = true;
                                        self.observe_worker_stop().await?;
                                        true
                                    }
                                    Err(error) => return Err(error),
                                }
                            } else {
                                false
                            };
                            if !continues {
                                abort_reason = self.hook_abort()?.or(Some(AbortReason::Failed));
                            }
                            if !queue.budget_stopped {
                                self.verify_workers().await?;
                            }
                            *segments = match self.reserve_active_segment(
                                deadline.saturating_duration_since(Instant::now()),
                            ) {
                                Ok(segment) => Some(segment),
                                Err(EngineError::GoalBudgetExhausted) => {
                                    queue.budget_stopped = true;
                                    None
                                }
                                Err(error) => return Err(error),
                            };
                            queue.primary_pending = false;
                        }
                    }
                    CallResult::Response(ProviderResponse { outcome, .. })
                        if self.ledger().tokens_within_limit()
                            && !self.controls.cancellation.is_cancelled() =>
                    {
                        queue.refusal = Some(match outcome {
                            ProviderOutcome::Finish(_) => {
                                "Finish deferred: workers are still active; answer again after they settle."
                            }
                            ProviderOutcome::Delegate(_) => {
                                "Delegate refused: workers are still active."
                            }
                            ProviderOutcome::Tools(_) => {
                                "Tool request refused: it was malformed or edits inside a live worker lease."
                            }
                        });
                        queue.deferred_at = Some(self.settled_agents()?);
                        queue.primary_pending = false;
                    }
                    _ => {
                        abort_reason = Some(if self.controls.cancellation.is_cancelled() {
                            AbortReason::Cancelled
                        } else {
                            AbortReason::Failed
                        })
                    }
                }
                if abort_reason.is_some() {
                    queue.calls.abort_all();
                    drain_deadline = Some(Instant::now() + CANCELLATION_DRAIN);
                } else {
                    self.fill_slots(queue, children, root_request, segments, deadline)
                        .await?;
                }
                continue;
            }
            let index = index.ok_or(EngineError::CoordinatorFailed)?;
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
            let step_disposition = if children[index].editor {
                self.append(Event::ProviderCallRecorded {
                    run_id: self.run_id,
                    agent_run_id: children[index].agent_run_id,
                    record: call_record(AgentPhase::ChildWork, &result, valid_response, None),
                })
                .await?;
                self.ledger().child_call_finished()?;
                let finish = match result {
                    CallResult::Response(ProviderResponse {
                        outcome: ProviderOutcome::Finish(f),
                        turn,
                        ..
                    }) if valid_response && abort_reason.is_none() => {
                        let child = &mut children[index];
                        match Box::pin(self.worker_stop(child, &f, turn, deadline)).await? {
                            ChildStop::Proceed => Some(f),
                            ChildStop::Reprompt => {
                                queue.pending.push_back(index);
                                if let Err(error) = self
                                    .fill_slots(queue, children, root_request, segments, deadline)
                                    .await
                                {
                                    return Err(queue.abort_with(error).await);
                                }
                                continue;
                            }
                            ChildStop::End(_) => None,
                        }
                    }
                    CallResult::Response(ProviderResponse {
                        outcome: ProviderOutcome::Tools(mut requests),
                        turn,
                        ..
                    }) if valid_response
                        && abort_reason.is_none()
                        && !self.observe_worker_stop().await? =>
                    {
                        let child = &mut children[index];
                        let continues = match Box::pin(self.execute_step(
                            Grant::Researcher { child, deadline },
                            requests.pop().ok_or(EngineError::CoordinatorFailed)?,
                            turn,
                        ))
                        .await
                        {
                            Ok(continues) => continues,
                            Err(EngineError::Tool(crate::tools::ToolError::Limit)) => false,
                            Err(EngineError::GoalBudgetExhausted) => {
                                queue.budget_stopped = true;
                                self.observe_worker_stop().await?;
                                false
                            }
                            Err(error) => return Err(error),
                        };
                        child.tool_calls_remaining = child.tool_calls_remaining.saturating_sub(1);
                        let succeeded = self.ledger().view(|v| {
                            v.runs
                                .last()
                                .and_then(|r| r.agents.iter().find(|a| a.id == child.agent_run_id))
                                .and_then(|a| a.tools.last())
                                .and_then(|t| t.observation.as_ref())
                                .is_some_and(|o| {
                                    o.disposition == crate::tools::ToolDisposition::Succeeded
                                })
                        })?;
                        if continues && succeeded {
                            queue.pending.push_back(index);
                            if let Err(error) = self
                                .fill_slots(queue, children, root_request, segments, deadline)
                                .await
                            {
                                return Err(queue.abort_with(error).await);
                            }
                            continue;
                        }
                        None
                    }
                    _ => None,
                };
                let completed = self
                    .complete_worker(
                        &mut children[index],
                        finish,
                        deadline,
                        matches!(
                            abort_reason,
                            Some(AbortReason::Failed | AbortReason::Cancelled)
                        ),
                    )
                    .await?;
                if let Some(s) = segments.take() {
                    self.settle_active_segment(s)?;
                }
                self.settle_worker_handover(root_request, &children[index], completed)
                    .await?;
                *segments = match self
                    .reserve_active_segment(deadline.saturating_duration_since(Instant::now()))
                {
                    Ok(segment) => Some(segment),
                    Err(EngineError::GoalBudgetExhausted) => {
                        queue.budget_stopped = true;
                        None
                    }
                    Err(error) => return Err(error),
                };
                Some(
                    if self.actor_hook_requested_stop(children[index].agent_run_id)? {
                        AgentDisposition::StoppedByHook
                    } else {
                        AgentDisposition::Finished
                    },
                )
            } else {
                match self
                    .settle_child_call(
                        &mut children[index],
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
                }
            };
            if abort_reason.is_none() {
                abort_reason = if queue.budget_stopped {
                    Some(AbortReason::Budget)
                } else if self.controls.cancellation.is_cancelled() {
                    Some(AbortReason::Cancelled)
                } else if self.pause_requested() {
                    Some(AbortReason::Paused)
                } else if step_disposition == Some(AgentDisposition::StoppedByHook) {
                    Some(AbortReason::Hook)
                } else if let Some(reason) = self.hook_abort()? {
                    Some(reason)
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
                    .fill_slots(queue, children, root_request, segments, deadline)
                    .await
                {
                    return Err(queue.abort_with(error).await);
                }
                abort_reason = self.unstarted_abort(queue);
            }
        }
        Ok(Drained {
            abort_reason,
            pending: std::mem::take(&mut queue.pending),
        })
    }

    async fn settle_child_call(
        &self,
        child: &mut ChildWork,
        result: CallResult,
        valid_response: bool,
        abort_reason: &mut Option<AbortReason>,
        deadline: Instant,
    ) -> Result<Option<AgentDisposition>, EngineError> {
        let agent_run_id = child.agent_run_id;
        self.append(Event::ProviderCallRecorded {
            run_id: self.run_id,
            agent_run_id,
            record: call_record(AgentPhase::ChildWork, &result, valid_response, None),
        })
        .await?;
        if matches!(abort_reason, Some(AbortReason::Hook)) || self.hook_abort()?.is_some() {
            self.finish_child(agent_run_id, AgentDisposition::Cancelled, None, true)
                .await?;
            return Ok(Some(AgentDisposition::Cancelled));
        }
        let disposition = match result {
            CallResult::Response(ProviderResponse {
                outcome: ProviderOutcome::Finish(finish),
                turn,
                ..
            }) if valid_response => {
                if child.tools.is_some()
                    && !self.controls.cancellation.is_cancelled()
                    && abort_reason.is_none()
                {
                    match self
                        .subagent_stop(child, &finish, turn, abort_reason, deadline)
                        .await?
                    {
                        ChildStop::Proceed => {}
                        ChildStop::Reprompt => return Ok(None),
                        ChildStop::End(disposition) => return Ok(Some(disposition)),
                    }
                }
                self.finish_child(
                    agent_run_id,
                    AgentDisposition::Finished,
                    Some(&finish),
                    true,
                )
                .await?;
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
                let disposition = if self.actor_hook_requested_stop(agent_run_id)? {
                    AgentDisposition::StoppedByHook
                } else if skipped
                    || self.controls.cancellation.is_cancelled()
                    || self.hook_requested_stop()?
                {
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

    async fn subagent_stop(
        &self,
        child: &mut ChildWork,
        finish: &Finish,
        turn: ProviderTurn,
        abort_reason: &mut Option<AbortReason>,
        deadline: Instant,
    ) -> Result<ChildStop, EngineError> {
        let agent_run_id = child.agent_run_id;
        let stop = self
            .run_subagent_stop(child, "researcher", &finish.result, turn, deadline)
            .await;
        match stop {
            Ok(Some(())) => {}
            Ok(None) => {
                if self.actor_hook_requested_stop(agent_run_id)? {
                    self.finish_child(agent_run_id, AgentDisposition::StoppedByHook, None, true)
                        .await?;
                    return Ok(ChildStop::End(AgentDisposition::StoppedByHook));
                }
                return Ok(ChildStop::Proceed);
            }
            // A cut-off or cancelled hook ends this child only, unless its outcome stops the Run.
            Err(EngineError::GoalBudgetExhausted | EngineError::ContextTooLarge) => {
                let settled = self.ledger().view(|view| {
                    view.runs
                        .iter()
                        .find(|run| run.id == self.run_id)
                        .is_some_and(crate::session::RunView::hooks_settled)
                })?;
                if !settled && abort_reason.is_none() {
                    *abort_reason = Some(AbortReason::Budget);
                }
                let disposition = if self.controls.cancellation.is_cancelled() {
                    AgentDisposition::Cancelled
                } else {
                    AgentDisposition::Failed
                };
                self.finish_child(agent_run_id, disposition, None, true)
                    .await?;
                return Ok(ChildStop::End(disposition));
            }
            Err(error) => return Err(error),
        };
        self.ledger().child_call_finished()?;
        if child.turns_remaining > 0
            && tokio::time::Instant::now() < deadline
            && self.ledger().can_start_provider_call()
        {
            return Ok(ChildStop::Reprompt);
        }
        self.finish_child(agent_run_id, AgentDisposition::Failed, None, false)
            .await?;
        Ok(ChildStop::End(AgentDisposition::Failed))
    }

    pub(super) async fn run_subagent_stop(
        &self,
        child: &mut ChildWork,
        default_subject: &str,
        message: &str,
        turn: ProviderTurn,
        deadline: Instant,
    ) -> Result<Option<()>, EngineError> {
        let subject = child
            .definition
            .as_ref()
            .map_or(default_subject, |definition| definition.name.as_str())
            .to_owned();
        let fits = self
            .controls
            .tools
            .zip(child.tools.as_ref())
            .is_none_or(|(runtime, tools)| {
                tools.has_stop_room(&turn, runtime.context_limit_bytes())
            });
        let Some(feedback) = self
            .lifecycle_hooks(
                child.agent_run_id,
                crate::tools::permissions::HookEvent::SubagentStop,
                &subject,
                message,
                Some(deadline),
                fits,
            )
            .await?
        else {
            return Ok(None);
        };
        if let Some(tools) = child.tools.as_mut() {
            tools.add_stop_feedback(&turn, &feedback);
        }
        child
            .stop_hook_feedback
            .push(crate::provider::StopHookFeedback {
                after_step: child.tools.as_ref().map_or(0, |tools| tools.steps.len()),
                turn,
                text: feedback,
                facts: None,
                steering_notes: Vec::new(),
            });
        Ok(Some(()))
    }

    pub(super) async fn worker_stop(
        &self,
        child: &mut ChildWork,
        finish: &Finish,
        turn: ProviderTurn,
        deadline: Instant,
    ) -> Result<ChildStop, EngineError> {
        let stop = self
            .run_subagent_stop(child, "editor", &finish.result, turn, deadline)
            .await;
        match stop {
            Ok(Some(())) => Ok(ChildStop::Reprompt),
            Ok(None) if !self.actor_hook_requested_stop(child.agent_run_id)? => {
                Ok(ChildStop::Proceed)
            }
            Ok(None)
            | Err(
                EngineError::GoalBudgetExhausted
                | EngineError::ContextTooLarge
                | EngineError::Tool(crate::tools::ToolError::Uncertain),
            ) => Ok(ChildStop::End(AgentDisposition::Failed)),
            Err(error) => Err(error),
        }
    }

    fn hook_abort(&self) -> Result<Option<AbortReason>, EngineError> {
        if self.uncertain_hook()? {
            return Ok(Some(AbortReason::Failed));
        }
        if self.hook_requested_stop()? {
            return Ok(Some(AbortReason::Hook));
        }
        let settled = self.ledger().view(|view| {
            view.runs
                .last()
                .is_some_and(crate::session::RunView::hooks_settled)
        })?;
        Ok((!settled).then_some(AbortReason::Failed))
    }

    pub(super) async fn begin_synthesis(
        &self,
        root_request: &mut ProviderRequest,
    ) -> Result<bool, EngineError> {
        if !self.ledger().can_start_provider_call() {
            return self.settle_stopped_primary().await.map(|()| false);
        }
        root_request.phase = AgentPhase::RootSynthesis;
        self.refresh_team_context(root_request)?;
        Ok(true)
    }

    fn refresh_team_context(&self, root: &mut ProviderRequest) -> Result<(), EngineError> {
        let (reports, pending, integrations) = self.ledger().view(|v| {
            let run = v.runs.last().expect("Run");
            let proposals: Vec<_> = run.primary().into_iter().flat_map(|a| &a.provider_calls).filter_map(|c| c.delegation.as_ref()).flatten().collect();
            let mut order = std::collections::HashMap::new();
            for agent in run.agents.iter().skip(1) {
                let index = proposals.iter().enumerate().find(|(i, p)| !order.values().any(|v| v == i) && match p {
                    crate::worker::ChildAssignment::Editor { assignment } => agent.assignment.as_ref().is_some_and(|w| &w.pins.request == assignment.as_ref()),
                    crate::worker::ChildAssignment::Researcher { objective, definition, profile } => agent.assignment.is_none() && agent.objective.as_ref() == Some(objective) && &agent.definition == definition && &agent.profile == profile,
                }).map(|(index, _)| index).expect("spawn matches its proposal");
                order.insert(agent.id, index);
            }
            let mut reports = run
                .agents
                .iter()
                .skip(1)
                .filter(|a| a.status != crate::session::AgentStatus::Active)
                .map(|a| (order[&a.id], ChildResult {
                    objective: a.objective.clone().unwrap_or_default(),
                    summary: a
                        .summary
                        .clone()
                        .unwrap_or_else(|| "Worker did not finish".into()),
                    result: if let Some(w) = &a.assignment {
                        let mut report = format!(
                            "Worker {}: {:?}; integration: {:?}\n{}",
                            a.id,
                            w.outcome.as_ref().map(|(_, o)| o.disposition),
                            w.integration.as_ref().map(|(_, i)| i.outcome),
                            a.result
                                .as_deref()
                                .unwrap_or("Unpublished private work discarded")
                        );
                        let mut end = report.len().min(MAX_CHILD_RESULT_BYTES);
                        while !report.is_char_boundary(end) {
                            end -= 1;
                        }
                        report.truncate(end);
                        report
                    } else {
                        a.result.clone().unwrap_or_default()
                    },
                }))
                .collect::<Vec<_>>();
            for tool in &run.tools {
                if let ToolCall::PrepareWorker { preparation } = &tool.intent.call
                    && preparation.purpose == crate::worker::WorkerDataPurpose::Base
                    && !run.agents.iter().any(|a| a.id == preparation.assignment_id)
                    && let Some(o) = &tool.observation
                    && o.disposition != crate::tools::ToolDisposition::Succeeded {
                    let index = proposals.iter().enumerate().find(|(i, p)| matches!(p, crate::worker::ChildAssignment::Editor { .. }) && !order.values().any(|v| v == i)).map(|(i, _)| i).expect("refused capture matches a proposal");
                    order.insert(preparation.assignment_id, index);
                    reports.push((index, ChildResult { objective: "Editor base preparation".into(), summary: "Assignment did not start".into(), result: format!("Assignment {}: {:?}; no private work or Workspace publication", preparation.assignment_id, o.disposition) }));
                }
            }
            let pending = run
                .agents
                .iter()
                .skip(1)
                .filter(|a| a.status == crate::session::AgentStatus::Active)
                .map(|a| a.id.to_string())
                .collect::<Vec<_>>();
            let wave = run.primary().into_iter().flat_map(|a| &a.provider_calls).filter(|c| c.delegation.is_some()).count();
            let observations = run
                .tools
                .iter()
                .filter(|t| {
                    matches!(
                        &t.intent.call,
                        ToolCall::ApplyChangeset {
                            source: crate::worker::ChangesetSource::WorkerHandover { assignment_id, .. }
                        } if run.agents.iter().find(|a| a.id == *assignment_id).and_then(|a| a.assignment.as_ref()).is_some_and(|w| usize::from(w.pins.request.wave) == wave)
                    )
                })
                .filter(|t| t.observation.is_some())
                .cloned()
                .collect::<Vec<_>>();
            reports.sort_by_key(|(index, _)| *index);
            (reports.into_iter().map(|(_, report)| report).collect::<Vec<_>>(), pending, observations)
        })?;
        root.input.child_results = reports;
        if let Some(tools) = root.input.tools.as_mut() {
            if let Some(step) = tools.steps.iter_mut().rev().find(|s| s.requests.is_empty()) {
                step.observations = integrations
                    .iter()
                    .filter_map(|tool| tool.observation.clone())
                    .collect();
                step.hook_feedback = integrations
                    .iter()
                    .enumerate()
                    .filter_map(|(index, tool)| {
                        let feedback = tool.hook_feedback();
                        (!feedback.is_empty()).then_some((index, feedback))
                    })
                    .collect();
            }
            let mut catalog: serde_json::Value =
                serde_json::from_str(&tools.catalog).map_err(|_| EngineError::InvalidRequest)?;
            if let Some(fields) = catalog.as_object_mut() {
                fields.remove("last_answer_refused");
            }
            catalog["pending_workers"] = serde_json::json!(pending);
            catalog["worker_coordination"] = serde_json::json!(
                "While workers are pending, inspect or edit only paths outside their leases. Commands wait for lease quiescence. Finish or propose the next wave only after settlement and fresh primary verification."
            );
            tools.catalog = catalog.to_string();
        }
        Ok(())
    }

    // Honor pause before reporting an exhausted queued-child call budget.
    fn unstarted_abort(&self, queue: &ChildQueue) -> Option<AbortReason> {
        if self.pause_requested() {
            Some(AbortReason::Paused)
        } else if queue.budget_stopped {
            Some(AbortReason::Budget)
        } else if queue.pending.is_empty() && queue.unspawned.is_empty() {
            None
        } else if !self.ledger().can_start_provider_call()
            || self.ledger().remaining_calls() == Some(0)
        {
            Some(AbortReason::Budget)
        } else {
            None
        }
    }

    fn fill_slots<'a>(
        &'a self,
        queue: &'a mut ChildQueue,
        children: &'a mut Vec<ChildWork>,
        root_request: &'a mut ProviderRequest,
        segments: &'a mut Option<ActiveSegment>,
        deadline: Instant,
    ) -> super::step::BoxedStep<'a, ()> {
        Box::pin(self.fill_slots_inner(queue, children, root_request, segments, deadline))
    }

    async fn fill_slots_inner(
        &self,
        queue: &mut ChildQueue,
        children: &mut Vec<ChildWork>,
        root_request: &mut ProviderRequest,
        segments: &mut Option<ActiveSegment>,
        deadline: Instant,
    ) -> Result<(), EngineError> {
        while queue.active.len() < self.ledger().provider_concurrency(queue.parallel)
            && !self.controls.cancellation.is_cancelled()
            && !self.pause_requested()
            && self.ledger().can_start_provider_call()
            && self.ledger().remaining_calls() != Some(0)
            && !queue.budget_stopped
            && !self.worker_stopped()?
        {
            if queue.pending.is_empty()
                && self
                    .fill_primary_slot(queue, root_request, deadline)
                    .await?
            {
                continue;
            }
            let index = if let Some(index) = queue.pending.pop_front() {
                index
            } else {
                let next = self
                    .spawn_next_child(queue, root_request, segments, deadline)
                    .await;
                let Some(child) = (match next {
                    Ok(child) => child,
                    Err(EngineError::GoalBudgetExhausted) => {
                        queue.budget_stopped = true;
                        break;
                    }
                    Err(error) => return Err(error),
                }) else {
                    if queue.unspawned.is_empty() {
                        break;
                    } else {
                        continue;
                    }
                };
                let index = children.len();
                children.push(child);
                index
            };
            let child = &mut children[index];
            let researcher = child.tools.is_some();
            let mut request = self.child_request(child, root_request)?;
            let cancellation = self.controls.cancellation.clone();
            let provider = self.routing.bind(
                crate::provider::ProviderRole::Child,
                child.agent_run_id,
                &mut request.model,
                &mut request.max_output_tokens,
            )?;
            if child.editor {
                self.admit_worker_action(0).await?;
            }
            self.reserve_provider_call(
                child.agent_run_id,
                Action::Child {
                    in_flight: self.ledger().in_flight_children(),
                    research: researcher,
                },
                0,
            )
            .await?;
            let preview = (self
                .routing
                .chain(crate::provider::ProviderRole::Child, child.agent_run_id)?
                .len()
                > 1)
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
            queue.active.insert(
                handle.id(),
                ActiveChild {
                    index: Some(index),
                    request,
                },
            );
        }
        Ok(())
    }

    fn child_request(
        &self,
        child: &mut ChildWork,
        root_request: &ProviderRequest,
    ) -> Result<ProviderRequest, EngineError> {
        let guidance = child.definition.as_ref().map_or_else(
            || root_request.input.workspace_guidance.clone(),
            |definition| {
                Some(format!(
                    "{}\n\nAgent definition (untrusted assignment guidance):\n{}",
                    root_request
                        .input
                        .workspace_guidance
                        .as_deref()
                        .unwrap_or_default(),
                    definition.body
                ))
            },
        );
        let guidance = if child.editor {
            Some(format!(
                "{}\n\nUntrusted worker assignment: {}",
                guidance.as_deref().unwrap_or_default(),
                serde_json::to_string(&self.proposed_editor(child.agent_run_id)?)
                    .map_err(|_| EngineError::InvalidRequest)?
            ))
        } else {
            guidance
        };
        child.turns_remaining = child.turns_remaining.saturating_sub(1);
        if child.turns_remaining == 0 {
            child.tool_calls_remaining = 0;
        }
        if let Some(tools) = child.tools.as_mut() {
            let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
            let room = runtime
                .result_capacity(
                    tools.rendered_len()
                        + guidance.as_ref().map_or(0, String::len)
                        + CHILD_INTENT_RESERVE_BYTES,
                    0,
                )
                .is_some_and(|capacity| capacity >= MIN_CHILD_RESULT_BYTES);
            if !room {
                child.tool_calls_remaining = 0;
            }
            // Keep the request head frozen; later calls carry their remaining budget in the newest step.
            match tools.steps.last_mut() {
                Some(step) => step.calls_remaining = Some(child.tool_calls_remaining),
                None => {
                    if child.editor {
                        let mut catalog: serde_json::Value =
                            serde_json::from_str(&tools.catalog)
                                .map_err(|_| EngineError::InvalidRequest)?;
                        catalog["tool_calls_remaining"] =
                            serde_json::json!(child.tool_calls_remaining);
                        tools.catalog = catalog.to_string();
                    } else {
                        tools.catalog = runtime
                            .research_context(child.tool_calls_remaining)?
                            .catalog;
                    }
                    // Only the first call rebuilds the catalog; later calls keep this restriction.
                    if !child.editor
                        && let Some(definition) = &child.definition
                    {
                        restrict_catalog(tools, definition)?;
                    }
                }
            }
        }
        let researcher = child.tools.is_some();
        let root = &root_request.input;
        let request = ProviderRequest {
            session_id: self.session_id,
            run_id: self.run_id,
            agent_run_id: child.agent_run_id,
            phase: AgentPhase::ChildWork,
            collaboration: CollaborationPolicy::Single,
            model: String::new(),
            max_output_tokens: CHILD_OUTPUT_TOKEN_CAP,
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
                stop_hook_feedback: child.stop_hook_feedback.clone(),
                workspace_guidance: guidance,
                includes: root.includes.clone(),
                tools: child.tools.clone(),
                ..RunInput::new(
                    if child.editor {
                        PromptRole::Editor
                    } else if researcher {
                        PromptRole::Researcher
                    } else {
                        PromptRole::ReadOnly
                    },
                    child.objective.clone(),
                )
            },
        };
        Ok(request)
    }

    async fn fill_primary_slot(
        &self,
        queue: &mut ChildQueue,
        root_request: &mut ProviderRequest,
        deadline: Instant,
    ) -> Result<bool, EngineError> {
        if queue
            .deferred_at
            .is_some_and(|at| self.settled_agents().is_ok_and(|n| n > at))
        {
            queue.deferred_at = None;
        }
        if !queue.primary_pending
            && queue.deferred_at.is_none()
            && !queue.budget_stopped
            && self.controls.tools.is_some()
            && self.coordination_ready()?
        {
            self.refresh_team_context(root_request)?;
            if let (Some(note), Some(tools)) =
                (queue.refusal.take(), root_request.input.tools.as_mut())
            {
                let mut catalog: serde_json::Value = serde_json::from_str(&tools.catalog)
                    .map_err(|_| EngineError::InvalidRequest)?;
                catalog["last_answer_refused"] = serde_json::json!(note);
                tools.catalog = catalog.to_string();
            }
            root_request.phase = AgentPhase::RootSynthesis;
            let mut request = root_request.clone();
            self.routing.bind(
                crate::provider::ProviderRole::Primary,
                self.primary_id,
                &mut request.model,
                &mut request.max_output_tokens,
            )?;
            self.admit_worker_action(0).await?;
            self.reserve_provider_call(self.primary_id, self.primary_call(), 0)
                .await?;
            self.primary_context(root_request).await?;
            request.input = root_request.input.clone();
            let provider = self
                .routing
                .provider(crate::provider::ProviderRole::Primary, self.primary_id)?;
            let attempt = request.clone();
            let cancellation = self.controls.cancellation.clone();
            let handle = queue.calls.spawn(async move {
                crate::engine::routing::invoke_attempt(
                    provider.as_ref(),
                    attempt,
                    deadline,
                    cancellation,
                    Some(crate::provider::ResponsePreview::new().0),
                )
                .await
            });
            queue.active.insert(
                handle.id(),
                ActiveChild {
                    index: None,
                    request,
                },
            );
            queue.primary_pending = true;
            return Ok(true);
        }
        Ok(false)
    }

    fn valid_child_tools(&self, child: &ChildWork, requests: &[ToolRequest]) -> bool {
        matches!(requests, [request]
        if request.plan_contract.is_none()
            && child.tools.is_some()
            && child.tool_calls_remaining > 0 && child.turns_remaining > 0
            && request.call.valid()
            && (if child.editor {
                self.proposed_editor(child.agent_run_id)
                    .and_then(|a| self.editor_grant(&a))
                    .is_ok_and(|g| g.allows(&request.call))
            } else {
                request.call.is_child_research()
                    && child.definition.as_ref().is_none_or(|definition| definition.allows(&request.call))
            }))
    }

    async fn finish_child(
        &self,
        agent_run_id: AgentRunId,
        disposition: AgentDisposition,
        finish: Option<&Finish>,
        call_reserved: bool,
    ) -> Result<(), EngineError> {
        self.append(Event::AgentFinished {
            worker: None,
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

pub(super) enum ChildStop {
    Proceed,
    Reprompt,
    End(AgentDisposition),
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

fn delegation_active_bound(research: bool, call_deadline: Duration, waves: usize) -> Duration {
    if research {
        MAX_DELEGATION_DEADLINE
    } else {
        call_deadline
            .saturating_mul(u32::try_from(waves).unwrap_or(u32::MAX))
            .min(MAX_DELEGATION_DEADLINE)
    }
}

fn restrict_catalog(
    tools: &mut ToolContext,
    definition: &crate::tools::definitions::AgentDefinition,
) -> Result<(), EngineError> {
    let mut catalog: serde_json::Value =
        serde_json::from_str(&tools.catalog).map_err(|_| EngineError::InvalidRequest)?;
    let operations: Vec<_> = [
        ToolCall::List { path: ".".into() },
        ToolCall::Read {
            path: "file".into(),
            offset: 0,
            limit: 1,
        },
        ToolCall::Search {
            path: ".".into(),
            query: "x".into(),
        },
        ToolCall::Skill {
            name: "skill".into(),
            resource: None,
        },
        ToolCall::McpList {
            server: "server".into(),
        },
    ]
    .into_iter()
    .filter(|call| definition.allows(call))
    .map(|call| call.label())
    .collect();
    catalog["operations"] = serde_json::json!(operations);
    tools.catalog = serde_json::to_string(&catalog).map_err(|_| EngineError::InvalidRequest)?;
    Ok(())
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

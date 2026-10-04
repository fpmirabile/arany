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
        !matches!(self.controls.policy, CollaborationPolicy::Single)
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
        deadline: Instant,
        primary: AgentTrace,
    ) -> Result<(), EngineError> {
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
            for mut child in children {
                self.finish_child(child.agent_run_id, AgentDisposition::Cancelled, None)
                    .await?;
                child
                    .trace
                    .take()
                    .expect("child trace exists")
                    .finish(TraceOutcome::Cancelled);
            }
            return self.cancel_primary(primary).await;
        }

        let mut results = vec![None; children.len()];
        let parallel = usize::from(self.controls.concurrency)
            .min(usize::from(self.controls.policy.max_children()));
        let mut queue = ChildQueue::new(children.len(), parallel);
        self.fill_slots(&mut queue, &children, root_request, deadline);
        let mut abort_reason = None;
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
            let valid_response = valid_child_response(&result);
            let (disposition, finish) = match &result {
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Finish(finish),
                    ..
                }) if valid_response => (AgentDisposition::Finished, Some(finish)),
                CallResult::Cancelled => (AgentDisposition::Cancelled, None),
                _ => (AgentDisposition::Failed, None),
            };
            let Some(index) = queue.close_provider_span(task_id, &result, valid_response) else {
                queue.abort_and_drain().await?;
                return Err(EngineError::CoordinatorFailed);
            };
            if let Err(error) = self
                .append(Event::ProviderCallRecorded {
                    run_id: self.run_id,
                    agent_run_id: children[index].agent_run_id,
                    record: call_record(AgentPhase::ChildWork, &result, valid_response),
                })
                .await
            {
                queue.abort_and_drain().await?;
                return Err(error);
            }
            if let Err(error) = self
                .finish_child(children[index].agent_run_id, disposition, finish)
                .await
            {
                queue.abort_and_drain().await?;
                return Err(error);
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
            if abort_reason.is_none() {
                self.fill_slots(&mut queue, &children, root_request, deadline);
            }
        }

        if abort_reason.is_none() && self.controls.cancellation.is_cancelled() {
            abort_reason = Some(AbortReason::Cancelled);
        }
        if let Some(reason) = abort_reason {
            for index in queue.pending {
                self.finish_child(
                    children[index].agent_run_id,
                    AgentDisposition::Cancelled,
                    None,
                )
                .await?;
                children[index]
                    .trace
                    .take()
                    .expect("pending child trace exists")
                    .finish(TraceOutcome::Cancelled);
            }
            return match reason {
                AbortReason::Failed => self.fail_primary(primary).await,
                AbortReason::Cancelled => self.cancel_primary(primary).await,
            };
        }
        root_request.phase = AgentPhase::RootSynthesis;
        root_request.child_results = results
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or(EngineError::CoordinatorFailed)?;
        for _ in 0..=crate::tools::MAX_TOOL_CALLS {
            let synthesis_trace =
                primary.begin_provider(AgentPhase::RootSynthesis, &root_request.model);
            let synthesis = invoke_bounded(
                self.provider.as_ref(),
                root_request.clone(),
                deadline,
                self.controls.cancellation.clone(),
            )
            .await;
            let (input_tokens, output_tokens) = call_trace_usage(&synthesis);
            let valid_response = matches!(
                &synthesis,
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Finish(finish),
                    ..
                }) if valid_finish(finish, MAX_PRIMARY_RESULT_BYTES)
            ) || matches!(&synthesis, CallResult::Response(ProviderResponse { outcome: ProviderOutcome::Tool(call), .. }) if self.valid_tool(root_request, call));
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
            match synthesis {
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Finish(finish),
                    ..
                }) if valid_response => return self.finish_primary(finish, primary).await,
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Tool(call),
                    ..
                }) if valid_response => {
                    if !self.execute_tool(root_request, call, deadline).await? {
                        return self.fail_primary(primary).await;
                    }
                }
                CallResult::Cancelled => return self.cancel_primary(primary).await,
                _ => return self.fail_primary(primary).await,
            }
        }
        self.fail_primary(primary).await
    }

    fn fill_slots(
        &self,
        queue: &mut ChildQueue,
        children: &[ChildWork],
        root_request: &ProviderRequest,
        deadline: Instant,
    ) {
        while queue.active.len() < queue.parallel && !self.controls.cancellation.is_cancelled() {
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
                child_results: Vec::new(),
                max_output_tokens: OUTPUT_TOKEN_CAP,
                tools: None,
            };
            let provider_trace = children[index]
                .trace
                .as_ref()
                .expect("child trace exists")
                .begin_provider(AgentPhase::ChildWork, &request.model);
            let handle = queue.calls.spawn(async move {
                invoke_bounded(provider.as_ref(), request, deadline, cancellation).await
            });
            queue.active.insert(
                handle.id(),
                ActiveChild {
                    index,
                    provider_trace,
                },
            );
        }
    }

    async fn finish_child(
        &self,
        agent_run_id: AgentRunId,
        disposition: AgentDisposition,
        finish: Option<&Finish>,
    ) -> Result<(), EngineError> {
        self.append(Event::AgentFinished {
            run_id: self.run_id,
            agent_run_id,
            disposition,
            summary: finish.map(|value| value.summary.clone()),
            result: finish.map(|value| value.result.clone()),
        })
        .await?;
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

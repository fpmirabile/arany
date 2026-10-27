use super::super::children::ChildWork;
use super::super::stop::{SizingRefusal, StopCause};
use super::dispatch::Verdict;
use super::dispatch::{NotDispatched, not_dispatched};
use super::permission::{Permission, effective_permission_mode};
use super::{
    ActiveSegment, Answer, BoxedStep, Grant, IntentResult, RunLoop, STORE_ACTION_OVERHEAD,
    TOOL_CLEANUP_OVERHEAD,
};
use crate::engine::ledger::{
    Action, ActiveDeadlineCause, Admission, NARROWING_EVENTS, duration_ms,
};
use crate::engine::{EngineError, RunCancellation};
use crate::session::Event;
use crate::store::AdmissionKind;
use crate::tools::permissions::{Decision, PermissionSource, PermissionVerdict};
use crate::tools::types::{CommandChangeset, MAX_TOOL_RESULT_BYTES};
use crate::tools::{
    ActiveActionRequest, ActiveToolWindow, EffectIntent, ToolCall, ToolError, ToolObservation,
    ToolResourceClass, ToolRuntime, ToolStep,
};
use std::time::Duration;
use tokio::time::Instant;

pub(super) const MAX_INTENT_RECORDS_PER_TOOL: usize = 2;
const COMMAND_CACHE_PHASES: u64 = 3;

#[must_use]
pub(super) enum AdmissionRefusal {
    RemainingCalls,
    StoreCapacity,
}

impl From<AdmissionRefusal> for StopCause {
    fn from(refusal: AdmissionRefusal) -> Self {
        match refusal {
            AdmissionRefusal::RemainingCalls => Self::Budget,
            AdmissionRefusal::StoreCapacity => Self::Capacity,
        }
    }
}

pub(super) fn sizing_deadline_refusal(cause: Option<ActiveDeadlineCause>) -> SizingRefusal {
    match cause {
        Some(ActiveDeadlineCause::Goal) => SizingRefusal::ActiveTime,
        Some(ActiveDeadlineCause::Delegation) | None => SizingRefusal::DelegationDeadline,
    }
}

pub(super) struct PendingToolRefusal {
    agent_run_id: crate::session::AgentRunId,
    intent_id: uuid::Uuid,
    pub(super) record: Box<crate::session::ToolPreflightRefusal>,
}

impl PendingToolRefusal {
    pub(super) fn new(
        intent: &EffectIntent,
        refusal: crate::session::ToolRefusal,
        verdict: Option<PermissionVerdict>,
    ) -> Self {
        Self {
            agent_run_id: intent.agent_run_id,
            intent_id: intent.id,
            record: Box::new(crate::session::ToolPreflightRefusal {
                intent_digest: intent.digest(),
                refusal,
                verdict,
            }),
        }
    }
}

impl RunLoop<'_> {
    pub(in crate::engine::run_loop) async fn primary_intent(
        &self,
        runtime: &ToolRuntime,
        call: ToolCall,
    ) -> Result<Option<EffectIntent>, EngineError> {
        self.workspace_intent(runtime, self.primary_id, call).await
    }

    async fn workspace_intent(
        &self,
        runtime: &ToolRuntime,
        actor: crate::session::AgentRunId,
        call: ToolCall,
    ) -> Result<Option<EffectIntent>, EngineError> {
        let command = matches!(call, ToolCall::Command { .. });
        let timeout_limit = self.ledger().command_timeout_limit(&call)?;
        let mut intent = if call.is_unguarded() {
            let runtime = runtime.clone();
            let run = self.run_id;
            tokio::task::spawn_blocking(move || runtime.intent(run, actor, call))
                .await
                .map_err(|_| ToolError::Operation)?
        } else {
            runtime.intent(self.run_id, actor, call)
        };
        let (resource_class, requested_runtime_ms, resource_settings) =
            self.ledger().requested_command_resources()?;
        if command {
            command_limits(
                runtime,
                &mut intent,
                resource_class,
                resource_settings,
                timeout_limit.is_some(),
            )?;
        }
        let fixed_overhead_ms = duration_ms(
            runtime
                .reservation_overhead(&intent.call)
                .saturating_add(TOOL_CLEANUP_OVERHEAD)
                .saturating_add(STORE_ACTION_OVERHEAD),
        );
        let requested_runtime_ms = if command {
            requested_runtime_ms
        } else if intent.call.is_unguarded() {
            requested_runtime_ms.min(resource_settings.max_command_timeout_ms)
        } else {
            intent.limits.runtime_ms
        };
        let transfer_multiplier = if command && intent.limits.cache_bytes > 0 {
            COMMAND_CACHE_PHASES
        } else {
            1
        };
        let runtime_ms = if timeout_limit.is_some() {
            requested_runtime_ms
        } else {
            self.ledger()
                .active_remaining_ms()
                .map_or(requested_runtime_ms, |remaining| {
                    let usable = remaining.saturating_sub(fixed_overhead_ms);
                    let per_action = usable / transfer_multiplier;
                    requested_runtime_ms.min(u32::try_from(per_action).unwrap_or(u32::MAX))
                })
        };
        if runtime_ms < 1_000 {
            return Ok(None);
        }
        intent.limits.runtime_ms = runtime_ms;
        intent.limits.cache_transfer_ms = if command && intent.limits.cache_bytes > 0 {
            runtime_ms
        } else {
            0
        };
        Ok(Some(intent))
    }

    pub(in crate::engine::run_loop) fn researcher_intent(
        &self,
        runtime: &ToolRuntime,
        child: &ChildWork,
        call: ToolCall,
        deadline: Instant,
    ) -> Option<EffectIntent> {
        let intent = runtime.intent(self.run_id, child.agent_run_id, call);
        self.limit_child_intent(runtime, intent, deadline)
    }

    fn limit_child_intent(
        &self,
        runtime: &ToolRuntime,
        mut intent: EffectIntent,
        deadline: Instant,
    ) -> Option<EffectIntent> {
        let overhead = runtime.reservation_overhead(&intent.call);
        let available = deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(overhead + TOOL_CLEANUP_OVERHEAD + STORE_ACTION_OVERHEAD);
        let available = available.saturating_sub(self.hook_time(runtime, &intent.call, available));
        // Half for the post-approval Guard capacity wait, half for the run itself.
        let transfers =
            if matches!(intent.call, ToolCall::Command { .. }) && intent.limits.cache_bytes > 0 {
                COMMAND_CACHE_PHASES
            } else {
                1
            };
        let runtime_ms = intent
            .limits
            .runtime_ms
            .min(u32::try_from(duration_ms(available) / (2 * transfers)).unwrap_or(u32::MAX));
        if runtime_ms < 1_000 {
            return None;
        }
        intent.limits.runtime_ms = runtime_ms;
        if intent.limits.cache_transfer_ms > 0 {
            intent.limits.cache_transfer_ms = runtime_ms;
        }
        Some(intent)
    }

    pub(super) async fn admit_tool(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        intent: &EffectIntent,
        permission: Permission,
        hook_events: usize,
        narrowable: bool,
    ) -> Result<Result<(), AdmissionRefusal>, EngineError> {
        let worker = matches!(grant, Grant::Researcher { child, .. } if child.kind.is_editor());
        let (tail, observations) = match grant {
            Grant::Primary { review_allowed, .. } => {
                // The primary keeps one Provider call for its next turn and one for a review.
                let review =
                    self.review_will_run(runtime, &intent.call, *review_allowed, permission)?;
                if self
                    .ledger()
                    .remaining_calls()
                    .is_some_and(|remaining| remaining < 1 + u32::from(review))
                {
                    return Ok(Err(AdmissionRefusal::RemainingCalls));
                }
                // A hook may narrow this Tool: the replacement is a second intent with its own hook set.
                let narrowing = if narrowable && hook_events > 0 {
                    hook_events
                } else {
                    0
                };
                (
                    Action::Tool
                        .remaining(1, &self.routing.providers)
                        .saturating_add(self.ledger().verdicts(Action::Tool))
                        .saturating_add(self.ledger().drain_headroom())
                        .saturating_add(2 * (hook_events + narrowing))
                        .saturating_add(if narrowing > 0 { NARROWING_EVENTS } else { 0 }),
                    Action::Tool
                        .tool_observations()
                        .saturating_add(hook_events + narrowing),
                )
            }
            Grant::Researcher { .. } => {
                let step = Action::Child {
                    in_flight: self.ledger().in_flight_children(),
                    research: true,
                };
                (
                    step.max_events_for_admitted(&self.routing.providers)
                        .saturating_add(2 * hook_events)
                        .saturating_add(self.ledger().verdicts(step)),
                    step.tool_observations().saturating_add(hook_events),
                )
            }
        };
        // Proposal and start each carry the complete intent and can exceed the ordinary Event bound.
        let observations = observations.saturating_add(MAX_INTENT_RECORDS_PER_TOOL);
        let admitted = self
            .journal
            .admit_as(
                self.session_id,
                tail,
                observations,
                AdmissionKind::Operation,
            )
            .await;
        match admitted {
            Ok(()) => Ok(Ok(())),
            // A worker effect's capacity stops the wave; any other stays a Store failure.
            Err(crate::store::StoreError::StorageFull | crate::store::StoreError::ReplayLimit)
                if worker =>
            {
                Ok(Err(AdmissionRefusal::StoreCapacity))
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) async fn prepare_step_intent(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        step: &ToolStep,
        call: ToolCall,
        post_call: &ToolCall,
    ) -> Result<Result<EffectIntent, StopCause>, EngineError> {
        let intent = match self
            .size_step_intent(grant, runtime, step, call, post_call)
            .await?
        {
            Ok(intent) => intent,
            Err(cause) => return Ok(Err(cause)),
        };
        Ok(self
            .validate_step_intent(runtime, &intent)
            .await?
            .map(|()| intent))
    }

    pub(super) async fn size_step_intent(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        step: &ToolStep,
        call: ToolCall,
        post_call: &ToolCall,
    ) -> Result<Result<EffectIntent, StopCause>, EngineError> {
        let sized = self
            .sized_intent(grant, runtime, step, call.clone(), post_call)
            .await?;
        match sized {
            Ok(intent) => Ok(Ok(intent)),
            Err(refusal @ SizingRefusal::DelegationDeadline) => Ok(Err(
                match self
                    .record_delegation_deadline(grant, runtime, &[call])
                    .await?
                {
                    Ok(()) => refusal.into(),
                    Err(admission) => admission.into(),
                },
            )),
            Err(refusal @ SizingRefusal::ActiveTime) => {
                self.active_time_floor
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(Err(refusal.into()))
            }
            Err(refusal @ SizingRefusal::Cancelled) => Ok(Err(refusal.into())),
            Err(refusal @ SizingRefusal::ContextFull) => Ok(Err(refusal.into())),
        }
    }

    pub(super) async fn validate_step_intent(
        &self,
        runtime: &ToolRuntime,
        intent: &EffectIntent,
    ) -> Result<Result<(), StopCause>, EngineError> {
        match self.preflight_step_intent(runtime, intent)? {
            Ok(()) => Ok(Ok(())),
            Err(record) => {
                let cause = record.record.refusal;
                self.record_tool_refusal(record).await?;
                Ok(Err(StopCause::ToolRefused(cause)))
            }
        }
    }

    pub(super) fn preflight_step_intent(
        &self,
        runtime: &ToolRuntime,
        intent: &EffectIntent,
    ) -> Result<Result<(), PendingToolRefusal>, EngineError> {
        let mut event = Event::ToolProposed {
            run_id: self.run_id,
            agent_run_id: intent.agent_run_id,
            intent: intent.clone().into(),
            active_ceiling_ms: self.tool_ceiling(runtime, intent)?,
        };
        crate::fault::tool_preflight(self.session_id, &mut event);
        self.ledger().view(|view| {
            crate::session::validate_tool_started(view, &event)
                .map_err(|refusal| PendingToolRefusal::new(intent, refusal, None))
        })
    }

    pub(super) async fn record_tool_refusal(
        &self,
        pending: PendingToolRefusal,
    ) -> Result<(), EngineError> {
        self.append(Event::ToolPreflightRefused {
            run_id: self.run_id,
            agent_run_id: pending.agent_run_id,
            record: *pending.record,
        })
        .await?;
        self.commit_approval_withdrawal(pending.intent_id)
    }

    pub(super) async fn sized_intent(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        step: &ToolStep,
        call: ToolCall,
        post_call: &ToolCall,
    ) -> Result<Result<EffectIntent, SizingRefusal>, EngineError> {
        let intent = match grant {
            Grant::Primary { .. } => self.primary_intent(runtime, call).await?,
            Grant::Researcher {
                child,
                deadline,
                active_deadline,
            } => {
                let deadline_refusal = sizing_deadline_refusal(
                    active_deadline
                        .as_ref()
                        .filter(|deadline| deadline.reached())
                        .map(|deadline| deadline.cause),
                );
                let deadline = grant.deadline().unwrap_or(*deadline);
                let intent = if child.kind.is_editor() && matches!(call, ToolCall::Command { .. }) {
                    self.workspace_intent(runtime, child.agent_run_id, call)
                        .await?
                        .and_then(|intent| self.limit_child_intent(runtime, intent, deadline))
                } else {
                    self.researcher_intent(runtime, child, call, deadline)
                };
                let Some(intent) = intent else {
                    return Ok(Err(deadline_refusal));
                };
                Some(intent)
            }
        };
        let Some(mut intent) = intent else {
            return Ok(Err(SizingRefusal::ActiveTime));
        };
        let integration = (matches!(intent.call, ToolCall::Command { .. })
            && matches!(intent.target, crate::worker::EffectTarget::Workspace))
        .then(|| ToolCall::ApplyChangeset {
            source: crate::session::worker::ChangesetSource::CommandReceipt {
                source_intent_digest: intent.digest(),
                changeset: CommandChangeset { files: Vec::new() },
            },
        });
        let reserved: Vec<_> = std::iter::once(&intent.call).chain(&integration).collect();
        let overhead = grant.context()?.reserved_len(step, &reserved);
        let reserve = ToolRuntime::step_context_reserve(
            integration.is_some(),
            self.feedback_reserve(runtime, post_call),
        );
        let Some(capacity) = runtime.result_capacity(overhead, reserve) else {
            return Ok(Err(SizingRefusal::ContextFull));
        };
        intent.limits.result_bytes = if matches!(intent.call, ToolCall::ApplyChangeset { .. }) {
            128
        } else if matches!(
            intent.call,
            ToolCall::Read { .. }
                | ToolCall::List { .. }
                | ToolCall::Search { .. }
                | ToolCall::Glob { .. }
                | ToolCall::Skill { .. }
        ) || intent.call.is_git() && !intent.call.mutates()
        {
            capacity.min(MAX_TOOL_RESULT_BYTES) as u32
        } else if capacity >= MAX_TOOL_RESULT_BYTES {
            MAX_TOOL_RESULT_BYTES as u32
        } else {
            return Ok(Err(SizingRefusal::ContextFull));
        };
        Ok(Ok(intent))
    }

    pub(super) async fn propose_tool(&self, intent: &EffectIntent) -> Result<(), EngineError> {
        let proposed = self.ledger().view(|view| {
            view.runs.last().is_some_and(|run| {
                run.tools
                    .iter()
                    .chain(run.agents.iter().flat_map(|agent| &agent.tools))
                    .any(|tool| tool.intent.id == intent.id)
            })
        })?;
        if !proposed {
            let runtime = self.controls.tools.ok_or(EngineError::CoordinatorFailed)?;
            self.append(Event::ToolProposed {
                run_id: self.run_id,
                agent_run_id: intent.agent_run_id,
                intent: intent.clone().into(),
                active_ceiling_ms: self.tool_ceiling(runtime, intent)?,
            })
            .await?;
        }
        Ok(())
    }

    pub(super) fn tool_ceiling(
        &self,
        runtime: &ToolRuntime,
        intent: &EffectIntent,
    ) -> Result<u64, EngineError> {
        let post_call = match &intent.call {
            ToolCall::ApplyChangeset {
                source:
                    crate::session::worker::ChangesetSource::CommandReceipt {
                        source_intent_digest,
                        ..
                    },
            } => self.ledger().view(|view| {
                view.runs
                    .last()
                    .and_then(|run| {
                        run.tools
                            .iter()
                            .chain(run.agents.iter().flat_map(|agent| &agent.tools))
                            .find(|tool| {
                                tool.intent.agent_run_id == intent.agent_run_id
                                    && tool.intent.digest() == *source_intent_digest
                            })
                    })
                    .map(|tool| tool.intent.call.clone())
            })?,
            _ => None,
        };
        let (_, pre_ms, post_ms) = self.hook_admission(
            runtime,
            &intent.call,
            post_call.as_ref().unwrap_or(&intent.call),
        );
        let editor = self.ledger().view(|view| {
            view.runs.last().is_some_and(|run| {
                run.agents
                    .iter()
                    .any(|agent| agent.id == intent.agent_run_id && agent.assignment.is_some())
            })
        })?;
        let action_ms = if intent.agent_run_id == self.primary_id || editor {
            tool_action_ms(runtime, intent)
                .saturating_add(pre_ms)
                .saturating_add(post_ms)
        } else {
            0
        };
        self.ledger().active_ceiling(action_ms)
    }

    pub(super) async fn record_delegation_deadline(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        calls: &[ToolCall],
    ) -> Result<Result<(), AdmissionRefusal>, EngineError> {
        let Grant::Researcher { child, .. } = grant else {
            return Err(EngineError::CoordinatorFailed);
        };
        let mut intents = Vec::with_capacity(calls.len());
        for call in calls {
            let intent = runtime.intent(self.run_id, child.agent_run_id, call.clone());
            let permission = self
                .permission(grant, runtime, &intent.call)
                .narrow_terminal(self.terminal_mode(), &intent.call);
            if let Err(refusal) = self
                .admit_tool(grant, runtime, &intent, permission, 0, false)
                .await?
            {
                return Ok(Err(refusal));
            }
            intents.push(intent);
        }
        self.admit_step_journal(runtime, &intents).await?;
        if intents.len() > 1 {
            for intent in &intents {
                self.propose_tool(intent).await?;
            }
        }
        for intent in intents {
            self.finish_delegation_deadline(grant, runtime, intent)
                .await?;
        }
        Ok(Ok(()))
    }

    async fn finish_delegation_deadline(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        intent: EffectIntent,
    ) -> Result<(), EngineError> {
        let agent_run_id = intent.agent_run_id;
        let permission = self
            .permission(grant, runtime, &intent.call)
            .narrow_terminal(self.terminal_mode(), &intent.call);
        let verdict =
            effective_permission_mode(runtime.permissions(), grant).map(|mode| PermissionVerdict {
                host: None,
                intent_digest: intent.digest(),
                mode: Some(mode),
                decision: permission.decision,
                basis: permission.basis,
                source: if permission.decision == Decision::Ask {
                    PermissionSource::UnavailableApproval
                } else {
                    permission.basis
                },
                approved: false,
                hook_restriction: None,
                hooks: 0,
                narrowed: false,
            });
        self.append(Event::ToolStarted {
            run_id: self.run_id,
            agent_run_id,
            intent: intent.clone().into(),
            active_ceiling_ms: self.ledger().active_ceiling(0)?,
            verdict,
        })
        .await?;
        self.append(Event::ToolFinished {
            run_id: self.run_id,
            agent_run_id,
            observation: not_dispatched(intent, &NotDispatched::DelegationDeadline),
            checkpoint: None,
        })
        .await?;
        Ok(())
    }
}

impl RunLoop<'_> {
    pub(in crate::engine::run_loop) fn run_intent<'a>(
        &'a self,
        grant: &'a Grant<'_>,
        runtime: &'a ToolRuntime,
        intent: EffectIntent,
        post_call: &'a ToolCall,
        narrowable: bool,
    ) -> BoxedStep<'a, IntentResult> {
        Box::pin(self.run_intent_inner(grant, runtime, intent, post_call, narrowable))
    }

    async fn run_intent_inner(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        intent: EffectIntent,
        post_call: &ToolCall,
        narrowable: bool,
    ) -> Result<IntentResult, EngineError> {
        if let Err(refusal) = self
            .start_intent(grant, runtime, &intent, post_call, narrowable)
            .await?
        {
            return Ok(Err(refusal.into()));
        }
        self.dispatch_intent(grant, runtime, intent, narrowable)
            .await
    }

    pub(super) async fn start_intent(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        intent: &EffectIntent,
        post_call: &ToolCall,
        narrowable: bool,
    ) -> Result<Result<(), AdmissionRefusal>, EngineError> {
        let permission = self.permission(grant, runtime, &intent.call);
        let (hook_events, _, _) = self.hook_admission(runtime, &intent.call, post_call);
        if let Err(refusal) = self
            .admit_tool(grant, runtime, intent, permission, hook_events, narrowable)
            .await?
        {
            return Ok(Err(refusal));
        }
        Ok(Ok(()))
    }

    pub(super) async fn finish_answer(&self, answer: &Answer) -> Result<(), EngineError> {
        self.append(Event::ToolFinished {
            checkpoint: answer.checkpoint.clone(),
            run_id: self.run_id,
            agent_run_id: answer.observation.intent.agent_run_id,
            observation: answer.observation.clone(),
        })
        .await?;
        self.commit_approval_withdrawal(answer.observation.intent.id)
    }

    pub(super) async fn dispatch_intent(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        intent: EffectIntent,
        narrowable: bool,
    ) -> Result<IntentResult, EngineError> {
        let permission = self.permission(grant, runtime, &intent.call);
        let mut narrowed = None;
        let verdict = match self
            .before_dispatch(grant, runtime, intent, permission, narrowable, None)
            .await?
        {
            Ok(Admission::Reserved(verdict)) => verdict,
            Ok(Admission::Exhausted) => return Ok(Err(StopCause::ActiveTime)),
            Err(record) => {
                let cause = record.record.refusal;
                self.record_tool_refusal(record).await?;
                return Ok(Err(StopCause::ToolRefused(cause)));
            }
        };
        let mut answer = match verdict {
            Verdict::Deny(answer) => answer,
            Verdict::Narrow(answer, call) => {
                narrowed = Some(call);
                answer
            }
            Verdict::Dispatch {
                intent,
                queue_pause,
            } => match grant {
                Grant::Primary { .. } => {
                    self.dispatch_primary(runtime, intent, queue_pause).await?
                }
                Grant::Researcher { .. } => {
                    self.dispatch_child(grant, runtime, intent, queue_pause)
                        .await?
                }
            },
        };
        runtime
            .persist_spill(&mut answer.observation, self.controls.cancellation.clone())
            .await;
        self.finish_answer(&answer).await?;
        runtime.retain_spill(&answer.observation);
        if let Some(active_time) = answer.active_time {
            if let Some(segment) = active_time.segment {
                self.settle_active_segment(segment)?;
            }
            if active_time.exhausted || self.ledger().active_time_exhausted() {
                return Ok(Err(StopCause::ActiveTime));
            }
        }
        Ok(match answer.stop {
            Some(cause) => Err(cause),
            None => Ok((answer.observation, narrowed)),
        })
    }
}

pub(super) fn tool_action_bound(runtime: &ToolRuntime, intent: &EffectIntent) -> Duration {
    runtime
        .reservation_overhead(&intent.call)
        .saturating_add(
            Duration::from_millis(u64::from(intent.limits.cache_transfer_ms))
                .saturating_add(crate::tools::transfer_time(intent))
                .saturating_mul(2),
        )
        .saturating_add(Duration::from_millis(u64::from(intent.limits.runtime_ms)))
        .saturating_add(TOOL_CLEANUP_OVERHEAD)
}

pub(super) fn tool_action_ms(runtime: &ToolRuntime, intent: &EffectIntent) -> u64 {
    duration_ms(tool_action_bound(runtime, intent).saturating_add(STORE_ACTION_OVERHEAD))
}

pub(super) fn active_window(
    segment: &ActiveSegment,
    cache_transfer: Duration,
    runtime_ms: u32,
) -> Result<ActiveToolWindow, ToolError> {
    let completion_deadline = segment.deadline().checked_sub(STORE_ACTION_OVERHEAD);
    let payload_deadline = completion_deadline.and_then(|deadline| {
        deadline.checked_sub(TOOL_CLEANUP_OVERHEAD.saturating_add(cache_transfer))
    });
    let go_deadline = payload_deadline
        .and_then(|deadline| deadline.checked_sub(Duration::from_millis(u64::from(runtime_ms))));
    let preparation_deadline =
        go_deadline.and_then(|deadline| deadline.checked_sub(cache_transfer));
    preparation_deadline
        .zip(go_deadline)
        .zip(payload_deadline)
        .zip(completion_deadline)
        .filter(
            |(((preparation_deadline, go_deadline), payload_deadline), completion_deadline)| {
                let now = Instant::now();
                *preparation_deadline > now
                    && *go_deadline > now
                    && *payload_deadline > now
                    && *completion_deadline > now
            },
        )
        .map(
            |(((preparation_deadline, go_deadline), payload_deadline), completion_deadline)| {
                ActiveToolWindow {
                    preparation_deadline,
                    go_deadline,
                    payload_deadline,
                    completion_deadline,
                }
            },
        )
        .ok_or(ToolError::Limit)
}

pub(super) async fn settle_cancelled<T>(
    mut execution: std::pin::Pin<&mut impl std::future::Future<Output = T>>,
    receiver: &mut tokio::sync::mpsc::Receiver<ActiveActionRequest>,
) -> T {
    receiver.close();
    loop {
        tokio::select! {
            biased;
            output = &mut execution => return output,
            Some(_) = receiver.recv() => {}
        }
    }
}

pub(super) async fn execute_cancellable(
    runtime: &ToolRuntime,
    intent: EffectIntent,
    cancellation: &RunCancellation,
) -> ToolObservation {
    let worker_cancellation = RunCancellation::new();
    let mut run_cancellation = cancellation.clone();
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
}

fn command_limits(
    runtime: &ToolRuntime,
    intent: &mut EffectIntent,
    resource_class: ToolResourceClass,
    resource_settings: crate::tools::ToolResourceSettings,
    timeout_refused: bool,
) -> Result<(), EngineError> {
    intent.limits.resource_class = resource_class;
    intent.limits.project_bytes = crate::tools::types::DEFAULT_COMMAND_PROJECT_BYTES;
    let requested_cache_bytes = if resource_class == ToolResourceClass::Build {
        resource_settings.build_cache_bytes
    } else {
        intent.limits.cache_requested_bytes
    };
    intent.limits.cache_requested_bytes = requested_cache_bytes;
    if timeout_refused {
        intent.limits.cache_bytes = 0;
        intent.limits.cache_candidate_bytes = 0;
    } else {
        let (cache_view_bytes, cache_candidate_bytes) =
            runtime.effective_cache_bytes(requested_cache_bytes)?;
        intent.limits.cache_bytes = cache_view_bytes;
        intent.limits.cache_candidate_bytes = cache_candidate_bytes;
    }
    if resource_class == ToolResourceClass::Build {
        intent.limits.memory_bytes = resource_settings.build_memory_bytes;
        intent.limits.max_processes = resource_settings.build_processes;
        intent.limits.build_jobs = resource_settings
            .build_jobs
            .ok_or(EngineError::InvalidRequest)?;
        intent.limits.build_cpu_cores = resource_settings
            .build_cpu_cores
            .ok_or(EngineError::InvalidRequest)?;
        runtime.bind_build_jobs(intent)?;
        intent.limits.project_bytes = resource_settings.build_project_bytes;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_refuses_buffered_and_late_active_window_requests() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let permit = sender
            .clone()
            .reserve_owned()
            .await
            .expect("reserve a slot");
        let (reply, receipt) = tokio::sync::oneshot::channel();
        let execution = async move {
            let _ = receipt.await;
            "settled"
        };
        tokio::pin!(execution);
        let late = tokio::spawn(async move {
            tokio::task::yield_now().await;
            permit.send(ActiveActionRequest {
                admitted_at: Instant::now(),
                reply,
            });
        });
        let settled = tokio::time::timeout(
            Duration::from_secs(5),
            settle_cancelled(execution.as_mut(), &mut receiver),
        )
        .await
        .expect("the Guard must not wait for a reply nobody sends");
        assert_eq!(settled, "settled");
        late.await.expect("late sender");
        let (again, _) = tokio::sync::oneshot::channel();
        assert!(
            sender
                .send(ActiveActionRequest {
                    admitted_at: Instant::now(),
                    reply: again,
                })
                .await
                .is_err()
        );
    }
}

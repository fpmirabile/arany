use super::super::children::ChildWork;
use super::super::stop::{SizingRefusal, StopCause};
use super::dispatch::{NotDispatched, not_dispatched};
use super::permission::{Permission, effective_permission_mode};
use super::{Grant, RunLoop, STORE_ACTION_OVERHEAD, TOOL_CLEANUP_OVERHEAD};
use crate::engine::EngineError;
use crate::engine::ledger::{Action, ActiveDeadlineCause, duration_ms};
use crate::session::Event;
use crate::store::AdmissionKind;
use crate::tools::permissions::{Decision, PermissionSource, PermissionVerdict};
use crate::tools::types::{CommandChangeset, MAX_TOOL_RESULT_BYTES};
use crate::tools::{EffectIntent, ToolCall, ToolError, ToolResourceClass, ToolRuntime, ToolStep};
use tokio::time::Instant;

const EDITOR_RUNTIME_DIVISOR: u64 = 4;
// Cache restoration and publication need additional time within the delegation allowance.
const EDITOR_RUNTIME_WITH_CACHE_DIVISOR: u64 = 6;

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

impl RunLoop<'_> {
    pub(in crate::engine::run_loop) async fn primary_intent(
        &self,
        runtime: &ToolRuntime,
        call: ToolCall,
    ) -> Result<Option<EffectIntent>, EngineError> {
        let command = matches!(call, ToolCall::Command { .. });
        let timeout_limit = self.ledger().command_timeout_limit(&call)?;
        let mut intent = if call.is_unguarded() {
            let runtime = runtime.clone();
            let (run, actor) = (self.run_id, self.primary_id);
            tokio::task::spawn_blocking(move || runtime.intent(run, actor, call))
                .await
                .map_err(|_| ToolError::Operation)?
        } else {
            runtime.intent(self.run_id, self.primary_id, call)
        };
        let (resource_class, requested_runtime_ms, resource_settings) =
            self.ledger().requested_command_resources()?;
        if command {
            intent.limits.resource_class = resource_class;
            intent.limits.project_bytes = crate::tools::types::DEFAULT_COMMAND_PROJECT_BYTES;
            let requested_cache_bytes = if resource_class == ToolResourceClass::Build {
                resource_settings.build_cache_bytes
            } else {
                intent.limits.cache_requested_bytes
            };
            intent.limits.cache_requested_bytes = requested_cache_bytes;
            if timeout_limit.is_some() {
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
                intent.limits.project_bytes = resource_settings.build_project_bytes;
            }
        }
        let fixed_overhead_ms = duration_ms(
            runtime
                .intent_reservation_overhead(&intent)
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
        let transfer_multiplier = if command && intent.limits.cache_bytes > 0
            || matches!(
                intent.call,
                ToolCall::ApplyChangeset {
                    source: crate::session::worker::ChangesetSource::WorkerHandover { .. }
                }
            ) {
            3
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
        let mut intent = runtime.intent(self.run_id, child.agent_run_id, call);
        let overhead = if child.editor {
            runtime.worker_reservation_overhead(&intent.call)
        } else {
            runtime.reservation_overhead(&intent.call)
        };
        let available = deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(overhead + TOOL_CLEANUP_OVERHEAD + STORE_ACTION_OVERHEAD);
        let available = available.saturating_sub(self.hook_time(runtime, &intent.call, available));
        // Half for the post-approval Guard capacity wait, half for the run itself.
        let runtime_ms = intent
            .limits
            .runtime_ms
            .min(u32::try_from(duration_ms(available) / 2).unwrap_or(u32::MAX));
        if runtime_ms < 1_000 {
            return None;
        }
        intent.limits.runtime_ms = runtime_ms;
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
        let worker = matches!(
            intent.target,
            crate::session::worker::EffectTarget::WorkerView { .. }
        ) || matches!(
            intent.call,
            ToolCall::PrepareWorker { .. }
                | ToolCall::ApplyChangeset {
                    source: crate::session::worker::ChangesetSource::WorkerHandover { .. }
                }
        );
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
                        .saturating_add(if narrowing > 0 { 3 } else { 0 }),
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
        // This intent's ToolStarted can exceed the ordinary Event bound; an observation slot covers it.
        let observations = observations.saturating_add(1);
        let mut kind = AdmissionKind::Operation;
        let tail = if worker {
            let chunks = self.worker_call_chunks(intent.agent_run_id, &intent.call)?;
            kind = AdmissionKind::for_chunks(chunks);
            let action = Action::Worker { chunks };
            tail.max(
                action
                    .max_events_for_admitted(&self.routing.providers)
                    .saturating_add(self.ledger().verdicts(action))
                    .saturating_add(2 * hook_events),
            )
        } else {
            tail
        };
        let admitted = self
            .journal
            .admit_as(self.session_id, tail, observations, kind)
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
        let sized = self
            .sized_intent(grant, runtime, step, call.clone(), post_call)
            .await?;
        match sized {
            Ok(intent) => Ok(Ok(intent)),
            Err(refusal @ SizingRefusal::DelegationDeadline) => Ok(Err(
                match self
                    .record_delegation_deadline(grant, runtime, call)
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
            Err(refusal @ SizingRefusal::ContextFull) => Ok(Err(refusal.into())),
        }
    }

    async fn sized_intent(
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
                let Some(mut intent) = self.researcher_intent(runtime, child, call, deadline)
                else {
                    return Ok(Err(deadline_refusal));
                };
                if child.editor {
                    self.shape_worker_intent(&mut intent, child.agent_run_id)?;
                    self.size_editor_runtime(runtime, &mut intent, deadline);
                    if intent.limits.runtime_ms < 1_000 {
                        return Ok(Err(deadline_refusal));
                    }
                }
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

    fn size_editor_runtime(
        &self,
        runtime: &ToolRuntime,
        intent: &mut EffectIntent,
        deadline: Instant,
    ) {
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(
                runtime.intent_reservation_overhead(intent)
                    + TOOL_CLEANUP_OVERHEAD
                    + STORE_ACTION_OVERHEAD,
            );
        let remaining = remaining.saturating_sub(self.hook_time(runtime, &intent.call, remaining));
        let runtime_divisor = if intent.limits.cache_bytes > 0 {
            EDITOR_RUNTIME_WITH_CACHE_DIVISOR
        } else {
            EDITOR_RUNTIME_DIVISOR
        };
        intent.limits.runtime_ms = intent
            .limits
            .runtime_ms
            .min(u32::try_from(duration_ms(remaining) / runtime_divisor).unwrap_or(u32::MAX));
        intent.limits.cache_transfer_ms = if intent.limits.cache_bytes > 0 {
            intent.limits.runtime_ms
        } else {
            0
        };
    }

    async fn record_delegation_deadline(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        call: ToolCall,
    ) -> Result<Result<(), AdmissionRefusal>, EngineError> {
        let Grant::Researcher { child, .. } = grant else {
            return Err(EngineError::CoordinatorFailed);
        };
        let mut intent = runtime.intent(self.run_id, child.agent_run_id, call);
        if child.editor {
            self.shape_worker_intent(&mut intent, child.agent_run_id)?;
        }
        let permission = self
            .permission(grant, runtime, &intent.call)
            .narrow_terminal(self.terminal_mode(), &intent.call);
        if let Err(refusal) = self
            .admit_tool(grant, runtime, &intent, permission, 0, false)
            .await?
        {
            return Ok(Err(refusal));
        }
        self.append(Event::ToolStarted {
            run_id: self.run_id,
            agent_run_id: child.agent_run_id,
            intent: intent.clone(),
            active_ceiling_ms: self.ledger().active_ceiling(0)?,
        })
        .await?;
        if child.editor {
            self.worker
                .lock()
                .await
                .get_mut(&child.agent_run_id)
                .ok_or(EngineError::CoordinatorFailed)?
                .note_intent(&intent);
        }
        if let Some(mode) = effective_permission_mode(runtime.permissions(), grant) {
            self.append(Event::ToolPermissionDecided {
                run_id: self.run_id,
                agent_run_id: child.agent_run_id,
                verdict: PermissionVerdict {
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
                },
            })
            .await?;
        }
        self.append(Event::ToolFinished {
            run_id: self.run_id,
            agent_run_id: child.agent_run_id,
            observation: not_dispatched(intent, &NotDispatched::DelegationDeadline),
            checkpoint: None,
        })
        .await?;
        Ok(Ok(()))
    }
}

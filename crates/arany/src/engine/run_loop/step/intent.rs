use super::super::stop::{SizingRefusal, StopCause};
use super::dispatch::{NotDispatched, not_dispatched};
use super::permission::effective_permission_mode;
use super::{AdmissionRefusal, Grant, RunLoop, STORE_ACTION_OVERHEAD, TOOL_CLEANUP_OVERHEAD};
use crate::engine::EngineError;
use crate::engine::ledger::{ActiveDeadlineCause, duration_ms};
use crate::session::Event;
use crate::tools::permissions::{Decision, PermissionSource, PermissionVerdict};
use crate::tools::types::{CommandChangeset, MAX_TOOL_RESULT_BYTES};
use crate::tools::{EffectIntent, ToolCall, ToolRuntime, ToolStep};
use tokio::time::Instant;

const EDITOR_RUNTIME_DIVISOR: u64 = 4;
// Cache restoration and publication need additional time within the delegation allowance.
const EDITOR_RUNTIME_WITH_CACHE_DIVISOR: u64 = 6;

pub(super) fn sizing_deadline_refusal(cause: Option<ActiveDeadlineCause>) -> SizingRefusal {
    match cause {
        Some(ActiveDeadlineCause::Goal) => SizingRefusal::ActiveTime,
        Some(ActiveDeadlineCause::Delegation) | None => SizingRefusal::DelegationDeadline,
    }
}

impl RunLoop<'_> {
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

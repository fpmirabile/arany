use super::children::ChildWork;
use super::stop::{RunProgress, RunRefusal, SizingRefusal, StepEnd, StopCause};
use super::{ActivePause, ActiveSegment, RunLoop, TOOL_CLEANUP_OVERHEAD};
use crate::engine::hints::HintPublisher;
use crate::engine::ledger::{Action, ActiveDeadlineCause, Admission, duration_ms};
use crate::engine::{EngineError, RunCancellation, STORE_ACTION_OVERHEAD};
use crate::provider::{ProviderRequest, ProviderTurn};
use crate::session::Event;
use crate::store::AdmissionKind;
use crate::tools::permissions::{Decision, PermissionSource, PermissionVerdict};
use crate::tools::types::{CommandChangeset, CommandResult, MAX_TOOL_RESULT_BYTES};
use crate::tools::{
    ActiveActionRequest, ActiveToolWindow, ApprovalMode, ApprovalReply, ApprovalWithdrawal,
    CacheUpdate, EffectIntent, ToolCall, ToolContext, ToolDisposition, ToolError, ToolObservation,
    ToolRequest, ToolResourceClass, ToolRuntime, ToolStep,
};
use std::time::Duration;
use tokio::time::Instant;

const INVOKED_SKILL_ENVELOPE_BYTES: usize = 1024;
const EDITOR_RUNTIME_DIVISOR: u64 = 4;
// Cache restoration and publication need additional time within the delegation allowance.
const EDITOR_RUNTIME_WITH_CACHE_DIVISOR: u64 = 6;

mod ask;
mod checkpoint;
mod dispatch;
pub(super) mod hooks;
mod permission;
mod review;

use ask::{AskOptions, AskSite, Resolution, unguarded_gate};
use checkpoint::{CHECKPOINT_GO_MARGIN, CHECKPOINT_WAIT};
use dispatch::{
    AfterAsk, Dispatch, DispatchFacts, DispatchSite, GuardCompletion, NotDispatched, Unguarded,
    Verdict, decide_dispatch, decide_queue_pause, not_dispatched, undispatched,
};
use permission::{Permission, RuleSave, effective_permission_mode, remember_candidate};
use review::ToolReviewDecision;

use hooks::{HookFold, HookStop};

pub(super) enum Grant<'s> {
    Primary {
        request: &'s mut ProviderRequest,
        review_allowed: bool,
    },
    Researcher {
        child: &'s mut ChildWork,
        deadline: Instant,
        active_deadline: Option<crate::engine::ledger::ActiveDeadline>,
    },
}

impl Grant<'_> {
    pub(super) fn deadline(&self) -> Option<Instant> {
        match self {
            Self::Primary { .. } => None,
            Self::Researcher {
                deadline,
                active_deadline,
                ..
            } => Some(active_deadline.as_ref().map_or(
                *deadline,
                crate::engine::ledger::ActiveDeadline::wall_deadline,
            )),
        }
    }

    fn site(&self) -> DispatchSite {
        match self {
            Self::Primary { .. } => DispatchSite::Primary,
            Self::Researcher { .. } => DispatchSite::Researcher,
        }
    }

    fn context(&self) -> Result<&ToolContext, EngineError> {
        match self {
            Self::Primary { request, .. } => request
                .input
                .tools
                .as_ref()
                .ok_or(EngineError::InvalidRequest),
            Self::Researcher { child, .. } => {
                child.tools.as_ref().ok_or(EngineError::CoordinatorFailed)
            }
        }
    }

    fn context_mut(&mut self) -> Result<&mut ToolContext, EngineError> {
        match self {
            Self::Primary { request, .. } => request
                .input
                .tools
                .as_mut()
                .ok_or(EngineError::InvalidRequest),
            Self::Researcher { child, .. } => {
                child.tools.as_mut().ok_or(EngineError::CoordinatorFailed)
            }
        }
    }
}

const MAX_NARROWINGS_PER_STEP: usize = 1;

#[derive(Debug, Eq, PartialEq)]
enum ReviewProgress {
    Continue,
    Finish,
    Exhausted,
}

enum ReviewOutcome {
    Stopped(ActiveDeadlineCause),
    Reviewed(ToolReviewDecision),
    Exhausted,
}

struct ReviewFacts {
    site: DispatchSite,
    outcome: ReviewOutcome,
    mode_unchanged: bool,
    paused: bool,
    cancelled: bool,
}

#[derive(Debug, Eq, PartialEq)]
struct ReviewResolution {
    progress: ReviewProgress,
    approved: bool,
    source: PermissionSource,
    active_time_exhausted: bool,
}

fn decide_review(facts: ReviewFacts) -> ReviewResolution {
    let (progress, approved, source, active_time_exhausted) = match facts.outcome {
        ReviewOutcome::Stopped(cause) => (
            ReviewProgress::Finish,
            false,
            PermissionSource::UnavailableApproval,
            match cause {
                ActiveDeadlineCause::Goal => true,
                ActiveDeadlineCause::Delegation => false,
            },
        ),
        ReviewOutcome::Exhausted => (
            ReviewProgress::Exhausted,
            false,
            PermissionSource::UnavailableApproval,
            false,
        ),
        ReviewOutcome::Reviewed(decision) => {
            let approved = match decision {
                ToolReviewDecision::Approve => facts.mode_unchanged,
                ToolReviewDecision::Deny => false,
            };
            let source = if approved {
                PermissionSource::AiReview
            } else {
                PermissionSource::UnavailableApproval
            };
            let delegated_approval = match facts.site {
                DispatchSite::Primary => false,
                DispatchSite::Researcher => approved,
            };
            let progress = if delegated_approval {
                ReviewProgress::Finish
            } else {
                ReviewProgress::Continue
            };
            (
                progress,
                approved && !(delegated_approval && (facts.paused || facts.cancelled)),
                source,
                false,
            )
        }
    };
    ReviewResolution {
        progress,
        approved,
        source,
        active_time_exhausted,
    }
}

struct Answer {
    checkpoint: Option<crate::session::Checkpoint>,
    observation: ToolObservation,
    stop: Option<StopCause>,
    active_time: Option<ActiveTime>,
}

struct ActiveTime {
    segment: Option<ActiveSegment>,
    exhausted: bool,
}

fn sizing_deadline_refusal(cause: Option<ActiveDeadlineCause>) -> SizingRefusal {
    match cause {
        Some(ActiveDeadlineCause::Goal) => SizingRefusal::ActiveTime,
        Some(ActiveDeadlineCause::Delegation) | None => SizingRefusal::DelegationDeadline,
    }
}

fn timeout_limit_cause(cause: Option<ActiveDeadlineCause>) -> Option<crate::tools::LimitCause> {
    match cause {
        Some(ActiveDeadlineCause::Goal) => None,
        Some(ActiveDeadlineCause::Delegation) | None => Some(crate::tools::LimitCause::Timeout),
    }
}

// Boxing a large future in a non-async wrapper keeps debug-build callers from holding it in their own frames.
pub(in crate::engine) type BoxedStep<'a, T> =
    std::pin::Pin<Box<dyn Future<Output = Result<T, EngineError>> + Send + 'a>>;

pub(super) type IntentResult = Result<(ToolObservation, Option<ToolCall>), StopCause>;

impl RunLoop<'_> {
    pub(super) async fn execute_step(
        &self,
        mut grant: Grant<'_>,
        request: ToolRequest,
        turn: ProviderTurn,
    ) -> Result<StepEnd, EngineError> {
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        let source_call = request.call.clone();
        let mut call = source_call.clone();
        let calls_remaining = match &grant {
            Grant::Primary { .. } => None,
            Grant::Researcher { child, .. } => Some(child.tool_calls_remaining.saturating_sub(1)),
        };
        let mut narrowings = 0;
        let mut step = ToolStep {
            refusal: None,
            plan: None,
            turn,
            requests: vec![request],
            observations: Vec::new(),
            facts: None,
            steering_notes: Vec::new(),
            calls_remaining,
            cleared: false,
            hook_feedback: Default::default(),
        };
        loop {
            let intent = match self
                .prepare_step_intent(&grant, runtime, &step, call.clone(), &source_call)
                .await?
            {
                Ok(intent) => intent,
                Err(cause) => return Ok(StepEnd::Stopped(cause)),
            };
            let command = matches!(intent.call, ToolCall::Command { .. })
                && matches!(intent.target, crate::worker::EffectTarget::Workspace);
            let source_intent_digest = intent.digest();
            let narrowable =
                matches!(grant, Grant::Primary { .. }) && narrowings < MAX_NARROWINGS_PER_STEP;
            // Boxed: approval and dispatch futures otherwise inflate every enclosing agent step.
            let (observation, narrowed) = match self
                .run_intent(&grant, runtime, intent, &source_call, narrowable)
                .await?
            {
                Ok(observed) => observed,
                Err(cause) => return Ok(StepEnd::Stopped(cause)),
            };
            if observation.stops_run() || self.controls.cancellation.is_cancelled() {
                return Ok(StepEnd::Stopped(Self::observation_stop(
                    &observation,
                    self.controls.cancellation.is_cancelled(),
                )));
            }
            let changeset = if command
                && observation.attestation.is_some()
                && matches!(
                    observation.disposition,
                    ToolDisposition::Succeeded | ToolDisposition::Failed
                ) {
                match serde_json::from_str::<CommandResult>(&observation.output) {
                    Ok(result) if result.valid() => Some(result.changeset),
                    Err(_) if observation.disposition == ToolDisposition::Failed => None,
                    _ => return Ok(StepEnd::Stopped(StopCause::InvalidCommandResult)),
                }
            } else {
                None
            };
            step.observations.push(observation);
            if let Some(narrowed) = narrowed {
                narrowings += 1;
                call = narrowed;
                continue;
            }
            let Some(changeset) = changeset.filter(|changeset| !changeset.files.is_empty()) else {
                break;
            };
            if self.controls.cancellation.is_cancelled() {
                return Ok(StepEnd::Stopped(StopCause::Cancelled));
            }
            if grant.context()?.reserved_len(&step, &[]) > runtime.context_limit_bytes() {
                return Ok(StepEnd::Stopped(StopCause::ContextFull));
            }
            call = ToolCall::ApplyChangeset {
                source: crate::session::worker::ChangesetSource::CommandReceipt {
                    source_intent_digest,
                    changeset,
                },
            };
            if !call.valid() {
                return Ok(StepEnd::Stopped(StopCause::InvalidCommandResult));
            }
        }
        // The hooks see the published tree: a Command's changeset is integrated first.
        if let Some(config) = runtime.permissions()
            && let Some(source) = step.observations.get(narrowings)
            && source.disposition == ToolDisposition::Succeeded
            && !source.intent.call.is_internal()
            && step.observations[narrowings + 1..]
                .iter()
                .all(|integration| integration.disposition == ToolDisposition::Succeeded)
        {
            let post =
                Box::pin(self.post_tool_use_hooks(runtime, config, source, grant.deadline()))
                    .await?;
            match post.stop {
                HookStop::None if !post.feedback.is_empty() => {
                    step.hook_feedback.insert(narrowings, post.feedback);
                }
                HookStop::None => {}
                HookStop::Uncertain => return Ok(StepEnd::Stopped(StopCause::Uncertain)),
                HookStop::Requested => return Ok(StepEnd::Stopped(StopCause::HookStop)),
                HookStop::ActiveTime if grant.deadline().is_some() => {
                    return Ok(StepEnd::Stopped(StopCause::DelegationDeadline));
                }
                HookStop::ActiveTime => return Ok(StepEnd::Stopped(StopCause::ActiveTime)),
            }
        }
        grant.context_mut()?.steps.push(step);
        if self.hook_requested_stop()? {
            return Ok(StepEnd::Stopped(StopCause::HookStop));
        }
        if grant.context()?.rendered_len() > runtime.context_limit_bytes() {
            return Ok(StepEnd::Stopped(StopCause::ContextFull));
        }
        Ok(StepEnd::Continue)
    }

    fn observation_stop(observation: &ToolObservation, cancelled: bool) -> StopCause {
        let cause = if observation.disposition == ToolDisposition::Uncertain
            || observation.cache_update == CacheUpdate::Uncertain
        {
            StopCause::Uncertain
        } else if observation.disposition == ToolDisposition::Cancelled {
            StopCause::Cancelled
        } else {
            StopCause::ToolStopped
        };
        if cancelled {
            cause.merge(StopCause::Cancelled)
        } else {
            cause
        }
    }

    async fn prepare_step_intent(
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

    pub(super) async fn primary_intent(
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

    pub(super) fn researcher_intent(
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

    pub(super) fn run_intent<'a>(
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
        let permission = self.permission(grant, runtime, &intent.call);
        let (hook_events, pre_ms, post_ms) = self.hook_admission(runtime, &intent.call, post_call);
        if let Err(refusal) = self
            .admit_tool(grant, runtime, &intent, permission, hook_events, narrowable)
            .await?
        {
            return Ok(Err(refusal.into()));
        }
        let agent_run_id = intent.agent_run_id;
        let intent_id = intent.id;
        // A researcher's Tool runs inside its delegation's reservation.
        let action_ms = match grant {
            Grant::Primary { .. } => tool_action_ms(runtime, &intent)
                .saturating_add(pre_ms)
                .saturating_add(post_ms),
            Grant::Researcher { .. } => 0,
        };
        self.append(Event::ToolStarted {
            run_id: self.run_id,
            agent_run_id,
            intent: intent.clone(),
            active_ceiling_ms: self.ledger().active_ceiling(action_ms)?,
        })
        .await?;
        if matches!(grant, Grant::Researcher { child, .. } if child.editor) {
            self.worker
                .lock()
                .await
                .get_mut(&agent_run_id)
                .ok_or(EngineError::CoordinatorFailed)?
                .note_intent(&intent);
        }
        let mut narrowed = None;
        let mut candidate = None;
        let Admission::Reserved(verdict) = self
            .before_dispatch(grant, runtime, intent, permission, narrowable)
            .await?
        else {
            return Ok(Err(StopCause::ActiveTime));
        };
        let answer = match verdict {
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
                Grant::Researcher {
                    child, deadline, ..
                } => {
                    let deadline = grant.deadline().unwrap_or(*deadline);
                    let active_deadline = match grant {
                        Grant::Researcher {
                            active_deadline, ..
                        } => active_deadline.as_ref(),
                        Grant::Primary { .. } => None,
                    };
                    let cancellation = RunCancellation::new();
                    let mut parent = self.controls.cancellation.clone();
                    let mut timed_out = false;
                    let execution = async {
                        let _active_work = self.ledger().active_work();
                        if child.editor {
                            let (observation, pending) = self
                                .dispatch_worker(runtime, intent, cancellation.clone())
                                .await?;
                            candidate = pending;
                            Ok::<_, EngineError>(observation)
                        } else {
                            Ok(runtime.execute(intent, cancellation.clone()).await)
                        }
                    };
                    tokio::pin!(execution);
                    let mut observation = tokio::select! {
                        result = &mut execution => result?,
                        () = async {
                            match active_deadline {
                                Some(deadline) => deadline.elapsed().await,
                                None => tokio::time::sleep_until(deadline).await,
                            }
                        } => { timed_out = true; cancellation.cancel(); execution.await? },
                        () = parent.cancelled() => { cancellation.cancel(); execution.await? },
                    };
                    if timed_out && observation.disposition == ToolDisposition::Cancelled {
                        observation.disposition = ToolDisposition::Limit;
                        observation.limit_cause =
                            timeout_limit_cause(active_deadline.map(|d| d.cause));
                    }
                    Answer {
                        checkpoint: None,
                        observation,
                        stop: timed_out.then(|| match active_deadline.map(|d| d.cause) {
                            Some(crate::engine::ledger::ActiveDeadlineCause::Goal) => {
                                StopCause::ActiveTime
                            }
                            Some(crate::engine::ledger::ActiveDeadlineCause::Delegation) => {
                                StopCause::DelegationDeadline
                            }
                            None => StopCause::ToolStopped,
                        }),
                        active_time: None,
                    }
                }
            },
        };
        self.append(Event::ToolFinished {
            checkpoint: answer.checkpoint.clone(),
            run_id: self.run_id,
            agent_run_id,
            observation: answer.observation.clone(),
        })
        .await?;
        self.commit_approval_withdrawal(intent_id)?;
        if let Some((candidate, data)) = candidate {
            if matches!(answer.observation.intent.call, ToolCall::Command { .. }) {
                self.seal_worker_command(&answer.observation.intent, &candidate, &data)
                    .await?;
            }
            self.worker
                .lock()
                .await
                .get_mut(&agent_run_id)
                .ok_or(EngineError::CoordinatorFailed)?
                .acknowledge(candidate, data)?;
        }
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

    async fn admit_tool(
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

    #[expect(
        clippy::too_many_lines,
        reason = "keep hook narrowing, human approval and journal-before-dispatch ordering together"
    )]
    async fn before_dispatch(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        intent: EffectIntent,
        permission: Permission,
        narrowable: bool,
    ) -> Result<Admission<Verdict>, EngineError> {
        let config = runtime.permissions();
        let site = grant.site();
        let allow_auto_edits = permission.decision == Decision::Allow
            && self.terminal_mode() == Some(ApprovalMode::Request)
            && intent.call.facts().mutates;
        let permission = permission.narrow_for_intent(self.terminal_mode(), &intent);
        let timeout_limit = self.ledger().command_timeout_limit(&intent.call)?;
        let hooks_allowed = self
            .host_permission(runtime, &intent, permission, &HookFold::none())?
            .permission
            .decision
            != Decision::Deny;
        let goal_allows = intent.call.is_internal()
            || match grant {
                Grant::Primary { .. } => self.plan_allows(&intent.call)?,
                Grant::Researcher { .. } => self.ledger().allows_tool(&intent.call),
            };
        let hooks = match config {
            Some(config)
                if !intent.call.is_internal()
                    && hooks_allowed
                    && timeout_limit.is_none()
                    && goal_allows =>
            {
                // Boxed like the other Tool futures so the hook loop does not inflate every agent step.
                Box::pin(self.pre_tool_use_hooks(grant, runtime, config, &intent, narrowable))
                    .await?
            }
            _ => HookFold::none(),
        };
        let permission =
            permission.narrow_hooks(hooks.restriction, config.map(|config| config.mode()));
        let host_permission = self.host_permission(runtime, &intent, permission, &hooks)?;
        let permission = host_permission.permission;
        let facts = DispatchFacts {
            site,
            permission,
            hooks: &hooks,
            timeout_limit,
            host: intent.call.is_host(),
            host_unpinned: intent.call.is_host() && intent.host.is_none(),
            human_only_unavailable: (intent.call.is_host() && self.controls.approvals.is_none())
                || self.controls.approvals.is_some_and(|approvals| {
                    !approvals
                        .admits_human_only(&intent.call, hooks.restriction == Some(Decision::Ask))
                }),
            unguarded: match grant {
                Grant::Primary { .. } if intent.call.is_unguarded() => Some(Unguarded {
                    gate: unguarded_gate(runtime, &intent).await.map(|mut prompt| {
                        prompt.remember = host_permission.remember;
                        prompt.prefix =
                            host_permission.prefix && runtime.host_prefix_candidate(&intent);
                        prompt
                    }),
                }),
                Grant::Primary { .. } | Grant::Researcher { .. } => None,
            },
            goal_allows,
            runtime_allows: runtime.allows(&intent.call),
            can_start_provider_call: self.ledger().can_start_provider_call(),
            guard_site: intent.call.facts().site == crate::tools::catalog::Site::Guard,
            interactive: self.controls.approvals.is_some(),
        };
        let remember_prefix = remember_candidate(
            grant,
            config.map(|config| config.mode()),
            permission,
            &hooks,
            &intent.call,
        );
        let approval = match facts.ask() {
            Some(mode) => Box::pin(self.ask(
                grant,
                &intent,
                mode,
                AskOptions {
                    unguarded_prompt: facts.unguarded_prompt(),
                    allow_auto_edits: allow_auto_edits && hooks.restriction != Some(Decision::Ask),
                    remember_prefix,
                },
            ))
            .await?
            .map(Some),
            None => Admission::Reserved(None),
        };
        let Admission::Reserved(mut approval) = approval else {
            return Ok(Admission::Exhausted);
        };
        if let Some(withdrawal) = approval
            .as_mut()
            .and_then(|resolution| resolution.withdrawal.take())
        {
            self.defer_approval_withdrawal(intent.id, withdrawal)?;
        }
        let save = approval.as_ref().and_then(|resolution| resolution.save);
        let decided = decide_dispatch(
            &facts,
            &AfterAsk {
                approval,
                paused: self.pause_requested(),
                can_start_provider_call: self.ledger().can_start_provider_call(),
                cancelled: self.controls.cancellation.is_cancelled(),
                deadline: match grant {
                    Grant::Primary { .. } => None,
                    Grant::Researcher {
                        active_deadline: Some(deadline),
                        ..
                    } => deadline.reached().then_some(deadline.cause),
                    Grant::Researcher {
                        deadline,
                        active_deadline: None,
                        ..
                    } => (Instant::now() >= *deadline).then_some(ActiveDeadlineCause::Delegation),
                },
            },
        );
        // An in-process read waits for no Guard capacity, so it reserves its action directly.
        let (outcome, queue_pause) = match decided.outcome {
            Ok(Dispatch::QueuePause) => match self.reserve_active_pause()?.map(|pause| {
                (
                    decide_queue_pause(true, self.pause_requested()),
                    Some(pause),
                )
            }) {
                Admission::Reserved(outcome) => outcome,
                Admission::Exhausted => (decide_queue_pause(false, self.pause_requested()), None),
            },
            Ok(Dispatch::Direct) => (Ok(()), None),
            Err(refusal) => (Err(refusal), None),
        };
        let (agent_run_id, digest) = (intent.agent_run_id, intent.digest());
        let unguarded = intent.call.is_unguarded();
        let git_write = intent.call.is_git() && intent.call.facts().mutates;
        let verdict = match outcome {
            Ok(()) => Verdict::Dispatch {
                intent,
                queue_pause,
            },
            Err(refusal) => {
                if matches!(refusal.reason, NotDispatched::Paused) {
                    // A pause requested during review or the approval wait stops the Run before dispatch.
                    self.controls.cancellation.cancel();
                }
                if let Some(pause) = queue_pause {
                    self.settle_active_pause(pause, None)?;
                }
                self.refused(site, intent, refusal)
            }
        };
        let mode = effective_permission_mode(config, grant);
        if mode.is_some() || unguarded || matches!(grant, Grant::Primary { .. }) && git_write {
            let approved = matches!(&verdict, Verdict::Dispatch { .. });
            // An approving review that did not lead to dispatch is not an AI approval.
            let source = if !approved && decided.source == PermissionSource::AiReview {
                PermissionSource::UnavailableApproval
            } else {
                decided.source
            };
            self.append(Event::ToolPermissionDecided {
                run_id: self.run_id,
                agent_run_id,
                verdict: PermissionVerdict {
                    host: host_permission.verdict(approved, unguarded, save),
                    intent_digest: digest,
                    decision: decided.decision,
                    mode,
                    basis: permission.basis,
                    source,
                    approved,
                    hook_restriction: hooks.restriction,
                    hooks: hooks.count,
                    narrowed: matches!(verdict, Verdict::Narrow(..)),
                },
            })
            .await?;
        }
        self.save_remembered_rule(runtime, site, verdict, save)
            .map(Admission::Reserved)
    }

    async fn dispatch_primary(
        &self,
        runtime: &ToolRuntime,
        intent: EffectIntent,
        queue_pause: Option<ActivePause>,
    ) -> Result<Answer, EngineError> {
        if matches!(intent.call, ToolCall::PrepareWorker { .. }) {
            if let Some(pause) = queue_pause {
                self.settle_active_pause(pause, None)?;
            }
            let Admission::Reserved(segment) = self.reserve_active_segment(
                tool_action_bound(runtime, &intent),
                crate::session::BudgetAction::Tool,
                intent.agent_run_id,
            )?
            else {
                return Ok(Answer {
                    checkpoint: None,
                    observation: not_dispatched(intent, &NotDispatched::ActiveTime),
                    stop: None,
                    active_time: Some(ActiveTime {
                        segment: None,
                        exhausted: true,
                    }),
                });
            };
            let _active_work = self.ledger().active_work();
            let observation = self.capture_worker_base(runtime, intent).await?;
            return Ok(Answer {
                checkpoint: None,
                observation,
                stop: None,
                active_time: Some(ActiveTime {
                    segment: Some(segment),
                    exhausted: false,
                }),
            });
        }
        let runtime = if let ToolCall::ApplyChangeset {
            source: crate::session::worker::ChangesetSource::WorkerHandover { assignment_id, .. },
        } = &intent.call
        {
            let bytes = self
                .handover
                .lock()
                .map_err(|_| EngineError::CoordinatorFailed)?
                .get(assignment_id)
                .cloned()
                .ok_or(EngineError::CoordinatorFailed)?;
            runtime.with_handover(bytes)
        } else {
            runtime.clone()
        };
        let runtime = &runtime;
        let (observation, segment, exhausted, checkpoint) =
            if intent.call.facts().site == crate::tools::catalog::Site::Guard {
                let Some(queue_pause) = queue_pause else {
                    return Err(EngineError::CoordinatorFailed);
                };
                Box::pin(self.dispatch_guard(runtime, intent, queue_pause)).await?
            } else {
                match self.reserve_active_segment(
                    tool_action_bound(runtime, &intent),
                    crate::session::BudgetAction::Tool,
                    intent.agent_run_id,
                )? {
                    Admission::Reserved(segment) => (
                        {
                            let _active_work = self.ledger().active_work();
                            execute_cancellable(runtime, intent, &self.controls.cancellation).await
                        },
                        Some(segment),
                        false,
                        None,
                    ),
                    Admission::Exhausted => (
                        not_dispatched(intent, &NotDispatched::ActiveTime),
                        None,
                        true,
                        None,
                    ),
                }
            };
        self.clear_tool_queue();
        Ok(Answer {
            checkpoint,
            observation,
            stop: None,
            active_time: Some(ActiveTime { segment, exhausted }),
        })
    }

    async fn dispatch_guard(
        &self,
        runtime: &ToolRuntime,
        intent: EffectIntent,
        queue_pause: ActivePause,
    ) -> Result<
        (
            ToolObservation,
            Option<ActiveSegment>,
            bool,
            Option<crate::session::Checkpoint>,
        ),
        EngineError,
    > {
        let mut queue_pause = Some(queue_pause);
        let mut active_time_exhausted = false;
        let runtime = runtime.with_queue_progress(self.hints.map(HintPublisher::tool_queue));
        let (active_sender, mut active_receiver) =
            tokio::sync::mpsc::channel::<ActiveActionRequest>(1);
        let runtime = runtime.with_active_action_gate(active_sender);
        let (checkpoint_sender, mut checkpoint_receiver) = tokio::sync::mpsc::channel(1);
        let runtime = runtime.with_checkpoint_gate(checkpoint_sender);
        let checkpoint_intent = intent.clone();
        let mut checkpoint = None;
        let mut _active_work = None;
        let worker_cancellation = RunCancellation::new();
        let mut run_cancellation = self.controls.cancellation.clone();
        let cache_transfer = Duration::from_millis(u64::from(intent.limits.cache_transfer_ms))
            .saturating_add(crate::tools::worker::transfer_time(&intent));
        let action_bound = tool_action_bound(&runtime, &intent);
        let runtime_ms = intent.limits.runtime_ms;
        let actor = intent.agent_run_id;
        let mut execution = Box::pin(runtime.execute(intent, worker_cancellation.clone()));
        let mut active_segment = None;
        let mut observation = loop {
            tokio::select! {
                biased;
                () = run_cancellation.cancelled() => {
                    checkpoint_receiver.close();
                    while checkpoint_receiver.try_recv().is_ok() {}
                    worker_cancellation.cancel();
                    break settle_cancelled(execution.as_mut(), &mut active_receiver).await;
                }
                request = active_receiver.recv(), if active_segment.is_none() => {
                    if let Some(ActiveActionRequest { admitted_at, reply }) = request {
                        if let Some(pause) = queue_pause.take() {
                            self.settle_active_pause(pause, Some(admitted_at))?;
                        }
                        match self.reserve_active_segment(action_bound, crate::session::BudgetAction::Tool, actor) {
                            Ok(Admission::Reserved(segment)) => {
                                let window = active_window(&segment, cache_transfer, runtime_ms);
                                active_segment = Some(segment);
                                _active_work = Some(self.ledger().active_work());
                                let _ = reply.send(window);
                            }
                            Ok(Admission::Exhausted) => {
                                active_time_exhausted = true;
                                let _ = reply.send(Err(ToolError::Limit));
                            }
                            Err(_) => {
                                let _ = reply.send(Err(ToolError::Limit));
                            }
                        }
                    }
                }
                request = checkpoint_receiver.recv() => {
                    if let Some(crate::tools::CheckpointRequest { reply, go_deadline }) = request {
                        if crate::session::checkpoint::publication(&checkpoint_intent) {
                            let deadline = [
                                active_segment.as_ref().map(|s| s.deadline()),
                                go_deadline.checked_sub(CHECKPOINT_GO_MARGIN),
                                Instant::now().checked_add(CHECKPOINT_WAIT),
                            ]
                            .into_iter()
                            .map(|bound| bound.unwrap_or_else(Instant::now))
                            .min()
                            .unwrap_or_else(Instant::now);
                            checkpoint = Some(self.capture_checkpoint(&runtime, &checkpoint_intent, deadline).await?);
                        }
                        let _ = reply.send(());
                    }
                }
                observation = &mut execution => break observation,
            }
        };
        if let Some(pause) = queue_pause.take() {
            self.settle_active_pause(pause, None)?;
        }
        observation = match GuardCompletion::decide(
            observation.disposition,
            observation.attestation.is_some(),
            active_time_exhausted,
        ) {
            GuardCompletion::Observed => observation,
            GuardCompletion::ActiveTime => {
                not_dispatched(observation.intent, &NotDispatched::ActiveTime)
            }
        };
        let reached_go = observation.attestation.is_some()
            || matches!(observation.disposition, ToolDisposition::Uncertain);
        if !reached_go {
            checkpoint = None;
        }
        if let Some(capture) = &mut checkpoint
            && !matches!(
                observation.disposition,
                ToolDisposition::Succeeded | ToolDisposition::Uncertain
            )
        {
            capture.status = crate::session::CheckpointStatus::NotNeeded;
            capture.reason = None;
        }
        Ok((
            observation,
            active_segment,
            active_time_exhausted,
            checkpoint,
        ))
    }

    fn clear_tool_queue(&self) {
        if let Some(hints) = self.hints {
            hints.tool_queue().send_replace(None);
        }
    }

    async fn resolve_primary_human(
        &self,
        intent: &EffectIntent,
        options: &AskOptions,
        resolution: &mut Resolution,
    ) -> Result<(), EngineError> {
        if !resolution.active_time_exhausted
            && !self.pause_at_boundary()
            && let Some(approvals) = self.controls.approvals
        {
            match self.reserve_active_pause()? {
                Admission::Reserved(pause) => {
                    let reply = self
                        .human_reply(
                            approvals,
                            intent,
                            options.unguarded_prompt.clone(),
                            options.allow_auto_edits,
                            options.remember_prefix,
                        )
                        .await;
                    resolution.source = if reply == ApprovalReply::Unavailable {
                        PermissionSource::UnavailableApproval
                    } else {
                        PermissionSource::Human
                    };
                    resolution.approved = matches!(
                        reply,
                        ApprovalReply::AllowOnce
                            | ApprovalReply::AllowForSession
                            | ApprovalReply::AllowPrefixInFolder
                    ) && !self.controls.cancellation.is_cancelled()
                        && !self.pause_requested();
                    // A remembered rule is saved only after this human verdict is committed.
                    resolution.save = match reply {
                        ApprovalReply::AllowForSession => Some(RuleSave::Session),
                        ApprovalReply::AllowPrefixInFolder => {
                            options.remember_prefix.map(RuleSave::Prefix).or_else(|| {
                                options
                                    .unguarded_prompt
                                    .as_ref()
                                    .filter(|prompt| prompt.prefix)
                                    .map(|_| RuleSave::HostPrefix)
                            })
                        }
                        ApprovalReply::Deny
                        | ApprovalReply::Unavailable
                        | ApprovalReply::AllowOnce => None,
                    };
                    if reply == ApprovalReply::AllowPrefixInFolder && resolution.save.is_none()
                        || reply == ApprovalReply::AllowForSession
                            && !options
                                .unguarded_prompt
                                .as_ref()
                                .is_some_and(|prompt| prompt.remember)
                    {
                        resolution.approved = false;
                    }
                    let resume_at = resolution.approved.then(Instant::now);
                    self.settle_active_pause(pause, resume_at)?;
                }
                Admission::Exhausted => {
                    resolution.active_time_exhausted = true;
                }
            }
        }
        Ok(())
    }

    async fn resolve_review(
        &self,
        objective: &str,
        intent: &EffectIntent,
        site: &AskSite,
        initial_mode: Option<ApprovalMode>,
    ) -> Result<ReviewResolution, EngineError> {
        let outcome = match self.review_exhaustion_cause(site) {
            Some(cause) => ReviewOutcome::Stopped(cause),
            None => {
                let review = match site {
                    AskSite::Primary(_) => self.review_tool(objective, intent, None).await?,
                    AskSite::Delegated { active_deadline } => {
                        self.review_tool(objective, intent, active_deadline.as_ref())
                            .await?
                    }
                };
                match self.review_exhaustion_cause(site) {
                    Some(cause) => ReviewOutcome::Stopped(cause),
                    None => match review {
                        Admission::Reserved(decision) => ReviewOutcome::Reviewed(decision),
                        Admission::Exhausted => ReviewOutcome::Exhausted,
                    },
                }
            }
        };
        Ok(decide_review(ReviewFacts {
            site: match site {
                AskSite::Primary(_) => DispatchSite::Primary,
                AskSite::Delegated { .. } => DispatchSite::Researcher,
            },
            outcome,
            mode_unchanged: self.terminal_mode() == initial_mode,
            paused: self.pause_requested(),
            cancelled: self.controls.cancellation.is_cancelled(),
        }))
    }

    fn review_exhaustion_cause(
        &self,
        site: &AskSite,
    ) -> Option<crate::engine::ledger::ActiveDeadlineCause> {
        match site {
            AskSite::Primary(_) if self.ledger().active_time_exhausted() => {
                Some(crate::engine::ledger::ActiveDeadlineCause::Goal)
            }
            AskSite::Delegated {
                active_deadline: Some(deadline),
            } if deadline.reached() => Some(deadline.cause),
            AskSite::Primary(_)
            | AskSite::Delegated {
                active_deadline: None,
            }
            | AskSite::Delegated {
                active_deadline: Some(_),
            } => None,
        }
    }

    async fn delegated_human_reply(
        &self,
        intent: &EffectIntent,
        deadline: Option<&crate::engine::ledger::ActiveDeadline>,
        approvals: &crate::tools::ToolApprovals,
    ) -> Result<(ApprovalReply, Option<ApprovalWithdrawal>), EngineError> {
        let mut cancellation = self.controls.cancellation.clone();
        let activity_started = self.ledger().active_elapsed();
        let wall_started = Instant::now();
        let mut pending = approvals.begin_local(intent, false, None).await;
        let (reply, interrupted) = tokio::select! {
            biased;
            () = cancellation.cancelled() => (ApprovalReply::Unavailable, true),
            () = self.approval_paused() => (ApprovalReply::Unavailable, true),
            () = async {
                match deadline {
                    Some(deadline) => deadline.elapsed().await,
                    None => std::future::pending().await,
                }
            } => (ApprovalReply::Unavailable, true),
            reply = pending.reply() => (reply, false),
        };
        let active_during = self
            .ledger()
            .active_elapsed()
            .saturating_sub(activity_started);
        self.ledger()
            .exclude_idle_approval(wall_started.elapsed().saturating_sub(active_during))?;
        let withdrawal = interrupted.then(|| pending.reject_late_replies()).flatten();
        Ok((reply, withdrawal))
    }
}

fn tool_action_bound(runtime: &ToolRuntime, intent: &EffectIntent) -> Duration {
    runtime
        .intent_reservation_overhead(intent)
        .saturating_add(
            Duration::from_millis(u64::from(intent.limits.cache_transfer_ms))
                .saturating_add(crate::tools::worker::transfer_time(intent))
                .saturating_mul(2),
        )
        .saturating_add(Duration::from_millis(u64::from(intent.limits.runtime_ms)))
        .saturating_add(TOOL_CLEANUP_OVERHEAD)
}

fn tool_action_ms(runtime: &ToolRuntime, intent: &EffectIntent) -> u64 {
    duration_ms(tool_action_bound(runtime, intent).saturating_add(STORE_ACTION_OVERHEAD))
}

fn active_window(
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

// Dropping a request's reply makes the Guard read an unstarted action. A send that reserved its
// slot before the close can still arrive, so keep draining while the worker finishes.
async fn settle_cancelled<T>(
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

async fn execute_cancellable(
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

#[derive(Debug, Eq, PartialEq)]
pub(super) enum HookDenial {
    ActiveTime,
    DelegationDeadline,
    Uncertain,
    Requested,
    Narrowed(String),
    Denied(String),
}

#[must_use]
#[derive(Debug, Eq, PartialEq)]
pub(super) enum SkillInitialization {
    Ready,
    Stopped,
    Failed(crate::session::RunFailure),
}

#[must_use]
#[derive(Debug, Eq, PartialEq)]
enum SkillReadPlan {
    Skip,
    Stop,
    Read(String),
}

fn skill_read_plan(name: Option<String>, stopping: bool) -> SkillReadPlan {
    match (name, stopping) {
        (_, true) => SkillReadPlan::Stop,
        (None, false) => SkillReadPlan::Skip,
        (Some(name), false) => SkillReadPlan::Read(name),
    }
}

fn skill_read_decision(
    disposition: ToolDisposition,
    attested: bool,
    context_bytes: usize,
    context_limit: usize,
) -> SkillInitialization {
    match disposition {
        ToolDisposition::Succeeded if attested => {
            if context_bytes <= context_limit {
                SkillInitialization::Ready
            } else {
                SkillInitialization::Failed(crate::session::RunFailure::ToolContextFull)
            }
        }
        ToolDisposition::Cancelled | ToolDisposition::Uncertain => SkillInitialization::Stopped,
        ToolDisposition::Succeeded
        | ToolDisposition::Denied
        | ToolDisposition::Failed
        | ToolDisposition::Conflict
        | ToolDisposition::Limit => {
            SkillInitialization::Failed(crate::session::RunFailure::ToolStopped)
        }
    }
}

impl RunLoop<'_> {
    pub(super) async fn initialize_skill(
        &self,
        request: &mut ProviderRequest,
    ) -> Result<RunProgress<SkillInitialization>, EngineError> {
        let name = self
            .ledger()
            .view(|view| view.runs.last().and_then(|run| run.skill.clone()))?;
        let name = match skill_read_plan(
            name,
            self.pause_at_boundary() || self.controls.cancellation.is_cancelled(),
        ) {
            SkillReadPlan::Skip => return Ok(RunProgress::Ready(SkillInitialization::Ready)),
            SkillReadPlan::Stop => return Ok(RunProgress::Ready(SkillInitialization::Stopped)),
            SkillReadPlan::Read(name) => name,
        };
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        let call = ToolCall::Skill {
            name,
            resource: None,
        };
        if !self.ledger().allows_tool(&call) {
            return Ok(RunProgress::Ready(SkillInitialization::Failed(
                crate::session::RunFailure::ToolStopped,
            )));
        }
        let mut intent = match self.primary_intent(runtime, call.clone()).await? {
            None => return Ok(RunProgress::Ready(SkillInitialization::Stopped)),
            Some(intent) => intent,
        };
        let tools = request
            .input
            .tools
            .as_ref()
            .ok_or(EngineError::InvalidRequest)?;
        let Some(capacity) =
            runtime.result_capacity(tools.rendered_len(), INVOKED_SKILL_ENVELOPE_BYTES)
        else {
            return Ok(RunProgress::Ready(SkillInitialization::Failed(
                crate::session::RunFailure::ToolContextFull,
            )));
        };
        intent.limits.result_bytes = capacity.min(MAX_TOOL_RESULT_BYTES) as u32;
        let grant = Grant::Primary {
            request,
            review_allowed: false,
        };
        let observation = match self
            .run_intent(&grant, runtime, intent, &call, false)
            .await?
        {
            Ok((observation, _)) => observation,
            Err(StopCause::ActiveTime) => return Ok(RunProgress::Stopped(RunRefusal::Budget)),
            Err(StopCause::Capacity) => return Ok(RunProgress::Stopped(RunRefusal::Capacity)),
            Err(
                StopCause::Uncertain
                | StopCause::Cancelled
                | StopCause::HookStop
                | StopCause::Paused
                | StopCause::Budget
                | StopCause::ContextFull
                | StopCause::DelegationDeadline
                | StopCause::Refused(_)
                | StopCause::ToolStopped
                | StopCause::InvalidCommandResult,
            ) => return Ok(RunProgress::Ready(SkillInitialization::Stopped)),
        };
        self.apply_invoked_skill(request, runtime, observation)
            .map(RunProgress::Ready)
    }

    fn apply_invoked_skill(
        &self,
        request: &mut ProviderRequest,
        runtime: &ToolRuntime,
        observation: ToolObservation,
    ) -> Result<SkillInitialization, EngineError> {
        let tools = request
            .input
            .tools
            .as_mut()
            .ok_or(EngineError::InvalidRequest)?;
        let decision = skill_read_decision(
            observation.disposition,
            observation.attestation.is_some(),
            tools.with_invoked_skill_len(&observation),
            runtime.context_limit_bytes(),
        );
        match decision {
            SkillInitialization::Ready => tools.invoked_skill = Some(observation),
            SkillInitialization::Stopped | SkillInitialization::Failed(_) => {}
        }
        Ok(decision)
    }
}

#[must_use]
pub(super) enum DefinitionInitialization {
    Ready,
    Stopped,
}

#[must_use]
enum DefinitionRead {
    Output(String),
    Stopped,
}

impl RunLoop<'_> {
    pub(super) async fn initialize_definitions(
        &self,
        request: &mut ProviderRequest,
    ) -> Result<RunProgress<DefinitionInitialization>, EngineError> {
        let Some(runtime) = self.controls.tools else {
            return Ok(RunProgress::Ready(DefinitionInitialization::Ready));
        };
        let Some(pin) = runtime.definitions.pin() else {
            return Ok(RunProgress::Ready(DefinitionInitialization::Ready));
        };
        let mut outputs = Vec::with_capacity(pin.files.len());
        let mut normalized_bytes = 0;
        for (index, file) in pin.files.iter().enumerate() {
            if self.pause_at_boundary() || self.controls.cancellation.is_cancelled() {
                return Ok(RunProgress::Ready(DefinitionInitialization::Stopped));
            }
            let call = ToolCall::ParseAgentDefinition {
                index: u16::try_from(index).map_err(|_| EngineError::InvalidRequest)?,
                raw_digest: file.digest,
            };
            let output = match self.read_definition(request, runtime, call).await? {
                RunProgress::Ready(DefinitionRead::Output(output)) => output,
                RunProgress::Ready(DefinitionRead::Stopped) => {
                    return Ok(RunProgress::Ready(DefinitionInitialization::Stopped));
                }
                RunProgress::Stopped(cause) => return Ok(RunProgress::Stopped(cause)),
            };
            normalized_bytes += output.len();
            if normalized_bytes > 1024 * 1024 {
                return Ok(RunProgress::Ready(DefinitionInitialization::Stopped));
            }
            outputs.push(output);
        }
        let child_model = &self.routing.providers.child[0].identity().model;
        let Ok(registry) = crate::tools::definitions::registry(
            &pin,
            outputs.iter().map(String::as_str),
            child_model,
        ) else {
            return Ok(RunProgress::Ready(DefinitionInitialization::Stopped));
        };
        self.install_definitions(request, runtime, registry)
            .map(RunProgress::Ready)
    }

    fn install_definitions(
        &self,
        request: &mut ProviderRequest,
        runtime: &ToolRuntime,
        registry: crate::tools::definitions::Registry,
    ) -> Result<DefinitionInitialization, EngineError> {
        let registry = registry.definitions;
        let tools = request
            .input
            .tools
            .as_mut()
            .ok_or(EngineError::InvalidRequest)?;
        let mut catalog: serde_json::Value =
            serde_json::from_str(tools.catalog()).map_err(|_| EngineError::InvalidRequest)?;
        catalog["agents"] = serde_json::json!(
            registry
                .values()
                .map(|definition| {
                    let mut entry = definition.catalog_entry();
                    if definition.editor() {
                        let supported = runtime.worker_policy().is_some()
                            && self.controls.goal_phase
                                == crate::session::GoalPhase::Implementation;
                        entry["supported"] = serde_json::json!(supported);
                        entry["unsupported_reason"] = serde_json::json!(if !supported {
                            Some("editor_parent_grant_unavailable")
                        } else {
                            None
                        });
                        entry["editor"] = serde_json::json!(true);
                        entry["background"] = serde_json::json!(definition.background);
                        entry["digest"] = serde_json::json!(crate::validate::digest(
                            &serde_json::to_vec(definition).expect("normalized definition")
                        ));
                        entry["max_turns"] = serde_json::json!(definition.max_turns);
                    }
                    entry
                })
                .collect::<Vec<_>>()
        );
        tools.set_catalog(&catalog);
        if serde_json::to_vec(&catalog["agents"])
            .map_err(|_| EngineError::InvalidRequest)?
            .len()
            > 8 * 1024
            || tools.catalog().len() > crate::tools::types::MAX_TOOL_CATALOG_BYTES
            || tools.rendered_len() > runtime.context_limit_bytes()
        {
            return Ok(DefinitionInitialization::Stopped);
        }
        self.definitions
            .set(registry)
            .map_err(|_| EngineError::CoordinatorFailed)?;
        Ok(DefinitionInitialization::Ready)
    }

    async fn read_definition(
        &self,
        request: &mut ProviderRequest,
        runtime: &ToolRuntime,
        call: ToolCall,
    ) -> Result<RunProgress<DefinitionRead>, EngineError> {
        let Some(mut intent) = self.primary_intent(runtime, call).await? else {
            self.active_time_floor
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return Ok(RunProgress::Ready(DefinitionRead::Stopped));
        };
        intent.limits.result_bytes = MAX_TOOL_RESULT_BYTES as u32;
        let grant = Grant::Primary {
            request,
            review_allowed: false,
        };
        let post_call = intent.call.clone();
        let observation = match self
            .run_intent(&grant, runtime, intent, &post_call, false)
            .await?
        {
            Ok((observation, _)) => observation,
            Err(StopCause::ActiveTime) => return Ok(RunProgress::Stopped(RunRefusal::Budget)),
            Err(StopCause::Capacity) => return Ok(RunProgress::Stopped(RunRefusal::Capacity)),
            Err(
                StopCause::Uncertain
                | StopCause::Cancelled
                | StopCause::HookStop
                | StopCause::Paused
                | StopCause::Budget
                | StopCause::ContextFull
                | StopCause::DelegationDeadline
                | StopCause::Refused(_)
                | StopCause::ToolStopped
                | StopCause::InvalidCommandResult,
            ) => return Ok(RunProgress::Ready(DefinitionRead::Stopped)),
        };
        if observation.disposition != ToolDisposition::Succeeded
            || observation.attestation.is_none()
        {
            return Ok(RunProgress::Ready(DefinitionRead::Stopped));
        }
        Ok(RunProgress::Ready(DefinitionRead::Output(
            observation.output,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::permission::{HostReuse, HostReuseFacts, decide_host_reuse};
    use super::*;

    #[test]
    fn sizing_and_timeout_limits_preserve_distinct_deadline_causes() {
        use crate::tools::LimitCause;

        for (cause, sizing, limit) in [
            (
                Some(ActiveDeadlineCause::Goal),
                SizingRefusal::ActiveTime,
                None,
            ),
            (
                Some(ActiveDeadlineCause::Delegation),
                SizingRefusal::DelegationDeadline,
                Some(LimitCause::Timeout),
            ),
            (
                None,
                SizingRefusal::DelegationDeadline,
                Some(LimitCause::Timeout),
            ),
        ] {
            assert_eq!(sizing_deadline_refusal(cause), sizing, "sizing: {cause:?}");
            assert_eq!(timeout_limit_cause(cause), limit, "timeout: {cause:?}");
        }
    }

    #[test]
    fn review_resolution_preserves_site_exhaustion_and_interruption_decisions() {
        use ActiveDeadlineCause::{Delegation, Goal};
        use DispatchSite::{Primary, Researcher};
        use PermissionSource::{AiReview, UnavailableApproval};
        use ReviewOutcome::{Exhausted, Reviewed, Stopped};
        use ReviewProgress::{Continue, Exhausted as Budget, Finish};
        use ToolReviewDecision::{Approve, Deny};
        #[rustfmt::skip]
        let rows = [
            (Primary, Stopped(Goal), [true, false, false], (Finish, false, UnavailableApproval, true)),
            (Researcher, Stopped(Goal), [true, false, false], (Finish, false, UnavailableApproval, true)),
            (Primary, Stopped(Delegation), [true, false, false], (Finish, false, UnavailableApproval, false)),
            (Researcher, Stopped(Delegation), [true, false, false], (Finish, false, UnavailableApproval, false)),
            (Primary, Stopped(Goal), [false, true, true], (Finish, false, UnavailableApproval, true)),
            (Researcher, Stopped(Delegation), [false, true, true], (Finish, false, UnavailableApproval, false)),
            (Primary, Exhausted, [true, false, false], (Budget, false, UnavailableApproval, false)),
            (Researcher, Exhausted, [true, false, false], (Budget, false, UnavailableApproval, false)),
            (Primary, Reviewed(Approve), [true, false, false], (Continue, true, AiReview, false)),
            (Researcher, Reviewed(Approve), [true, false, false], (Finish, true, AiReview, false)),
            (Primary, Reviewed(Approve), [false, false, false], (Continue, false, UnavailableApproval, false)),
            (Researcher, Reviewed(Approve), [false, false, false], (Continue, false, UnavailableApproval, false)),
            (Primary, Reviewed(Approve), [true, true, false], (Continue, true, AiReview, false)),
            (Primary, Reviewed(Approve), [true, false, true], (Continue, true, AiReview, false)),
            (Researcher, Reviewed(Approve), [true, true, false], (Finish, false, AiReview, false)),
            (Researcher, Reviewed(Approve), [true, false, true], (Finish, false, AiReview, false)),
            (Researcher, Reviewed(Approve), [true, true, true], (Finish, false, AiReview, false)),
            (Researcher, Reviewed(Approve), [false, true, true], (Continue, false, UnavailableApproval, false)),
            (Primary, Reviewed(Deny), [true, false, false], (Continue, false, UnavailableApproval, false)),
            (Researcher, Reviewed(Deny), [true, false, false], (Continue, false, UnavailableApproval, false)),
            (Primary, Reviewed(Deny), [false, true, true], (Continue, false, UnavailableApproval, false)),
            (Researcher, Reviewed(Deny), [false, true, true], (Continue, false, UnavailableApproval, false)),
        ];
        for (index, (site, outcome, flags, expected)) in rows.into_iter().enumerate() {
            let [mode_unchanged, paused, cancelled] = flags;
            let (progress, approved, source, active_time_exhausted) = expected;
            assert_eq!(
                decide_review(ReviewFacts {
                    site,
                    outcome,
                    mode_unchanged,
                    paused,
                    cancelled,
                }),
                ReviewResolution {
                    progress,
                    approved,
                    source,
                    active_time_exhausted,
                },
                "review resolution row {index}"
            );
        }
    }

    #[test]
    fn skill_read_outcomes_are_closed_before_egress() {
        for (name, stopping, expected) in [
            (None, false, SkillReadPlan::Skip),
            (None, true, SkillReadPlan::Stop),
            (Some("guide".into()), true, SkillReadPlan::Stop),
            (
                Some("guide".into()),
                false,
                SkillReadPlan::Read("guide".into()),
            ),
        ] {
            assert_eq!(skill_read_plan(name, stopping), expected);
        }
        for (disposition, attested, expected) in [
            (ToolDisposition::Succeeded, true, SkillInitialization::Ready),
            (
                ToolDisposition::Succeeded,
                false,
                SkillInitialization::Failed(crate::session::RunFailure::ToolStopped),
            ),
            (
                ToolDisposition::Denied,
                false,
                SkillInitialization::Failed(crate::session::RunFailure::ToolStopped),
            ),
            (
                ToolDisposition::Failed,
                false,
                SkillInitialization::Failed(crate::session::RunFailure::ToolStopped),
            ),
            (
                ToolDisposition::Conflict,
                true,
                SkillInitialization::Failed(crate::session::RunFailure::ToolStopped),
            ),
            (
                ToolDisposition::Limit,
                true,
                SkillInitialization::Failed(crate::session::RunFailure::ToolStopped),
            ),
            (
                ToolDisposition::Cancelled,
                false,
                SkillInitialization::Stopped,
            ),
            (
                ToolDisposition::Uncertain,
                true,
                SkillInitialization::Stopped,
            ),
        ] {
            assert_eq!(skill_read_decision(disposition, attested, 64, 64), expected);
        }
        assert_eq!(
            skill_read_decision(ToolDisposition::Succeeded, true, 65, 64),
            SkillInitialization::Failed(crate::session::RunFailure::ToolContextFull)
        );
    }

    #[test]
    fn agent_definition_modes_never_resolve_parent_asks_and_dont_ask_denies_them() {
        use crate::tools::permissions::PermissionMode;
        let read = ToolCall::Read {
            path: "file".into(),
            offset: None,
            limit: Some(1),
        };
        let mut definition = crate::tools::definitions::AgentDefinition {
            version: 1,
            name: "reader".into(),
            description: "Read".into(),
            tools: None,
            disallowed_tools: Vec::new(),
            model: None,
            permission_mode: None,
            max_turns: crate::session::MAX_CHILD_TURNS,
            isolation: None,
            background: false,
            body: String::new(),
        };
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Plan,
            PermissionMode::Auto,
            PermissionMode::DontAsk,
        ] {
            definition.permission_mode = Some(mode);
            for decision in [Decision::Allow, Decision::Ask, Decision::Deny] {
                let permission = Permission {
                    decision,
                    basis: PermissionSource::Rule,
                    approval: (decision == Decision::Ask).then_some(ApprovalMode::Request),
                }
                .narrow_definition(&definition, &read);
                let expected = if mode == PermissionMode::DontAsk && decision == Decision::Ask {
                    Decision::Deny
                } else {
                    decision
                };
                assert_eq!(permission.decision, expected);
                assert_eq!(
                    permission.approval,
                    (expected == Decision::Ask).then_some(ApprovalMode::Request)
                );
            }
        }
        definition.permission_mode = Some(PermissionMode::Auto);
        let catalog = ToolCall::McpList {
            server: "server".into(),
        };
        let restriction = Permission {
            decision: Decision::Allow,
            basis: PermissionSource::Rule,
            approval: None,
        }
        .narrow_definition(&definition, &catalog);
        assert_eq!(restriction.decision, Decision::Ask);
        assert_eq!(restriction.basis, PermissionSource::Mode);
        assert_eq!(restriction.approval, Some(ApprovalMode::Auto));
    }

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

    #[test]
    fn human_wait_and_dispatch_pins_do_not_consult_the_system_clock() {
        let step = include_str!("step.rs");
        let ask = include_str!("step/ask.rs");
        let tools = include_str!("../../tools.rs");
        let guard = include_str!("../../tools/guard.rs");
        for (source, start, end) in [
            (ask, "    pub(super) async fn human_reply(", "\n    }\n}"),
            (ask, "    async fn resolve_ask(", "\n    }\n}"),
            (
                step,
                "    async fn dispatch_primary(",
                "    async fn dispatch_guard(",
            ),
            (tools, "    async fn run_unguarded(", "    fn pinned_root("),
            (tools, "    fn pinned_root(", "    async fn run_in_process("),
            (guard, "fn validate_task(", "fn restore_cache("),
        ] {
            let body = source
                .split_once(start)
                .unwrap()
                .1
                .split_once(end)
                .unwrap()
                .0;
            for clock in ["now_ms(", "SystemTime", "expires_at_ms"] {
                assert!(!body.contains(clock), "{start} reintroduced {clock}");
            }
        }
    }

    #[test]
    fn host_reuse_and_session_offers_share_eligibility() {
        use crate::tools::permissions::PermissionMode as M;
        let baseline = HostReuseFacts {
            host: true,
            settings_admitted: true,
            mode: Some(M::Default),
            terminal_mode: Some(ApprovalMode::Request),
            session_scope: true,
            permission: Permission {
                basis: PermissionSource::Mode,
                ..asks(ApprovalMode::Request)
            },
            hook_restriction: None,
            state_target: Some(false),
            epoch: Some(uuid::Uuid::now_v7()),
            source: None,
            grants: 0,
        };
        for (facts, expected) in [
            (baseline, HostReuse::OfferSession),
            (
                HostReuseFacts {
                    mode: None,
                    ..baseline
                },
                HostReuse::OfferSession,
            ),
            (
                HostReuseFacts {
                    source: Some([1; 32]),
                    ..baseline
                },
                HostReuse::Reuse([1; 32]),
            ),
            (
                HostReuseFacts {
                    source: Some([1; 32]),
                    grants: crate::session::MAX_HOST_SESSION_GRANTS,
                    ..baseline
                },
                HostReuse::Reuse([1; 32]),
            ),
            (
                HostReuseFacts {
                    grants: crate::session::MAX_HOST_SESSION_GRANTS,
                    ..baseline
                },
                HostReuse::OnceOnly,
            ),
            (
                HostReuseFacts {
                    mode: Some(M::DontAsk),
                    permission: deny(PermissionSource::Mode),
                    source: Some([1; 32]),
                    ..baseline
                },
                HostReuse::Reuse([1; 32]),
            ),
            (
                HostReuseFacts {
                    mode: Some(M::DontAsk),
                    permission: deny(PermissionSource::Mode),
                    ..baseline
                },
                HostReuse::OnceOnly,
            ),
        ] {
            assert_eq!(decide_host_reuse(&facts), expected);
        }
        for ineligible in [
            HostReuseFacts {
                host: false,
                ..baseline
            },
            HostReuseFacts {
                settings_admitted: false,
                ..baseline
            },
            HostReuseFacts {
                mode: Some(M::Auto),
                ..baseline
            },
            HostReuseFacts {
                mode: Some(M::AcceptEdits),
                ..baseline
            },
            HostReuseFacts {
                mode: Some(M::Plan),
                ..baseline
            },
            HostReuseFacts {
                terminal_mode: None,
                ..baseline
            },
            HostReuseFacts {
                terminal_mode: Some(ApprovalMode::Auto),
                ..baseline
            },
            HostReuseFacts {
                terminal_mode: Some(ApprovalMode::AutoEdits),
                ..baseline
            },
            HostReuseFacts {
                session_scope: false,
                ..baseline
            },
            HostReuseFacts {
                permission: allow(PermissionSource::Rule),
                ..baseline
            },
            HostReuseFacts {
                permission: asks(ApprovalMode::Request),
                ..baseline
            },
            HostReuseFacts {
                permission: deny(PermissionSource::Rule),
                mode: Some(M::DontAsk),
                ..baseline
            },
            HostReuseFacts {
                permission: deny(PermissionSource::Mode),
                ..baseline
            },
            HostReuseFacts {
                permission: deny(PermissionSource::PinnedGrant),
                mode: Some(M::DontAsk),
                ..baseline
            },
            HostReuseFacts {
                hook_restriction: Some(Decision::Ask),
                ..baseline
            },
            HostReuseFacts {
                hook_restriction: Some(Decision::Deny),
                ..baseline
            },
            HostReuseFacts {
                state_target: Some(true),
                ..baseline
            },
            HostReuseFacts {
                state_target: None,
                ..baseline
            },
            HostReuseFacts {
                epoch: None,
                ..baseline
            },
        ] {
            for source in [None, Some([1; 32])] {
                assert_eq!(
                    decide_host_reuse(&HostReuseFacts {
                        source,
                        ..ineligible
                    }),
                    HostReuse::OnceOnly
                );
            }
        }
    }

    #[test]
    fn remembering_is_offered_only_where_a_typed_allow_takes_effect() {
        use super::permission::typed_allow_takes_effect;
        use crate::tools::permissions::PermissionMode as M;
        assert!(typed_allow_takes_effect(Some(M::Default)));
        for mode in [
            None,
            Some(M::AcceptEdits),
            Some(M::Plan),
            Some(M::Auto),
            Some(M::DontAsk),
        ] {
            assert!(!typed_allow_takes_effect(mode), "{mode:?}");
        }
    }

    #[test]
    fn typed_prefix_allow_skips_request_and_auto_edits_prompts_but_keeps_auto_review() {
        let commit = ToolCall::GitCommit {
            message: "m".into(),
            paths: vec!["src/lib.rs".into()],
            staged: false,
        };
        let typed = allow(PermissionSource::TypedPrefixRule);
        for (mode, asks) in [
            (ApprovalMode::Request, false),
            (ApprovalMode::AutoEdits, false),
            (ApprovalMode::Auto, true),
        ] {
            let narrowed = typed.narrow_terminal(Some(mode), &commit);
            assert_eq!(narrowed.decision == Decision::Ask, asks, "{mode:?}");
            if asks {
                assert_eq!(narrowed.approval, Some(ApprovalMode::Auto));
                assert_eq!(narrowed.basis, PermissionSource::Mode);
            } else {
                assert_eq!(narrowed.basis, PermissionSource::TypedPrefixRule);
            }
        }
        let ordinary =
            allow(PermissionSource::Rule).narrow_terminal(Some(ApprovalMode::Request), &commit);
        assert_eq!(ordinary.decision, Decision::Ask);
        let ask = Permission {
            decision: Decision::Ask,
            basis: PermissionSource::TypedPrefixRule,
            approval: Some(ApprovalMode::Request),
        }
        .narrow_terminal(Some(ApprovalMode::AutoEdits), &commit);
        assert_eq!(
            (ask.decision, ask.approval),
            (Decision::Ask, Some(ApprovalMode::Request))
        );
        let deny = deny(PermissionSource::TypedPrefixRule)
            .narrow_terminal(Some(ApprovalMode::Request), &commit);
        assert_eq!(deny.decision, Decision::Deny);
    }

    fn allow(basis: PermissionSource) -> Permission {
        Permission {
            decision: Decision::Allow,
            basis,
            approval: None,
        }
    }

    fn asks(mode: ApprovalMode) -> Permission {
        Permission {
            decision: Decision::Ask,
            basis: PermissionSource::Rule,
            approval: Some(mode),
        }
    }

    fn deny(basis: PermissionSource) -> Permission {
        Permission {
            decision: Decision::Deny,
            basis,
            approval: None,
        }
    }
}

use super::children::ChildWork;
use super::stop::{StepEnd, StopCause};
use super::{ActivePause, ActiveSegment, RunLoop, TOOL_CLEANUP_OVERHEAD};
use crate::engine::hints::HintPublisher;
use crate::engine::ledger::{ActiveDeadlineCause, Admission};
use crate::engine::{EngineError, RunCancellation, STORE_ACTION_OVERHEAD};
use crate::provider::{ProviderRequest, ProviderTurn};
use crate::session::Event;
use crate::tools::permissions::{Decision, PermissionSource, PermissionVerdict};
use crate::tools::types::CommandResult;
use crate::tools::{
    ActiveActionRequest, ApprovalMode, ApprovalReply, ApprovalWithdrawal, CacheUpdate,
    EffectIntent, ToolCall, ToolContext, ToolDisposition, ToolError, ToolObservation, ToolRequest,
    ToolRuntime, ToolStep,
};
use std::time::Duration;
use tokio::time::Instant;

mod ask;
mod checkpoint;
mod dispatch;
pub(super) mod hooks;
mod intent;
mod permission;
mod review;
mod skills;

use ask::{AskOptions, AskSite, Resolution, unguarded_gate};
use checkpoint::{CHECKPOINT_GO_MARGIN, CHECKPOINT_WAIT};
use dispatch::{
    AfterAsk, Dispatch, DispatchFacts, DispatchSite, GuardCompletion, NotDispatched, Unguarded,
    Verdict, decide_dispatch, decide_queue_pause, not_dispatched, undispatched,
};
use intent::{
    active_window, execute_cancellable, settle_cancelled, tool_action_bound, tool_action_ms,
};
use permission::{Permission, RuleSave, effective_permission_mode, remember_candidate};
use review::ToolReviewDecision;
pub(super) use skills::{DefinitionInitialization, SkillInitialization};

use hooks::{HookFold, HookStop};

#[cfg(test)]
use super::stop::SizingRefusal;
#[cfg(test)]
use intent::sizing_deadline_refusal;

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

struct BeforeDispatch {
    site: DispatchSite,
    intent: EffectIntent,
    permission: Permission,
    hooks: HookFold,
    host_permission: permission::HostPermission,
    timeout_limit: Option<(u32, u32)>,
    goal_allows: bool,
    allow_auto_edits: bool,
}

struct DispatchResolution {
    verdict: Verdict,
    save: Option<RuleSave>,
    decision: Decision,
    source: PermissionSource,
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

    async fn before_dispatch(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        intent: EffectIntent,
        permission: Permission,
        narrowable: bool,
    ) -> Result<Admission<Verdict>, EngineError> {
        let phase = self
            .hook_gate(grant, runtime, intent, permission, narrowable)
            .await?;
        let facts = self.dispatch_facts(grant, runtime, &phase).await?;
        let (approval, save) = match self
            .dispatch_approval(grant, runtime, &phase, &facts)
            .await?
        {
            Admission::Reserved(approval) => approval,
            Admission::Exhausted => return Ok(Admission::Exhausted),
        };
        let decided = self.dispatch_decision(grant, &facts, approval);
        let resolution = self.dispatch_verdict(&phase, decided, save)?;
        self.record_permission_verdict(grant, runtime, &phase, &facts, &resolution)
            .await?;
        self.save_remembered_rule(runtime, phase.site, resolution.verdict, resolution.save)
            .map(Admission::Reserved)
    }

    async fn hook_gate(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        intent: EffectIntent,
        permission: Permission,
        narrowable: bool,
    ) -> Result<BeforeDispatch, EngineError> {
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
                // Boxed like other Tool futures to avoid inflating each agent step.
                Box::pin(self.pre_tool_use_hooks(grant, runtime, config, &intent, narrowable))
                    .await?
            }
            _ => HookFold::none(),
        };
        let permission =
            permission.narrow_hooks(hooks.restriction, config.map(|config| config.mode()));
        let host_permission = self.host_permission(runtime, &intent, permission, &hooks)?;
        Ok(BeforeDispatch {
            site,
            intent,
            permission: host_permission.permission,
            hooks,
            host_permission,
            timeout_limit,
            goal_allows,
            allow_auto_edits,
        })
    }

    async fn dispatch_facts<'a>(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        phase: &'a BeforeDispatch,
    ) -> Result<DispatchFacts<'a>, EngineError> {
        let intent = &phase.intent;
        Ok(DispatchFacts {
            site: phase.site,
            permission: phase.permission,
            hooks: &phase.hooks,
            timeout_limit: phase.timeout_limit,
            host: intent.call.is_host(),
            host_unpinned: intent.call.is_host() && intent.host.is_none(),
            human_only_unavailable: (intent.call.is_host() && self.controls.approvals.is_none())
                || self.controls.approvals.is_some_and(|approvals| {
                    !approvals.admits_human_only(
                        &intent.call,
                        phase.hooks.restriction == Some(Decision::Ask),
                    )
                }),
            unguarded: match grant {
                Grant::Primary { .. } if intent.call.is_unguarded() => Some(Unguarded {
                    gate: unguarded_gate(runtime, intent).await.map(|mut prompt| {
                        prompt.remember = phase.host_permission.remember;
                        prompt.prefix =
                            phase.host_permission.prefix && runtime.host_prefix_candidate(intent);
                        prompt
                    }),
                }),
                Grant::Primary { .. } | Grant::Researcher { .. } => None,
            },
            goal_allows: phase.goal_allows,
            runtime_allows: runtime.allows(&intent.call),
            can_start_provider_call: self.ledger().can_start_provider_call(),
            guard_site: intent.call.facts().site == crate::tools::catalog::Site::Guard,
            interactive: self.controls.approvals.is_some(),
        })
    }

    async fn dispatch_approval(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        phase: &BeforeDispatch,
        facts: &DispatchFacts<'_>,
    ) -> Result<Admission<(Option<Resolution>, Option<RuleSave>)>, EngineError> {
        let config = runtime.permissions();
        let remember_prefix = remember_candidate(
            grant,
            config.map(|config| config.mode()),
            phase.permission,
            &phase.hooks,
            &phase.intent.call,
        );
        let approval = match facts.ask() {
            Some(mode) => Box::pin(self.ask(
                grant,
                &phase.intent,
                mode,
                AskOptions {
                    unguarded_prompt: facts.unguarded_prompt(),
                    allow_auto_edits: phase.allow_auto_edits
                        && phase.hooks.restriction != Some(Decision::Ask),
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
            self.defer_approval_withdrawal(phase.intent.id, withdrawal)?;
        }
        let save = approval.as_ref().and_then(|resolution| resolution.save);
        Ok(Admission::Reserved((approval, save)))
    }

    fn dispatch_decision<'a>(
        &self,
        grant: &Grant<'_>,
        facts: &DispatchFacts<'a>,
        approval: Option<Resolution>,
    ) -> dispatch::DispatchDecision<'a> {
        decide_dispatch(
            facts,
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
        )
    }

    fn dispatch_verdict(
        &self,
        phase: &BeforeDispatch,
        decided: dispatch::DispatchDecision<'_>,
        save: Option<RuleSave>,
    ) -> Result<DispatchResolution, EngineError> {
        let decision = decided.decision;
        let source = decided.source;
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
        let BeforeDispatch { site, intent, .. } = phase;
        let verdict = match outcome {
            Ok(()) => Verdict::Dispatch {
                intent: intent.clone(),
                queue_pause,
            },
            Err(refusal) => {
                if matches!(refusal.reason, NotDispatched::Paused) {
                    // A pause during review or approval must stop the Run before dispatch.
                    self.controls.cancellation.cancel();
                }
                if let Some(pause) = queue_pause {
                    self.settle_active_pause(pause, None)?;
                }
                self.refused(*site, intent.clone(), refusal)
            }
        };
        Ok(DispatchResolution {
            verdict,
            save,
            decision,
            source,
        })
    }

    async fn record_permission_verdict(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        phase: &BeforeDispatch,
        facts: &DispatchFacts<'_>,
        resolution: &DispatchResolution,
    ) -> Result<(), EngineError> {
        let unguarded = phase.intent.call.is_unguarded();
        let git_write = phase.intent.call.is_git() && phase.intent.call.facts().mutates;
        let mode = effective_permission_mode(runtime.permissions(), grant);
        if mode.is_none() && !unguarded && !(matches!(grant, Grant::Primary { .. }) && git_write) {
            return Ok(());
        }
        let approved = matches!(resolution.verdict, Verdict::Dispatch { .. });
        // An approving review that did not lead to dispatch is not an AI approval.
        let source = if !approved && resolution.source == PermissionSource::AiReview {
            PermissionSource::UnavailableApproval
        } else {
            resolution.source
        };
        self.append(Event::ToolPermissionDecided {
            run_id: self.run_id,
            agent_run_id: phase.intent.agent_run_id,
            verdict: PermissionVerdict {
                host: phase
                    .host_permission
                    .verdict(approved, unguarded, resolution.save),
                intent_digest: phase.intent.digest(),
                decision: resolution.decision,
                mode,
                basis: facts.permission.basis,
                source,
                approved,
                hook_restriction: phase.hooks.restriction,
                hooks: phase.hooks.count,
                narrowed: matches!(resolution.verdict, Verdict::Narrow(..)),
            },
        })
        .await
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

#[derive(Debug, Eq, PartialEq)]
pub(super) enum HookDenial {
    ActiveTime,
    DelegationDeadline,
    Uncertain,
    Requested,
    Narrowed(String),
    Denied(String),
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

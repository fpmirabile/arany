use super::children::ChildWork;
use super::stop::{SizingRefusal, StepEnd, StopCause};
use super::{
    ActivePause, ActiveSegment, CallResult, RunLoop, TOOL_CLEANUP_OVERHEAD, TOOL_REVIEW_DEADLINE,
    call_record,
};
use crate::engine::hints::HintPublisher;
use crate::engine::ledger::{Action, duration_ms};
use crate::engine::{EngineError, RunCancellation, STORE_ACTION_OVERHEAD};
use crate::provider::{
    AgentPhase, PromptRole, ProviderOutcome, ProviderRequest, ProviderResponse, ProviderTurn,
    RunInput,
};
use crate::session::{CollaborationPolicy, Event};
use crate::tools::permissions::{Decision, PermissionSource, PermissionVerdict};
use crate::tools::types::{CommandChangeset, CommandResult, MAX_TOOL_RESULT_BYTES};
use crate::tools::{
    ActiveActionRequest, ActiveToolWindow, ApprovalMode, ApprovalReply, CacheUpdate, EffectIntent,
    ToolCall, ToolContext, ToolDisposition, ToolError, ToolObservation, ToolRequest,
    ToolResourceClass, ToolRuntime, ToolStep, UnguardedPrompt,
};
use std::time::Duration;
use tokio::time::Instant;

pub(super) mod hooks;

use hooks::{HookFold, HookStop};

pub(super) enum Grant<'s> {
    Primary {
        request: &'s mut ProviderRequest,
        review_allowed: bool,
    },
    Researcher {
        child: &'s mut ChildWork,
        deadline: Instant,
    },
}

impl Grant<'_> {
    pub(super) fn deadline(&self) -> Option<Instant> {
        match self {
            Self::Primary { .. } => None,
            Self::Researcher { deadline, .. } => Some(*deadline),
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

#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolReviewDecision {
    Approve,
    Deny,
}

#[must_use]
enum Verdict {
    Dispatch {
        intent: EffectIntent,
        queue_pause: Option<ActivePause>,
    },
    Deny(Answer),
    Narrow(Answer, ToolCall),
}

const MAX_NARROWINGS_PER_STEP: usize = 1;
const CHECKPOINT_WAIT: Duration = Duration::from_secs(3);
const CHECKPOINT_GO_MARGIN: Duration = Duration::from_secs(1);

enum Resolver<'a> {
    AiReview { objective: &'a str },
    Human,
}

enum AskSite {
    Primary {
        unguarded_prompt: Option<UnguardedPrompt>,
        allow_auto_edits: bool,
        remember_prefix: Option<crate::tools::permissions::TypedGitPrefix>,
    },
    Delegated {
        deadline: Instant,
    },
}

struct AskOptions {
    unguarded_prompt: Option<UnguardedPrompt>,
    allow_auto_edits: bool,
    remember_prefix: Option<crate::tools::permissions::TypedGitPrefix>,
}

struct Resolution {
    approved: bool,
    source: PermissionSource,
    active_time_exhausted: bool,
    save: Option<RuleSave>,
}

#[derive(Clone, Copy)]
enum RuleSave {
    Prefix(crate::tools::permissions::TypedGitPrefix),
    ExactCommand,
}

#[derive(Clone, Copy)]
struct Permission {
    decision: Decision,
    basis: PermissionSource,
    approval: Option<ApprovalMode>,
}

impl Permission {
    fn narrow_hooks(
        mut self,
        restriction: Option<Decision>,
        mode: Option<crate::tools::permissions::PermissionMode>,
    ) -> Self {
        match restriction {
            Some(Decision::Deny) => {
                self.decision = Decision::Deny;
                self.basis = PermissionSource::Hook;
                self.approval = None;
            }
            // dontAsk has no human to ask, so a hook ask is the narrower deny.
            Some(Decision::Ask)
                if self.decision != Decision::Deny
                    && mode == Some(crate::tools::permissions::PermissionMode::DontAsk) =>
            {
                self.decision = Decision::Deny;
                self.basis = PermissionSource::Mode;
                self.approval = None;
            }
            Some(Decision::Ask) if self.decision != Decision::Deny => {
                if self.decision == Decision::Allow {
                    self.basis = PermissionSource::Hook;
                }
                self.decision = Decision::Ask;
                self.approval = Some(ApprovalMode::Request);
            }
            _ => {}
        }
        self
    }

    fn narrow_definition(
        mut self,
        definition: &crate::tools::definitions::AgentDefinition,
        call: &ToolCall,
    ) -> Self {
        self.decision = self.decision.narrow(definition.decision(call));
        if let Some(mode) = definition.permission_mode {
            let (restriction, _, required_approval) =
                crate::tools::permissions::PermissionConfig::evaluate_mode(mode, call);
            if restriction == Decision::Deny && self.decision != Decision::Deny
                || restriction == Decision::Ask && self.decision == Decision::Allow
            {
                self.basis = PermissionSource::Mode;
            }
            self.decision = self.decision.narrow(restriction);
            self.approval = match (self.approval, required_approval) {
                (Some(ApprovalMode::Request), _) | (_, Some(ApprovalMode::Request)) => {
                    Some(ApprovalMode::Request)
                }
                (existing, required) => existing.or(required),
            };
            if mode == crate::tools::permissions::PermissionMode::DontAsk
                && self.decision == Decision::Ask
            {
                self.decision = Decision::Deny;
                self.basis = PermissionSource::Mode;
            }
        }
        if self.decision == Decision::Deny {
            self.approval = None;
        }
        self
    }

    fn narrow_terminal(mut self, mode: Option<ApprovalMode>, call: &ToolCall) -> Self {
        if self.decision != Decision::Deny
            && self.basis != PermissionSource::ExactCommandRule
            && let Some(mode) = mode.filter(|mode| {
                // A user-owned typed allow is the prompt-free path in Request and Auto-edits; Auto keeps its review.
                mode.asks(call)
                    && !(self.basis == PermissionSource::TypedPrefixRule
                        && self.decision == Decision::Allow
                        && *mode != ApprovalMode::Auto)
            })
        {
            if self.decision == Decision::Allow {
                self.basis = PermissionSource::Mode;
            }
            self.decision = self.decision.narrow(Decision::Ask);
            self.approval = Some(
                if self.approval == Some(ApprovalMode::Request) || mode != ApprovalMode::Auto {
                    ApprovalMode::Request
                } else {
                    ApprovalMode::Auto
                },
            );
        }
        self
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
            let intent =
                match self.sized_intent(&grant, runtime, &step, call.clone(), &source_call)? {
                    Ok(intent) => intent,
                    Err(refusal @ SizingRefusal::DelegationDeadline) => {
                        return Ok(StepEnd::Stopped(
                            match self
                                .record_delegation_deadline(&grant, runtime, call)
                                .await?
                            {
                                Ok(()) => refusal.into(),
                                Err(admission) => admission.into(),
                            },
                        ));
                    }
                    Err(refusal @ SizingRefusal::ActiveTime) => {
                        self.active_time_floor
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                        return Ok(StepEnd::Stopped(refusal.into()));
                    }
                    Err(refusal @ SizingRefusal::ContextFull) => {
                        return Ok(StepEnd::Stopped(refusal.into()));
                    }
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
        if matches!(grant, Grant::Primary { .. }) {
            for (index, observation) in step.observations.iter().enumerate() {
                let proposals = self.hook_rerun_feedback(observation.intent.digest())?;
                if proposals.is_empty() {
                    continue;
                }
                let feedback = step.hook_feedback.entry(index).or_default();
                for proposal in proposals {
                    if feedback.iter().map(String::len).sum::<usize>() + proposal.len()
                        <= crate::tools::permissions::MAX_HOOK_FEEDBACK_BYTES
                    {
                        feedback.push(proposal);
                    }
                }
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

    fn sized_intent(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        step: &ToolStep,
        call: ToolCall,
        post_call: &ToolCall,
    ) -> Result<Result<EffectIntent, SizingRefusal>, EngineError> {
        let intent = match grant {
            Grant::Primary { .. } => self.primary_intent(runtime, call)?,
            Grant::Researcher { child, deadline } => {
                let Some(mut intent) =
                    self.researcher_intent(runtime, child.agent_run_id, call, *deadline)
                else {
                    return Ok(Err(SizingRefusal::DelegationDeadline));
                };
                if child.editor {
                    self.shape_worker_intent(&mut intent, child.agent_run_id)?;
                    let remaining = deadline
                        .saturating_duration_since(Instant::now())
                        .saturating_sub(
                            runtime.reservation_overhead(&intent.call)
                                + TOOL_CLEANUP_OVERHEAD
                                + STORE_ACTION_OVERHEAD,
                        );
                    let remaining =
                        remaining.saturating_sub(self.hook_time(runtime, &intent.call, remaining));
                    intent.limits.runtime_ms = intent.limits.runtime_ms.min(
                        u32::try_from(
                            duration_ms(remaining)
                                / if intent.limits.cache_bytes > 0 { 6 } else { 4 },
                        )
                        .unwrap_or(u32::MAX),
                    );
                    intent.limits.cache_transfer_ms = if intent.limits.cache_bytes > 0 {
                        intent.limits.runtime_ms
                    } else {
                        0
                    };
                    if intent.limits.runtime_ms < 1_000 {
                        return Ok(Err(SizingRefusal::DelegationDeadline));
                    }
                    intent.expires_at_ms =
                        crate::tools::now_ms().saturating_add(u64::from(intent.limits.runtime_ms));
                }
                Some(intent)
            }
        };
        let Some(mut intent) = intent else {
            return Ok(Err(SizingRefusal::ActiveTime));
        };
        // The PreToolUse hooks run before dispatch, so the authorization outlives them.
        intent.expires_at_ms = intent
            .expires_at_ms
            .saturating_add(self.hook_admission(runtime, &intent.call, post_call).1);
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
        } else {
            if capacity < MAX_TOOL_RESULT_BYTES {
                return Ok(Err(SizingRefusal::ContextFull));
            }
            MAX_TOOL_RESULT_BYTES as u32
        };
        Ok(Ok(intent))
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

    pub(super) fn primary_intent(
        &self,
        runtime: &ToolRuntime,
        call: ToolCall,
    ) -> Result<Option<EffectIntent>, EngineError> {
        let command = matches!(call, ToolCall::Command { .. });
        let timeout_limit = self.ledger().command_timeout_limit(&call)?;
        let mut intent = runtime.intent(self.run_id, self.primary_id, call);
        let (resource_class, requested_runtime_ms, resource_settings) =
            self.ledger().requested_command_resources()?;
        if command {
            intent.limits.resource_class = resource_class;
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
        intent.expires_at_ms =
            crate::tools::now_ms().saturating_add(u64::from(intent.limits.runtime_ms));
        Ok(Some(intent))
    }

    pub(super) fn researcher_intent(
        &self,
        runtime: &ToolRuntime,
        agent_run_id: crate::session::AgentRunId,
        call: ToolCall,
        deadline: Instant,
    ) -> Option<EffectIntent> {
        let mut intent = runtime.intent(self.run_id, agent_run_id, call);
        let available = deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(
                runtime.reservation_overhead(&intent.call)
                    + TOOL_CLEANUP_OVERHEAD
                    + STORE_ACTION_OVERHEAD,
            );
        let available = available.saturating_sub(self.hook_time(runtime, &intent.call, available));
        // Half for the Guard capacity wait (it ends at `expires_at_ms`), half for the run itself.
        let runtime_ms = intent
            .limits
            .runtime_ms
            .min(u32::try_from(duration_ms(available) / 2).unwrap_or(u32::MAX));
        if runtime_ms < 1_000 {
            return None;
        }
        intent.limits.runtime_ms = runtime_ms;
        intent.expires_at_ms = crate::tools::now_ms().saturating_add(u64::from(runtime_ms));
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
        let mut permission = self.permission(grant, runtime, &intent.call);
        if intent.call.is_unguarded()
            && permission.decision != Decision::Deny
            && self.ledger().view(|view| {
                view.runs
                    .last()
                    .and_then(|run| run.unguarded_failure(&intent.call))
                    .is_some_and(|(_, hook)| hook)
            })?
        {
            permission.decision = Decision::Ask;
            permission.approval = Some(ApprovalMode::Request);
            if permission.basis == PermissionSource::ExactCommandRule {
                permission.basis = PermissionSource::Mode;
            }
        }
        let (hook_events, pre_ms, post_ms) = self.hook_admission(runtime, &intent.call, post_call);
        if let Err(refusal) = self
            .admit_tool(grant, runtime, &intent, permission, hook_events, narrowable)
            .await?
        {
            return Ok(Err(refusal.into()));
        }
        let agent_run_id = intent.agent_run_id;
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
        let verdict = match self
            .before_dispatch(grant, runtime, intent, permission, narrowable)
            .await
        {
            Ok(verdict) => verdict,
            Err(EngineError::GoalBudgetExhausted) => return Ok(Err(StopCause::ActiveTime)),
            Err(error) => return Err(error),
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
                Grant::Researcher { child, deadline } => {
                    let cancellation = RunCancellation::new();
                    let mut parent = self.controls.cancellation.clone();
                    let mut timed_out = false;
                    let execution = async {
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
                        () = tokio::time::sleep_until(*deadline) => { timed_out = true; cancellation.cancel(); execution.await? },
                        () = parent.cancelled() => { cancellation.cancel(); execution.await? },
                    };
                    if timed_out && observation.disposition == ToolDisposition::Cancelled {
                        observation.disposition = ToolDisposition::Limit;
                        observation.limit_cause = Some(crate::tools::LimitCause::Timeout);
                    }
                    Answer {
                        checkpoint: None,
                        observation,
                        stop: None,
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
        let mut sealed_data = false;
        let tail = if worker {
            let chunks = self.worker_call_chunks(intent.agent_run_id, &intent.call)?;
            sealed_data = chunks > 0;
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
        let admitted = if sealed_data {
            self.journal
                .admit_sealed_data(self.session_id, tail, observations)
                .await
        } else {
            self.journal
                .admit(self.session_id, tail, observations)
                .await
        };
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

    fn plan_allows(&self, call: &ToolCall) -> Result<bool, EngineError> {
        Ok(self.primary_lease_allows(call)?.is_ok()
            && (matches!(
                call,
                ToolCall::ApplyChangeset {
                    source: crate::session::worker::ChangesetSource::WorkerHandover { .. }
                }
            ) || self.ledger().allows_tool(call)))
    }

    pub(super) fn primary_lease_allows(
        &self,
        call: &ToolCall,
    ) -> Result<Result<(), super::ProposalRefusalCause>, EngineError> {
        let allowed = self.ledger().view(|v| {
            v.runs.last().is_some_and(|r| {
                let leases = r
                    .agents
                    .iter()
                    .filter_map(|a| a.assignment.as_ref())
                    .filter(|w| w.released.is_none())
                    .flat_map(|w| &w.pins.request.lease)
                    .collect::<Vec<_>>();
                match call {
                    ToolCall::ApplyChangeset {
                        source: crate::worker::ChangesetSource::WorkerHandover { .. },
                    } => true,
                    ToolCall::Write { path, .. }
                    | ToolCall::Edit { path, .. }
                    | ToolCall::Mkdir { path } => {
                        !leases.iter().any(|l| crate::worker::overlaps(l, path))
                    }
                    _ => !call.is_process() || leases.is_empty(),
                }
            })
        })?;
        Ok(if allowed {
            Ok(())
        } else {
            Err(super::ProposalRefusalCause::LeaseOverlap)
        })
    }

    fn review_will_run(
        &self,
        runtime: &ToolRuntime,
        call: &ToolCall,
        review_allowed: bool,
        permission: Permission,
    ) -> Result<bool, EngineError> {
        Ok(self.ledger().command_timeout_limit(call)?.is_none()
            && self.plan_allows(call)?
            && runtime.allows(call)
            && permission
                .narrow_terminal(self.terminal_mode(), call)
                .approval
                == Some(ApprovalMode::Auto)
            && self.ledger().can_start_provider_call()
            && review_allowed
            && self.ledger().admits(Action::Review))
    }

    fn terminal_mode(&self) -> Option<ApprovalMode> {
        self.controls.approvals.map(|approvals| approvals.mode())
    }

    fn permission(&self, grant: &Grant<'_>, runtime: &ToolRuntime, call: &ToolCall) -> Permission {
        // Engine-owned definition parsing and worker preparation are never model Tools, so rules and modes do not apply; each runs only in its fixed grant.
        if call.is_internal() {
            return Permission {
                decision: Decision::Allow,
                basis: PermissionSource::Mode,
                approval: None,
            };
        }
        let (mut decision, mut basis, mut approval) = runtime.permissions().map_or(
            (Decision::Allow, PermissionSource::Mode, None),
            |config| {
                if let ToolCall::ApplyChangeset {
                    source: crate::session::worker::ChangesetSource::WorkerHandover { data, .. },
                } = call
                {
                    self.ledger()
                        .view(|view| {
                            view.runs
                                .last()
                                .and_then(|run| {
                                    run.workers
                                        .data
                                        .iter()
                                        .find(|sealed| &sealed.reference == data)
                                })
                                .and_then(|sealed| match &sealed.facts {
                                    crate::session::worker::WorkerDataFacts::Changes(paths) => {
                                        Some(config.evaluate_worker_paths(call, Some(paths)))
                                    }
                                    _ => None,
                                })
                        })
                        .ok()
                        .flatten()
                        .unwrap_or((Decision::Deny, PermissionSource::PinnedGrant, None))
                } else {
                    config.evaluate_skill_file(call, runtime.skill_workspace_path(call).as_deref())
                }
            },
        );
        let unguarded = call.is_unguarded();
        let researcher = matches!(grant, Grant::Researcher { .. });
        let git_write = call.is_git() && call.facts().mutates;
        if (runtime.permissions().is_some() || unguarded || git_write)
            && (!runtime.allows(call)
                || researcher
                    && !call.is_child_research()
                    && !matches!(grant, Grant::Researcher { child, .. } if child.editor && child.definition.as_ref().is_some_and(|definition| definition.allows_worker(call))))
        {
            decision = decision.narrow(Decision::Deny);
            basis = PermissionSource::PinnedGrant;
            approval = None;
        }
        if unguarded && decision != Decision::Deny {
            // Only a human or an exact project rule approves a host run; no mode or review does.
            decision = Decision::Ask;
            approval = Some(ApprovalMode::Request);
            if basis != PermissionSource::Rule && runtime.matches_exact_command_rule(call) {
                decision = Decision::Allow;
                basis = PermissionSource::ExactCommandRule;
                approval = None;
            }
        }
        let permission = Permission {
            decision,
            basis,
            approval,
        };
        match grant {
            Grant::Researcher { child, .. } => {
                child.definition.as_ref().map_or(permission, |definition| {
                    permission.narrow_definition(definition, call)
                })
            }
            Grant::Primary { .. } => permission,
        }
    }

    async fn before_dispatch(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        intent: EffectIntent,
        permission: Permission,
        narrowable: bool,
    ) -> Result<Verdict, EngineError> {
        let config = runtime.permissions();
        let site = grant.site();
        let allow_auto_edits = permission.decision == Decision::Allow
            && self.terminal_mode() == Some(ApprovalMode::Request)
            && intent.call.facts().mutates;
        let permission = permission.narrow_terminal(self.terminal_mode(), &intent.call);
        let timeout_limit = self.ledger().command_timeout_limit(&intent.call)?;
        let hooks = match config {
            Some(config)
                if !intent.call.is_internal()
                    && permission.decision != Decision::Deny
                    && timeout_limit.is_none()
                    && match grant {
                        Grant::Primary { .. } => self.plan_allows(&intent.call)?,
                        Grant::Researcher { .. } => self.ledger().allows_tool(&intent.call),
                    } =>
            {
                // Boxed like the other Tool futures so the hook loop does not inflate every agent step.
                Box::pin(self.pre_tool_use_hooks(grant, runtime, config, &intent, narrowable))
                    .await?
            }
            _ => HookFold::none(),
        };
        let permission =
            permission.narrow_hooks(hooks.restriction, config.map(|config| config.mode()));
        let facts = DispatchFacts {
            site,
            permission,
            hooks: &hooks,
            timeout_limit,
            human_only_unavailable: self.controls.approvals.is_some_and(|approvals| {
                !approvals.admits_human_only(&intent.call, hooks.restriction == Some(Decision::Ask))
            }),
            unguarded: match grant {
                Grant::Primary { request, .. } if intent.call.is_unguarded() => {
                    Some(self.unguarded(runtime, request, &intent)?)
                }
                Grant::Primary { .. } | Grant::Researcher { .. } => None,
            },
            goal_allows: intent.call.is_internal()
                || match grant {
                    Grant::Primary { .. } => self.plan_allows(&intent.call)?,
                    Grant::Researcher { .. } => self.ledger().allows_tool(&intent.call),
                },
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
            Some(mode) => Some(
                Box::pin(self.ask(
                    grant,
                    &intent,
                    mode,
                    AskOptions {
                        unguarded_prompt: facts.unguarded_prompt(),
                        allow_auto_edits: allow_auto_edits
                            && hooks.restriction != Some(Decision::Ask),
                        remember_prefix,
                    },
                ))
                .await?,
            ),
            None => None,
        };
        let save = approval.as_ref().and_then(|resolution| resolution.save);
        let decided = decide_dispatch(
            &facts,
            &AfterAsk {
                approval,
                paused: self.pause_requested(),
                can_start_provider_call: self.ledger().can_start_provider_call(),
                cancelled: self.controls.cancellation.is_cancelled(),
                deadline_reached: grant
                    .deadline()
                    .is_some_and(|deadline| Instant::now() >= deadline),
            },
        );
        // An in-process read waits for no Guard capacity, so it reserves its action directly.
        let (outcome, queue_pause) = match decided.outcome {
            Ok(Dispatch::QueuePause) => match self.reserve_active_pause() {
                Ok(pause) => (
                    decide_queue_pause(true, self.pause_requested()),
                    Some(pause),
                ),
                Err(EngineError::GoalBudgetExhausted) => {
                    (decide_queue_pause(false, self.pause_requested()), None)
                }
                Err(error) => return Err(error),
            },
            Ok(Dispatch::Direct) => (Ok(()), None),
            Err(refusal) => (Err(refusal), None),
        };
        let agent_run_id = intent.agent_run_id;
        let digest = intent.digest();
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
        let local_git_write = matches!(grant, Grant::Primary { .. }) && git_write;
        if mode.is_some() || unguarded || local_git_write {
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
        let verdict = self.save_remembered_rule(runtime, site, verdict, save)?;
        Ok(verdict)
    }

    // The rule is saved only after the human verdict is committed; a failed save refuses dispatch.
    fn save_remembered_rule(
        &self,
        runtime: &ToolRuntime,
        site: DispatchSite,
        verdict: Verdict,
        save: Option<RuleSave>,
    ) -> Result<Verdict, EngineError> {
        Ok(match (verdict, save) {
            (
                Verdict::Dispatch {
                    intent,
                    queue_pause,
                },
                Some(save),
            ) => {
                let saved = match save {
                    RuleSave::Prefix(prefix) => runtime.record_typed_prefix_rule(prefix),
                    RuleSave::ExactCommand => runtime.record_exact_command_rule(&intent.call),
                };
                if saved.is_ok() {
                    Verdict::Dispatch {
                        intent,
                        queue_pause,
                    }
                } else {
                    if let Some(pause) = queue_pause {
                        self.settle_active_pause(pause, None)?;
                    }
                    // The rule may already be installed, so the outcome is unknown and the Run stops.
                    let refusal = if matches!(saved, Err(crate::tools::ToolError::Uncertain)) {
                        refuse(NotDispatched::SaveUnconfirmed, Some(StopCause::Uncertain))
                    } else {
                        refuse(NotDispatched::UnsavedRule, None)
                    };
                    self.refused(site, intent, refusal)
                }
            }
            (verdict, _) => verdict,
        })
    }

    fn unguarded(
        &self,
        runtime: &ToolRuntime,
        request: &ProviderRequest,
        intent: &EffectIntent,
    ) -> Result<Unguarded, EngineError> {
        let hook_failure = self.ledger().view(|view| {
            view.runs
                .last()
                .and_then(|run| run.unguarded_failure(&intent.call))
                .filter(|(_, hook)| *hook)
                .map(|(observation, _)| observation.clone())
        })?;
        Ok(Unguarded {
            late_hook_failure: hook_failure.is_some(),
            gate: unguarded_gate(runtime, request, intent, hook_failure.as_ref()),
        })
    }

    async fn ask(
        &self,
        grant: &Grant<'_>,
        intent: &EffectIntent,
        mode: ApprovalMode,
        options: AskOptions,
    ) -> Result<Resolution, EngineError> {
        match grant {
            Grant::Primary {
                request,
                review_allowed,
            } => {
                let resolvers;
                let chain = if mode == ApprovalMode::Auto
                    && *review_allowed
                    && self.ledger().admits(Action::Review)
                {
                    resolvers = [
                        Resolver::AiReview {
                            objective: &request.input.objective,
                        },
                        Resolver::Human,
                    ];
                    &resolvers[..]
                } else {
                    &[Resolver::Human]
                };
                self.resolve_ask(
                    intent,
                    chain,
                    AskSite::Primary {
                        unguarded_prompt: options.unguarded_prompt,
                        allow_auto_edits: options.allow_auto_edits,
                        remember_prefix: options.remember_prefix,
                    },
                )
                .await
            }
            Grant::Researcher { deadline, .. } => {
                let objective =
                    if mode == ApprovalMode::Auto && self.ledger().admits(Action::Review) {
                        Some(
                            self.ledger()
                                .view(|view| {
                                    self.goal_id
                                        .and_then(|id| view.goal(id))
                                        .map(|goal| goal.objective.clone())
                                })?
                                .ok_or(EngineError::InvalidHistory)?,
                        )
                    } else {
                        None
                    };
                let resolvers;
                let chain = if let Some(objective) = &objective {
                    resolvers = [Resolver::AiReview { objective }, Resolver::Human];
                    &resolvers[..]
                } else {
                    &[Resolver::Human]
                };
                self.resolve_ask(
                    intent,
                    chain,
                    AskSite::Delegated {
                        deadline: *deadline,
                    },
                )
                .await
            }
        }
    }

    fn refused(&self, site: DispatchSite, intent: EffectIntent, refusal: Refusal<'_>) -> Verdict {
        let approval_stage = site == DispatchSite::Primary && refusal.reason.decided_at_approval();
        if approval_stage {
            self.clear_tool_queue();
        }
        // The primary re-reads active time after the ToolFinished it journals.
        let exhausted = match (refusal.reason.active_time_effect(), site) {
            (ActiveTimeEffect::Exhausted, DispatchSite::Primary | DispatchSite::Researcher) => {
                Some(true)
            }
            (ActiveTimeEffect::Recheck, DispatchSite::Primary) => Some(false),
            (ActiveTimeEffect::Recheck, DispatchSite::Researcher)
            | (ActiveTimeEffect::Untouched, DispatchSite::Primary | DispatchSite::Researcher) => {
                None
            }
        };
        let active_time = exhausted.map(|exhausted| ActiveTime {
            segment: None,
            exhausted,
        });
        let answer = Answer {
            checkpoint: None,
            observation: not_dispatched(intent, &refusal.reason),
            stop: refusal.stop,
            active_time,
        };
        match refusal.narrowed {
            Some(call) => Verdict::Narrow(answer, call.clone()),
            None => Verdict::Deny(answer),
        }
    }

    async fn human_reply(
        &self,
        approvals: &crate::tools::ToolApprovals,
        intent: &EffectIntent,
        prompt: Option<UnguardedPrompt>,
        allow_auto_edits: bool,
        remember_prefix: Option<crate::tools::permissions::TypedGitPrefix>,
    ) -> ApprovalReply {
        let mut cancellation = self.controls.cancellation.clone();
        let expires = intent.expires_at_ms.saturating_sub(crate::tools::now_ms());
        let reply = tokio::select! {
            biased;
            () = cancellation.cancelled() => ApprovalReply::Deny,
            result = tokio::time::timeout(Duration::from_millis(expires), async {
                match prompt.clone() {
                    Some(prompt) => approvals.ask_unguarded(intent, prompt).await,
                    None => {
                        approvals
                            .ask_local(intent, allow_auto_edits, remember_prefix)
                            .await
                    }
                }
            }) => result.unwrap_or(ApprovalReply::Deny),
        };
        let remember = prompt.as_ref().is_some_and(|prompt| prompt.remember);
        match reply {
            ApprovalReply::AllowAlwaysInProject if !remember => ApprovalReply::AllowOnce,
            ApprovalReply::AllowPrefixInFolder if remember_prefix.is_none() => {
                ApprovalReply::AllowOnce
            }
            reply => reply,
        }
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
            let segment = match self.reserve_active_segment(tool_action_bound(runtime, &intent)) {
                Ok(segment) => segment,
                Err(EngineError::GoalBudgetExhausted) => {
                    return Ok(Answer {
                        checkpoint: None,
                        observation: not_dispatched(intent, &NotDispatched::ActiveTime),
                        stop: None,
                        active_time: Some(ActiveTime {
                            segment: None,
                            exhausted: true,
                        }),
                    });
                }
                Err(error) => return Err(error),
            };
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
                match self.reserve_active_segment(tool_action_bound(runtime, &intent)) {
                    Ok(segment) => (
                        execute_cancellable(runtime, intent, &self.controls.cancellation).await,
                        Some(segment),
                        false,
                        None,
                    ),
                    Err(EngineError::GoalBudgetExhausted) => (
                        not_dispatched(intent, &NotDispatched::ActiveTime),
                        None,
                        true,
                        None,
                    ),
                    Err(error) => return Err(error),
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
        let worker_cancellation = RunCancellation::new();
        let mut run_cancellation = self.controls.cancellation.clone();
        let cache_transfer = Duration::from_millis(u64::from(intent.limits.cache_transfer_ms))
            .saturating_add(crate::tools::worker::transfer_time(&intent));
        let action_bound = tool_action_bound(&runtime, &intent);
        let runtime_ms = intent.limits.runtime_ms;
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
                        match self.reserve_active_segment(action_bound) {
                            Ok(segment) => {
                                let window = active_window(&segment, cache_transfer, runtime_ms);
                                active_segment = Some(segment);
                                let _ = reply.send(window);
                            }
                            Err(EngineError::GoalBudgetExhausted) => {
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
        if active_time_exhausted && observation.disposition != ToolDisposition::Limit {
            observation = not_dispatched(observation.intent, &NotDispatched::ActiveTime);
        }
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

    fn capture_checkpoint<'a>(
        &'a self,
        runtime: &'a ToolRuntime,
        intent: &'a EffectIntent,
        deadline: Instant,
    ) -> BoxedStep<'a, crate::session::Checkpoint> {
        Box::pin(async move {
            use crate::session::{Checkpoint, CheckpointReason};
            let paths = self.ledger().view(|view| {
                let run = view
                    .runs
                    .iter()
                    .find(|run| run.id == self.run_id)
                    .ok_or(EngineError::InvalidHistory)?;
                let paths = crate::session::checkpoint::before_versions(run, &intent.call)
                    .into_iter()
                    .filter_map(|(path, before)| {
                        before
                            .filter(|digest| {
                                crate::session::checkpoint::capture_needed(
                                    view,
                                    self.run_id,
                                    &path,
                                    digest,
                                )
                            })
                            .map(|digest| (path, digest))
                    })
                    .collect::<Vec<_>>();
                Ok::<_, EngineError>(paths)
            })??;
            if paths.is_empty() {
                return Ok(Checkpoint::not_needed());
            }
            let intent_id = intent.id;
            let (runtime, intent) = (runtime.clone(), intent.clone());
            let capture =
                tokio::task::spawn_blocking(move || runtime.checkpoint_bytes(&intent, &paths));
            let files = match tokio::time::timeout_at(deadline, capture).await {
                Ok(Ok(Ok(files))) => files,
                Ok(Ok(Err(ToolError::ChangedInput | ToolError::Path))) => {
                    return Ok(Checkpoint::skipped(CheckpointReason::DigestMismatch));
                }
                Ok(Ok(Err(ToolError::Limit))) => {
                    return Ok(Checkpoint::skipped(CheckpointReason::TooLarge));
                }
                _ => return Ok(Checkpoint::skipped(CheckpointReason::StoreUnavailable)),
            };
            let capture = crate::store::CheckpointCapture {
                session: self.session_id,
                owner: crate::store::CheckpointOwner::Tool {
                    goal: self.goal()?,
                    run: self.run_id,
                    intent_id,
                },
                files,
            };
            #[cfg(any(test, feature = "test-support"))]
            let stalled = crate::fault::FaultPlan::checkpoint_stalled(self.session_id);
            let store = self.journal.store();
            let stored = async {
                #[cfg(any(test, feature = "test-support"))]
                if stalled {
                    std::future::pending::<()>().await;
                }
                store.capture_checkpoint(capture).await
            };
            let checkpoint = match tokio::time::timeout_at(deadline, stored).await {
                Ok(Ok(checkpoint)) => checkpoint,
                _ => Checkpoint::skipped(CheckpointReason::StoreUnavailable),
            };
            #[cfg(any(test, feature = "test-support"))]
            if checkpoint.status == crate::CheckpointStatus::Captured
                && let Some((entered, release)) =
                    crate::fault::FaultPlan::checkpoint_gate(self.session_id)
            {
                let _ = entered.send(());
                let _ = tokio::time::timeout_at(deadline, release).await;
            }
            Ok(checkpoint)
        })
    }

    fn clear_tool_queue(&self) {
        if let Some(hints) = self.hints {
            hints.tool_queue().send_replace(None);
        }
    }

    async fn resolve_ask(
        &self,
        intent: &EffectIntent,
        chain: &[Resolver<'_>],
        site: AskSite,
    ) -> Result<Resolution, EngineError> {
        let initial_mode = self.terminal_mode();
        let mut resolution = Resolution {
            approved: false,
            source: PermissionSource::UnavailableApproval,
            active_time_exhausted: false,
            save: None,
        };
        for resolver in chain {
            match resolver {
                Resolver::AiReview { objective } => {
                    let review = match &site {
                        AskSite::Primary { .. } => self.review_tool(objective, intent, None).await,
                        AskSite::Delegated { deadline } => {
                            self.review_tool(objective, intent, Some(*deadline)).await
                        }
                    };
                    match review {
                        Ok(decision) => {
                            resolution.approved = match decision {
                                ToolReviewDecision::Approve => self.terminal_mode() == initial_mode,
                                ToolReviewDecision::Deny => false,
                            };
                            resolution.source = if resolution.approved {
                                PermissionSource::AiReview
                            } else {
                                PermissionSource::UnavailableApproval
                            };
                            match &site {
                                AskSite::Delegated { .. } => {
                                    if resolution.approved {
                                        resolution.approved = !self.pause_requested()
                                            && !self.controls.cancellation.is_cancelled();
                                        return Ok(resolution);
                                    }
                                }
                                AskSite::Primary { .. } => {
                                    resolution.active_time_exhausted =
                                        self.ledger().active_time_exhausted();
                                }
                            }
                        }
                        Err(EngineError::GoalBudgetExhausted)
                            if matches!(site, AskSite::Primary { .. })
                                && self.ledger().active_time_exhausted() =>
                        {
                            resolution.active_time_exhausted = true;
                        }
                        Err(error) => return Err(error),
                    }
                }
                Resolver::Human => match &site {
                    AskSite::Delegated { deadline } => {
                        let Some(approvals) = self.controls.approvals else {
                            return Ok(resolution);
                        };
                        let mut cancellation = self.controls.cancellation.clone();
                        let expires = Duration::from_millis(
                            intent.expires_at_ms.saturating_sub(crate::tools::now_ms()),
                        );
                        // A sibling Provider call keeps the shared delegation interval active.
                        let idle = self.ledger().providers_idle()?;
                        let started = Instant::now();
                        let (approved, source) = tokio::select! {
                            biased;
                            () = cancellation.cancelled() => (false, PermissionSource::UnavailableApproval),
                            result = tokio::time::timeout_at((*deadline).min(Instant::now() + expires), approvals.ask(intent)) => {
                                (result.unwrap_or(false), PermissionSource::Human)
                            }
                        };
                        if idle {
                            self.ledger().exclude_idle_approval(started.elapsed())?;
                        }
                        resolution.approved = approved
                            && !self.pause_requested()
                            && !self.controls.cancellation.is_cancelled();
                        resolution.source = source;
                    }
                    AskSite::Primary {
                        unguarded_prompt,
                        allow_auto_edits,
                        remember_prefix,
                    } => {
                        if !resolution.active_time_exhausted
                            && !self.pause_at_boundary()
                            && let Some(approvals) = self.controls.approvals
                        {
                            match self.reserve_active_pause() {
                                Ok(pause) => {
                                    let reply = self
                                        .human_reply(
                                            approvals,
                                            intent,
                                            unguarded_prompt.clone(),
                                            *allow_auto_edits,
                                            *remember_prefix,
                                        )
                                        .await;
                                    resolution.source = PermissionSource::Human;
                                    resolution.approved = reply != ApprovalReply::Deny;
                                    // A remembered rule is saved only after this human verdict is committed.
                                    resolution.save = match reply {
                                        ApprovalReply::AllowAlwaysInProject => {
                                            Some(RuleSave::ExactCommand)
                                        }
                                        ApprovalReply::AllowPrefixInFolder => {
                                            remember_prefix.map(RuleSave::Prefix)
                                        }
                                        ApprovalReply::Deny | ApprovalReply::AllowOnce => None,
                                    };
                                    let resume_at = resolution.approved.then(Instant::now);
                                    self.settle_active_pause(pause, resume_at)?;
                                }
                                Err(EngineError::GoalBudgetExhausted) => {
                                    resolution.active_time_exhausted = true;
                                }
                                Err(error) => return Err(error),
                            }
                        }
                    }
                },
            }
            if resolution.approved {
                break;
            }
        }
        Ok(resolution)
    }

    async fn review_tool(
        &self,
        objective: &str,
        intent: &EffectIntent,
        delegation_deadline: Option<Instant>,
    ) -> Result<ToolReviewDecision, EngineError> {
        let mut model = String::new();
        let mut max_output_tokens = 256;
        self.routing.bind(
            crate::provider::ProviderRole::Review,
            intent.agent_run_id,
            &mut model,
            &mut max_output_tokens,
        )?;
        let mut review = ProviderRequest {
            session_id: self.session_id,
            run_id: self.run_id,
            agent_run_id: intent.agent_run_id,
            phase: AgentPhase::ToolReview,
            collaboration: CollaborationPolicy::Single,
            model,
            max_output_tokens,
            input: RunInput {
                goal_budget: Some(self.ledger().effective_budget()?),
                goal_plan_contract: self.ledger().plan_contract()?,
                ..RunInput::new(
                    PromptRole::ToolReview,
                    serde_json::json!({"user_request":objective,"proposed_action":intent})
                        .to_string(),
                )
            },
        };
        let active_segment = if delegation_deadline.is_none() {
            Some(self.reserve_active_segment(TOOL_REVIEW_DEADLINE + STORE_ACTION_OVERHEAD)?)
        } else {
            None
        };
        let review_deadline = active_segment
            .as_ref()
            .map(|segment| {
                segment
                    .deadline()
                    .checked_sub(STORE_ACTION_OVERHEAD)
                    .unwrap_or(segment.started)
            })
            .or(delegation_deadline)
            .ok_or(EngineError::CoordinatorFailed)?
            .min(Instant::now() + TOOL_REVIEW_DEADLINE);
        // A researcher's Tool runs inside the delegation's reservation, so its review adds no time.
        let pending_ms = match (delegation_deadline, self.controls.tools) {
            (None, Some(runtime)) => tool_action_ms(runtime, intent),
            _ => 0,
        };
        if let Err(error) = self
            .reserve_provider_call(intent.agent_run_id, Action::Review, pending_ms)
            .await
        {
            if matches!(error, EngineError::GoalBudgetExhausted)
                && let Some(active_segment) = active_segment
            {
                self.settle_active_segment(active_segment)?;
            }
            return Err(error);
        }
        review.input.goal_context_facts = Some(self.ledger().context_facts()?);
        let result = self
            .routing
            .invoke(
                review,
                review_deadline,
                self.controls.cancellation.clone(),
                None,
            )
            .await?;
        self.settle_result(&result)?;
        let valid = matches!(
            &result,
            CallResult::Response(ProviderResponse {
                outcome: ProviderOutcome::Review(_),
                ..
            })
        );
        let valid = valid && self.ledger().tokens_within_limit();
        self.append(Event::ProviderCallRecorded {
            run_id: self.run_id,
            agent_run_id: intent.agent_run_id,
            record: call_record(AgentPhase::ToolReview, &result, valid, None, None),
        })
        .await?;
        if let Some(active_segment) = active_segment {
            self.settle_active_segment(active_segment)?;
        }
        let exhausted = if delegation_deadline.is_some() {
            self.ledger().spent_active_time_exhausted()
        } else {
            self.ledger().active_time_exhausted()
        };
        Ok(tool_review_decision(&result, valid, exhausted))
    }
}

fn tool_review_decision(result: &CallResult, valid: bool, exhausted: bool) -> ToolReviewDecision {
    if exhausted {
        return ToolReviewDecision::Deny;
    }
    match result {
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Review(crate::ReviewVerdict::Approve),
            ..
        }) if valid => ToolReviewDecision::Approve,
        CallResult::Response(_) | CallResult::Failed(_) | CallResult::Cancelled => {
            ToolReviewDecision::Deny
        }
    }
}

fn tool_action_bound(runtime: &ToolRuntime, intent: &EffectIntent) -> Duration {
    runtime
        .reservation_overhead(&intent.call)
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DispatchSite {
    Primary,
    Researcher,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UnguardedRefusal {
    NoPriorFailure,
    GuardAlias,
    ProtectedPath,
}

#[must_use]
#[derive(Debug, Eq, PartialEq)]
pub(super) enum NotDispatched {
    TimeoutLimit { requested: u32, maximum: u32 },
    HookDenied(HookDenial),
    DelegationDeadline,
    DenyRule,
    OutsideGrant,
    ModeDenied,
    HumanOnlyUnavailable,
    HookFailureAfterClassify,
    UnguardedNotOffered(UnguardedRefusal),
    OutsidePlan,
    ActiveTime,
    Paused,
    TokenBudget,
    UnsavedRule,
    SaveUnconfirmed,
    UnguardedNeedsHuman,
    ApprovalRefused,
}

#[derive(Clone, Copy)]
enum ActiveTimeEffect {
    Untouched,
    Recheck,
    Exhausted,
}

impl NotDispatched {
    fn active_time_effect(&self) -> ActiveTimeEffect {
        match self {
            Self::HookDenied(HookDenial::ActiveTime) | Self::ActiveTime => {
                ActiveTimeEffect::Exhausted
            }
            Self::HookFailureAfterClassify
            | Self::UnguardedNotOffered(_)
            | Self::OutsidePlan
            | Self::Paused
            | Self::TokenBudget
            | Self::UnsavedRule
            | Self::SaveUnconfirmed
            | Self::UnguardedNeedsHuman
            | Self::ApprovalRefused => ActiveTimeEffect::Recheck,
            Self::TimeoutLimit { .. }
            | Self::HookDenied(
                HookDenial::DelegationDeadline
                | HookDenial::Uncertain
                | HookDenial::Requested
                | HookDenial::Narrowed(_)
                | HookDenial::Denied(_),
            )
            | Self::DelegationDeadline
            | Self::DenyRule
            | Self::OutsideGrant
            | Self::ModeDenied
            | Self::HumanOnlyUnavailable => ActiveTimeEffect::Untouched,
        }
    }

    // The primary decides these after its gates, where it clears the queue hint and re-reads active time.
    fn decided_at_approval(&self) -> bool {
        match self {
            Self::HookFailureAfterClassify
            | Self::UnguardedNotOffered(_)
            | Self::OutsidePlan
            | Self::ActiveTime
            | Self::Paused
            | Self::TokenBudget
            | Self::UnsavedRule
            | Self::SaveUnconfirmed
            | Self::UnguardedNeedsHuman
            | Self::ApprovalRefused => true,
            Self::TimeoutLimit { .. }
            | Self::HookDenied(_)
            | Self::DelegationDeadline
            | Self::DenyRule
            | Self::OutsideGrant
            | Self::ModeDenied
            | Self::HumanOnlyUnavailable => false,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct Refusal<'a> {
    reason: NotDispatched,
    stop: Option<StopCause>,
    narrowed: Option<&'a ToolCall>,
}

#[derive(Debug, Eq, PartialEq)]
enum Dispatch {
    Direct,
    QueuePause,
}

#[must_use]
#[derive(Debug, Eq, PartialEq)]
struct DispatchDecision<'a> {
    outcome: Result<Dispatch, Refusal<'a>>,
    decision: Decision,
    source: PermissionSource,
}

struct Unguarded {
    late_hook_failure: bool,
    gate: Result<UnguardedPrompt, UnguardedRefusal>,
}

// Read after the PreToolUse hooks, before the Ask.
struct DispatchFacts<'a> {
    site: DispatchSite,
    permission: Permission,
    hooks: &'a HookFold,
    timeout_limit: Option<(u32, u32)>,
    human_only_unavailable: bool,
    unguarded: Option<Unguarded>,
    goal_allows: bool,
    runtime_allows: bool,
    can_start_provider_call: bool,
    guard_site: bool,
    interactive: bool,
}

// Read after the Ask, or where it would have been.
struct AfterAsk {
    approval: Option<Resolution>,
    paused: bool,
    can_start_provider_call: bool,
    cancelled: bool,
    deadline_reached: bool,
}

// A typed allow takes effect only in `default`; the other modes ignore it or never ask.
fn typed_allow_takes_effect(mode: Option<crate::tools::permissions::PermissionMode>) -> bool {
    mode == Some(crate::tools::permissions::PermissionMode::Default)
}

fn remember_candidate(
    grant: &Grant<'_>,
    mode: Option<crate::tools::permissions::PermissionMode>,
    permission: Permission,
    hooks: &HookFold,
    call: &ToolCall,
) -> Option<crate::tools::permissions::TypedGitPrefix> {
    (matches!(grant, Grant::Primary { .. })
        && typed_allow_takes_effect(mode)
        && permission.decision == Decision::Ask
        && permission.basis == PermissionSource::Mode
        && hooks.restriction.is_none())
    .then(|| crate::tools::permissions::TypedGitPrefix::of_call(call))
    .flatten()
}

fn refuse<'a>(reason: NotDispatched, stop: Option<StopCause>) -> Refusal<'a> {
    Refusal {
        reason,
        stop,
        narrowed: None,
    }
}

impl<'a> DispatchFacts<'a> {
    fn gate(&self) -> Result<(), Refusal<'a>> {
        let Permission {
            decision, basis, ..
        } = self.permission;
        if let Some((requested, maximum)) = self.timeout_limit {
            return Err(refuse(
                NotDispatched::TimeoutLimit { requested, maximum },
                None,
            ));
        }
        if decision == Decision::Deny || self.hooks.exhausted() {
            let (reason, stop) = if self.hooks.exhausted() || basis == PermissionSource::Hook {
                self.hooks.refusal(self.site)
            } else {
                let reason = match basis {
                    PermissionSource::Rule | PermissionSource::TypedPrefixRule => {
                        NotDispatched::DenyRule
                    }
                    PermissionSource::PinnedGrant => NotDispatched::OutsideGrant,
                    PermissionSource::Mode
                    | PermissionSource::Human
                    | PermissionSource::AiReview
                    | PermissionSource::UnavailableApproval
                    | PermissionSource::ExactCommandRule
                    | PermissionSource::Hook => NotDispatched::ModeDenied,
                };
                (reason, None)
            };
            return Err(Refusal {
                reason,
                stop,
                narrowed: self.hooks.narrowed.as_ref(),
            });
        }
        if self.human_only_unavailable {
            return Err(refuse(
                NotDispatched::HumanOnlyUnavailable,
                Some(StopCause::ToolStopped),
            ));
        }
        if let Some(unguarded) = &self.unguarded {
            if unguarded.late_hook_failure
                && (decision != Decision::Ask || basis == PermissionSource::ExactCommandRule)
            {
                return Err(refuse(NotDispatched::HookFailureAfterClassify, None));
            }
            if let Err(refusal) = &unguarded.gate {
                return Err(refuse(NotDispatched::UnguardedNotOffered(*refusal), None));
            }
        }
        if !self.goal_allows {
            return Err(refuse(NotDispatched::OutsidePlan, None));
        }
        Ok(())
    }

    fn ask(&self) -> Option<ApprovalMode> {
        self.gate().ok()?;
        self.permission
            .approval
            .filter(|_| self.runtime_allows && self.can_start_provider_call)
    }

    fn unguarded_prompt(&self) -> Option<UnguardedPrompt> {
        self.unguarded
            .as_ref()
            .and_then(|unguarded| unguarded.gate.as_ref().ok())
            .cloned()
    }

    fn unresolved_source(&self) -> PermissionSource {
        if self.permission.decision == Decision::Ask {
            PermissionSource::UnavailableApproval
        } else {
            self.permission.basis
        }
    }
}

fn decide_dispatch<'a>(facts: &DispatchFacts<'a>, after: &AfterAsk) -> DispatchDecision<'a> {
    if let Err(refusal) = facts.gate() {
        let (decision, source) = match refusal.reason {
            NotDispatched::HumanOnlyUnavailable => {
                (Decision::Ask, PermissionSource::UnavailableApproval)
            }
            NotDispatched::TimeoutLimit { .. }
            | NotDispatched::HookDenied(_)
            | NotDispatched::DelegationDeadline
            | NotDispatched::DenyRule
            | NotDispatched::OutsideGrant
            | NotDispatched::ModeDenied
            | NotDispatched::HookFailureAfterClassify
            | NotDispatched::UnguardedNotOffered(_)
            | NotDispatched::OutsidePlan
            | NotDispatched::ActiveTime
            | NotDispatched::Paused
            | NotDispatched::TokenBudget
            | NotDispatched::UnsavedRule
            | NotDispatched::SaveUnconfirmed
            | NotDispatched::UnguardedNeedsHuman
            | NotDispatched::ApprovalRefused => {
                (facts.permission.decision, facts.unresolved_source())
            }
        };
        return DispatchDecision {
            outcome: Err(refusal),
            decision,
            source,
        };
    }
    let (approved, source, active_time_exhausted) = match (&after.approval, facts.site) {
        (Some(resolution), _) => (
            resolution.approved,
            resolution.source,
            resolution.active_time_exhausted,
        ),
        (None, DispatchSite::Primary) => (true, facts.unresolved_source(), false),
        (None, DispatchSite::Researcher) if facts.permission.approval.is_some() => {
            (false, PermissionSource::UnavailableApproval, false)
        }
        (None, DispatchSite::Researcher) => (true, facts.permission.basis, false),
    };
    let refused = |reason| {
        Err(refuse(
            reason,
            Some(refusal_cause(
                after.paused,
                after.cancelled,
                after.deadline_reached,
            )),
        ))
    };
    let outcome = match facts.site {
        DispatchSite::Researcher if approved => Ok(Dispatch::Direct),
        DispatchSite::Researcher => refused(NotDispatched::ApprovalRefused),
        DispatchSite::Primary if active_time_exhausted => Err(refuse(
            NotDispatched::ActiveTime,
            Some(StopCause::ActiveTime),
        )),
        // The Guard path reserves its queue wait first, then reads the pause.
        DispatchSite::Primary if approved && after.can_start_provider_call && facts.guard_site => {
            Ok(Dispatch::QueuePause)
        }
        DispatchSite::Primary if after.paused => {
            Err(refuse(NotDispatched::Paused, Some(StopCause::Paused)))
        }
        DispatchSite::Primary if approved && !after.can_start_provider_call => {
            Err(refuse(NotDispatched::TokenBudget, None))
        }
        DispatchSite::Primary if approved => Ok(Dispatch::Direct),
        DispatchSite::Primary
            if !facts.interactive
                && facts
                    .unguarded
                    .as_ref()
                    .is_some_and(|unguarded| unguarded.gate.is_ok()) =>
        {
            refused(NotDispatched::UnguardedNeedsHuman)
        }
        DispatchSite::Primary => refused(NotDispatched::ApprovalRefused),
    };
    DispatchDecision {
        outcome,
        decision: facts.permission.decision,
        source,
    }
}

fn decide_queue_pause(reserved: bool, paused: bool) -> Result<(), Refusal<'static>> {
    if !reserved {
        Err(refuse(
            NotDispatched::ActiveTime,
            Some(StopCause::ActiveTime),
        ))
    } else if paused {
        Err(refuse(NotDispatched::Paused, Some(StopCause::Paused)))
    } else {
        Ok(())
    }
}

fn refusal_cause(paused: bool, cancelled: bool, deadline_reached: bool) -> StopCause {
    if paused {
        StopCause::Paused
    } else if cancelled {
        StopCause::Cancelled
    } else if deadline_reached {
        StopCause::DelegationDeadline
    } else {
        StopCause::ToolStopped
    }
}

fn not_dispatched(intent: EffectIntent, reason: &NotDispatched) -> ToolObservation {
    // Every other Limit here, including a hook's delegation deadline, stays a Resource limit.
    let timed = match reason {
        NotDispatched::TimeoutLimit { .. } => Some(crate::tools::LimitCause::RequestedTime),
        NotDispatched::DelegationDeadline => Some(crate::tools::LimitCause::Timeout),
        NotDispatched::HookDenied(_)
        | NotDispatched::DenyRule
        | NotDispatched::OutsideGrant
        | NotDispatched::ModeDenied
        | NotDispatched::HumanOnlyUnavailable
        | NotDispatched::HookFailureAfterClassify
        | NotDispatched::UnguardedNotOffered(_)
        | NotDispatched::OutsidePlan
        | NotDispatched::ActiveTime
        | NotDispatched::Paused
        | NotDispatched::TokenBudget
        | NotDispatched::UnsavedRule
        | NotDispatched::SaveUnconfirmed
        | NotDispatched::UnguardedNeedsHuman
        | NotDispatched::ApprovalRefused => None,
    };
    let (disposition, output) = match reason {
        NotDispatched::TimeoutLimit { requested, maximum } => (
            ToolDisposition::Limit,
            crate::tools::types::command_timeout_limit_message(*requested, *maximum),
        ),
        NotDispatched::ActiveTime => (ToolDisposition::Limit, ACTIVE_TIME_EXHAUSTED.into()),
        NotDispatched::DelegationDeadline => (ToolDisposition::Limit, DELEGATION_DEADLINE.into()),
        NotDispatched::TokenBudget => (
            ToolDisposition::Limit,
            "Goal token budget exhausted; no operation was dispatched".into(),
        ),
        NotDispatched::HookDenied(denial) => hook_denial(&intent, denial),
        NotDispatched::SaveUnconfirmed => (
            ToolDisposition::Uncertain,
            "rule save not confirmed; no operation was dispatched".into(),
        ),
        NotDispatched::DenyRule => (
            ToolDisposition::Denied,
            "Permission deny rule blocked this Tool".into(),
        ),
        NotDispatched::OutsideGrant => (
            ToolDisposition::Denied,
            "Tool operation is outside the pinned grant".into(),
        ),
        NotDispatched::ModeDenied => (
            ToolDisposition::Denied,
            "Permission mode denied this Tool".into(),
        ),
        NotDispatched::HumanOnlyUnavailable => (
            ToolDisposition::Denied,
            "Human-only approval is unavailable on this channel; no operation was dispatched".into(),
        ),
        NotDispatched::HookFailureAfterClassify => (
            ToolDisposition::Denied,
            "A hook failure arrived after this re-run was classified; propose it again; no operation was dispatched".into(),
        ),
        NotDispatched::UnguardedNotOffered(UnguardedRefusal::NoPriorFailure) => (
            ToolDisposition::Denied,
            "No identical command failed under the Guard in this Run; no operation was dispatched".into(),
        ),
        NotDispatched::UnguardedNotOffered(UnguardedRefusal::GuardAlias) => (
            ToolDisposition::Denied,
            "Guard path aliases such as /workspace do not exist on the host; an unguarded re-run is not offered; no operation was dispatched".into(),
        ),
        NotDispatched::UnguardedNotOffered(UnguardedRefusal::ProtectedPath) => (
            ToolDisposition::Denied,
            "The Guard denied a protected path or set-ID entry; an unguarded re-run is not offered; no operation was dispatched".into(),
        ),
        NotDispatched::OutsidePlan => (
            ToolDisposition::Denied,
            "The effect is outside the active Goal plan; no operation was dispatched".into(),
        ),
        NotDispatched::Paused => (
            ToolDisposition::Denied,
            "Goal paused before this operation; no operation was dispatched".into(),
        ),
        NotDispatched::UnsavedRule => (
            ToolDisposition::Denied,
            "The always-allow rule could not be saved; no operation was dispatched".into(),
        ),
        NotDispatched::UnguardedNeedsHuman => (
            ToolDisposition::Denied,
            "Unguarded re-run needs interactive approval or an exact-command rule for this project; no operation was dispatched".into(),
        ),
        NotDispatched::ApprovalRefused => (
            ToolDisposition::Denied,
            "Approval was not granted; no operation was dispatched".into(),
        ),
    };
    let mut observation = undispatched(intent, disposition, output);
    if timed.is_some() {
        observation.limit_cause = timed;
    }
    observation
}

const DELEGATION_DEADLINE: &str = "Delegation deadline reached; no operation was dispatched";
const ACTIVE_TIME_EXHAUSTED: &str =
    "Goal active-time budget exhausted; no operation was dispatched";

fn hook_denial(intent: &EffectIntent, denial: &HookDenial) -> (ToolDisposition, String) {
    let output = match denial {
        HookDenial::ActiveTime => return (ToolDisposition::Limit, ACTIVE_TIME_EXHAUSTED.into()),
        HookDenial::DelegationDeadline => {
            return (ToolDisposition::Limit, DELEGATION_DEADLINE.into());
        }
        HookDenial::Uncertain => "Hook outcome uncertain; no operation was dispatched".into(),
        HookDenial::Requested => "Hook stopped this Run; no operation was dispatched".into(),
        HookDenial::Narrowed(name) => format!("Hook narrowed this Tool to: {name}"),
        HookDenial::Denied(reason) => {
            let room = (intent.limits.result_bytes as usize).saturating_sub(32);
            let reason: String = reason
                .chars()
                .scan(0, |bytes, ch| {
                    *bytes += ch.len_utf8();
                    (*bytes <= room).then_some(ch)
                })
                .collect();
            if reason.is_empty() {
                "Hook denied this Tool".to_owned()
            } else {
                format!("Hook denied this Tool: {reason}")
            }
        }
    };
    (ToolDisposition::Denied, output)
}

fn undispatched(
    intent: EffectIntent,
    disposition: ToolDisposition,
    output: impl Into<String>,
) -> ToolObservation {
    ToolObservation {
        enforcement_cause: None,
        limit_cause: (disposition == ToolDisposition::Limit)
            .then_some(crate::tools::LimitCause::Resource),
        intent,
        disposition,
        output: output.into(),
        attestation: None,
        cache_update: CacheUpdate::NotApplicable,
    }
}

fn unguarded_gate(
    runtime: &ToolRuntime,
    request: &ProviderRequest,
    intent: &EffectIntent,
    hook_failure: Option<&ToolObservation>,
) -> Result<UnguardedPrompt, UnguardedRefusal> {
    let ToolCall::CommandUnguarded {
        program, args, cwd, ..
    } = &intent.call
    else {
        return Err(UnguardedRefusal::NoPriorFailure);
    };
    let observed = request
        .input
        .tools
        .as_ref()
        .map(ToolContext::observations)
        .unwrap_or_default();
    if crate::tools::types::uses_guard_alias(args) {
        return Err(UnguardedRefusal::GuardAlias);
    }
    let Some(prior) = hook_failure.or_else(|| {
        crate::tools::types::qualifying_guard_failure(&intent.call, observed.iter().copied())
    }) else {
        let denied_path = observed.iter().rev().any(|observation| {
            observation.enforcement_cause == Some(crate::tools::EnforcementCause::FilesystemDenied)
                && matches!(&observation.intent.call,
                    ToolCall::Command { program: p, args: a, cwd: c } if p == program && a == args && c == cwd)
        });
        return Err(if denied_path {
            UnguardedRefusal::ProtectedPath
        } else {
            UnguardedRefusal::NoPriorFailure
        });
    };
    Ok(UnguardedPrompt {
        executable: runtime
            .command_executable(program)
            .ok_or(UnguardedRefusal::NoPriorFailure)?
            .to_owned(),
        attribution: prior.enforcement_cause,
        remember: hook_failure.is_none(),
    })
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
    ) -> Result<SkillInitialization, EngineError> {
        let name = self
            .ledger()
            .view(|view| view.runs.last().and_then(|run| run.skill.clone()))?;
        let name = match skill_read_plan(
            name,
            self.pause_at_boundary() || self.controls.cancellation.is_cancelled(),
        ) {
            SkillReadPlan::Skip => return Ok(SkillInitialization::Ready),
            SkillReadPlan::Stop => return Ok(SkillInitialization::Stopped),
            SkillReadPlan::Read(name) => name,
        };
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        let call = ToolCall::Skill {
            name,
            resource: None,
        };
        if !self.ledger().allows_tool(&call) {
            return Ok(SkillInitialization::Failed(
                crate::session::RunFailure::ToolStopped,
            ));
        }
        let intent = match self.primary_intent(runtime, call.clone())? {
            None => return Ok(SkillInitialization::Stopped),
            Some(intent) => intent,
        };
        let grant = Grant::Primary {
            request,
            review_allowed: false,
        };
        let observation = match self
            .run_intent(&grant, runtime, intent, &call, false)
            .await
            .and_then(intent_observation)?
        {
            None => return Ok(SkillInitialization::Stopped),
            Some((observation, _)) => observation,
        };
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

impl RunLoop<'_> {
    pub(super) async fn initialize_definitions(
        &self,
        request: &mut ProviderRequest,
    ) -> Result<DefinitionInitialization, EngineError> {
        let Some(runtime) = self.controls.tools else {
            return Ok(DefinitionInitialization::Ready);
        };
        let Some(pin) = runtime.definitions.pin() else {
            return Ok(DefinitionInitialization::Ready);
        };
        let mut outputs = Vec::with_capacity(pin.files.len());
        let mut normalized_bytes = 0;
        for (index, file) in pin.files.iter().enumerate() {
            if self.pause_at_boundary() || self.controls.cancellation.is_cancelled() {
                return Ok(DefinitionInitialization::Stopped);
            }
            let call = ToolCall::ParseAgentDefinition {
                index: u16::try_from(index).map_err(|_| EngineError::InvalidRequest)?,
                raw_digest: file.digest,
            };
            let Some(mut intent) = self.primary_intent(runtime, call)? else {
                self.active_time_floor
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                return Ok(DefinitionInitialization::Stopped);
            };
            intent.limits.result_bytes = MAX_TOOL_RESULT_BYTES as u32;
            let grant = Grant::Primary {
                request,
                review_allowed: false,
            };
            let post_call = intent.call.clone();
            let Some((observation, _)) = self
                .run_intent(&grant, runtime, intent, &post_call, false)
                .await
                .and_then(intent_observation)?
            else {
                return Ok(DefinitionInitialization::Stopped);
            };
            if observation.disposition != ToolDisposition::Succeeded
                || observation.attestation.is_none()
            {
                return Ok(DefinitionInitialization::Stopped);
            }
            normalized_bytes += observation.output.len();
            if normalized_bytes > 1024 * 1024 {
                return Ok(DefinitionInitialization::Stopped);
            }
            outputs.push(observation.output);
        }
        let child_model = &self.routing.providers.child[0].identity().model;
        let Ok(registry) = crate::tools::definitions::registry(
            &pin,
            outputs.iter().map(String::as_str),
            child_model,
        ) else {
            return Ok(DefinitionInitialization::Stopped);
        };
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
                        entry["digest"] = serde_json::json!(crate::tools::types::digest(
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
}

fn effective_permission_mode(
    config: Option<&crate::tools::permissions::PermissionConfig>,
    grant: &Grant<'_>,
) -> Option<crate::tools::permissions::PermissionMode> {
    config
        .map(crate::tools::permissions::PermissionConfig::mode)
        .or_else(|| match grant {
            Grant::Researcher { child, .. } => child
                .definition
                .as_ref()
                .and_then(|definition| definition.permission_mode),
            Grant::Primary { .. } => None,
        })
}

pub(super) fn intent_observation(
    result: IntentResult,
) -> Result<Option<(ToolObservation, Option<ToolCall>)>, EngineError> {
    match result {
        Ok(observation) => Ok(Some(observation)),
        Err(StopCause::ActiveTime) => Err(EngineError::GoalBudgetExhausted),
        Err(StopCause::Capacity) => Err(EngineError::Tool(crate::tools::ToolError::Limit)),
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
        ) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_review_requires_valid_approval_without_exhaustion() {
        use ToolReviewDecision::{Approve, Deny};

        let response = |outcome| {
            CallResult::Response(ProviderResponse {
                outcome,
                turn: ProviderTurn::scripted(),
                response_id: None,
                usage: crate::provider::Usage::default(),
                wire_provenance: None,
            })
        };
        let rows = [
            (
                "approve",
                response(ProviderOutcome::Review(crate::ReviewVerdict::Approve)),
                [Approve, Deny, Deny, Deny],
            ),
            (
                "ask",
                response(ProviderOutcome::Review(crate::ReviewVerdict::Ask)),
                [Deny; 4],
            ),
            (
                "non-review outcome",
                response(ProviderOutcome::Answer(crate::provider::Answer {
                    text: "answer".into(),
                })),
                [Deny; 4],
            ),
            (
                "failure",
                CallResult::Failed(super::super::CallFailure::Timeout),
                [Deny; 4],
            ),
            ("cancelled", CallResult::Cancelled, [Deny; 4]),
        ];
        for (name, result, expected) in rows {
            for ((valid, exhausted), expected) in
                [(true, false), (false, false), (true, true), (false, true)]
                    .into_iter()
                    .zip(expected)
            {
                assert_eq!(
                    tool_review_decision(&result, valid, exhausted),
                    expected,
                    "{name}, valid={valid}, exhausted={exhausted}"
                );
            }
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
    fn refusals_keep_their_own_cause() {
        for (flags, expected) in [
            ([true, true, true], StopCause::Paused),
            ([false, true, true], StopCause::Cancelled),
            ([false, false, true], StopCause::DelegationDeadline),
            ([false, false, false], StopCause::ToolStopped),
        ] {
            let [paused, cancelled, deadline] = flags;
            assert_eq!(refusal_cause(paused, cancelled, deadline), expected);
        }
    }

    #[test]
    fn remembering_is_offered_only_where_a_typed_allow_takes_effect() {
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

    fn hook(stop: hooks::HookStop, narrowed: Option<ToolCall>) -> HookFold {
        HookFold {
            restriction: Some(Decision::Deny),
            count: 1,
            narrowed,
            reason: Some("no".into()),
            stop,
        }
    }

    fn resolution(approved: bool, source: PermissionSource) -> Resolution {
        Resolution {
            approved,
            source,
            active_time_exhausted: false,
            save: None,
        }
    }

    fn prompt() -> Result<UnguardedPrompt, UnguardedRefusal> {
        Ok(UnguardedPrompt {
            executable: "/bin/make".into(),
            attribution: None,
            remember: true,
        })
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Fact {
        Timeout,
        HumanOnly,
        LateHookFailure,
        NotOffered,
        Offered,
        OutsidePlan,
        Blocked,
        TokensSpent,
        TokensSpentAfterAsk,
        InProcess,
        Batch,
        Paused,
        Cancelled,
        Deadline,
    }

    fn decide<'a>(
        site: DispatchSite,
        permission: Permission,
        hooks: &'a HookFold,
        facts: &[Fact],
        approval: Option<Resolution>,
    ) -> (Option<ApprovalMode>, DispatchDecision<'a>) {
        let has = |fact| facts.contains(&fact);
        let gate = if has(Fact::NotOffered) {
            Some(Err(UnguardedRefusal::GuardAlias))
        } else if has(Fact::Offered) || has(Fact::LateHookFailure) {
            Some(prompt())
        } else {
            None
        };
        let facts = DispatchFacts {
            site,
            permission,
            hooks,
            timeout_limit: has(Fact::Timeout).then_some((9_000, 5_000)),
            human_only_unavailable: has(Fact::HumanOnly),
            unguarded: gate.map(|gate| Unguarded {
                late_hook_failure: has(Fact::LateHookFailure),
                gate,
            }),
            goal_allows: !has(Fact::OutsidePlan),
            runtime_allows: !has(Fact::Blocked),
            can_start_provider_call: !has(Fact::TokensSpent),
            guard_site: !has(Fact::InProcess),
            interactive: !has(Fact::Batch),
        };
        let after = AfterAsk {
            approval,
            paused: has(Fact::Paused),
            can_start_provider_call: !has(Fact::TokensSpent) && !has(Fact::TokensSpentAfterAsk),
            cancelled: has(Fact::Cancelled),
            deadline_reached: has(Fact::Deadline),
        };
        (facts.ask(), decide_dispatch(&facts, &after))
    }

    #[test]
    fn dispatch_decisions_cover_every_reason_verdict_and_site() {
        use ApprovalMode::{Auto, Request};
        use DispatchSite::{Primary, Researcher};
        use Fact::*;
        use NotDispatched as N;
        use PermissionSource as S;
        use hooks::HookStop;
        let call = || ToolCall::Read {
            path: "file".into(),
            offset: None,
            limit: Some(1),
        };
        let none = HookFold::none();
        let denied = hook(HookStop::None, None);
        let narrowing = hook(HookStop::None, Some(call()));
        let stopped = hook(HookStop::Requested, None);
        let uncertain = hook(HookStop::Uncertain, None);
        let exhausted = hook(HookStop::ActiveTime, None);
        let no = |reason, stop| Err(refuse(reason, stop));
        let narrow = |reason| {
            Err(Refusal {
                reason,
                stop: None,
                narrowed: narrowing.narrowed.as_ref(),
            })
        };
        let ok = |approved, source| Some(resolution(approved, source));
        let exhausted_ask = Some(Resolution {
            active_time_exhausted: true,
            ..resolution(false, S::Human)
        });
        let timeout = || N::TimeoutLimit {
            requested: 9_000,
            maximum: 5_000,
        };
        let (allow_, ask_, deny_) = (Decision::Allow, Decision::Ask, Decision::Deny);
        #[rustfmt::skip]
        let rows = [
            // Gate refusals, decided before any Ask.
            (decide(Primary, allow(S::Rule), &none, &[Timeout], None), None, no(timeout(), None), allow_, S::Rule),
            (decide(Researcher, asks(Request), &none, &[Timeout], None), None, no(timeout(), None), ask_, S::UnavailableApproval),
            (decide(Primary, deny(S::Rule), &none, &[], None), None, no(N::DenyRule, None), deny_, S::Rule),
            (decide(Researcher, deny(S::PinnedGrant), &none, &[], None), None, no(N::OutsideGrant, None), deny_, S::PinnedGrant),
            (decide(Primary, deny(S::Mode), &none, &[], None), None, no(N::ModeDenied, None), deny_, S::Mode),
            (decide(Primary, deny(S::Hook), &denied, &[], None), None, no(N::HookDenied(HookDenial::Denied("no".into())), None), deny_, S::Hook),
            (decide(Primary, deny(S::Hook), &narrowing, &[], None), None, narrow(N::HookDenied(HookDenial::Narrowed("Read".into()))), deny_, S::Hook),
            (decide(Primary, deny(S::Rule), &narrowing, &[], None), None, narrow(N::DenyRule), deny_, S::Rule),
            (decide(Primary, deny(S::Hook), &stopped, &[], None), None, no(N::HookDenied(HookDenial::Requested), Some(StopCause::HookStop)), deny_, S::Hook),
            (decide(Researcher, deny(S::Hook), &uncertain, &[], None), None, no(N::HookDenied(HookDenial::Uncertain), Some(StopCause::Uncertain)), deny_, S::Hook),
            (decide(Primary, asks(Request), &exhausted, &[], None), None, no(N::HookDenied(HookDenial::ActiveTime), None), ask_, S::UnavailableApproval),
            (decide(Researcher, allow(S::Rule), &exhausted, &[], None), None, no(N::HookDenied(HookDenial::DelegationDeadline), Some(StopCause::DelegationDeadline)), allow_, S::Rule),
            (decide(Primary, asks(Request), &none, &[HumanOnly], None), None, no(N::HumanOnlyUnavailable, Some(StopCause::ToolStopped)), ask_, S::UnavailableApproval),
            (decide(Researcher, allow(S::Rule), &none, &[HumanOnly], None), None, no(N::HumanOnlyUnavailable, Some(StopCause::ToolStopped)), ask_, S::UnavailableApproval),
            (decide(Primary, allow(S::ExactCommandRule), &none, &[LateHookFailure], None), None, no(N::HookFailureAfterClassify, None), allow_, S::ExactCommandRule),
            (decide(Primary, asks(Request), &none, &[NotOffered], None), None, no(N::UnguardedNotOffered(UnguardedRefusal::GuardAlias), None), ask_, S::UnavailableApproval),
            (decide(Primary, asks(Auto), &none, &[OutsidePlan], None), None, no(N::OutsidePlan, None), ask_, S::UnavailableApproval),
            (decide(Researcher, allow(S::Mode), &none, &[OutsidePlan], None), None, no(N::OutsidePlan, None), allow_, S::Mode),
            // The primary after its Ask, or where it would have been.
            (decide(Primary, asks(Request), &none, &[LateHookFailure], ok(true, S::Human)), Some(Request), Ok(Dispatch::QueuePause), ask_, S::Human),
            (decide(Primary, allow(S::Rule), &none, &[Paused], None), None, Ok(Dispatch::QueuePause), allow_, S::Rule),
            (decide(Primary, allow(S::Rule), &none, &[InProcess], None), None, Ok(Dispatch::Direct), allow_, S::Rule),
            (decide(Primary, asks(Auto), &none, &[Blocked], None), None, Ok(Dispatch::QueuePause), ask_, S::UnavailableApproval),
            (decide(Primary, allow(S::Rule), &none, &[InProcess, Paused], None), None, no(N::Paused, Some(StopCause::Paused)), allow_, S::Rule),
            (decide(Primary, asks(Request), &none, &[TokensSpent, Paused], None), None, no(N::Paused, Some(StopCause::Paused)), ask_, S::UnavailableApproval),
            (decide(Primary, asks(Request), &none, &[TokensSpent], None), None, no(N::TokenBudget, None), ask_, S::UnavailableApproval),
            (decide(Primary, asks(Auto), &none, &[TokensSpentAfterAsk], ok(true, S::AiReview)), Some(Auto), no(N::TokenBudget, None), ask_, S::AiReview),
            (decide(Primary, asks(Request), &none, &[Paused], exhausted_ask), Some(Request), no(N::ActiveTime, Some(StopCause::ActiveTime)), ask_, S::Human),
            (decide(Primary, asks(Request), &none, &[Offered, Batch], ok(false, S::UnavailableApproval)), Some(Request), no(N::UnguardedNeedsHuman, Some(StopCause::ToolStopped)), ask_, S::UnavailableApproval),
            (decide(Primary, asks(Request), &none, &[Offered, Cancelled], ok(false, S::Human)), Some(Request), no(N::ApprovalRefused, Some(StopCause::Cancelled)), ask_, S::Human),
            (decide(Primary, asks(Auto), &none, &[Batch], ok(false, S::UnavailableApproval)), Some(Auto), no(N::ApprovalRefused, Some(StopCause::ToolStopped)), ask_, S::UnavailableApproval),
            // A researcher after its Ask, or where it would have been.
            (decide(Researcher, allow(S::Rule), &none, &[Paused], None), None, Ok(Dispatch::Direct), allow_, S::Rule),
            (decide(Researcher, asks(Auto), &none, &[], ok(true, S::AiReview)), Some(Auto), Ok(Dispatch::Direct), ask_, S::AiReview),
            (decide(Researcher, asks(Request), &none, &[Blocked, Deadline], None), None, no(N::ApprovalRefused, Some(StopCause::DelegationDeadline)), ask_, S::UnavailableApproval),
            (decide(Researcher, asks(Request), &none, &[TokensSpent, Cancelled], None), None, no(N::ApprovalRefused, Some(StopCause::Cancelled)), ask_, S::UnavailableApproval),
            (decide(Researcher, asks(Request), &none, &[Paused, Cancelled], ok(false, S::Human)), Some(Request), no(N::ApprovalRefused, Some(StopCause::Paused)), ask_, S::Human),
        ];
        for (index, ((asked, decided), asks, outcome, decision, source)) in
            rows.into_iter().enumerate()
        {
            assert_eq!(asked, asks, "row {index}");
            assert_eq!(
                decided,
                DispatchDecision {
                    outcome,
                    decision,
                    source,
                },
                "row {index}"
            );
        }
        for (reserved, paused, reason, stop) in [
            (true, true, N::Paused, StopCause::Paused),
            (false, true, N::ActiveTime, StopCause::ActiveTime),
            (false, false, N::ActiveTime, StopCause::ActiveTime),
        ] {
            assert_eq!(
                decide_queue_pause(reserved, paused),
                Err(refuse(reason, Some(stop)))
            );
        }
        assert_eq!(decide_queue_pause(true, false), Ok(()));
    }
}

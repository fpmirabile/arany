use super::children::ChildWork;
use super::{
    ActivePause, ActiveSegment, CallResult, RunLoop, TOOL_CLEANUP_OVERHEAD, TOOL_REVIEW_DEADLINE,
    call_record, valid_finish,
};
use crate::engine::ledger::{Action, duration_ms};
use crate::engine::progress::ProgressPublisher;
use crate::engine::{EngineError, RunCancellation, STORE_ACTION_OVERHEAD};
use crate::provider::{
    AgentPhase, PromptRole, ProviderOutcome, ProviderRequest, ProviderResponse, ProviderTurn,
    RunInput,
};
use crate::session::{CollaborationPolicy, Event};
use crate::tools::permissions::{Decision, PermissionSource, PermissionVerdict};
use crate::tools::types::{
    CommandChangeset, CommandResult, MAX_TOOL_ARGUMENT_BYTES, MAX_TOOL_RESULT_BYTES,
};
use crate::tools::{
    ActiveActionRequest, ActiveToolWindow, ApprovalMode, ApprovalReply, CacheUpdate, EffectIntent,
    ToolCall, ToolContext, ToolDisposition, ToolError, ToolObservation, ToolRequest,
    ToolResourceClass, ToolRuntime, ToolStep, UnguardedPrompt,
};
use std::time::Duration;
use tokio::time::Instant;

mod hooks;

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

enum Verdict {
    Dispatch {
        intent: EffectIntent,
        queue_pause: Option<ActivePause>,
    },
    Deny(Answer),
    Narrow(Answer, ToolCall),
}

const MAX_NARROWINGS_PER_STEP: usize = 1;

enum Resolver<'a> {
    AiReview { objective: &'a str },
    Human,
}

enum AskSite {
    Primary {
        unguarded_prompt: Option<UnguardedPrompt>,
    },
    Delegated {
        deadline: Instant,
    },
}

struct Resolution {
    approved: bool,
    source: PermissionSource,
    active_time_exhausted: bool,
    unsaved_rule: bool,
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
            && let Some(mode) = mode.filter(|mode| mode.asks(call))
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
    observation: ToolObservation,
    refused: bool,
    active_time: Option<ActiveTime>,
}

struct ActiveTime {
    segment: Option<ActiveSegment>,
    exhausted: bool,
}

impl RunLoop<'_> {
    pub(super) async fn execute_step(
        &self,
        mut grant: Grant<'_>,
        request: ToolRequest,
        turn: ProviderTurn,
    ) -> Result<bool, EngineError> {
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        let source_call = request.call.clone();
        let mut call = source_call.clone();
        let calls_remaining = match &grant {
            Grant::Primary { .. } => None,
            Grant::Researcher { child, .. } => Some(child.tool_calls_remaining.saturating_sub(1)),
        };
        let mut narrowings = 0;
        let mut step = ToolStep {
            turn,
            requests: vec![request],
            observations: Vec::new(),
            facts: None,
            steering_notes: Vec::new(),
            calls_remaining,
            hook_feedback: Default::default(),
        };
        loop {
            let Some(intent) = self.sized_intent(&grant, runtime, &step, call, &source_call)?
            else {
                return Ok(false);
            };
            let command = matches!(intent.call, ToolCall::Command { .. });
            let source_intent_digest = intent.digest();
            let narrowable = narrowings < MAX_NARROWINGS_PER_STEP;
            // Boxed: approval and dispatch futures otherwise inflate every enclosing agent step.
            let Some((observation, narrowed)) =
                Box::pin(self.run_intent(&grant, runtime, intent, &source_call, narrowable))
                    .await?
            else {
                return Ok(false);
            };
            if observation.stops_run() || self.controls.cancellation.is_cancelled() {
                return Ok(false);
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
                    _ => return Ok(false),
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
            if self.controls.cancellation.is_cancelled()
                || grant.context()?.reserved_len(&step, &[]) > runtime.context_limit_bytes()
            {
                return Ok(false);
            }
            call = ToolCall::ApplyChangeset {
                source: crate::session::worker::ChangesetSource::CommandReceipt {
                    source_intent_digest,
                    changeset,
                },
            };
            if !call.valid() {
                return Ok(false);
            }
        }
        // The hooks see the published tree: a Command's changeset is integrated first.
        if matches!(grant, Grant::Primary { .. })
            && let Some(config) = runtime.permissions()
            && let Some(source) = step.observations.get(narrowings)
            && source.disposition == ToolDisposition::Succeeded
            && !source.intent.call.is_internal()
            && step.observations[narrowings + 1..]
                .iter()
                .all(|integration| integration.disposition == ToolDisposition::Succeeded)
        {
            let post = Box::pin(self.post_tool_use_hooks(runtime, config, source)).await?;
            match post.stop {
                HookStop::None if !post.feedback.is_empty() => {
                    step.hook_feedback.insert(narrowings, post.feedback);
                }
                HookStop::None => {}
                HookStop::Uncertain => return Ok(false),
                HookStop::ActiveTime => return Err(EngineError::GoalBudgetExhausted),
            }
        }
        grant.context_mut()?.steps.push(step);
        if let Grant::Primary { request, .. } = &mut grant {
            self.compact_tool_context_if_needed(request).await?;
            if self.controls.cancellation.is_cancelled() {
                return Ok(false);
            }
        }
        Ok(grant.context()?.rendered_len() <= runtime.context_limit_bytes())
    }

    fn sized_intent(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        step: &ToolStep,
        call: ToolCall,
        post_call: &ToolCall,
    ) -> Result<Option<EffectIntent>, EngineError> {
        let intent = match grant {
            Grant::Primary { .. } => self.primary_intent(runtime, call)?,
            Grant::Researcher { child, deadline } => {
                self.researcher_intent(runtime, child.agent_run_id, call, *deadline)
            }
        };
        let Some(mut intent) = intent else {
            return Ok(None);
        };
        // The PreToolUse hooks run before dispatch, so the authorization outlives them.
        intent.expires_at_ms = intent.expires_at_ms.saturating_add(
            self.hook_admission(grant, runtime, &intent.call, post_call)
                .1,
        );
        let integration =
            matches!(intent.call, ToolCall::Command { .. }).then(|| ToolCall::ApplyChangeset {
                source: crate::session::worker::ChangesetSource::CommandReceipt {
                    source_intent_digest: intent.digest(),
                    changeset: CommandChangeset { files: Vec::new() },
                },
            });
        let reserved: Vec<_> = std::iter::once(&intent.call).chain(&integration).collect();
        let overhead = grant.context()?.reserved_len(step, &reserved);
        let integration_capacity = if integration.is_some() {
            MAX_TOOL_ARGUMENT_BYTES + 128 * 6 + 512
        } else {
            0
        } + self.feedback_reserve(runtime, post_call);
        let Some(capacity) = runtime.result_capacity(overhead, integration_capacity) else {
            return Ok(None);
        };
        intent.limits.result_bytes = if matches!(intent.call, ToolCall::ApplyChangeset { .. }) {
            128
        } else {
            capacity.min(MAX_TOOL_RESULT_BYTES) as u32
        };
        Ok(Some(intent))
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
        let transfer_multiplier = if command && intent.limits.cache_bytes > 0 {
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
        if runtime_ms == 0 {
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

    fn researcher_intent(
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
        // Half for the Guard capacity wait (it ends at `expires_at_ms`), half for the run itself.
        let runtime_ms = intent
            .limits
            .runtime_ms
            .min(u32::try_from(duration_ms(available) / 2).unwrap_or(u32::MAX));
        if runtime_ms == 0 {
            return None;
        }
        intent.limits.runtime_ms = runtime_ms;
        intent.expires_at_ms = crate::tools::now_ms().saturating_add(u64::from(runtime_ms));
        Some(intent)
    }

    pub(super) async fn run_intent(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        intent: EffectIntent,
        post_call: &ToolCall,
        narrowable: bool,
    ) -> Result<Option<(ToolObservation, Option<ToolCall>)>, EngineError> {
        let permission = self.permission(grant, runtime, &intent.call);
        let (hook_events, pre_ms, post_ms) =
            self.hook_admission(grant, runtime, &intent.call, post_call);
        if !self
            .admit_tool(grant, runtime, &intent, permission, hook_events, narrowable)
            .await?
        {
            return Ok(None);
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
        let mut narrowed = None;
        let answer = match self
            .before_dispatch(grant, runtime, intent, permission, narrowable)
            .await?
        {
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
                Grant::Researcher { .. } => Answer {
                    observation: execute_cancellable(runtime, intent, &self.controls.cancellation)
                        .await,
                    refused: false,
                    active_time: None,
                },
            },
        };
        self.append(Event::ToolFinished {
            run_id: self.run_id,
            agent_run_id,
            observation: answer.observation.clone(),
        })
        .await?;
        if let Some(active_time) = answer.active_time {
            if let Some(segment) = active_time.segment {
                self.settle_active_segment(segment)?;
            }
            if active_time.exhausted || self.ledger().active_time_exhausted() {
                return Err(EngineError::GoalBudgetExhausted);
            }
        }
        Ok((!answer.refused).then_some((answer.observation, narrowed)))
    }

    async fn admit_tool(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        intent: &EffectIntent,
        permission: Permission,
        hook_events: usize,
        narrowable: bool,
    ) -> Result<bool, EngineError> {
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
                    return Ok(false);
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
                    step.max_events_for_admitted(&self.routing.providers),
                    step.tool_observations(),
                )
            }
        };
        self.journal
            .admit(self.session_id, tail, observations)
            .await?;
        Ok(true)
    }

    fn review_will_run(
        &self,
        runtime: &ToolRuntime,
        call: &ToolCall,
        review_allowed: bool,
        permission: Permission,
    ) -> Result<bool, EngineError> {
        Ok(self.ledger().command_timeout_limit(call)?.is_none()
            && self.ledger().allows_tool(call)
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
        let (mut decision, mut basis, mut approval) = runtime
            .permissions()
            .map_or((Decision::Allow, PermissionSource::Mode, None), |config| {
                config.evaluate(call)
            });
        let unguarded = call.is_unguarded();
        let researcher = matches!(grant, Grant::Researcher { .. });
        if (runtime.permissions().is_some() || unguarded)
            && (!runtime.allows(call) || researcher && !call.is_child_research())
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
        let unguarded = intent.call.is_unguarded();
        let permission = permission.narrow_terminal(self.terminal_mode(), &intent.call);
        let timeout_limit = match grant {
            Grant::Primary { .. } => self.ledger().command_timeout_limit(&intent.call)?,
            Grant::Researcher { .. } => None,
        };
        let hooks = match config {
            Some(config)
                if !intent.call.is_internal()
                    && permission.decision != Decision::Deny
                    && timeout_limit.is_none()
                    && self.ledger().allows_tool(&intent.call) =>
            {
                // Boxed like the other Tool futures so the hook loop does not inflate every agent step.
                Box::pin(self.pre_tool_use_hooks(grant, runtime, config, &intent, narrowable))
                    .await?
            }
            _ => HookFold::none(),
        };
        let permission =
            permission.narrow_hooks(hooks.restriction, config.map(|config| config.mode()));
        let Permission {
            decision, basis, ..
        } = permission;
        let digest = intent.digest();
        let source = if decision == Decision::Ask {
            PermissionSource::UnavailableApproval
        } else {
            basis
        };
        let (verdict, source) = if let Some((requested, maximum)) = timeout_limit {
            (
                Verdict::Deny(Answer {
                    observation: not_dispatched(
                        intent,
                        ToolDisposition::Limit,
                        crate::tools::types::command_timeout_limit_message(requested, maximum),
                    ),
                    refused: false,
                    active_time: None,
                }),
                source,
            )
        } else if decision == Decision::Deny || hooks.exhausted() {
            let answer = if hooks.exhausted() || source == PermissionSource::Hook {
                hooks.denial(intent)
            } else {
                Answer {
                    observation: not_dispatched(
                        intent,
                        ToolDisposition::Denied,
                        match source {
                            PermissionSource::Rule => "Permission deny rule blocked this Tool",
                            PermissionSource::PinnedGrant => {
                                "Tool operation is outside the pinned grant"
                            }
                            _ => "Permission mode denied this Tool",
                        },
                    ),
                    refused: false,
                    active_time: None,
                }
            };
            (
                match hooks.narrowed.clone() {
                    Some(call) => Verdict::Narrow(answer, call),
                    None => Verdict::Deny(answer),
                },
                source,
            )
        } else {
            match grant {
                Grant::Primary {
                    request,
                    review_allowed,
                } => {
                    Box::pin(self.before_primary_dispatch(
                        runtime,
                        request,
                        *review_allowed && self.ledger().admits(Action::Review),
                        intent,
                        permission,
                    ))
                    .await?
                }
                Grant::Researcher { deadline, .. } => {
                    if !self.ledger().allows_tool(&intent.call) {
                        (
                            Verdict::Deny(Answer {
                                observation: outside_plan(intent),
                                refused: false,
                                active_time: None,
                            }),
                            source,
                        )
                    } else {
                        self.approve_researcher(intent, *deadline, permission)
                            .await?
                    }
                }
            }
        };
        let mode = config
            .map(crate::tools::permissions::PermissionConfig::mode)
            .or_else(|| match grant {
                Grant::Researcher { child, .. } => child
                    .definition
                    .as_ref()
                    .and_then(|definition| definition.permission_mode),
                Grant::Primary { .. } => None,
            });
        if mode.is_some() || unguarded {
            let approved = matches!(&verdict, Verdict::Dispatch { .. });
            // An approving review that did not lead to dispatch is not an AI approval.
            let source = if !approved && source == PermissionSource::AiReview {
                PermissionSource::UnavailableApproval
            } else {
                source
            };
            self.append(Event::ToolPermissionDecided {
                run_id: self.run_id,
                agent_run_id: match &verdict {
                    Verdict::Dispatch { intent, .. } => intent.agent_run_id,
                    Verdict::Deny(answer) | Verdict::Narrow(answer, _) => {
                        answer.observation.intent.agent_run_id
                    }
                },
                verdict: PermissionVerdict {
                    intent_digest: digest,
                    decision,
                    mode,
                    basis,
                    source,
                    approved,
                    hook_restriction: hooks.restriction,
                    hooks: hooks.count,
                    narrowed: matches!(verdict, Verdict::Narrow(..)),
                },
            })
            .await?;
        }
        Ok(verdict)
    }

    async fn before_primary_dispatch(
        &self,
        runtime: &ToolRuntime,
        request: &ProviderRequest,
        review_allowed: bool,
        intent: EffectIntent,
        permission: Permission,
    ) -> Result<(Verdict, PermissionSource), EngineError> {
        let mut source = if permission.decision == Decision::Ask {
            PermissionSource::UnavailableApproval
        } else {
            permission.basis
        };
        let mut unguarded_prompt = None;
        if intent.call.is_unguarded() {
            match unguarded_gate(runtime, request, &intent) {
                Ok(prompt) => unguarded_prompt = Some(prompt),
                Err(message) => return Ok(self.not_offered(intent, message, source)),
            }
        }
        let mut unsaved_rule = false;
        let goal_allows = matches!(intent.call, ToolCall::ParseAgentDefinition { .. })
            || self.ledger().allows_tool(&intent.call);
        let mut approved = true;
        let mut active_time_exhausted = false;
        if goal_allows
            && runtime.allows(&intent.call)
            && let Some(mode) = permission.approval
            && self.ledger().can_start_provider_call()
        {
            let resolvers;
            let chain = if mode == ApprovalMode::Auto && review_allowed {
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
            let resolution = self
                .resolve_ask(
                    runtime,
                    &intent,
                    chain,
                    AskSite::Primary {
                        unguarded_prompt: unguarded_prompt.clone(),
                    },
                )
                .await?;
            approved = resolution.approved;
            source = resolution.source;
            active_time_exhausted = resolution.active_time_exhausted;
            unsaved_rule = resolution.unsaved_rule;
        }
        // An in-process read waits for no Guard capacity, so it reserves its action directly.
        let guard_site = intent.call.facts().site == crate::tools::catalog::Site::Guard;
        let mut queue_pause = if approved
            && guard_site
            && goal_allows
            && !active_time_exhausted
            && self.ledger().can_start_provider_call()
        {
            match self.reserve_active_pause() {
                Ok(pause) => Some(pause),
                Err(EngineError::GoalBudgetExhausted) => {
                    approved = false;
                    active_time_exhausted = true;
                    None
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let observation = if active_time_exhausted {
            active_time_limit_observation(intent)
        } else if !goal_allows {
            outside_plan(intent)
        } else if self.pause_at_boundary() {
            // A pause requested during review or the approval wait stops the Run before dispatch.
            approved = false;
            if let Some(pause) = queue_pause.take() {
                self.settle_active_pause(pause, None)?;
            }
            not_dispatched(
                intent,
                ToolDisposition::Denied,
                "Goal paused before this operation; no operation was dispatched",
            )
        } else if approved && !self.ledger().can_start_provider_call() {
            not_dispatched(
                intent,
                ToolDisposition::Limit,
                "Goal token budget exhausted; no operation was dispatched",
            )
        } else if approved {
            return Ok((
                Verdict::Dispatch {
                    intent,
                    queue_pause,
                },
                source,
            ));
        } else {
            not_dispatched(
                intent,
                ToolDisposition::Denied,
                if unsaved_rule {
                    "The always-allow rule could not be saved; no operation was dispatched"
                } else if unguarded_prompt.is_some() && self.controls.approvals.is_none() {
                    "Unguarded re-run needs interactive approval or an exact-command rule for this project; no operation was dispatched"
                } else {
                    "Approval was not granted; no operation was dispatched"
                },
            )
        };
        self.clear_tool_queue();
        Ok((
            Verdict::Deny(Answer {
                observation,
                refused: !approved,
                active_time: Some(ActiveTime {
                    segment: None,
                    exhausted: active_time_exhausted,
                }),
            }),
            source,
        ))
    }

    fn not_offered(
        &self,
        intent: EffectIntent,
        message: &'static str,
        source: PermissionSource,
    ) -> (Verdict, PermissionSource) {
        self.clear_tool_queue();
        (
            Verdict::Deny(Answer {
                observation: not_dispatched(intent, ToolDisposition::Denied, message),
                refused: false,
                active_time: Some(ActiveTime {
                    segment: None,
                    exhausted: false,
                }),
            }),
            source,
        )
    }

    async fn human_reply(
        &self,
        runtime: &ToolRuntime,
        approvals: &crate::tools::ToolApprovals,
        intent: &EffectIntent,
        prompt: Option<UnguardedPrompt>,
    ) -> (ApprovalReply, bool) {
        let mut cancellation = self.controls.cancellation.clone();
        let expires = intent.expires_at_ms.saturating_sub(crate::tools::now_ms());
        let reply = tokio::select! {
            biased;
            () = cancellation.cancelled() => ApprovalReply::Deny,
            result = tokio::time::timeout(Duration::from_millis(expires), async {
                match prompt.clone() {
                    Some(prompt) => approvals.ask_unguarded(intent, prompt).await,
                    None => ApprovalReply::from(approvals.ask(intent).await),
                }
            }) => result.unwrap_or(ApprovalReply::Deny),
        };
        // The rule is durable before the verdict, so a crash cannot leave a recorded approval without it.
        let unsaved = reply == ApprovalReply::AllowAlwaysInProject
            && prompt.is_some()
            && runtime.record_exact_command_rule(&intent.call).is_err();
        (reply, unsaved)
    }

    async fn dispatch_primary(
        &self,
        runtime: &ToolRuntime,
        intent: EffectIntent,
        queue_pause: Option<ActivePause>,
    ) -> Result<Answer, EngineError> {
        let (observation, segment, exhausted) =
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
                    ),
                    Err(EngineError::GoalBudgetExhausted) => {
                        (active_time_limit_observation(intent), None, true)
                    }
                    Err(error) => return Err(error),
                }
            };
        self.clear_tool_queue();
        Ok(Answer {
            observation,
            refused: false,
            active_time: Some(ActiveTime { segment, exhausted }),
        })
    }

    async fn dispatch_guard(
        &self,
        runtime: &ToolRuntime,
        intent: EffectIntent,
        queue_pause: ActivePause,
    ) -> Result<(ToolObservation, Option<ActiveSegment>, bool), EngineError> {
        let mut queue_pause = Some(queue_pause);
        let mut active_time_exhausted = false;
        let runtime = runtime.with_queue_progress(self.progress.map(ProgressPublisher::tool_queue));
        let (active_sender, mut active_receiver) =
            tokio::sync::mpsc::channel::<ActiveActionRequest>(1);
        let runtime = runtime.with_active_action_gate(active_sender);
        let worker_cancellation = RunCancellation::new();
        let mut run_cancellation = self.controls.cancellation.clone();
        let cache_transfer = Duration::from_millis(u64::from(intent.limits.cache_transfer_ms));
        let action_bound = tool_action_bound(&runtime, &intent);
        let runtime_ms = intent.limits.runtime_ms;
        let mut execution = Box::pin(runtime.execute(intent, worker_cancellation.clone()));
        let mut active_segment = None;
        let mut observation = loop {
            tokio::select! {
                biased;
                () = run_cancellation.cancelled() => {
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
                observation = &mut execution => break observation,
            }
        };
        if let Some(pause) = queue_pause.take() {
            self.settle_active_pause(pause, None)?;
        }
        if active_time_exhausted && observation.disposition != ToolDisposition::Limit {
            observation = active_time_limit_observation(observation.intent);
        }
        Ok((observation, active_segment, active_time_exhausted))
    }

    fn clear_tool_queue(&self) {
        if let Some(progress) = self.progress {
            progress.tool_queue().send_replace(None);
        }
    }

    async fn approve_researcher(
        &self,
        intent: EffectIntent,
        deadline: Instant,
        permission: Permission,
    ) -> Result<(Verdict, PermissionSource), EngineError> {
        let (approved, source) = if let Some(mode) = permission.approval {
            self.researcher_approval(&intent, deadline, mode).await?
        } else {
            (true, permission.basis)
        };
        let verdict = if approved {
            Verdict::Dispatch {
                intent,
                queue_pause: None,
            }
        } else {
            Verdict::Deny(Answer {
                observation: not_dispatched(
                    intent,
                    ToolDisposition::Denied,
                    "Approval was not granted; no operation was dispatched",
                ),
                refused: true,
                active_time: None,
            })
        };
        Ok((verdict, source))
    }

    async fn researcher_approval(
        &self,
        intent: &EffectIntent,
        deadline: Instant,
        mode: ApprovalMode,
    ) -> Result<(bool, PermissionSource), EngineError> {
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        if !runtime.allows(&intent.call) || !self.ledger().can_start_provider_call() {
            return Ok((false, PermissionSource::UnavailableApproval));
        }
        let objective = if mode == ApprovalMode::Auto && self.ledger().admits(Action::Review) {
            Some(
                self.ledger()
                    .view(|view| view.goal(self.goal_id).map(|goal| goal.objective.clone()))?
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
        let resolution = self
            .resolve_ask(runtime, intent, chain, AskSite::Delegated { deadline })
            .await?;
        Ok((resolution.approved, resolution.source))
    }

    async fn resolve_ask(
        &self,
        runtime: &ToolRuntime,
        intent: &EffectIntent,
        chain: &[Resolver<'_>],
        site: AskSite,
    ) -> Result<Resolution, EngineError> {
        let initial_mode = self.terminal_mode();
        let mut resolution = Resolution {
            approved: false,
            source: PermissionSource::UnavailableApproval,
            active_time_exhausted: false,
            unsaved_rule: false,
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
                        Ok(review_approved) => {
                            resolution.approved =
                                review_approved && self.terminal_mode() == initial_mode;
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
                        let idle = self.ledger().in_flight_children() == 0;
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
                    AskSite::Primary { unguarded_prompt } => {
                        if !resolution.active_time_exhausted
                            && !self.pause_at_boundary()
                            && let Some(approvals) = self.controls.approvals
                        {
                            match self.reserve_active_pause() {
                                Ok(pause) => {
                                    let (reply, unsaved) = self
                                        .human_reply(
                                            runtime,
                                            approvals,
                                            intent,
                                            unguarded_prompt.clone(),
                                        )
                                        .await;
                                    resolution.source = PermissionSource::Human;
                                    resolution.approved = reply != ApprovalReply::Deny && !unsaved;
                                    resolution.unsaved_rule = unsaved;
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
    ) -> Result<bool, EngineError> {
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
        let valid = matches!(&result, CallResult::Response(ProviderResponse { outcome: ProviderOutcome::Finish(finish), .. }) if valid_finish(finish, 1024) && matches!(finish.result.as_str(), "approve" | "ask"));
        let valid = valid && self.ledger().tokens_within_limit();
        self.append(Event::ProviderCallRecorded {
            run_id: self.run_id,
            agent_run_id: intent.agent_run_id,
            record: call_record(AgentPhase::ToolReview, &result, valid, None),
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
        if exhausted {
            return Ok(false);
        }
        Ok(
            matches!(&result, CallResult::Response(ProviderResponse { outcome: ProviderOutcome::Finish(finish), .. }) if valid && finish.result == "approve"),
        )
    }
}

fn tool_action_bound(runtime: &ToolRuntime, intent: &EffectIntent) -> Duration {
    runtime
        .reservation_overhead(&intent.call)
        .saturating_add(
            Duration::from_millis(u64::from(intent.limits.cache_transfer_ms)).saturating_mul(2),
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

fn not_dispatched(
    intent: EffectIntent,
    disposition: ToolDisposition,
    output: impl Into<String>,
) -> ToolObservation {
    ToolObservation {
        enforcement_cause: None,
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
) -> Result<UnguardedPrompt, &'static str> {
    let ToolCall::CommandUnguarded {
        program, args, cwd, ..
    } = &intent.call
    else {
        return Err(NO_PRIOR_FAILURE);
    };
    let observed = request
        .input
        .tools
        .as_ref()
        .map(ToolContext::observations)
        .unwrap_or_default();
    if crate::tools::types::uses_guard_alias(args) {
        return Err(
            "Guard path aliases such as /workspace do not exist on the host; an unguarded re-run is not offered; no operation was dispatched",
        );
    }
    let Some(prior) =
        crate::tools::types::qualifying_guard_failure(&intent.call, observed.iter().copied())
    else {
        let denied_path = observed.iter().rev().any(|observation| {
            observation.enforcement_cause == Some(crate::tools::EnforcementCause::FilesystemDenied)
                && matches!(&observation.intent.call,
                    ToolCall::Command { program: p, args: a, cwd: c } if p == program && a == args && c == cwd)
        });
        return Err(if denied_path {
            "The Guard denied a protected path or set-ID entry; an unguarded re-run is not offered; no operation was dispatched"
        } else {
            NO_PRIOR_FAILURE
        });
    };
    Ok(UnguardedPrompt {
        executable: runtime
            .command_executable(program)
            .ok_or(NO_PRIOR_FAILURE)?
            .to_owned(),
        attribution: prior.enforcement_cause,
    })
}

const NO_PRIOR_FAILURE: &str =
    "No identical command failed under the Guard in this Run; no operation was dispatched";

fn outside_plan(intent: EffectIntent) -> ToolObservation {
    not_dispatched(
        intent,
        ToolDisposition::Denied,
        "The effect is outside the active Goal plan; no operation was dispatched",
    )
}

fn active_time_limit_observation(intent: EffectIntent) -> ToolObservation {
    not_dispatched(
        intent,
        ToolDisposition::Limit,
        "Goal active-time budget exhausted; no operation was dispatched",
    )
}

impl RunLoop<'_> {
    pub(super) async fn initialize_definitions(
        &self,
        request: &mut ProviderRequest,
    ) -> Result<bool, EngineError> {
        let Some(runtime) = self.controls.tools else {
            return Ok(true);
        };
        let Some(pin) = runtime.definitions.pin() else {
            return Ok(true);
        };
        let mut outputs = Vec::with_capacity(pin.files.len());
        let mut normalized_bytes = 0;
        for (index, file) in pin.files.iter().enumerate() {
            if self.pause_at_boundary() || self.controls.cancellation.is_cancelled() {
                return Ok(false);
            }
            let call = ToolCall::ParseAgentDefinition {
                index: u16::try_from(index).map_err(|_| EngineError::InvalidRequest)?,
                raw_digest: file.digest,
            };
            let Some(mut intent) = self.primary_intent(runtime, call)? else {
                return Ok(false);
            };
            intent.limits.result_bytes = MAX_TOOL_RESULT_BYTES as u32;
            let grant = Grant::Primary {
                request,
                review_allowed: false,
            };
            let post_call = intent.call.clone();
            let Some((observation, _)) =
                Box::pin(self.run_intent(&grant, runtime, intent, &post_call, false)).await?
            else {
                return Ok(false);
            };
            if observation.disposition != ToolDisposition::Succeeded
                || observation.attestation.is_none()
            {
                return Ok(false);
            }
            normalized_bytes += observation.output.len();
            if normalized_bytes > 1024 * 1024 {
                return Ok(false);
            }
            outputs.push(observation.output);
        }
        let child_model = &self.routing.providers.child[0].identity().model;
        let Ok(registry) = crate::tools::definitions::registry(
            &pin,
            outputs.iter().map(String::as_str),
            child_model,
        ) else {
            return Ok(false);
        };
        let registry = registry.definitions;
        let tools = request
            .input
            .tools
            .as_mut()
            .ok_or(EngineError::InvalidRequest)?;
        let mut catalog: serde_json::Value =
            serde_json::from_str(&tools.catalog).map_err(|_| EngineError::InvalidRequest)?;
        catalog["agents"] = serde_json::json!(
            registry
                .values()
                .map(crate::tools::definitions::AgentDefinition::catalog_entry)
                .collect::<Vec<_>>()
        );
        tools.catalog = serde_json::to_string(&catalog).map_err(|_| EngineError::InvalidRequest)?;
        if tools.catalog.len() > crate::tools::types::MAX_TOOL_CATALOG_BYTES
            || tools.rendered_len() > runtime.context_limit_bytes()
        {
            return Ok(false);
        }
        self.definitions
            .set(registry)
            .map_err(|_| EngineError::CoordinatorFailed)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_definition_modes_never_resolve_parent_asks_and_dont_ask_denies_them() {
        use crate::tools::permissions::PermissionMode;
        let read = ToolCall::Read {
            path: "file".into(),
            offset: 0,
            limit: 1,
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
}

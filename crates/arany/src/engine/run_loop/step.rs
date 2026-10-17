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
    ActiveActionRequest, ActiveToolWindow, ApprovalMode, CacheUpdate, EffectIntent, ToolCall,
    ToolContext, ToolDisposition, ToolError, ToolObservation, ToolRequest, ToolResourceClass,
    ToolRuntime, ToolStep,
};
use std::time::Duration;
use tokio::time::Instant;

const MAX_RESEARCHER_TOOL_RESULT_BYTES: usize = 16 * 1024;

/// The agent a Tool step runs for, with the state its rules read.
pub(super) enum Grant<'s> {
    Primary {
        request: &'s mut ProviderRequest,
        review_allowed: bool,
    },
    /// A read-only child inside its delegation's active segment, which ends at `deadline`.
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

/// What `before_dispatch` decided for one journaled intent.
enum Verdict {
    /// Run the intent; a primary Guard call carries the pause covering its capacity wait.
    Dispatch {
        intent: EffectIntent,
        queue_pause: Option<ActivePause>,
    },
    Deny(Answer),
}

#[derive(Clone, Copy)]
struct Permission {
    decision: Decision,
    basis: PermissionSource,
    approval: Option<ApprovalMode>,
}

impl Permission {
    fn narrow_terminal(mut self, mode: Option<ApprovalMode>, call: &ToolCall) -> Self {
        if self.decision != Decision::Deny
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

/// An intent's observation and what the step settles once it is journaled.
struct Answer {
    observation: ToolObservation,
    /// Not approved to dispatch: the agent stops.
    refused: bool,
    /// The primary's active-time segment and exhaustion; `None` settles nothing.
    active_time: Option<ActiveTime>,
}

struct ActiveTime {
    segment: Option<ActiveSegment>,
    exhausted: bool,
}

impl RunLoop<'_> {
    /// Runs one Tool request and any changeset integration it produces, then records the step;
    /// `false` stops the agent.
    pub(super) async fn execute_step(
        &self,
        mut grant: Grant<'_>,
        request: ToolRequest,
        turn: ProviderTurn,
    ) -> Result<bool, EngineError> {
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        let mut call = request.call.clone();
        let calls_remaining = match &grant {
            Grant::Primary { .. } => None,
            Grant::Researcher { child, .. } => Some(child.tool_calls_remaining.saturating_sub(1)),
        };
        let mut step = ToolStep {
            turn,
            requests: vec![request],
            observations: Vec::new(),
            facts: None,
            steering_notes: Vec::new(),
            calls_remaining,
        };
        loop {
            let Some(intent) = self.sized_intent(&grant, runtime, &step, call)? else {
                return Ok(false);
            };
            let command = matches!(intent.call, ToolCall::Command { .. });
            let source_intent_digest = intent.digest();
            // Boxed: approval and dispatch futures otherwise inflate every enclosing agent step.
            let Some(observation) = Box::pin(self.run_intent(&grant, runtime, intent)).await?
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
            let Some(changeset) = changeset.filter(|changeset| !changeset.files.is_empty()) else {
                break;
            };
            if self.controls.cancellation.is_cancelled()
                || grant.context()?.reserved_len(&step, &[]) > runtime.context_limit_bytes()
            {
                return Ok(false);
            }
            call = ToolCall::ApplyChangeset {
                source_intent_digest,
                changeset,
            };
            if !call.valid() {
                return Ok(false);
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

    /// The intent with its runtime and result bounds; `None` when no bound leaves room to run it.
    fn sized_intent(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        step: &ToolStep,
        call: ToolCall,
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
        let integration =
            matches!(intent.call, ToolCall::Command { .. }).then(|| ToolCall::ApplyChangeset {
                source_intent_digest: intent.digest(),
                changeset: CommandChangeset { files: Vec::new() },
            });
        let reserved: Vec<_> = std::iter::once(&intent.call).chain(&integration).collect();
        let overhead = grant.context()?.reserved_len(step, &reserved);
        let integration_capacity = if integration.is_some() {
            MAX_TOOL_ARGUMENT_BYTES + 128 * 6 + 512
        } else {
            0
        };
        let Some(capacity) = runtime.result_capacity(overhead, integration_capacity) else {
            return Ok(None);
        };
        let ceiling = match grant {
            Grant::Primary { .. } => MAX_TOOL_RESULT_BYTES,
            Grant::Researcher { .. } => MAX_RESEARCHER_TOOL_RESULT_BYTES,
        };
        intent.limits.result_bytes = if matches!(intent.call, ToolCall::ApplyChangeset { .. }) {
            128
        } else {
            capacity.min(ceiling) as u32
        };
        Ok(Some(intent))
    }

    fn primary_intent(
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

    /// Asks the ledger, then journals the intent and its observation around the dispatch
    /// decision; `None` stops the agent.
    async fn run_intent(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        intent: EffectIntent,
    ) -> Result<Option<ToolObservation>, EngineError> {
        let permission = self.permission(grant, runtime, &intent.call);
        if !self.admit_tool(grant, runtime, &intent, permission).await? {
            return Ok(None);
        }
        let agent_run_id = intent.agent_run_id;
        // A researcher's Tool runs inside its delegation's reservation.
        let action_ms = match grant {
            Grant::Primary { .. } => tool_action_ms(runtime, &intent),
            Grant::Researcher { .. } => 0,
        };
        self.append(Event::ToolStarted {
            run_id: self.run_id,
            agent_run_id,
            intent: intent.clone(),
            active_ceiling_ms: self.ledger().active_ceiling(action_ms)?,
        })
        .await?;
        let answer = match self
            .before_dispatch(grant, runtime, intent, permission)
            .await?
        {
            Verdict::Deny(answer) => answer,
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
        Ok((!answer.refused).then_some(answer.observation))
    }

    /// Admits the step's Event tail; `false` stops the agent before anything is journaled.
    async fn admit_tool(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        intent: &EffectIntent,
        permission: Permission,
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
                (
                    Action::Tool.remaining(1, &self.routing.providers),
                    Action::Tool.tool_observations(),
                )
            }
            Grant::Researcher { .. } => {
                let step = Action::Child {
                    in_flight: self.ledger().in_flight_children(),
                    research: true,
                };
                (
                    step.max_events_for(&self.routing.providers),
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
        let (mut decision, mut basis, mut approval) = runtime
            .permissions()
            .map_or((Decision::Allow, PermissionSource::Mode, None), |config| {
                config.evaluate(call)
            });
        if runtime.permissions().is_some()
            && (!runtime.allows(call)
                || matches!(grant, Grant::Researcher { .. }) && !call.is_child_research())
        {
            decision = decision.narrow(Decision::Deny);
            basis = PermissionSource::PinnedGrant;
            approval = None;
        }
        Permission {
            decision,
            basis,
            approval,
        }
    }

    /// The single point that decides whether a journaled intent runs: timeout denial, the Goal
    /// plan, AI review, human approval and pauses.
    async fn before_dispatch(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        intent: EffectIntent,
        permission: Permission,
    ) -> Result<Verdict, EngineError> {
        let config = runtime.permissions();
        let permission = permission.narrow_terminal(self.terminal_mode(), &intent.call);
        let Permission {
            decision, basis, ..
        } = permission;
        let digest = intent.digest();
        let source = if decision == Decision::Ask {
            PermissionSource::UnavailableApproval
        } else {
            basis
        };
        let (verdict, source) = if decision == Decision::Deny {
            (
                Verdict::Deny(Answer {
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
                }),
                source,
            )
        } else {
            match grant {
                Grant::Primary {
                    request,
                    review_allowed,
                } => {
                    self.before_primary_dispatch(
                        runtime,
                        request,
                        *review_allowed && self.ledger().admits(Action::Review),
                        intent,
                        permission,
                    )
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
        if let Some(config) = config {
            self.append(Event::ToolPermissionDecided {
                run_id: self.run_id,
                agent_run_id: match &verdict {
                    Verdict::Dispatch { intent, .. } => intent.agent_run_id,
                    Verdict::Deny(answer) => answer.observation.intent.agent_run_id,
                },
                verdict: PermissionVerdict {
                    intent_digest: digest,
                    decision,
                    mode: config.mode(),
                    basis,
                    source,
                    approved: matches!(&verdict, Verdict::Dispatch { .. }),
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
        if let Some((requested, maximum)) = self.ledger().command_timeout_limit(&intent.call)? {
            let output = crate::tools::types::command_timeout_limit_message(requested, maximum);
            return Ok((
                Verdict::Deny(Answer {
                    observation: not_dispatched(intent, ToolDisposition::Limit, output),
                    refused: false,
                    active_time: None,
                }),
                source,
            ));
        }
        let goal_allows = self.ledger().allows_tool(&intent.call);
        let mut approved = true;
        let mut active_time_exhausted = false;
        if goal_allows
            && runtime.allows(&intent.call)
            && let Some(mode) = permission.approval
            && self.ledger().can_start_provider_call()
        {
            approved = false;
            source = PermissionSource::UnavailableApproval;
            let initial_mode = self.terminal_mode();
            if mode == ApprovalMode::Auto && review_allowed {
                match self
                    .review_tool(&request.input.objective, &intent, None)
                    .await
                {
                    Ok(review_approved) => {
                        approved = review_approved && self.terminal_mode() == initial_mode;
                        source = if approved {
                            PermissionSource::AiReview
                        } else {
                            PermissionSource::UnavailableApproval
                        };
                        active_time_exhausted = self.ledger().active_time_exhausted();
                    }
                    Err(EngineError::GoalBudgetExhausted)
                        if self.ledger().active_time_exhausted() =>
                    {
                        active_time_exhausted = true;
                    }
                    Err(error) => return Err(error),
                }
            }
            if !approved
                && !active_time_exhausted
                && !self.pause_at_boundary()
                && let Some(approvals) = self.controls.approvals
            {
                source = PermissionSource::Human;
                match self.reserve_active_pause() {
                    Ok(pause) => {
                        let mut cancellation = self.controls.cancellation.clone();
                        let expires = intent.expires_at_ms.saturating_sub(crate::tools::now_ms());
                        approved = tokio::select! {
                            biased;
                            () = cancellation.cancelled() => false,
                            result = tokio::time::timeout(Duration::from_millis(expires), approvals.ask(&intent)) => result.unwrap_or(false),
                        };
                        let resume_at = approved.then(Instant::now);
                        self.settle_active_pause(pause, resume_at)?;
                    }
                    Err(EngineError::GoalBudgetExhausted) => active_time_exhausted = true,
                    Err(error) => return Err(error),
                }
            }
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
                "Approval was not granted; no operation was dispatched",
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

    /// Runs an approved primary intent inside its own active-time segment.
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
                self.dispatch_guard(runtime, intent, queue_pause).await?
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

    /// Runs a Guard intent; its active-time segment starts when the Guard admits it, which ends
    /// the pause covering the capacity wait.
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
        let execution = runtime.execute(intent, worker_cancellation.clone());
        tokio::pin!(execution);
        let mut active_segment = None;
        let mut observation = loop {
            tokio::select! {
                biased;
                () = run_cancellation.cancelled() => {
                    worker_cancellation.cancel();
                    break execution.await;
                }
                request = active_receiver.recv(), if active_segment.is_none() => {
                    if let Some((admitted_at, reply)) = request {
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
        let initial_mode = self.terminal_mode();
        if mode == ApprovalMode::Auto && self.ledger().admits(Action::Review) {
            let objective = self
                .ledger()
                .view(|view| view.goal(self.goal_id).map(|goal| goal.objective.clone()))?
                .ok_or(EngineError::InvalidHistory)?;
            if self.review_tool(&objective, intent, Some(deadline)).await?
                && self.terminal_mode() == initial_mode
            {
                return Ok((
                    !self.pause_requested() && !self.controls.cancellation.is_cancelled(),
                    PermissionSource::AiReview,
                ));
            }
        }
        let Some(approvals) = self.controls.approvals else {
            return Ok((false, PermissionSource::UnavailableApproval));
        };
        let mut cancellation = self.controls.cancellation.clone();
        let expires =
            Duration::from_millis(intent.expires_at_ms.saturating_sub(crate::tools::now_ms()));
        // A sibling Provider call keeps the shared delegation interval active.
        let idle = self.ledger().in_flight_children() == 0;
        let started = Instant::now();
        let approved = tokio::select! {
            biased;
            () = cancellation.cancelled() => false,
            result = tokio::time::timeout_at(deadline.min(Instant::now() + expires), approvals.ask(intent)) => result.unwrap_or(false),
        };
        if idle {
            self.ledger().exclude_idle_approval(started.elapsed())?;
        }
        Ok((
            approved && !self.pause_requested() && !self.controls.cancellation.is_cancelled(),
            PermissionSource::Human,
        ))
    }

    async fn review_tool(
        &self,
        objective: &str,
        intent: &EffectIntent,
        delegation_deadline: Option<Instant>,
    ) -> Result<bool, EngineError> {
        let provider = self
            .routing
            .provider(crate::provider::ProviderRole::Review, intent.agent_run_id)?;
        let mut review = ProviderRequest {
            session_id: self.session_id,
            run_id: self.run_id,
            agent_run_id: intent.agent_run_id,
            phase: AgentPhase::ToolReview,
            collaboration: CollaborationPolicy::Single,
            model: provider.identity().model.clone(),
            max_output_tokens: 256.min(provider.identity().limits.max_output_tokens),
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
        self.settle_result(&result);
        let valid = matches!(&result, CallResult::Response(ProviderResponse { outcome: ProviderOutcome::Finish(finish), .. }) if valid_finish(finish, 1024) && matches!(finish.result.as_str(), "approve" | "ask"));
        let valid = valid && self.ledger().tokens_within_limit();
        self.append(Event::ProviderCallRecorded {
            run_id: self.run_id,
            agent_run_id: intent.agent_run_id,
            record: call_record(AgentPhase::ToolReview, &result, valid),
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

/// The active interval a Tool reserves once dispatched: setup, cache transfers, payload and cleanup.
fn tool_action_bound(runtime: &ToolRuntime, intent: &EffectIntent) -> Duration {
    runtime
        .reservation_overhead(&intent.call)
        .saturating_add(
            Duration::from_millis(u64::from(intent.limits.cache_transfer_ms)).saturating_mul(2),
        )
        .saturating_add(Duration::from_millis(u64::from(intent.limits.runtime_ms)))
        .saturating_add(TOOL_CLEANUP_OVERHEAD)
}

/// A Tool's reservation with its Store tail, as an action-start ceiling counts it.
fn tool_action_ms(runtime: &ToolRuntime, intent: &EffectIntent) -> u64 {
    duration_ms(tool_action_bound(runtime, intent).saturating_add(STORE_ACTION_OVERHEAD))
}

/// The deadlines the Guard must meet inside `segment`; a passed deadline refuses the action.
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

/// Runs the intent; a Run cancellation cancels the worker and still waits for its observation.
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

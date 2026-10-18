use super::{ActiveTime, Answer, Grant, active_time_limit_observation, not_dispatched};
use crate::engine::ledger::duration_ms;
use crate::engine::run_loop::{RunLoop, TOOL_CLEANUP_OVERHEAD};
use crate::engine::{EngineError, STORE_ACTION_OVERHEAD};
use crate::session::Event;
use crate::tools::permissions::{
    CommandHook, ConfigError, Decision, HOOK_RESULT_BYTES, HookEvent, HookInvocation, HookOutcome,
    HookResult, MAX_HOOK_FEEDBACK_BYTES, MAX_HOOK_INPUT_BYTES, PermissionConfig, feedback_lines,
    narrows, peer_name, post_tool_use_input, pre_tool_use_input,
};
use crate::tools::types::model_observation;
use crate::tools::{EffectIntent, ToolCall, ToolDisposition, ToolObservation, ToolRuntime};

const MIN_HOOK_RUNTIME_MS: u32 = 1_000;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(in crate::engine::run_loop) enum HookStop {
    None,
    Uncertain,
    ActiveTime,
}

pub(super) struct HookFold {
    pub(super) restriction: Option<Decision>,
    pub(super) count: u8,
    pub(super) narrowed: Option<ToolCall>,
    reason: Option<String>,
    stop: HookStop,
}

pub(super) struct PostFold {
    pub(super) feedback: Vec<String>,
    pub(super) stop: HookStop,
}

struct HookRun {
    result: HookResult,
    stops: bool,
    exhausted: bool,
}

struct HookPlan {
    group: u8,
    handler: u8,
    call: ToolCall,
    timeout_ms: u32,
}

impl HookFold {
    pub(super) fn none() -> Self {
        Self {
            restriction: None,
            count: 0,
            narrowed: None,
            reason: None,
            stop: HookStop::None,
        }
    }

    fn deny(reason: &str, count: u8) -> Self {
        Self {
            restriction: Some(Decision::Deny),
            count,
            narrowed: None,
            reason: Some(reason.into()),
            stop: HookStop::None,
        }
    }

    pub(super) fn exhausted(&self) -> bool {
        self.stop == HookStop::ActiveTime
    }

    pub(super) fn denial(&self, intent: EffectIntent) -> Answer {
        match self.stop {
            HookStop::ActiveTime => Answer {
                observation: active_time_limit_observation(intent),
                refused: false,
                active_time: Some(ActiveTime {
                    segment: None,
                    exhausted: true,
                }),
            },
            HookStop::Uncertain => Answer {
                observation: not_dispatched(
                    intent,
                    ToolDisposition::Denied,
                    "Hook outcome uncertain; no operation was dispatched",
                ),
                refused: true,
                active_time: None,
            },
            HookStop::None if self.narrowed.is_some() => Answer {
                observation: not_dispatched(
                    intent,
                    ToolDisposition::Denied,
                    format!(
                        "Hook narrowed this Tool to: {}",
                        self.narrowed
                            .as_ref()
                            .map_or_else(Default::default, peer_name)
                    ),
                ),
                refused: false,
                active_time: None,
            },
            HookStop::None => {
                let room = (intent.limits.result_bytes as usize).saturating_sub(32);
                let reason = self.reason.as_deref().unwrap_or_default();
                let reason: String = reason
                    .chars()
                    .scan(0, |bytes, ch| {
                        *bytes += ch.len_utf8();
                        (*bytes <= room).then_some(ch)
                    })
                    .collect();
                let output = if reason.is_empty() {
                    "Hook denied this Tool".to_owned()
                } else {
                    format!("Hook denied this Tool: {reason}")
                };
                Answer {
                    observation: not_dispatched(intent, ToolDisposition::Denied, output),
                    refused: false,
                    active_time: None,
                }
            }
        }
    }
}

// The replacement must narrow the call and be no more permissive than the original under the pinned rules.
fn narrowing_holds(
    runtime: &ToolRuntime,
    config: &PermissionConfig,
    original: &ToolCall,
    updated: &ToolCall,
) -> bool {
    let decision = config.decision(updated);
    narrows(original, updated)
        && config.decision(original).narrow(decision) == decision
        && runtime.allows(updated)
}

fn hook_call(runtime: &ToolRuntime, hook: &CommandHook) -> Option<ToolCall> {
    let call = match hook.args() {
        None => ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), hook.command().into()],
            cwd: String::new(),
        },
        Some(args) => ToolCall::Command {
            program: hook.command().into(),
            args: args.to_vec(),
            cwd: String::new(),
        },
    };
    runtime.allows(&call).then_some(call)
}

impl RunLoop<'_> {
    fn hook_plans(
        &self,
        runtime: &ToolRuntime,
        config: &PermissionConfig,
        event: HookEvent,
        call: &ToolCall,
    ) -> Result<Vec<HookPlan>, &'static str> {
        let matched = config
            .matched_tool_hooks(event, call)
            .map_err(|error| match error {
                ConfigError::Limit(_) => "hook limit",
                _ => "hook configuration blocks this Tool",
            })?;
        matched
            .into_iter()
            .map(|(group, handler, hook)| {
                Ok(HookPlan {
                    group,
                    handler,
                    call: hook_call(runtime, hook).ok_or("hook program is not pinned")?,
                    timeout_ms: u32::try_from(hook.timeout_seconds().saturating_mul(1000))
                        .unwrap_or(u32::MAX),
                })
            })
            .collect()
    }

    // Upper bound of Events and active time one step's hooks can add: its own PreToolUse set plus the
    // PostToolUse set of the requested Tool, which runs after a Command's integration step.
    pub(super) fn hook_admission(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        call: &ToolCall,
        post_call: &ToolCall,
    ) -> (usize, u64, u64) {
        let Some(config) = runtime.permissions() else {
            return (0, 0, 0);
        };
        if matches!(grant, Grant::Researcher { .. }) || call.is_internal() {
            return (0, 0, 0);
        }
        let (Ok(pre), Ok(post)) = (
            self.hook_plans(runtime, config, HookEvent::PreToolUse, call),
            self.hook_plans(runtime, config, HookEvent::PostToolUse, post_call),
        ) else {
            return (0, 0, 0);
        };
        let active_ms = |plans: &[HookPlan]| {
            plans
                .iter()
                .map(|plan| {
                    duration_ms(
                        runtime
                            .reservation_overhead(&plan.call)
                            .saturating_add(TOOL_CLEANUP_OVERHEAD)
                            .saturating_add(STORE_ACTION_OVERHEAD),
                    )
                    .saturating_add(u64::from(plan.timeout_ms))
                })
                .sum::<u64>()
                .min(self.ledger().active_remaining_ms().unwrap_or(u64::MAX))
        };
        (pre.len() + post.len(), active_ms(&pre), active_ms(&post))
    }

    // Room the next request keeps for the feedback lines PostToolUse hooks may add.
    pub(super) fn feedback_reserve(&self, runtime: &ToolRuntime, call: &ToolCall) -> usize {
        let Some(config) = runtime.permissions() else {
            return 0;
        };
        match self.hook_plans(runtime, config, HookEvent::PostToolUse, call) {
            Ok(plans) if !plans.is_empty() =>
            // JSON escaping can double a line of quotes or backslashes.
            {
                2 * MAX_HOOK_FEEDBACK_BYTES + 256
            }
            _ => 0,
        }
    }

    pub(super) async fn pre_tool_use_hooks(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        config: &PermissionConfig,
        intent: &EffectIntent,
        narrowable: bool,
    ) -> Result<HookFold, EngineError> {
        let plans = match self.hook_plans(runtime, config, HookEvent::PreToolUse, &intent.call) {
            Ok(plans) => plans,
            Err(reason) => return Ok(HookFold::deny(reason, 0)),
        };
        // A PostToolUse set that cannot run after the effect denies the Tool before it.
        let post = match self.hook_plans(runtime, config, HookEvent::PostToolUse, &intent.call) {
            Ok(plans) => plans,
            Err(reason) => return Ok(HookFold::deny(reason, 0)),
        };
        if plans.is_empty() && post.is_empty() {
            return Ok(HookFold::none());
        }
        if matches!(grant, Grant::Researcher { .. }) {
            return Ok(HookFold::deny(
                "hooks are unavailable to delegated research",
                0,
            ));
        }
        if !post.is_empty() {
            // The bare form is the smallest stdin a post hook can get, so it must fit the Guard task.
            let Ok(bare) = post_tool_use_input(
                self.session_id,
                config.mode(),
                &intent.call,
                intent.id,
                serde_json::json!("x".repeat(MAX_HOOK_INPUT_BYTES + 1)),
                0,
            ) else {
                return Ok(HookFold::deny("hook input exceeds its limit", 0));
            };
            if !self.task_fits_all(&runtime.with_hook_stdin(&bare), intent.agent_run_id, &post) {
                return Ok(HookFold::deny("hook input exceeds the Guard task limit", 0));
            }
        }
        if plans.is_empty() {
            return Ok(HookFold::none());
        }
        let Ok(input) = pre_tool_use_input(self.session_id, config.mode(), &intent.call, intent.id)
        else {
            return Ok(HookFold::deny("hook input exceeds its limit", 0));
        };
        let input_digest = crate::tools::types::digest(input.as_bytes());
        let subject_digest = intent.digest();
        let runtime = runtime.with_hook_stdin(&input);
        // The complete Guard task must encode before any hook starts; a rejected task would strand it.
        if !self.task_fits_all(&runtime, intent.agent_run_id, &plans) {
            return Ok(HookFold::deny("hook input exceeds the Guard task limit", 0));
        }
        let mut fold = HookFold::none();
        for plan in plans {
            let Some(run) = self
                .run_hook(
                    &runtime,
                    intent.agent_run_id,
                    HookEvent::PreToolUse,
                    &plan,
                    (subject_digest, input_digest),
                    None,
                )
                .await?
            else {
                fold.stop = HookStop::ActiveTime;
                return Ok(fold);
            };
            let HookRun {
                result,
                stops,
                exhausted,
            } = run;
            fold.count += 1;
            if stops {
                fold.stop = HookStop::Uncertain;
                fold.restriction = Some(Decision::Deny);
                return Ok(fold);
            }
            if exhausted {
                fold.stop = HookStop::ActiveTime;
                fold.restriction = Some(Decision::Deny);
                return Ok(fold);
            }
            match result {
                HookResult::Restricted {
                    decision: Decision::Deny,
                    reason,
                } => {
                    fold.restriction = Some(Decision::Deny);
                    fold.reason = reason;
                    return Ok(fold);
                }
                HookResult::Narrowed { call } if call == intent.call => {}
                HookResult::Narrowed { call } => {
                    fold.restriction = Some(Decision::Deny);
                    if !narrowable {
                        fold.reason = Some("hook narrowing limit".into());
                    } else if narrowing_holds(&runtime, config, &intent.call, &call) {
                        fold.narrowed = Some(call);
                    } else {
                        fold.reason = Some("hook input update does not narrow".into());
                    }
                    return Ok(fold);
                }
                HookResult::Restricted {
                    decision: Decision::Ask,
                    ..
                } => fold.restriction = Some(Decision::Ask),
                _ => {}
            }
        }
        Ok(fold)
    }

    // Runs after the Tool's `ToolFinished`: feedback never changes what the Tool did.
    pub(super) async fn post_tool_use_hooks(
        &self,
        runtime: &ToolRuntime,
        config: &PermissionConfig,
        observation: &ToolObservation,
    ) -> Result<PostFold, EngineError> {
        let mut fold = PostFold {
            feedback: Vec::new(),
            stop: HookStop::None,
        };
        let call = &observation.intent.call;
        let Ok(plans) = self.hook_plans(runtime, config, HookEvent::PostToolUse, call) else {
            return Ok(fold);
        };
        if plans.is_empty() {
            return Ok(fold);
        }
        // Halve the response budget until every Guard task encodes; admission proved the bare form fits.
        let response = model_observation(observation);
        let mut budget = MAX_HOOK_INPUT_BYTES;
        let (input, runtime) = loop {
            let input = post_tool_use_input(
                self.session_id,
                config.mode(),
                call,
                observation.intent.id,
                response.clone(),
                budget,
            )
            .map_err(|_| EngineError::InvalidRequest)?;
            let sized = runtime.with_hook_stdin(&input);
            if self.task_fits_all(&sized, observation.intent.agent_run_id, &plans) {
                break (input, sized);
            }
            if budget == 0 {
                return Err(EngineError::InvalidRequest);
            }
            budget = if budget < 2048 { 0 } else { budget / 2 };
        };
        let input_digest = crate::tools::types::digest(input.as_bytes());
        let subject_digest = observation.intent.digest();
        let mut results = Vec::new();
        for plan in plans {
            let Some(run) = self
                .run_hook(
                    &runtime,
                    observation.intent.agent_run_id,
                    HookEvent::PostToolUse,
                    &plan,
                    (subject_digest, input_digest),
                    None,
                )
                .await?
            else {
                fold.stop = HookStop::ActiveTime;
                break;
            };
            let stop = if run.stops {
                HookStop::Uncertain
            } else if run.exhausted {
                HookStop::ActiveTime
            } else {
                HookStop::None
            };
            results.push(run.result);
            if stop != HookStop::None {
                fold.stop = stop;
                break;
            }
        }
        fold.feedback = feedback_lines(&results);
        Ok(fold)
    }

    pub(in crate::engine) async fn lifecycle_hooks(
        &self,
        agent: crate::session::AgentRunId,
        event: HookEvent,
        subject: &str,
        message: &str,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Option<String>, EngineError> {
        // Boxed so the hook state does not inflate the run loop's future.
        Box::pin(self.run_lifecycle_hooks(agent, event, subject, message, deadline)).await
    }

    async fn run_lifecycle_hooks(
        &self,
        agent: crate::session::AgentRunId,
        event: HookEvent,
        subject: &str,
        message: &str,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Option<String>, EngineError> {
        use crate::tools::permissions::{
            MAX_STOP_CONTINUATIONS, compact_subject, lifecycle_input, stop_feedback, stop_subject,
        };
        let Some(runtime) = self.controls.tools else {
            return Ok(None);
        };
        let Some(config) = runtime.permissions() else {
            return Ok(None);
        };
        let (index, continuations, existing) = self.ledger().view(|view| {
            let run = view
                .runs
                .iter()
                .find(|run| run.id == self.run_id)
                .ok_or(EngineError::InvalidHistory)?;
            let actor = run
                .agents
                .iter()
                .find(|view| view.id == agent)
                .ok_or(EngineError::InvalidHistory)?;
            let digest = if event == HookEvent::PreCompact {
                compact_subject(subject, actor.provider_calls.len())
            } else {
                stop_subject(agent, actor.provider_calls.len().saturating_sub(1))
            };
            Ok::<_, EngineError>((
                actor.provider_calls.len(),
                run.stop_continuations(agent),
                run.lifecycle_hooks.iter().any(|hook| {
                    hook.invocation.event == event && hook.invocation.subject_digest == digest
                }),
            ))
        })??;
        if event != HookEvent::PreCompact && continuations >= MAX_STOP_CONTINUATIONS {
            return Ok(None);
        }
        if existing {
            return Ok(Some("hook occurrence already attempted".into()));
        }
        let matched = config.matched_hooks(event, |group| group.matches(subject));
        let plans = matched.map_err(|_| "hook limit").and_then(|matched| {
            matched
                .into_iter()
                .map(|(group, handler, hook)| {
                    Ok(HookPlan {
                        group,
                        handler,
                        call: hook_call(runtime, hook).ok_or("hook program is not pinned")?,
                        timeout_ms: u32::try_from(hook.timeout_seconds() * 1000)
                            .unwrap_or(u32::MAX),
                    })
                })
                .collect::<Result<Vec<_>, _>>()
        });
        let plans = match plans {
            Ok(plans) => plans,
            Err(reason) => {
                return self
                    .block_lifecycle(agent, event, subject, index, reason)
                    .await
                    .map(Some);
            }
        };
        if plans.is_empty() {
            return Ok(None);
        }
        self.journal
            .admit(
                self.session_id,
                2 * plans.len()
                    + self
                        .primary_call()
                        .max_events_for_admitted(&self.routing.providers),
                plans.len(),
            )
            .await?;
        let mut budget = MAX_HOOK_INPUT_BYTES;
        let (input, runtime) = loop {
            let input = lifecycle_input(
                self.session_id,
                config.mode(),
                event,
                continuations > 0,
                message,
                subject,
                budget,
            );
            let sized = runtime.with_hook_stdin(&input);
            if self.task_fits_all(&sized, agent, &plans) {
                break (input, sized);
            }
            if budget == 0 {
                return Err(EngineError::InvalidRequest);
            }
            budget = if budget < 2048 { 0 } else { budget / 2 };
        };
        let digest = if event == HookEvent::PreCompact {
            compact_subject(subject, index)
        } else {
            stop_subject(agent, index.saturating_sub(1))
        };
        for plan in plans {
            let Some(run) = self
                .run_hook(
                    &runtime,
                    agent,
                    event,
                    &plan,
                    (digest, crate::tools::types::digest(input.as_bytes())),
                    deadline,
                )
                .await?
            else {
                return Err(EngineError::GoalBudgetExhausted);
            };
            if self.controls.cancellation.is_cancelled() {
                return Err(EngineError::GoalBudgetExhausted);
            }
            if run.stops {
                return Err(crate::tools::ToolError::Uncertain.into());
            }
            if run.exhausted {
                return Err(EngineError::GoalBudgetExhausted);
            }
            if self.controls.cancellation.is_cancelled() {
                return Err(EngineError::GoalBudgetExhausted);
            }
            if matches!(run.result, HookResult::Blocked { .. }) {
                return Ok(Some(stop_feedback([&run.result])));
            }
        }
        Ok(None)
    }

    fn lifecycle_ceiling(&self, pending_ms: u64) -> Result<u64, EngineError> {
        let ceiling = self.ledger().active_ceiling(pending_ms)?;
        let remaining = self.ledger().view(|view| {
            view.goal(self.goal_id).and_then(|goal| {
                goal.effective_budget()
                    .max_active_ms
                    .map(|limit| limit.saturating_sub(goal.active_ms))
            })
        })?;
        Ok(ceiling.min(remaining.unwrap_or(u64::MAX)))
    }

    async fn block_lifecycle(
        &self,
        agent: crate::session::AgentRunId,
        event: HookEvent,
        subject: &str,
        index: usize,
        reason: &str,
    ) -> Result<String, EngineError> {
        use crate::tools::permissions::{compact_subject, stop_subject};
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        let config = runtime.permissions().ok_or(EngineError::InvalidRequest)?;
        let input = crate::tools::permissions::lifecycle_input(
            self.session_id,
            config.mode(),
            event,
            false,
            "",
            subject,
            MAX_HOOK_INPUT_BYTES,
        );
        let intent = runtime.intent(
            self.run_id,
            agent,
            ToolCall::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "exit 2".into()],
                cwd: String::new(),
            },
        );
        self.journal
            .admit(
                self.session_id,
                2 + self
                    .primary_call()
                    .max_events_for_admitted(&self.routing.providers),
                1,
            )
            .await?;
        self.append(Event::HookStarted {
            run_id: self.run_id,
            agent_run_id: agent,
            hook: HookInvocation {
                event,
                group: 0,
                handler: 0,
                subject_digest: if event == HookEvent::PreCompact {
                    compact_subject(subject, index)
                } else {
                    stop_subject(agent, index.saturating_sub(1))
                },
                intent: intent.clone(),
                input_digest: crate::tools::types::digest(input.as_bytes()),
                active_ceiling_ms: if self.ledger().view(|view| {
                    view.active_goal()
                        .is_some_and(|goal| goal.active_run.is_some())
                })? {
                    Some(self.lifecycle_ceiling(0)?)
                } else {
                    self.ledger().view(|view| view.active_goal().map(|_| 1))?
                },
            },
        })
        .await?;
        let observation = not_dispatched(intent, ToolDisposition::Denied, reason);
        self.append(Event::HookFinished {
            run_id: self.run_id,
            agent_run_id: agent,
            outcome: HookOutcome {
                result: HookResult::for_event(event, &observation),
                observation,
                boundary_active_ms: self.ledger().view(|view| {
                    view.active_goal()
                        .filter(|goal| goal.active_run.is_none())
                        .map(|_| 0)
                })?,
            },
        })
        .await?;
        Ok(reason.into())
    }

    fn task_fits_all(
        &self,
        runtime: &ToolRuntime,
        agent_run_id: crate::session::AgentRunId,
        plans: &[HookPlan],
    ) -> bool {
        plans.iter().all(|plan| {
            let mut probe = runtime.intent(self.run_id, agent_run_id, plan.call.clone());
            probe.limits.runtime_ms = u32::MAX;
            probe.expires_at_ms = u64::MAX;
            runtime.hook_task_fits(&probe)
        })
    }

    async fn dispatch_hook(
        &self,
        runtime: &ToolRuntime,
        hook_intent: EffectIntent,
        pause: Option<crate::engine::run_loop::ActivePause>,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<
        (
            ToolObservation,
            Option<crate::engine::run_loop::ActiveSegment>,
            bool,
        ),
        EngineError,
    > {
        Ok(if let Some(pause) = pause {
            self.dispatch_guard(runtime, hook_intent, pause).await?
        } else {
            let deadline = deadline.expect("delegated hook deadline");
            let cancellation = crate::engine::RunCancellation::new();
            let mut parent = self.controls.cancellation.clone();
            let execution = runtime.execute(hook_intent, cancellation.clone());
            tokio::pin!(execution);
            let observation = tokio::select! {
                observation = &mut execution => observation,
                () = tokio::time::sleep_until(deadline) => { cancellation.cancel(); execution.await },
                () = parent.cancelled() => { cancellation.cancel(); execution.await },
            };
            (observation, None, tokio::time::Instant::now() >= deadline)
        })
    }

    // `None` means the Goal had no active time left to start the hook.
    async fn run_hook(
        &self,
        runtime: &ToolRuntime,
        agent_run_id: crate::session::AgentRunId,
        event: HookEvent,
        plan: &HookPlan,
        digests: ([u8; 32], [u8; 32]),
        mut deadline: Option<tokio::time::Instant>,
    ) -> Result<Option<HookRun>, EngineError> {
        let fixed_ms = duration_ms(
            runtime
                .reservation_overhead(&plan.call)
                .saturating_add(TOOL_CLEANUP_OVERHEAD)
                .saturating_add(STORE_ACTION_OVERHEAD),
        );
        let boundary = self.ledger().view(|view| {
            view.runs
                .iter()
                .find(|run| run.id == self.run_id)
                .is_some_and(|run| run.status != crate::session::RunStatus::Active)
        })?;
        let boundary_goal = self.ledger().view(|view| {
            view.active_goal()
                .filter(|goal| {
                    goal.active_run.is_none() && goal.run_ids.last() == Some(&self.run_id)
                })
                .map(|goal| (goal.active_ms, goal.effective_budget().max_active_ms))
        })?;
        let boundary_started = tokio::time::Instant::now();
        let boundary_remaining =
            boundary_goal.and_then(|(used, limit)| limit.map(|limit| limit.saturating_sub(used)));
        if boundary {
            deadline = Some(
                boundary_started
                    + std::time::Duration::from_millis(
                        u64::from(plan.timeout_ms)
                            .saturating_add(fixed_ms)
                            .min(boundary_remaining.unwrap_or(u64::MAX)),
                    ),
            );
        }
        // The Goal ledger refuses a start that does not fit, so a shortfall never runs the hook.
        let active_ms = if let Some(deadline) = deadline {
            duration_ms(deadline.saturating_duration_since(tokio::time::Instant::now()))
                .saturating_sub(fixed_ms)
        } else {
            self.ledger()
                .active_remaining_ms()
                .map_or(u64::MAX, |remaining| remaining.saturating_sub(fixed_ms))
                .max(u64::from(MIN_HOOK_RUNTIME_MS))
        };
        if active_ms < u64::from(MIN_HOOK_RUNTIME_MS) {
            return Ok(None);
        }
        let runtime_ms =
            u32::try_from(active_ms.min(u64::from(plan.timeout_ms))).unwrap_or(u32::MAX);
        let mut hook_intent = runtime.intent(self.run_id, agent_run_id, plan.call.clone());
        let limits = &mut hook_intent.limits;
        limits.result_bytes = HOOK_RESULT_BYTES;
        limits.runtime_ms = runtime_ms;
        limits.cache_requested_bytes = 0;
        limits.cache_bytes = 0;
        limits.cache_candidate_bytes = 0;
        limits.cache_transfer_ms = 0;
        hook_intent.expires_at_ms = crate::tools::now_ms().saturating_add(u64::from(runtime_ms));
        let pause = if deadline.is_some() {
            None
        } else {
            Some(match self.reserve_active_pause() {
                Ok(pause) => pause,
                Err(EngineError::GoalBudgetExhausted) => return Ok(None),
                Err(error) => return Err(error),
            })
        };
        self.append(Event::HookStarted {
            run_id: self.run_id,
            agent_run_id,
            hook: HookInvocation {
                active_ceiling_ms: if boundary {
                    boundary_goal.map(|_| {
                        deadline.map_or(0, |deadline| {
                            duration_ms(deadline.saturating_duration_since(boundary_started))
                        })
                    })
                } else if matches!(
                    event,
                    HookEvent::Stop | HookEvent::SubagentStop | HookEvent::PreCompact
                ) {
                    Some(self.lifecycle_ceiling(if deadline.is_some() {
                        0
                    } else {
                        fixed_ms.saturating_add(u64::from(runtime_ms))
                    })?)
                } else {
                    None
                },
                event,
                group: plan.group,
                handler: plan.handler,
                subject_digest: digests.0,
                intent: hook_intent.clone(),
                input_digest: digests.1,
            },
        })
        .await?;
        let (observation, segment, exhausted) = self
            .dispatch_hook(runtime, hook_intent, pause, deadline)
            .await?;
        self.clear_tool_queue();
        if let Some(segment) = segment {
            self.settle_active_segment(segment)?;
        }
        let stops = observation.stops_run();
        let result = HookResult::for_event(event, &observation);
        self.append(Event::HookFinished {
            run_id: self.run_id,
            agent_run_id,
            outcome: HookOutcome {
                boundary_active_ms: if boundary {
                    boundary_goal.map(|_| duration_ms(boundary_started.elapsed()))
                } else {
                    None
                },
                observation,
                result: result.clone(),
            },
        })
        .await?;
        Ok(Some(HookRun {
            result,
            stops,
            exhausted,
        }))
    }
}

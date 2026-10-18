use super::{ActiveTime, Answer, Grant, active_time_limit_observation, not_dispatched};
use crate::engine::ledger::duration_ms;
use crate::engine::run_loop::{RunLoop, TOOL_CLEANUP_OVERHEAD};
use crate::engine::{EngineError, STORE_ACTION_OVERHEAD};
use crate::session::Event;
use crate::tools::permissions::{
    CommandHook, ConfigError, Decision, HOOK_RESULT_BYTES, HookEvent, HookInvocation, HookOutcome,
    HookResult, MAX_HOOK_FEEDBACK_BYTES, MAX_HOOK_INPUT_BYTES, PermissionConfig, feedback_lines,
    post_tool_use_input, pre_tool_use_input,
};
use crate::tools::types::model_observation;
use crate::tools::{EffectIntent, ToolCall, ToolDisposition, ToolObservation, ToolRuntime};

const MIN_HOOK_RUNTIME_MS: u32 = 1_000;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum HookStop {
    None,
    Uncertain,
    ActiveTime,
}

pub(super) struct HookFold {
    pub(super) restriction: Option<Decision>,
    pub(super) count: u8,
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
            reason: None,
            stop: HookStop::None,
        }
    }

    fn deny(reason: &str, count: u8) -> Self {
        Self {
            restriction: Some(Decision::Deny),
            count,
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

    // Upper bound of Events and active time the hooks of one primary Tool can add.
    pub(super) fn hook_admission(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        call: &ToolCall,
    ) -> (usize, u64) {
        let Some(config) = runtime.permissions() else {
            return (0, 0);
        };
        if matches!(grant, Grant::Researcher { .. }) || call.is_internal() {
            return (0, 0);
        }
        let (Ok(mut plans), Ok(post)) = (
            self.hook_plans(runtime, config, HookEvent::PreToolUse, call),
            self.hook_plans(runtime, config, HookEvent::PostToolUse, call),
        ) else {
            return (0, 0);
        };
        plans.extend(post);
        let active_ms = plans
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
            .min(self.ledger().active_remaining_ms().unwrap_or(u64::MAX));
        (plans.len(), active_ms)
    }

    // Room the next request keeps for the feedback lines PostToolUse hooks may add.
    pub(super) fn feedback_reserve(&self, runtime: &ToolRuntime, call: &ToolCall) -> usize {
        let Some(config) = runtime.permissions() else {
            return 0;
        };
        match self.hook_plans(runtime, config, HookEvent::PostToolUse, call) {
            Ok(plans) if !plans.is_empty() => MAX_HOOK_FEEDBACK_BYTES + 256,
            _ => 0,
        }
    }

    pub(super) async fn pre_tool_use_hooks(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        config: &PermissionConfig,
        intent: &EffectIntent,
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
        let post_probe = runtime.with_hook_stdin(&"\"".repeat(MAX_HOOK_INPUT_BYTES));
        if post.iter().any(|plan| {
            let mut probe = post_probe.intent(self.run_id, intent.agent_run_id, plan.call.clone());
            probe.limits.runtime_ms = u32::MAX;
            probe.expires_at_ms = u64::MAX;
            !post_probe.hook_task_fits(&probe)
        }) {
            return Ok(HookFold::deny("hook input exceeds the Guard task limit", 0));
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
        for plan in &plans {
            let mut probe = runtime.intent(self.run_id, intent.agent_run_id, plan.call.clone());
            probe.limits.runtime_ms = u32::MAX;
            probe.expires_at_ms = u64::MAX;
            if !runtime.hook_task_fits(&probe) {
                return Ok(HookFold::deny("hook input exceeds the Guard task limit", 0));
            }
        }
        let mut fold = HookFold::none();
        for plan in plans {
            let Some(run) = self
                .run_hook(
                    &runtime,
                    intent.agent_run_id,
                    HookEvent::PreToolUse,
                    &plan,
                    subject_digest,
                    input_digest,
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
        let Ok(input) = post_tool_use_input(
            self.session_id,
            config.mode(),
            call,
            observation.intent.id,
            model_observation(observation),
        ) else {
            return Ok(fold);
        };
        let input_digest = crate::tools::types::digest(input.as_bytes());
        let subject_digest = observation.intent.digest();
        let runtime = runtime.with_hook_stdin(&input);
        let mut results = Vec::new();
        for plan in plans {
            let Some(run) = self
                .run_hook(
                    &runtime,
                    observation.intent.agent_run_id,
                    HookEvent::PostToolUse,
                    &plan,
                    subject_digest,
                    input_digest,
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

    // `None` means the Goal had no active time left to start the hook.
    async fn run_hook(
        &self,
        runtime: &ToolRuntime,
        agent_run_id: crate::session::AgentRunId,
        event: HookEvent,
        plan: &HookPlan,
        subject_digest: [u8; 32],
        input_digest: [u8; 32],
    ) -> Result<Option<HookRun>, EngineError> {
        let fixed_ms = duration_ms(
            runtime
                .reservation_overhead(&plan.call)
                .saturating_add(TOOL_CLEANUP_OVERHEAD)
                .saturating_add(STORE_ACTION_OVERHEAD),
        );
        // The Goal ledger refuses a start that does not fit, so a shortfall never runs the hook.
        let active_ms = self
            .ledger()
            .active_remaining_ms()
            .map_or(u64::MAX, |remaining| remaining.saturating_sub(fixed_ms))
            .max(u64::from(MIN_HOOK_RUNTIME_MS));
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
        let pause = match self.reserve_active_pause() {
            Ok(pause) => pause,
            Err(EngineError::GoalBudgetExhausted) => return Ok(None),
            Err(error) => return Err(error),
        };
        self.append(Event::HookStarted {
            run_id: self.run_id,
            agent_run_id,
            hook: HookInvocation {
                event,
                group: plan.group,
                handler: plan.handler,
                subject_digest,
                intent: hook_intent.clone(),
                input_digest,
            },
        })
        .await?;
        let (observation, segment, exhausted) =
            self.dispatch_guard(runtime, hook_intent, pause).await?;
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

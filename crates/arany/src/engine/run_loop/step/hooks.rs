use super::{ActiveTime, Answer, Grant, active_time_limit_observation, not_dispatched};
use crate::engine::ledger::duration_ms;
use crate::engine::run_loop::{RunLoop, TOOL_CLEANUP_OVERHEAD};
use crate::engine::{EngineError, STORE_ACTION_OVERHEAD};
use crate::session::Event;
use crate::tools::permissions::{
    CommandHook, ConfigError, Decision, HOOK_RESULT_BYTES, HookEvent, HookInvocation, HookOutcome,
    HookResult, PermissionConfig, pre_tool_use_input,
};
use crate::tools::{EffectIntent, ToolCall, ToolDisposition, ToolRuntime};

const MIN_HOOK_RUNTIME_MS: u32 = 1_000;

#[derive(Clone, Copy, Eq, PartialEq)]
enum HookStop {
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
        call: &ToolCall,
    ) -> Result<Vec<HookPlan>, &'static str> {
        let matched = config
            .matched_pre_tool_use(call)
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
        if matches!(grant, Grant::Researcher { .. }) {
            return (0, 0);
        }
        let Ok(plans) = self.hook_plans(runtime, config, call) else {
            return (0, 0);
        };
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

    pub(super) async fn pre_tool_use_hooks(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        config: &PermissionConfig,
        intent: &EffectIntent,
    ) -> Result<HookFold, EngineError> {
        let plans = match self.hook_plans(runtime, config, &intent.call) {
            Ok(plans) => plans,
            Err(reason) => return Ok(HookFold::deny(reason, 0)),
        };
        if plans.is_empty() {
            return Ok(HookFold::none());
        }
        if matches!(grant, Grant::Researcher { .. }) {
            return Ok(HookFold::deny(
                "hooks are unavailable to delegated research",
                0,
            ));
        }
        let Ok(input) = pre_tool_use_input(self.session_id, config.mode(), &intent.call, intent.id)
        else {
            return Ok(HookFold::deny("hook input exceeds its limit", 0));
        };
        let input_digest = crate::tools::types::digest(input.as_bytes());
        let subject_digest = intent.digest();
        let runtime = runtime.with_hook_stdin(&input);
        let mut fold = HookFold::none();
        for plan in plans {
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
            let mut hook_intent =
                runtime.intent(self.run_id, intent.agent_run_id, plan.call.clone());
            let limits = &mut hook_intent.limits;
            limits.result_bytes = HOOK_RESULT_BYTES;
            limits.runtime_ms = runtime_ms;
            limits.cache_requested_bytes = 0;
            limits.cache_bytes = 0;
            limits.cache_candidate_bytes = 0;
            limits.cache_transfer_ms = 0;
            hook_intent.expires_at_ms =
                crate::tools::now_ms().saturating_add(u64::from(runtime_ms));
            let pause = match self.reserve_active_pause() {
                Ok(pause) => pause,
                Err(EngineError::GoalBudgetExhausted) => {
                    fold.stop = HookStop::ActiveTime;
                    return Ok(fold);
                }
                Err(error) => return Err(error),
            };
            self.append(Event::HookStarted {
                run_id: self.run_id,
                agent_run_id: intent.agent_run_id,
                hook: HookInvocation {
                    event: HookEvent::PreToolUse,
                    group: plan.group,
                    handler: plan.handler,
                    subject_digest,
                    intent: hook_intent.clone(),
                    input_digest,
                },
            })
            .await?;
            let (observation, segment, exhausted) =
                self.dispatch_guard(&runtime, hook_intent, pause).await?;
            self.clear_tool_queue();
            if let Some(segment) = segment {
                self.settle_active_segment(segment)?;
            }
            let stops = observation.stops_run();
            let result = HookResult::from_observation(&observation);
            self.append(Event::HookFinished {
                run_id: self.run_id,
                agent_run_id: intent.agent_run_id,
                outcome: HookOutcome {
                    observation,
                    result: result.clone(),
                },
            })
            .await?;
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
}

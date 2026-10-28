use super::*;
use crate::engine::run_loop::stop::SizingRefusal;
use crate::tools::command_tools::{self, CommandRewrite, CommandTool, RewriteStage};

enum RewriteStep {
    Finished(CommandRewrite, Option<String>),
    Stopped(SizingRefusal),
}

impl RunLoop<'_> {
    pub(in crate::engine::run_loop::step) fn rewrite_command<'a>(
        &'a self,
        runtime: &'a ToolRuntime,
        intent: EffectIntent,
    ) -> crate::engine::run_loop::step::BoxedStep<'a, Result<EffectIntent, SizingRefusal>> {
        Box::pin(self.rewrite_command_inner(runtime, intent))
    }

    async fn rewrite_command_inner(
        &self,
        runtime: &ToolRuntime,
        mut intent: EffectIntent,
    ) -> Result<Result<EffectIntent, SizingRefusal>, EngineError> {
        if command_tools::script(&intent.call).is_none() {
            return Ok(Ok(intent));
        }
        let tools = runtime.command_tools().entries();
        let count = tools
            .iter()
            .filter(|tool| tool.enabled && tool.role == command_tools::CommandToolRole::Rewrite)
            .count();
        if count == 0 {
            return Ok(Ok(intent));
        }
        self.journal
            .admit(
                self.session_id,
                2 * count + self.lifecycle_tail(self.primary_id, HookEvent::Stop),
                count,
            )
            .await?;
        for (index, tool) in tools.iter().enumerate().filter(|(_, tool)| {
            tool.enabled && tool.role == command_tools::CommandToolRole::Rewrite
        }) {
            if self.controls.cancellation.is_cancelled() {
                return Ok(Err(SizingRefusal::Cancelled));
            }
            match self.rewrite_once(runtime, &intent, index, tool).await? {
                RewriteStep::Stopped(refusal) => return Ok(Err(refusal)),
                RewriteStep::Finished(fact, command) => {
                    if let Some(command) = command {
                        intent.call = command_tools::rewritten(&intent.call, command);
                        if intent.call.is_unguarded() {
                            intent.host_command = runtime
                                .intent(intent.run_id, intent.agent_run_id, intent.call.clone())
                                .host_command;
                        }
                    }
                    intent.rewrite.push(fact);
                }
            }
        }
        Ok(Ok(intent))
    }

    fn rewrite_fits(&self, runtime: &ToolRuntime, plan: &HookPlan) -> bool {
        let occurrence = HookOccurrence {
            agent: self.primary_id,
            event: HookEvent::CommandRewrite,
            plan,
            digests: ([u8::MAX; 32], [u8::MAX; 32]),
        };
        let intent = self.hook_intent(runtime, &occurrence, u32::MAX);
        let hook = HookInvocation {
            command_subject: plan.command_subject,
            command_tool: plan.command_tool.clone(),
            event: HookEvent::CommandRewrite,
            group: plan.group,
            handler: plan.handler,
            subject_digest: occurrence.digests.0,
            intent,
            input_digest: occurrence.digests.1,
            active_ceiling_ms: Some(u64::MAX),
        };
        runtime.hook_task_fits(&hook.intent)
            && serde_json::to_vec(&hook)
                .is_ok_and(|bytes| bytes.len() <= crate::store::MAX_EVENT_BYTES)
    }

    async fn rewrite_once(
        &self,
        runtime: &ToolRuntime,
        intent: &EffectIntent,
        index: usize,
        tool: &CommandTool,
    ) -> Result<RewriteStep, EngineError> {
        let original_sha256 = crate::validate::digest(
            &serde_json::to_vec(&intent.call).expect("typed call is serializable"),
        );
        let fact = CommandRewrite {
            hook_intent_id: None,
            original_sha256,
            tool_sha256: tool.sha256.clone(),
            stage: RewriteStage::PinRefused,
        };
        let stdin = match command_tools::input(&intent.call) {
            Ok(stdin) => stdin,
            Err(_) => return Ok(unavailable(fact, RewriteStage::InputLimit)),
        };
        let hook_runtime = match runtime.command_tool_runtime(tool, &stdin) {
            Ok(runtime) => runtime,
            Err(_) => return Ok(unavailable(fact, RewriteStage::PinRefused)),
        };
        let plan = HookPlan {
            command_subject: Some(intent.id),
            command_tool: Some(Box::new(tool.clone())),
            group: u8::try_from(index).expect("command tools are bounded"),
            handler: 0,
            call: tool.call(),
            timeout_ms: command_tools::TIMEOUT_MS,
        };
        if !self.rewrite_fits(&hook_runtime, &plan) {
            return Ok(unavailable(fact, RewriteStage::InputLimit));
        }
        let outcome = self
            .run_hook(
                &hook_runtime,
                HookOccurrence {
                    agent: self.primary_id,
                    event: HookEvent::CommandRewrite,
                    plan: &plan,
                    digests: (original_sha256, crate::validate::digest(stdin.as_bytes())),
                },
                None,
            )
            .await?;
        Ok(fold_rewrite(fact, outcome))
    }
}

fn unavailable(mut fact: CommandRewrite, stage: RewriteStage) -> RewriteStep {
    fact.stage = stage;
    RewriteStep::Finished(fact, None)
}

fn fold_rewrite(mut fact: CommandRewrite, outcome: HookStart) -> RewriteStep {
    let run = match outcome {
        HookStart::Exhausted => return RewriteStep::Stopped(SizingRefusal::ActiveTime),
        HookStart::Ran(run) => run,
    };
    if run.stops {
        return RewriteStep::Stopped(SizingRefusal::Uncertain);
    }
    if run.cancelled {
        return RewriteStep::Stopped(SizingRefusal::Cancelled);
    }
    if run.exhausted {
        return RewriteStep::Stopped(SizingRefusal::ActiveTime);
    }
    fact.hook_intent_id = Some(run.intent_id);
    let (stage, command) = run.result.command_rewrite(run.disposition);
    fact.stage = stage;
    RewriteStep::Finished(fact, command.map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_stop_precedence_never_degrades_an_unknown_outcome() {
        for (stops, cancelled, exhausted, expected) in [
            (true, true, true, Some(SizingRefusal::Uncertain)),
            (true, false, false, Some(SizingRefusal::Uncertain)),
            (false, true, true, Some(SizingRefusal::Cancelled)),
            (false, false, true, Some(SizingRefusal::ActiveTime)),
            (false, false, false, None),
        ] {
            let fact = CommandRewrite {
                hook_intent_id: None,
                original_sha256: [1; 32],
                tool_sha256: "a".repeat(64),
                stage: RewriteStage::PinRefused,
            };
            let outcome = HookStart::Ran(HookRun {
                intent_id: uuid::Uuid::now_v7(),
                disposition: ToolDisposition::Succeeded,
                result: HookResult::Passed,
                stops,
                cancelled,
                exhausted,
            });
            match (fold_rewrite(fact, outcome), expected) {
                (RewriteStep::Stopped(actual), Some(expected)) => assert_eq!(actual, expected),
                (RewriteStep::Finished(fact, None), None) => {
                    assert_eq!(fact.stage, RewriteStage::Unchanged);
                }
                _ => panic!("rewrite stop precedence changed"),
            }
        }
    }
}

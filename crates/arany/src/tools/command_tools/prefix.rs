use super::super::permissions::{HookIgnoreReason, HookResult};
use super::super::{EffectIntent, ToolObservation};
use super::*;

pub(crate) const PREFIX_NAME: &str = "command-prefix";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WrapStage {
    Wrapped,
    PinRefused,
    Direct,
    Probe,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandWrap {
    pub tool: CommandTool,
    pub original: ToolCall,
    pub stage: WrapStage,
}

impl CommandWrap {
    pub(crate) fn valid(&self, call: &ToolCall) -> bool {
        self.tool.valid()
            && (self.tool.enabled || matches!(self.stage, WrapStage::Direct | WrapStage::Probe))
            && (self.stage != WrapStage::Probe || !self.tool.enabled && self.original == sample())
            && self.tool.role == CommandToolRole::Prefix
            && self.original.valid()
            && (shell_script(&self.original).is_some() || self.stage == WrapStage::Direct)
            && (self.stage != WrapStage::Direct
                || matches!(&self.original, ToolCall::CommandUnguarded { .. }))
            && *call
                == match self.stage {
                    WrapStage::Wrapped | WrapStage::Probe => wrapped(&self.original, &self.tool),
                    WrapStage::PinRefused | WrapStage::Direct => self.original.clone(),
                }
    }

    pub(crate) fn verify(&self, grants: &[RuntimeGrant], call: &ToolCall) -> Result<(), ToolError> {
        if !self.valid(call) {
            return Err(ToolError::ChangedInput);
        }
        if matches!(self.stage, WrapStage::Wrapped | WrapStage::Probe) {
            self.tool
                .verify(grants)
                .map_err(|_| ToolError::ChangedInput)?;
        }
        Ok(())
    }

    pub(crate) fn program(&self) -> Program {
        Program {
            name: PREFIX_NAME.into(),
            executable: self.tool.executable.clone(),
            sha256: self.tool.sha256.clone(),
            interpreter: false,
            inputs: Vec::new(),
        }
    }
}

fn shell_script(call: &ToolCall) -> Option<&str> {
    match call {
        ToolCall::Command { program, args, .. }
        | ToolCall::CommandUnguarded { program, args, .. }
            if program == "sh" && args.len() == 2 && args[0] == "-c" =>
        {
            super::super::guard::shell_script(args).ok()
        }
        _ => None,
    }
}

fn wrapped(call: &ToolCall, tool: &CommandTool) -> ToolCall {
    let mut args = tool.args.clone();
    args.push(
        shell_script(call)
            .expect("only shell proposals are wrapped")
            .into(),
    );
    match call {
        ToolCall::Command {
            cwd,
            resource_class,
            timeout_ms,
            ..
        } => ToolCall::Command {
            program: tool.executable.clone(),
            args,
            cwd: cwd.clone(),
            resource_class: *resource_class,
            timeout_ms: *timeout_ms,
            additional_reads: None,
        },
        ToolCall::CommandUnguarded {
            cwd, justification, ..
        } => ToolCall::CommandUnguarded {
            program: tool.executable.clone(),
            args,
            cwd: cwd.clone(),
            justification: justification.clone(),
            prefix_rule: None,
        },
        _ => unreachable!("only shell proposals are wrapped"),
    }
}

pub(crate) fn allowed(config: &super::super::config::Config, intent: &EffectIntent) -> bool {
    match &intent.wrap {
        Some(wrap) if wrap.stage == WrapStage::Probe => {
            wrap.valid(&intent.call)
                && config.commands.iter().any(|program| {
                    program.name == PREFIX_NAME
                        && program.executable == wrap.tool.executable
                        && program.sha256 == wrap.tool.sha256
                })
        }
        Some(wrap) => wrap.valid(&intent.call) && config.allows(&wrap.original),
        None => config.allows(&intent.call),
    }
}

impl ToolRuntime {
    pub(crate) async fn prepare_prefix(
        &self,
        intent: EffectIntent,
    ) -> Result<EffectIntent, ToolError> {
        if !matches!(
            intent.call,
            ToolCall::Command { .. } | ToolCall::CommandUnguarded { .. }
        ) || !self
            .command_tools
            .entries()
            .iter()
            .any(|tool| tool.role == CommandToolRole::Prefix)
        {
            return Ok(intent);
        }
        let runtime = self.clone();
        tokio::task::spawn_blocking(move || runtime.wrap_command(intent))
            .await
            .map_err(|_| ToolError::Operation)
    }

    pub(crate) async fn verify_wrap_async(&self, intent: &EffectIntent) -> Result<(), ToolError> {
        if intent.wrap.is_none() {
            return Ok(());
        }
        let runtime = self.clone();
        let intent = intent.clone();
        tokio::task::spawn_blocking(move || runtime.verify_wrap(&intent))
            .await
            .map_err(|_| ToolError::Operation)?
    }

    pub(crate) fn wrap_command(&self, mut intent: EffectIntent) -> EffectIntent {
        if shell_script(&intent.call).is_none() {
            let direct = intent.host_command.as_ref().and_then(|pin| {
                self.command_tools.entries().iter().find(|tool| {
                    tool.role == CommandToolRole::Prefix
                        && (tool.executable == pin.executable
                            || tool.sha256 == pin.sha256
                            || (tool.device, tool.inode) == (pin.device, pin.inode))
                })
            });
            if let Some(tool) = direct {
                intent.wrap = Some(Box::new(CommandWrap {
                    tool: tool.clone(),
                    original: intent.call.clone(),
                    stage: WrapStage::Direct,
                }));
            }
            return intent;
        }
        let Some(tool) = self
            .command_tools
            .entries()
            .iter()
            .find(|tool| tool.enabled && tool.role == CommandToolRole::Prefix)
        else {
            return intent;
        };
        let stage = if tool.verify(&self.config.runtime_grants).is_ok() {
            WrapStage::Wrapped
        } else {
            WrapStage::PinRefused
        };
        let wrap = CommandWrap {
            tool: tool.clone(),
            original: intent.call.clone(),
            stage,
        };
        if stage == WrapStage::Wrapped {
            intent.call = wrapped(&intent.call, tool);
            if intent.call.is_unguarded() {
                intent.host_command = self
                    .intent(intent.run_id, intent.agent_run_id, intent.call.clone())
                    .expect("a host command requests no Guard read directories")
                    .host_command;
            }
        }
        intent.wrap = Some(Box::new(wrap));
        intent
    }

    pub(crate) fn verify_wrap(&self, intent: &EffectIntent) -> Result<(), ToolError> {
        if let Some(wrap) = &intent.wrap {
            if !self.command_tools.entries().contains(&wrap.tool) {
                return Err(ToolError::ChangedInput);
            }
            wrap.verify(&self.config.runtime_grants, &intent.call)?;
        }
        Ok(())
    }
}

pub(super) fn probe_result(observation: &ToolObservation) -> HookResult {
    if observation.disposition == super::super::ToolDisposition::Succeeded
        && observation.attestation.is_some()
    {
        HookResult::Passed
    } else {
        HookResult::Ignored {
            why: HookIgnoreReason::NotCompleted,
        }
    }
}

fn sample() -> ToolCall {
    ToolCall::Command {
        program: "sh".into(),
        args: vec!["-c".into(), PROBE_COMMAND.into()],
        cwd: String::new(),
        resource_class: None,
        timeout_ms: None,
        additional_reads: None,
    }
}

pub(crate) fn probe_wrap(tool: &CommandTool) -> CommandWrap {
    CommandWrap {
        tool: tool.clone(),
        original: sample(),
        stage: WrapStage::Probe,
    }
}

pub(crate) fn probe_call(tool: &CommandTool) -> ToolCall {
    wrapped(&sample(), tool)
}

pub(crate) fn is_prefix_probe(intent: &EffectIntent) -> bool {
    intent
        .wrap
        .as_ref()
        .is_some_and(|wrap| wrap.stage == WrapStage::Probe)
}

#[cfg(test)]
mod tests;

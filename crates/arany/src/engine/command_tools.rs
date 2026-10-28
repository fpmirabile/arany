use super::*;
use crate::tools::command_tools::{self, CommandTool, CommandToolProbe};
use crate::tools::permissions::{HookEvent, HookOutcome, HookResult};
use crate::tools::{ToolCall, ToolDisposition, ToolObservation, ToolRuntime, WorkspacePermissions};

impl Engine {
    pub async fn probe_command_tool(
        &self,
        session_id: SessionId,
        workspace: &std::path::Path,
        tool: &CommandTool,
        cancellation: RunCancellation,
    ) -> Result<CommandToolProbe, EngineError> {
        let _operation = self.claim()?;
        let mut lock = lock_session(&self.state, session_id)?;
        let view = self
            .journal
            .store()
            .load_view(session_id)
            .await?
            .ok_or(EngineError::MissingSession)?;
        if view.runs.last().is_some_and(|run| {
            matches!(
                run.status,
                crate::session::RunStatus::Active | crate::session::RunStatus::Pending
            )
        }) {
            return Err(EngineError::SessionBusy);
        }
        if let Some(probe) = view
            .command_tool_probe
            .filter(|probe| probe.outcome.is_none())
        {
            return self.recover_command_probe(session_id, *probe).await;
        }
        let (runtime, intent) = self.command_probe_runtime(workspace, tool)?;
        let probe = CommandToolProbe {
            tool: tool.clone(),
            intent: intent.clone(),
            outcome: None,
        };
        self.journal.admit(session_id, 2, 1).await?;
        self.journal
            .append(
                session_id,
                Event::CommandToolProbeStarted {
                    probe: Box::new(probe.clone()),
                },
            )
            .await?;
        lock.keep();
        let observation = runtime.execute(intent, cancellation).await;
        self.finish_command_probe(session_id, probe, observation)
            .await
    }

    fn command_probe_runtime(
        &self,
        workspace: &std::path::Path,
        tool: &CommandTool,
    ) -> Result<(ToolRuntime, crate::tools::EffectIntent), EngineError> {
        if !tool.valid() || tool.enabled {
            return Err(crate::tools::ToolError::Configuration.into());
        }
        let config = WorkspacePermissions::load(workspace, &self.state)
            .map_err(|_| crate::tools::ToolError::Configuration)?
            .config(workspace)?;
        let runtime = ToolRuntime::admit(
            config,
            None,
            None,
            workspace,
            &self.state,
            self.config
                .guard_executable
                .clone()
                .ok_or(crate::tools::ToolError::ProtectionUnavailable)?,
        )?;
        let sample = ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), command_tools::PROBE_COMMAND.into()],
            cwd: String::new(),
            resource_class: None,
            timeout_ms: None,
        };
        let stdin = match tool.role {
            command_tools::CommandToolRole::Rewrite => command_tools::input(&sample)?,
            command_tools::CommandToolRole::Prefix => String::new(),
        };
        let runtime = runtime.command_tool_runtime(tool, &stdin)?;
        let mut intent = runtime.intent(RunId::new(), AgentRunId::new(), tool.call());
        if tool.role == command_tools::CommandToolRole::Prefix {
            intent.wrap = Some(Box::new(command_tools::prefix::probe_wrap(tool)));
        }
        intent.limits.runtime_ms = command_tools::TIMEOUT_MS;
        intent.limits.result_bytes = crate::tools::permissions::HOOK_RESULT_BYTES;
        intent.limits.cache_requested_bytes = 0;
        intent.limits.cache_bytes = 0;
        intent.limits.cache_candidate_bytes = 0;
        intent.limits.cache_transfer_ms = 0;
        Ok((runtime, intent))
    }

    async fn recover_command_probe(
        &self,
        session_id: SessionId,
        probe: CommandToolProbe,
    ) -> Result<CommandToolProbe, EngineError> {
        let observation = ToolObservation {
            intent: probe.intent.clone(),
            disposition: ToolDisposition::Uncertain,
            output: "Command tool probe outcome is unknown; no retry was attempted".into(),
            spill: None,
            enforcement_cause: None,
            limit_cause: None,
            attestation: None,
            approval_cause: None,
            guard_stage: None,
            cache_update: crate::tools::CacheUpdate::NotApplicable,
        };
        self.finish_command_probe(session_id, probe, observation)
            .await
    }

    async fn finish_command_probe(
        &self,
        session_id: SessionId,
        mut probe: CommandToolProbe,
        observation: ToolObservation,
    ) -> Result<CommandToolProbe, EngineError> {
        let outcome = HookOutcome {
            result: HookResult::for_event(HookEvent::CommandRewrite, &observation),
            observation,
            boundary_active_ms: None,
        };
        self.journal
            .append(
                session_id,
                Event::CommandToolProbeFinished {
                    outcome: Box::new(outcome.clone()),
                },
            )
            .await?;
        probe.outcome = Some(outcome);
        Ok(probe)
    }
}

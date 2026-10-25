use super::super::stop::{RunProgress, RunRefusal, StopCause};
use super::{Grant, RunLoop};
use crate::engine::EngineError;
use crate::provider::ProviderRequest;
use crate::session::RunFailure;
use crate::tools::types::MAX_TOOL_RESULT_BYTES;
use crate::tools::{ToolCall, ToolDisposition, ToolObservation, ToolRuntime};

const INVOKED_SKILL_ENVELOPE_BYTES: usize = 1024;

#[must_use]
#[derive(Debug, Eq, PartialEq)]
pub(in crate::engine::run_loop) enum SkillInitialization {
    Ready,
    Stopped,
    Failed(RunFailure),
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
                SkillInitialization::Failed(RunFailure::ToolContextFull)
            }
        }
        ToolDisposition::Cancelled | ToolDisposition::Uncertain => SkillInitialization::Stopped,
        ToolDisposition::Succeeded
        | ToolDisposition::Denied
        | ToolDisposition::Failed
        | ToolDisposition::Conflict
        | ToolDisposition::Limit => SkillInitialization::Failed(RunFailure::ToolStopped),
    }
}

impl RunLoop<'_> {
    pub(in crate::engine::run_loop) async fn initialize_skill(
        &self,
        request: &mut ProviderRequest,
    ) -> Result<RunProgress<SkillInitialization>, EngineError> {
        let name = self
            .ledger()
            .view(|view| view.runs.last().and_then(|run| run.skill.clone()))?;
        let name = match skill_read_plan(
            name,
            self.pause_at_boundary() || self.controls.cancellation.is_cancelled(),
        ) {
            SkillReadPlan::Skip => return Ok(RunProgress::Ready(SkillInitialization::Ready)),
            SkillReadPlan::Stop => return Ok(RunProgress::Ready(SkillInitialization::Stopped)),
            SkillReadPlan::Read(name) => name,
        };
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        let call = ToolCall::Skill {
            name,
            resource: None,
        };
        if !self.ledger().allows_tool(&call) {
            return Ok(RunProgress::Ready(SkillInitialization::Failed(
                RunFailure::ToolStopped,
            )));
        }
        let mut intent = match self.primary_intent(runtime, call.clone()).await? {
            None => return Ok(RunProgress::Ready(SkillInitialization::Stopped)),
            Some(intent) => intent,
        };
        let tools = request
            .input
            .tools
            .as_ref()
            .ok_or(EngineError::InvalidRequest)?;
        let Some(capacity) =
            runtime.result_capacity(tools.rendered_len(), INVOKED_SKILL_ENVELOPE_BYTES)
        else {
            return Ok(RunProgress::Ready(SkillInitialization::Failed(
                RunFailure::ToolContextFull,
            )));
        };
        intent.limits.result_bytes = capacity.min(MAX_TOOL_RESULT_BYTES) as u32;
        let grant = Grant::Primary {
            request,
            review_allowed: false,
        };
        let observation = match self
            .run_intent(&grant, runtime, intent, &call, false)
            .await?
        {
            Ok((observation, _)) => observation,
            Err(StopCause::ActiveTime) => return Ok(RunProgress::Stopped(RunRefusal::Budget)),
            Err(StopCause::Capacity) => return Ok(RunProgress::Stopped(RunRefusal::Capacity)),
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
            ) => return Ok(RunProgress::Ready(SkillInitialization::Stopped)),
        };
        self.apply_invoked_skill(request, runtime, observation)
            .map(RunProgress::Ready)
    }

    fn apply_invoked_skill(
        &self,
        request: &mut ProviderRequest,
        runtime: &ToolRuntime,
        observation: ToolObservation,
    ) -> Result<SkillInitialization, EngineError> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
                SkillInitialization::Failed(RunFailure::ToolStopped),
            ),
            (
                ToolDisposition::Denied,
                false,
                SkillInitialization::Failed(RunFailure::ToolStopped),
            ),
            (
                ToolDisposition::Failed,
                false,
                SkillInitialization::Failed(RunFailure::ToolStopped),
            ),
            (
                ToolDisposition::Conflict,
                true,
                SkillInitialization::Failed(RunFailure::ToolStopped),
            ),
            (
                ToolDisposition::Limit,
                true,
                SkillInitialization::Failed(RunFailure::ToolStopped),
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
            SkillInitialization::Failed(RunFailure::ToolContextFull)
        );
    }
}

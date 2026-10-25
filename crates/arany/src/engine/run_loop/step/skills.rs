use super::super::stop::{RunProgress, RunRefusal, StopCause};
use super::{Grant, RunLoop};
use crate::engine::EngineError;
use crate::provider::ProviderRequest;
use crate::session::RunFailure;
use crate::tools::definitions::MAX_AGENT_CATALOG_BYTES;
use crate::tools::types::MAX_TOOL_RESULT_BYTES;
use crate::tools::{ToolCall, ToolDisposition, ToolObservation, ToolRuntime};

const MAX_NORMALIZED_DEFINITION_BYTES: usize = 1024 * 1024;

#[must_use]
pub(in crate::engine::run_loop) enum DefinitionInitialization {
    Ready,
    Stopped,
}

#[must_use]
enum DefinitionRead {
    Output(String),
    Stopped,
}

impl RunLoop<'_> {
    pub(in crate::engine::run_loop) async fn initialize_definitions(
        &self,
        request: &mut ProviderRequest,
    ) -> Result<RunProgress<DefinitionInitialization>, EngineError> {
        let Some(runtime) = self.controls.tools else {
            return Ok(RunProgress::Ready(DefinitionInitialization::Ready));
        };
        let Some(pin) = runtime.definitions.pin() else {
            return Ok(RunProgress::Ready(DefinitionInitialization::Ready));
        };
        let mut outputs = Vec::with_capacity(pin.files.len());
        let mut normalized_bytes = 0;
        for (index, file) in pin.files.iter().enumerate() {
            if self.pause_at_boundary() || self.controls.cancellation.is_cancelled() {
                return Ok(RunProgress::Ready(DefinitionInitialization::Stopped));
            }
            let call = ToolCall::ParseAgentDefinition {
                index: u16::try_from(index).map_err(|_| EngineError::InvalidRequest)?,
                raw_digest: file.digest,
            };
            let output = match self.read_definition(request, runtime, call).await? {
                RunProgress::Ready(DefinitionRead::Output(output)) => output,
                RunProgress::Ready(DefinitionRead::Stopped) => {
                    return Ok(RunProgress::Ready(DefinitionInitialization::Stopped));
                }
                RunProgress::Stopped(cause) => return Ok(RunProgress::Stopped(cause)),
            };
            normalized_bytes += output.len();
            if normalized_bytes > MAX_NORMALIZED_DEFINITION_BYTES {
                return Ok(RunProgress::Ready(DefinitionInitialization::Stopped));
            }
            outputs.push(output);
        }
        let child_model = &self.routing.providers.child[0].identity().model;
        let Ok(registry) = crate::tools::definitions::registry(
            &pin,
            outputs.iter().map(String::as_str),
            child_model,
        ) else {
            return Ok(RunProgress::Ready(DefinitionInitialization::Stopped));
        };
        self.install_definitions(request, runtime, registry)
            .map(RunProgress::Ready)
    }

    fn install_definitions(
        &self,
        request: &mut ProviderRequest,
        runtime: &ToolRuntime,
        registry: crate::tools::definitions::Registry,
    ) -> Result<DefinitionInitialization, EngineError> {
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
                        entry["digest"] = serde_json::json!(crate::validate::digest(
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
            > MAX_AGENT_CATALOG_BYTES
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

    async fn read_definition(
        &self,
        request: &mut ProviderRequest,
        runtime: &ToolRuntime,
        call: ToolCall,
    ) -> Result<RunProgress<DefinitionRead>, EngineError> {
        let Some(mut intent) = self.primary_intent(runtime, call).await? else {
            self.active_time_floor
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return Ok(RunProgress::Ready(DefinitionRead::Stopped));
        };
        intent.limits.result_bytes = MAX_TOOL_RESULT_BYTES as u32;
        let grant = Grant::Primary {
            request,
            review_allowed: false,
        };
        let post_call = intent.call.clone();
        let observation = match self
            .run_intent(&grant, runtime, intent, &post_call, false)
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
            ) => return Ok(RunProgress::Ready(DefinitionRead::Stopped)),
        };
        if observation.disposition != ToolDisposition::Succeeded
            || observation.attestation.is_none()
        {
            return Ok(RunProgress::Ready(DefinitionRead::Stopped));
        }
        Ok(RunProgress::Ready(DefinitionRead::Output(
            observation.output,
        )))
    }
}

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

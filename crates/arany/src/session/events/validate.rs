use super::*;

impl Event {
    pub(in crate::session) fn validate(&self) -> Result<(), ReplayError> {
        let valid_goal_ids = match self {
            Self::GoalStarted { goal_id, .. }
            | Self::GoalRunLinked { goal_id, .. }
            | Self::GoalResumed { goal_id }
            | Self::GoalPlanContractRecorded { goal_id, .. }
            | Self::GoalCompactionCallReserved { goal_id, .. }
            | Self::GoalContextCompacted { goal_id, .. }
            | Self::GoalSteeringNoteRecorded { goal_id, .. }
            | Self::GoalFinished { goal_id, .. } => goal_id.valid(),
            _ => true,
        };
        if !valid_goal_ids {
            return Err(ReplayError::InvalidPayload);
        }
        match self {
            Self::SessionStarted { title, .. } | Self::SessionRenamed { title } => {
                bounded(title, 128)
            }
            Self::SessionDefaultChanged { defaults } => defaults.validate(),
            Self::SessionForked {
                title,
                source_sequence,
                ..
            } if *source_sequence > 0 => bounded(title, 128),
            Self::SessionForked { .. } => Err(ReplayError::InvalidPayload),
            Self::ContextCompacted { record, .. } => record.validate(),
            Self::GoalStarted {
                objective,
                workspace_identity: WorkspaceIdentity { inode, .. },
                budget,
                resource_settings,
                continues,
                ..
            } => {
                if continues.is_some_and(|id| !id.valid()) {
                    return Err(ReplayError::InvalidPayload);
                }
                if objective.len() > super::MAX_USER_MESSAGE_BYTES || *inode == 0 {
                    return Err(ReplayError::InvalidPayload);
                }
                budget.validate()?;
                if !resource_settings.valid() {
                    return Err(ReplayError::InvalidPayload);
                }
                Ok(())
            }
            Self::GoalRunLinked { .. } | Self::GoalResumed { .. } => Ok(()),
            Self::GoalCompactionCallReserved { .. } => Ok(()),
            Self::GoalContextCompacted {
                covered_tool_count,
                record,
                ..
            } => {
                if *covered_tool_count == 0 || record.validate().is_err() {
                    return Err(ReplayError::InvalidPayload);
                }
                Ok(())
            }
            Self::GoalPlanContractRecorded { contract, .. } => {
                contract.validate()?;
                Ok(())
            }
            Self::GoalSteeringNoteRecorded { text, .. } => {
                if text.trim().is_empty() || text.len() > super::MAX_STEERING_NOTE_BYTES {
                    return Err(ReplayError::InvalidPayload);
                }
                Ok(())
            }
            Self::GoalFinished { status, report, .. } => {
                if !matches!(status, GoalStatus::Completed | GoalStatus::Failed) {
                    return Err(ReplayError::InvalidPayload);
                }
                report.validate()
            }
            Self::MessageAccepted { text, images, .. } => {
                if text.len() > super::MAX_USER_MESSAGE_BYTES
                    || text.is_empty() && images.is_empty()
                    || !crate::provider::valid_image_collection(images)
                {
                    return Err(ReplayError::InvalidPayload);
                }
                Ok(())
            }
            Self::MessageCommitted { text, .. } => {
                bounded(text, super::MAX_ASSISTANT_MESSAGE_BYTES)
            }
            Self::RunStarted {
                config,
                active_deadline_ms,
                ..
            } => {
                if active_deadline_ms.is_some_and(|ms| ms == 0 || ms > MAX_GOAL_ACTIVE_MS) {
                    return Err(ReplayError::InvalidPayload);
                }
                config.validate()
            }
            Self::AgentSpawned {
                role,
                ordinal,
                objective,
                grant,
                definition,
                ..
            } => match (role, grant) {
                (AgentRole::Primary, None)
                    if *ordinal == 0 && objective.is_none() && definition.is_none() =>
                {
                    Ok(())
                }
                (AgentRole::Child, Some(_))
                    if (1..=8).contains(ordinal)
                        && definition.as_ref().is_none_or(|name| {
                            name.len() <= 64 && crate::tools::types::valid_name(name)
                        }) =>
                {
                    bounded(
                        objective.as_deref().ok_or(ReplayError::InvalidPayload)?,
                        2 * 1024,
                    )
                }
                _ => Err(ReplayError::InvalidPayload),
            },
            Self::ProviderFallbackUsed { fallback, .. } => {
                if fallback.from.valid()
                    && fallback.to.valid()
                    && fallback.from != fallback.to
                    && fallback.failure == ProviderCallDisposition::Unavailable
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::ProviderCallRecorded { record, .. } => record.validate(),
            Self::GoalProviderCallReserved { goal_id, .. } => {
                if goal_id.valid() {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::AssistantDisplayRecorded { chunk, .. } => chunk.validate(),
            Self::ToolStarted {
                run_id,
                agent_run_id,
                intent,
                ..
            } => {
                if intent.valid()
                    && intent.run_id == *run_id
                    && intent.agent_run_id == *agent_run_id
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::ToolFinished {
                run_id,
                agent_run_id,
                observation,
            } => {
                if observation.valid()
                    && !(observation.cache_update == crate::tools::CacheUpdate::NotAttempted
                        && observation.disposition == crate::tools::ToolDisposition::Succeeded)
                    && observation.intent.run_id == *run_id
                    && observation.intent.agent_run_id == *agent_run_id
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::HookStarted {
                run_id,
                agent_run_id,
                hook,
            } => {
                if hook.event == crate::tools::permissions::HookEvent::PreToolUse
                    && hook.intent.valid()
                    && matches!(hook.intent.call, crate::tools::ToolCall::Command { .. })
                    && hook.intent.run_id == *run_id
                    && hook.intent.agent_run_id == *agent_run_id
                    && hook.subject_digest != [0; 32]
                    && hook.input_digest != [0; 32]
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::HookFinished {
                run_id,
                agent_run_id,
                outcome,
            } => {
                let observation = &outcome.observation;
                if observation.valid()
                    && matches!(
                        observation.intent.call,
                        crate::tools::ToolCall::Command { .. }
                    )
                    && !(observation.cache_update == crate::tools::CacheUpdate::NotAttempted
                        && observation.disposition == crate::tools::ToolDisposition::Succeeded)
                    && observation.intent.run_id == *run_id
                    && observation.intent.agent_run_id == *agent_run_id
                    && outcome.result.valid_for(observation)
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::ToolPermissionDecided { verdict, .. } => {
                use crate::tools::permissions::{
                    Decision, MAX_HOOKS_PER_OCCURRENCE, PermissionSource,
                };
                if verdict.intent_digest != [0; 32]
                    && matches!(
                        verdict.basis,
                        PermissionSource::Rule
                            | PermissionSource::Mode
                            | PermissionSource::PinnedGrant
                            | PermissionSource::ExactCommandRule
                            | PermissionSource::Hook
                    )
                    && usize::from(verdict.hooks) <= MAX_HOOKS_PER_OCCURRENCE
                    && (verdict.basis != PermissionSource::Hook
                        || verdict.hook_restriction == Some(verdict.decision)
                            && verdict.decision != Decision::Allow)
                    && match verdict.hook_restriction {
                        None | Some(Decision::Allow) => verdict.hook_restriction.is_none(),
                        Some(Decision::Ask) => {
                            verdict.hooks > 0
                                && verdict.source != PermissionSource::AiReview
                                && matches!(verdict.decision, Decision::Ask | Decision::Deny)
                        }
                        Some(Decision::Deny) => verdict.decision == Decision::Deny,
                    }
                    && (verdict.source != PermissionSource::Hook
                        || verdict.decision == Decision::Deny)
                    && (verdict.decision != Decision::Deny || !verdict.approved)
                    && match verdict.decision {
                        Decision::Ask => matches!(
                            verdict.source,
                            PermissionSource::Human
                                | PermissionSource::AiReview
                                | PermissionSource::UnavailableApproval
                        ),
                        Decision::Allow | Decision::Deny => verdict.source == verdict.basis,
                    }
                    && (verdict.source != PermissionSource::UnavailableApproval
                        || !verdict.approved)
                    && (verdict.source != PermissionSource::PinnedGrant || !verdict.approved)
                    && (verdict.source != PermissionSource::AiReview
                        || verdict.basis == PermissionSource::Mode)
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::AgentFinished {
                disposition,
                summary,
                result,
                ..
            } => match disposition {
                AgentDisposition::Finished => {
                    bounded(
                        summary.as_deref().ok_or(ReplayError::InvalidPayload)?,
                        2 * 1024,
                    )?;
                    bounded(
                        result.as_deref().ok_or(ReplayError::InvalidPayload)?,
                        super::MAX_ASSISTANT_MESSAGE_BYTES,
                    )
                }
                AgentDisposition::Failed | AgentDisposition::Cancelled
                    if summary.is_none() && result.is_none() =>
                {
                    Ok(())
                }
                _ => Err(ReplayError::InvalidPayload),
            },
            Self::RunFinished { .. } => Ok(()),
        }
    }
}

pub(super) fn bounded(value: &str, max: usize) -> Result<(), ReplayError> {
    if value.is_empty() || value.len() > max {
        return Err(ReplayError::InvalidPayload);
    }
    Ok(())
}

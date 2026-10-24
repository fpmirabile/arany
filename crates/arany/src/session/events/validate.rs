use super::*;

mod goal;
mod session;

impl Event {
    pub(in crate::session) fn validate(&self) -> Result<(), ReplayError> {
        if self.scope().1.is_some_and(|id| !id.valid()) {
            return Err(ReplayError::InvalidPayload);
        }
        let valid_goal_ids = match self {
            Self::GoalStarted { goal_id, .. }
            | Self::GoalRunLinked { goal_id, .. }
            | Self::GoalResumed { goal_id }
            | Self::GoalPlanContractRecorded { goal_id, .. }
            | Self::GoalCompactionCallReserved { goal_id, .. }
            | Self::GoalContextCompacted { goal_id, .. }
            | Self::GoalSteeringNoteRecorded { goal_id, .. }
            | Self::AgentSteeringNoteRecorded { goal_id, .. }
            | Self::GoalFinished { goal_id, .. } => goal_id.valid(),
            _ => true,
        };
        if !valid_goal_ids {
            return Err(ReplayError::InvalidPayload);
        }
        match self {
            Self::SupervisorVerdictRecorded { .. }
            | Self::WorkspaceRewindStarted { .. }
            | Self::WorkspaceRewindFinished { .. }
            | Self::SessionStarted { .. }
            | Self::SessionRenamed { .. }
            | Self::SessionDefaultChanged { .. }
            | Self::SessionForked { .. }
            | Self::ContextCompacted { .. }
            | Self::ToolResultsCleared { .. }
            | Self::MessageAccepted { .. }
            | Self::MessageCommitted { .. } => Event::validate_session(self),
            Self::GoalStarted { .. }
            | Self::GoalRunLinked { .. }
            | Self::GoalResumed { .. }
            | Self::GoalCompactionCallReserved { .. }
            | Self::GoalContextCompacted { .. }
            | Self::GoalPlanContractRecorded { .. }
            | Self::GoalSteeringNoteRecorded { .. }
            | Self::AgentSteeringNoteRecorded { .. }
            | Self::GoalFinished { .. }
            | Self::GoalProviderCallReserved { .. } => Event::validate_goal(self),
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
                profile,
                assignment,
                ..
            } => {
                if !profile
                    .as_deref()
                    .is_none_or(crate::provider::valid_profile_name)
                    || !serde_json::to_vec(&SpawnPayload {
                        role: *role,
                        ordinal: *ordinal,
                        objective: objective.clone(),
                        grant: *grant,
                        definition: definition.clone(),
                        profile: profile.clone(),
                        assignment: assignment.clone(),
                    })
                    .is_ok_and(|b| b.len() <= crate::store::MAX_EVENT_BYTES)
                {
                    return Err(ReplayError::InvalidPayload);
                }
                match (role, grant) {
                    (AgentRole::Primary, None)
                        if *ordinal == 0
                            && objective.is_none()
                            && definition.is_none()
                            && assignment.is_none() =>
                    {
                        Ok(())
                    }
                    (AgentRole::Child, Some(_))
                        if (1..=8).contains(ordinal)
                            && definition.as_ref().is_none_or(|name| {
                                name.len() <= 64 && crate::validate::valid_name(name)
                            }) =>
                    {
                        if (matches!(*grant, Some(ChildGrant::EditOnly { .. })))
                            != assignment.is_some()
                        {
                            return Err(ReplayError::InvalidPayload);
                        }
                        if let Some(p) = assignment
                            && (!(ChildAssignment::Editor {
                                assignment: Box::new(p.request.clone()),
                            })
                            .valid()
                                || !p.grant.valid()
                                || !p.base.valid()
                                || p.policy_digest == [0; 32]
                                || p.permissions_digest == Some([0; 32])
                                || p.cache_generation == Some([0; 32])
                                || p.data_bytes == 0
                                || p.data_bytes as usize > WORKER_DATA_BYTES)
                        {
                            return Err(ReplayError::InvalidPayload);
                        }
                        bounded(
                            objective.as_deref().ok_or(ReplayError::InvalidPayload)?,
                            2 * 1024,
                        )
                    }
                    _ => Err(ReplayError::InvalidPayload),
                }
            }
            Self::ProviderFallbackUsed { fallback, .. } => {
                if fallback
                    .rate_limits
                    .as_ref()
                    .is_none_or(|limits| limits.valid())
                    && fallback.from.valid()
                    && fallback.to.valid()
                    && fallback.from != fallback.to
                    && fallback.failure == ProviderCallDisposition::Unavailable
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::ProviderRetryScheduled { retry, .. } => {
                use crate::provider::{MAX_PROVIDER_RETRIES, MAX_PROVIDER_RETRY_DELAY_MS};
                let backoff = crate::provider::ProviderRetry::backoff(retry.attempt).as_millis();
                if retry
                    .rate_limits
                    .as_ref()
                    .is_none_or(|limits| limits.valid())
                    && (1..=MAX_PROVIDER_RETRIES).contains(&retry.attempt)
                    && (backoff..=u128::from(MAX_PROVIDER_RETRY_DELAY_MS))
                        .contains(&u128::from(retry.delay_ms))
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::ProviderCallRecorded { record, .. } => record.validate(),
            Self::AssistantDisplayRecorded { chunk, .. } => chunk.validate(),
            Self::ToolStarted {
                run_id,
                agent_run_id,
                intent,
                ..
            } => {
                if intent.valid()
                    && !matches!(
                        intent.call,
                        crate::tools::ToolCall::ApplyChangeset {
                            source: ChangesetSource::Rewind { .. },
                        }
                    )
                    && intent.run_id == *run_id
                    && intent.agent_run_id == *agent_run_id
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::ToolFinished {
                checkpoint,
                run_id,
                agent_run_id,
                observation,
            } => {
                if checkpoint
                    .as_ref()
                    .is_none_or(|checkpoint| checkpoint.valid())
                    && observation.valid()
                    && !matches!(
                        observation.intent.call,
                        crate::tools::ToolCall::ApplyChangeset {
                            source: ChangesetSource::Rewind { .. },
                        }
                    )
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
                if hook.group < 64
                    && hook.handler < 128
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
                            | PermissionSource::SessionApproval
                            | PermissionSource::Hook
                            | PermissionSource::TypedPrefixRule
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
                    && (!verdict.narrowed
                        || verdict.decision == Decision::Deny
                            && verdict.source == PermissionSource::Hook
                            && verdict.hook_restriction == Some(Decision::Deny))
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
                worker,
                ..
            } => {
                if worker.as_ref().is_some_and(|o| !o.valid()) {
                    return Err(ReplayError::InvalidPayload);
                }
                match disposition {
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
                    AgentDisposition::Failed
                    | AgentDisposition::Cancelled
                    | AgentDisposition::StoppedByHook
                        if summary.is_none() && result.is_none() =>
                    {
                        Ok(())
                    }
                    _ => Err(ReplayError::InvalidPayload),
                }
            }
            Self::WorkerDataChunk { record, .. } => {
                if record.decoded().is_some() {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::WorkerDataSealed { record, .. } => {
                if record.data.valid() && record.tool_id.get_version() == Some(Version::SortRand) {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::WorkerStopObserved { .. } => Ok(()),
            Self::WorkerHandoverRecorded { record, .. } => {
                if record.assignment_id.valid()
                    && record.assignment_id == record.data.assignment_id
                    && record.data.valid()
                    && record.base_digest != [0; 32]
                    && record.criterion_evidence.len() <= 8
                    && record.criterion_evidence.iter().enumerate().all(|(i, s)| {
                        *s > 0 && record.criterion_evidence[..i].iter().all(|p| p < s)
                    })
                    && matches!(
                        record.data.purpose,
                        WorkerDataPurpose::Handover | WorkerDataPurpose::Mutation
                    )
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::WorkerIntegrationRecorded { record, .. } => {
                if record.assignment_id.valid()
                    && record.handover_sequence > 0
                    && record.finished_sequence > record.handover_sequence
                    && record.tool_id.get_version() == Some(Version::SortRand)
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::WorkerHandoverDiscarded { record, .. }
            | Self::WorkerLeaseReleased { record, .. } => {
                if record.assignment_id.valid()
                    && record.cause_sequence > 0
                    && record.handover_sequence != Some(0)
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::WorkerVerificationRecorded { record, .. } => {
                if record.assignment_id.valid()
                    && record.criterion_digest != [0; 32]
                    && !record.evidence.is_empty()
                    && record.evidence.len() <= 8
                    && record.evidence.iter().all(|s| *s > 0)
                    && record.evidence.windows(2).all(|s| s[0] < s[1])
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::RunFinished {
                disposition,
                failure,
                ..
            } => {
                if (*disposition == super::RunDisposition::Failed) == failure.is_some() {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
        }
    }
}

pub(super) fn bounded(value: &str, max: usize) -> Result<(), ReplayError> {
    if value.is_empty() || value.len() > max {
        return Err(ReplayError::InvalidPayload);
    }
    Ok(())
}

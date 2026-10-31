use super::*;

impl Event {
    pub(super) fn validate_tool(&self) -> Result<(), ReplayError> {
        match self {
            Self::ToolPreflightRefused { record, .. } => {
                checked(record.intent_digest != [0; 32] && record.refusal.valid())
            }
            Self::ToolActiveTimeReserved {
                intent_digest,
                active_ceiling_ms,
                ..
            } => {
                if *intent_digest == [0; 32] || *active_ceiling_ms == 0 {
                    Err(ReplayError::InvalidPayload)
                } else {
                    Ok(())
                }
            }
            Self::ToolProposed {
                run_id,
                agent_run_id,
                intent,
                ..
            } => checked(valid_proposal(intent, *run_id, *agent_run_id)),
            Self::ToolFinished {
                checkpoint,
                run_id,
                agent_run_id,
                observation,
            } => checked(valid_finish(
                checkpoint.as_ref(),
                observation,
                *run_id,
                *agent_run_id,
            )),
            Self::HookStarted {
                run_id,
                agent_run_id,
                hook,
            } => checked(valid_hook_start(hook, *run_id, *agent_run_id)),
            Self::HookFinished {
                run_id,
                agent_run_id,
                outcome,
            } => checked(valid_hook_finish(outcome, *run_id, *agent_run_id)),
            Self::ToolStarted {
                run_id,
                agent_run_id,
                intent,
                active_ceiling_ms,
                verdict,
            } => {
                Self::ToolProposed {
                    run_id: *run_id,
                    agent_run_id: *agent_run_id,
                    intent: intent.clone(),
                    active_ceiling_ms: *active_ceiling_ms,
                }
                .validate_tool()?;
                let Some(verdict) = verdict else {
                    return Ok(());
                };
                if verdict.intent_digest
                    != crate::tools::EffectIntent::from(intent.clone()).digest()
                {
                    return Err(ReplayError::InvalidPayload);
                }
                checked(valid_verdict(verdict))
            }
            _ => Err(ReplayError::InvalidPayload),
        }
    }
}

fn valid_hook_start(
    hook: &crate::tools::permissions::HookInvocation,
    run_id: RunId,
    agent_run_id: AgentRunId,
) -> bool {
    (hook.command_tool.is_some()
        == (hook.event == crate::tools::permissions::HookEvent::CommandRewrite))
        && (hook.command_subject.is_some() == hook.command_tool.is_some())
        && hook
            .command_subject
            .is_none_or(|id| id.get_version() == Some(uuid::Version::SortRand))
        && usize::from(hook.group) < crate::tools::permissions::MAX_HOOK_GROUPS
        && usize::from(hook.handler) < crate::tools::permissions::MAX_HOOKS
        && hook.intent.valid()
        && matches!(hook.intent.call, crate::tools::ToolCall::Command { .. })
        && hook.intent.run_id == run_id
        && hook.intent.agent_run_id == agent_run_id
        && hook.subject_digest != [0; 32]
        && hook.input_digest != [0; 32]
}

fn valid_verdict(verdict: &crate::tools::permissions::PermissionVerdict) -> bool {
    use crate::tools::permissions::{Decision, MAX_HOOKS_PER_OCCURRENCE, PermissionSource};
    verdict.intent_digest != [0; 32]
        && matches!(
            verdict.basis,
            PermissionSource::Rule
                | PermissionSource::Mode
                | PermissionSource::PinnedGrant
                | PermissionSource::SessionApproval
                | PermissionSource::Hook
                | PermissionSource::TypedPrefixRule
                | PermissionSource::HostPrefixRule
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
        && (verdict.source != PermissionSource::Hook || verdict.decision == Decision::Deny)
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
        && (verdict.source != PermissionSource::UnavailableApproval || !verdict.approved)
        && (verdict.source != PermissionSource::PinnedGrant || !verdict.approved)
        && (verdict.source != PermissionSource::AiReview || verdict.basis == PermissionSource::Mode)
}

fn valid_proposal(intent: &ToolIntentRecord, run_id: RunId, agent_run_id: AgentRunId) -> bool {
    crate::tools::EffectIntent::from(intent.clone()).valid()
        && !matches!(
            intent.call,
            crate::tools::ToolCall::ApplyChangeset {
                source: ChangesetSource::Rewind { .. },
            }
        )
        && intent.run_id == run_id
        && intent.agent_run_id == agent_run_id
}

fn valid_finish(
    checkpoint: Option<&crate::session::Checkpoint>,
    observation: &crate::tools::ToolObservation,
    run_id: RunId,
    agent_run_id: AgentRunId,
) -> bool {
    checkpoint.is_none_or(|checkpoint| checkpoint.valid())
        && observation.valid()
        && !matches!(
            observation.intent.call,
            crate::tools::ToolCall::ApplyChangeset {
                source: ChangesetSource::Rewind { .. },
            }
        )
        && !(observation.cache_update == crate::tools::CacheUpdate::NotAttempted
            && observation.disposition == crate::tools::ToolDisposition::Succeeded)
        && observation.intent.run_id == run_id
        && observation.intent.agent_run_id == agent_run_id
}

fn valid_hook_finish(
    outcome: &crate::tools::permissions::HookOutcome,
    run_id: RunId,
    agent_run_id: AgentRunId,
) -> bool {
    let observation = &outcome.observation;
    observation.valid()
        && matches!(
            observation.intent.call,
            crate::tools::ToolCall::Command { .. }
        )
        && !(observation.cache_update == crate::tools::CacheUpdate::NotAttempted
            && observation.disposition == crate::tools::ToolDisposition::Succeeded)
        && observation.intent.run_id == run_id
        && observation.intent.agent_run_id == agent_run_id
        && outcome.result.valid_for(observation)
}

#[cfg(test)]
mod tests;

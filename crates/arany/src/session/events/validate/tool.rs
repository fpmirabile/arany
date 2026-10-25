use super::*;

impl Event {
    pub(super) fn validate_tool(&self) -> Result<(), ReplayError> {
        match self {
            Self::ToolStarted {
                run_id,
                agent_run_id,
                intent,
                ..
            } => {
                if crate::tools::EffectIntent::from(intent.clone()).valid()
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
            _ => Err(ReplayError::InvalidPayload),
        }
    }
}

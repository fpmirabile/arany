use super::{SessionView, active_run, pending_tool, valid_host_approval};
use crate::provider::AgentPhase;
use crate::session::events::{Event, ProviderCallDisposition, ReplayError};
use crate::tools::permissions::{
    Decision, HookResult, PermissionMode, PermissionSource, TypedGitPrefix,
};

pub(super) fn apply(current: &mut SessionView, event: &Event) -> Result<(), ReplayError> {
    match event {
        Event::ToolPermissionDecided {
            run_id,
            agent_run_id,
            verdict,
        } => {
            let host_valid = valid_host_approval(current, *run_id, *agent_run_id, verdict);
            let run = active_run(current, *run_id)?;
            let definition_mode = super::definition_mode(run, *agent_run_id)?;
            let policy = run
                .config
                .as_ref()
                .and_then(|config| config.tool_policy.as_ref());
            let permissions_pinned =
                policy.is_some_and(|policy| policy.permissions_digest.is_some());
            let pending_call = pending_tool(run, *agent_run_id)?.intent.call.clone();
            let unguarded = pending_call.is_unguarded();
            let primary = run.primary().is_some_and(|agent| agent.id == *agent_run_id);
            let first_use = !permissions_pinned
                && definition_mode.is_none()
                && !unguarded
                && pending_call.is_git()
                && pending_call.facts().mutates;
            let first_unconfigured_host_read = !permissions_pinned
                && definition_mode.is_none()
                && matches!(&pending_call, crate::tools::ToolCall::GhPrList { .. });
            if first_use
                && !(primary
                    && verdict.hook_restriction.is_none()
                    && matches!(
                        verdict.basis,
                        PermissionSource::Mode | PermissionSource::PinnedGrant
                    )
                    && matches!(
                        verdict.source,
                        PermissionSource::Mode
                            | PermissionSource::PinnedGrant
                            | PermissionSource::Human
                            | PermissionSource::AiReview
                            | PermissionSource::UnavailableApproval
                    ))
            {
                return Err(ReplayError::InvalidTransition);
            }
            let typed_basis = verdict.basis == PermissionSource::TypedPrefixRule;
            let typed_source = verdict.source == PermissionSource::TypedPrefixRule;
            if (typed_basis || typed_source)
                && !(permissions_pinned
                    && primary
                    && typed_basis
                    && TypedGitPrefix::of_call(&pending_call).is_some()
                    && (!typed_source
                        // An allow whose dispatch a later plan, budget or pause gate refused is unapproved.
                        || verdict.decision == Decision::Allow
                            && (matches!(
                                verdict.mode,
                                Some(PermissionMode::Default | PermissionMode::DontAsk)
                            ) || matches!(&pending_call, crate::tools::ToolCall::GhPrList { .. }))
                        || !verdict.approved && verdict.decision == Decision::Deny))
            {
                return Err(ReplayError::InvalidTransition);
            }
            if (verdict.basis == PermissionSource::HostPrefixRule
                || verdict.source == PermissionSource::HostPrefixRule)
                && !(permissions_pinned
                    && primary
                    && unguarded
                    && verdict.basis == PermissionSource::HostPrefixRule
                    && (!verdict.approved || host_valid))
            {
                return Err(ReplayError::InvalidTransition);
            }
            if verdict.mode.is_some() != (permissions_pinned || definition_mode.is_some())
                || !(permissions_pinned
                    || definition_mode.is_some()
                    || unguarded
                    || first_use
                    || first_unconfigured_host_read)
                || definition_mode == Some(crate::tools::permissions::PermissionMode::DontAsk)
                    && verdict.decision == crate::tools::permissions::Decision::Ask
            {
                return Err(ReplayError::InvalidTransition);
            }
            if pending_call.is_host()
                && verdict.approved
                && (!primary
                    || pending_tool(run, *agent_run_id)?.intent.host.is_none()
                    || if matches!(&pending_call, crate::tools::ToolCall::GhPrList { .. }) {
                        !(verdict.source == PermissionSource::TypedPrefixRule
                            && verdict.basis == PermissionSource::TypedPrefixRule
                            && verdict.decision == Decision::Allow
                            && permissions_pinned)
                            && (verdict.source != PermissionSource::Human
                                || verdict.decision != Decision::Ask
                                || matches!(verdict.mode, Some(PermissionMode::DontAsk)))
                    } else {
                        verdict.source != PermissionSource::Human
                            || verdict.decision != Decision::Ask
                            || matches!(
                                verdict.mode,
                                Some(PermissionMode::Plan | PermissionMode::DontAsk)
                            )
                    })
            {
                return Err(ReplayError::InvalidTransition);
            }
            if unguarded && verdict.approved && !host_valid {
                return Err(ReplayError::InvalidTransition);
            }
            if !unguarded && verdict.host.is_some() {
                return Err(ReplayError::InvalidTransition);
            }
            if verdict.source == crate::tools::permissions::PermissionSource::AiReview
                && verdict.approved
            {
                let index = pending_tool(run, *agent_run_id)?
                    .review_call_index
                    .ok_or(ReplayError::InvalidTransition)?;
                let reviewed = run
                    .agents
                    .iter()
                    .find(|agent| agent.id == *agent_run_id)
                    .and_then(|agent| agent.provider_calls.get(index as usize));
                if reviewed.is_none_or(|call| {
                    call.phase != AgentPhase::ToolReview
                        || call.disposition != ProviderCallDisposition::Finished
                }) {
                    return Err(ReplayError::InvalidTransition);
                }
            }
            let chained = run
                .tools
                .len()
                .checked_sub(2)
                .is_some_and(|index| run.tools[index].narrowed_call().is_some());
            let tool = pending_tool(run, *agent_run_id)?;
            let narrowing_holds = !verdict.narrowed
                || primary
                    && !chained
                    && match tool.hooks.last().and_then(|hook| hook.outcome.as_ref()) {
                        Some(crate::tools::permissions::HookOutcome {
                            result: HookResult::Narrowed { call },
                            ..
                        }) => crate::tools::permissions::narrows(&tool.intent.call, call),
                        _ => false,
                    };
            if !narrowing_holds
                || tool.intent.digest() != verdict.intent_digest
                || tool.permission_verdict.is_some()
                || tool.observation.is_some()
            {
                return Err(ReplayError::InvalidTransition);
            }
            if (verdict.basis == PermissionSource::SessionApproval
                || verdict.source == PermissionSource::SessionApproval)
                && !(unguarded && host_valid)
            {
                return Err(ReplayError::InvalidTransition);
            }
            let hooks_open = tool.hooks.iter().any(|hook| hook.outcome.is_none());
            let folded = tool
                .hooks
                .iter()
                .filter_map(|hook| hook.outcome.as_ref())
                .fold(Decision::Allow, |folded, outcome| {
                    folded.narrow(outcome.result.restriction(&tool.intent.call))
                });
            let stopped = tool.hooks.iter().any(|hook| {
                hook.outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.observation.stops_run())
            });
            let hooks_explained = match verdict.hook_restriction {
                None => folded == Decision::Allow && !stopped,
                Some(Decision::Ask) => folded == Decision::Ask && !stopped,
                Some(_) => folded == Decision::Deny || stopped || verdict.hooks == 0,
            };
            if hooks_open || usize::from(verdict.hooks) != tool.hooks.len() || !hooks_explained {
                return Err(ReplayError::InvalidTransition);
            }
            tool.permission_verdict = Some(verdict.clone());
            if unguarded && verdict.approved {
                for agent in &mut run.agents {
                    if let Some(worker) = &mut agent.assignment {
                        worker.verification = None;
                    }
                }
                let goal = current
                    .goals
                    .iter_mut()
                    .find(|goal| goal.active_run == Some(*run_id))
                    .ok_or(ReplayError::InvalidTransition)?;
                if let Some(contract) = &goal.plan_contract {
                    goal.success_state = contract.initial_success_state();
                    goal.success_criterion_satisfied = false;
                }
            }
        }
        _ => return Err(ReplayError::InvalidTransition),
    }
    Ok(())
}

use super::agent::names_child;
use super::{
    AgentStatus, SessionView, ToolView, active_run, active_run_goal, current_goal_mut,
    pending_tool_requests, valid_integration_source,
};
use crate::session::events::{Event, GoalPhase, ProviderCallDisposition, ReplayError};

pub(super) fn apply(
    current: &mut SessionView,
    event: &Event,
    prior: u64,
) -> Result<(), ReplayError> {
    match event {
        Event::ToolStarted {
            run_id,
            agent_run_id,
            intent,
        } if names_child(current, *run_id, *agent_run_id) => {
            current.start_child_tool(*run_id, *agent_run_id, intent)?;
        }
        Event::ToolStarted {
            run_id,
            agent_run_id,
            intent,
        } => {
            let goal = active_run_goal(current, *run_id).ok_or(ReplayError::InvalidTransition)?;
            let phase_allows_call =
                goal.phase == GoalPhase::Implementation || intent.call.is_goal_plan_prelude();
            let plan_allows_call = if let Some(contract) = &goal.plan_contract {
                !goal.awaiting_plan_contract
                    && (contract.allows_call(&intent.call)
                        || matches!(&intent.call, crate::tools::ToolCall::ApplyChangeset { .. }))
            } else {
                goal.awaiting_plan_contract && intent.call.is_goal_plan_prelude()
            };
            if !phase_allows_call
                || !plan_allows_call
                || goal
                    .effective_budget()
                    .max_tokens
                    .is_some_and(|limit| goal.token_usage_unknown || goal.known_tokens >= limit)
            {
                return Err(ReplayError::InvalidTransition);
            }
            let run = active_run(current, *run_id)?;
            let config = run.config.as_ref().ok_or(ReplayError::InvalidTransition)?;
            let primary = run.primary().ok_or(ReplayError::InvalidTransition)?;
            let integration = matches!(&intent.call, crate::tools::ToolCall::ApplyChangeset { .. });
            let valid_origin =
                if integration {
                    valid_integration_source(run, intent)
                } else {
                    primary.provider_calls.last().is_some_and(|call| {
                        call.disposition == ProviderCallDisposition::ToolRequested
                    }) && pending_tool_requests(run).is_some_and(|pending| pending >= 1)
                        && run
                            .tools
                            .last()
                            .and_then(|tool| tool.observation.as_ref())
                            .is_none_or(|observation| !observation.stops_run())
                };
            if config
                .tool_policy
                .as_ref()
                .is_none_or(|policy| policy.config_digest != intent.policy_digest)
                || primary.id != *agent_run_id
                || primary.status != AgentStatus::Active
                || !valid_origin
                || run
                    .tools
                    .iter()
                    .any(|tool| tool.intent.id == intent.id || tool.observation.is_none())
                || intent.workspace_device != config.workspace_device
                || intent.workspace_inode != config.workspace_inode
            {
                return Err(ReplayError::InvalidTransition);
            }
            run.tools.push(ToolView {
                intent: intent.clone(),
                observation: None,
                review_call_index: None,
                finished_sequence: None,
            });
        }
        Event::ToolFinished {
            run_id,
            agent_run_id,
            observation,
        } if names_child(current, *run_id, *agent_run_id) => {
            current.finish_child_tool(*run_id, *agent_run_id, observation, prior)?;
        }
        Event::ToolFinished {
            run_id,
            agent_run_id,
            observation,
        } => {
            let goal = active_run_goal(current, *run_id).ok_or(ReplayError::InvalidTransition)?;
            let out_of_plan_integration = matches!(
                &observation.intent.call,
                crate::tools::ToolCall::ApplyChangeset { .. }
            ) && goal
                .plan_contract
                .as_ref()
                .is_some_and(|contract| !contract.allows_call(&observation.intent.call));
            if out_of_plan_integration
                && (observation.disposition != crate::tools::ToolDisposition::Denied
                    || observation.guard.is_some()
                    || observation.cache_update != crate::tools::CacheUpdate::NotApplicable)
            {
                return Err(ReplayError::InvalidTransition);
            }
            let timeout_limit = match &observation.intent.call {
                crate::tools::ToolCall::Command { .. } => {
                    let run = current
                        .runs
                        .iter()
                        .find(|run| run.id == *run_id)
                        .ok_or(ReplayError::InvalidTransition)?;
                    let contract_timeout = goal
                        .plan_contract
                        .as_ref()
                        .and_then(|contract| contract.command_timeout_ms);
                    let requested = contract_timeout
                        .unwrap_or(goal.resource_settings.default_command_timeout_ms);
                    (requested > goal.resource_settings.max_command_timeout_ms)
                        .then_some((requested, goal.resource_settings.max_command_timeout_ms))
                        .map(|limit| {
                            (
                                limit,
                                run.tools
                                    .last()
                                    .is_some_and(|tool| tool.review_call_index.is_none()),
                            )
                        })
                }
                _ => None,
            };
            if let Some(((requested, maximum), no_review)) = timeout_limit
                && (!no_review
                    || observation.intent.limits.runtime_ms != requested
                    || observation.disposition != crate::tools::ToolDisposition::Limit
                    || observation.guard.is_some()
                    || observation.cache_update != crate::tools::CacheUpdate::NotApplicable
                    || observation.output
                        != crate::tools::types::command_timeout_limit_message(requested, maximum))
            {
                return Err(ReplayError::InvalidTransition);
            }
            {
                let run = active_run(current, *run_id)?;
                if run.primary().is_none_or(|agent| {
                    agent.id != *agent_run_id || agent.status != AgentStatus::Active
                }) {
                    return Err(ReplayError::InvalidTransition);
                }
                let tool = run.tools.last_mut().ok_or(ReplayError::InvalidTransition)?;
                if tool.intent != observation.intent || tool.observation.is_some() {
                    return Err(ReplayError::InvalidTransition);
                }
                tool.observation = Some(observation.clone());
                tool.finished_sequence = Some(prior);
            }
            let goal_id = current
                .goals
                .iter()
                .find(|goal| goal.active_run == Some(*run_id))
                .map(|goal| goal.id)
                .ok_or(ReplayError::InvalidTransition)?;
            let goal = current_goal_mut(current, goal_id)?;
            if let Some(contract) = goal.plan_contract.as_ref() {
                contract.observe_success(&mut goal.success_state, observation);
                goal.success_criterion_satisfied =
                    contract.success_state_satisfied(&goal.success_state);
            }
            // Tools run one at a time, so once every declared request has started, this observation completes the step.
            let all_requests_observed = current
                .runs
                .iter()
                .find(|run| run.id == *run_id)
                .and_then(pending_tool_requests)
                == Some(0);
            let unplanned_prelude = current
                .goal(goal_id)
                .is_some_and(|goal| goal.plan_contract.is_none())
                && observation.intent.call.is_goal_plan_prelude();
            if unplanned_prelude && all_requests_observed {
                current_goal_mut(current, goal_id)?.awaiting_plan_contract = false;
            }
        }
        _ => return Err(ReplayError::InvalidTransition),
    }
    Ok(())
}

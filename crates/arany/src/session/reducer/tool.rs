use super::agent::names_child;
use super::{
    AgentStatus, RunView, SessionView, ToolView, active_run, active_run_goal, current_goal_mut,
    observed_command_changeset, pending_tool_requests,
};
use crate::provider::AgentPhase;
use crate::session::events::{Event, GoalPhase, ProviderCallDisposition, ReplayError};

pub(super) fn apply(
    current: &mut SessionView,
    event: &Event,
    prior: u64,
) -> Result<(), ReplayError> {
    if let Event::ToolFinished {
        run_id,
        agent_run_id,
        observation,
    } = event
    {
        let run = active_run(current, *run_id)?;
        let configured = run
            .config
            .as_ref()
            .and_then(|config| config.tool_policy.as_ref())
            .is_some_and(|policy| policy.permissions_digest.is_some());
        let definition_mode = super::definition_mode(run, *agent_run_id)?;
        if configured || definition_mode.is_some() {
            let tool = pending_tool(run, *agent_run_id)?;
            let verdict = tool
                .permission_verdict
                .as_ref()
                .ok_or(ReplayError::InvalidTransition)?;
            if let Some(mode) = definition_mode {
                let restriction = crate::tools::permissions::PermissionConfig::evaluate_mode(
                    mode,
                    &observation.intent.call,
                )
                .0;
                if verdict.approved
                    && (restriction == crate::tools::permissions::Decision::Deny
                        || restriction == crate::tools::permissions::Decision::Ask
                            && !matches!(
                                verdict.source,
                                crate::tools::permissions::PermissionSource::Human
                                    | crate::tools::permissions::PermissionSource::AiReview
                            ))
                {
                    return Err(ReplayError::InvalidTransition);
                }
            }
            if !verdict.approved
                && (observation.attestation.is_some()
                    || observation.cache_update != crate::tools::CacheUpdate::NotApplicable
                    || !matches!(
                        observation.disposition,
                        crate::tools::ToolDisposition::Denied
                            | crate::tools::ToolDisposition::Limit
                    ))
            {
                return Err(ReplayError::InvalidTransition);
            }
        }
    }
    match event {
        Event::ToolPermissionDecided {
            run_id,
            agent_run_id,
            verdict,
        } => {
            let run = active_run(current, *run_id)?;
            let definition_mode = super::definition_mode(run, *agent_run_id)?;
            let rules_pinned = match run
                .config
                .as_ref()
                .and_then(|config| config.tool_policy.as_ref())
            {
                Some(policy) if policy.permissions_digest.is_some() => {
                    policy.unguarded_rules_digest.is_some()
                }
                _ if definition_mode.is_some() => false,
                _ => return Err(ReplayError::InvalidTransition),
            };
            if verdict.mode.is_none()
                || definition_mode == Some(crate::tools::permissions::PermissionMode::DontAsk)
                    && verdict.decision == crate::tools::permissions::Decision::Ask
            {
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
            let tool = pending_tool(run, *agent_run_id)?;
            if tool.intent.digest() != verdict.intent_digest
                || tool.permission_verdict.is_some()
                || tool.observation.is_some()
            {
                return Err(ReplayError::InvalidTransition);
            }
            let exact_rule = verdict.basis
                == crate::tools::permissions::PermissionSource::ExactCommandRule
                || verdict.source == crate::tools::permissions::PermissionSource::ExactCommandRule;
            if exact_rule && !(rules_pinned && tool.intent.call.is_unguarded()) {
                return Err(ReplayError::InvalidTransition);
            }
            tool.permission_verdict = Some(verdict.clone());
        }
        Event::ToolStarted {
            run_id,
            agent_run_id,
            intent,
            active_ceiling_ms,
        } if names_child(current, *run_id, *agent_run_id) => {
            current.start_child_tool(*run_id, *agent_run_id, intent)?;
            record_run_ceiling(current, *run_id, *active_ceiling_ms)?;
        }
        Event::ToolStarted {
            run_id,
            agent_run_id,
            intent,
            active_ceiling_ms,
        } => {
            let goal = active_run_goal(current, *run_id).ok_or(ReplayError::InvalidTransition)?;
            let phase_allows_call =
                goal.phase == GoalPhase::Implementation || intent.call.is_goal_plan_prelude();
            let plan_allows_call = if matches!(
                intent.call,
                crate::tools::ToolCall::ParseAgentDefinition { .. }
            ) {
                true
            } else if let Some(contract) = &goal.plan_contract {
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
                if let crate::tools::ToolCall::ParseAgentDefinition { index, raw_digest } =
                    &intent.call
                {
                    let parsed = run.tools.len();
                    primary.provider_calls.is_empty()
                        && run.agents.len() == 1
                        && usize::from(*index) == parsed
                        && config
                            .agent_definitions
                            .as_ref()
                            .and_then(|pin| pin.files.get(parsed))
                            .is_some_and(|file| file.digest == *raw_digest)
                        && run.tools.iter().all(|tool| {
                            tool.observation.as_ref().is_some_and(|observation| {
                                observation.disposition == crate::tools::ToolDisposition::Succeeded
                            })
                        })
                } else if integration {
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
            // The approved host run has no replayable admission until its Engine gate lands.
            if config
                .tool_policy
                .as_ref()
                .is_none_or(|policy| policy.config_digest != intent.policy_digest)
                || intent.call.is_unguarded()
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
                permission_verdict: None,
                finished_sequence: None,
            });
            record_run_ceiling(current, *run_id, *active_ceiling_ms)?;
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
                    || observation.attestation.is_some()
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
                    || observation.attestation.is_some()
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

fn pending_tool(
    run: &mut RunView,
    actor: crate::session::AgentRunId,
) -> Result<&mut ToolView, ReplayError> {
    if run
        .primary()
        .is_some_and(|primary| primary.id == actor && primary.status == AgentStatus::Active)
    {
        run.tools.last_mut()
    } else {
        run.agents
            .iter_mut()
            .find(|agent| agent.id == actor && agent.status == AgentStatus::Active)
            .and_then(|agent| agent.tools.last_mut())
    }
    .ok_or(ReplayError::InvalidTransition)
}

fn valid_integration_source(run: &RunView, intent: &crate::tools::EffectIntent) -> bool {
    let crate::tools::ToolCall::ApplyChangeset {
        source_intent_digest,
        changeset,
    } = &intent.call
    else {
        return false;
    };
    let Some(primary) = run.primary() else {
        return false;
    };
    if primary.provider_calls.last().is_none_or(|call| {
        call.disposition != ProviderCallDisposition::ToolRequested
            && call.phase != AgentPhase::ToolReview
    }) || pending_tool_requests(run) != Some(0)
    {
        return false;
    }
    run.tools
        .last()
        .and_then(|tool| tool.observation.as_ref())
        .is_some_and(|observation| {
            observation.intent.digest() == *source_intent_digest
                && observed_command_changeset(observation)
                    .is_some_and(|source| source == *changeset)
        })
}

fn record_run_ceiling(
    current: &mut SessionView,
    run_id: crate::session::RunId,
    ceiling_ms: u64,
) -> Result<(), ReplayError> {
    let goal_id = active_run_goal(current, run_id)
        .ok_or(ReplayError::InvalidTransition)?
        .id;
    super::goal::record_active_ceiling(current_goal_mut(current, goal_id)?, ceiling_ms);
    Ok(())
}

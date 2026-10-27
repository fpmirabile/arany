use super::super::agent::names_child;
use super::super::{
    AgentStatus, AgentView, RunView, SessionView, ToolView, active_run, active_run_goal,
    pending_original_tool_requests, pending_tool_requests,
};
use crate::session::events::{Event, ProviderCallDisposition, ReplayError, RunConfig};
use crate::session::{AgentRunId, RunId};
use crate::tools::EffectIntent;
use crate::tools::permissions::{PermissionMode, PermissionSource};

pub(super) fn validate_finished(
    current: &mut SessionView,
    event: &Event,
) -> Result<(), ReplayError> {
    let Event::ToolFinished {
        checkpoint,
        run_id,
        agent_run_id,
        observation,
    } = event
    else {
        return Ok(());
    };
    super::super::super::checkpoint::validate_finished(current, observation, checkpoint.as_ref())?;
    let run = active_run(current, *run_id)?;
    if super::tool_by_digest(run, *agent_run_id, &observation.intent.digest())?
        .started_sequence
        .is_none()
    {
        return Err(ReplayError::InvalidTransition);
    }
    if declared_intents_pending(run, *agent_run_id, &observation.intent.call) {
        return Err(ReplayError::InvalidTransition);
    }
    let configured = run
        .config
        .as_ref()
        .and_then(|config| config.tool_policy.as_ref())
        .is_some_and(|policy| policy.permissions_digest.is_some());
    let definition_mode = super::definition_mode(run, *agent_run_id)?;
    let restrictions = super::super::mode_restrictions(run, *agent_run_id)?;
    let unguarded = observation.intent.call.is_unguarded();
    let git_write = observation.intent.call.is_git() && observation.intent.call.facts().mutates;
    let first_use_verdict = git_write
        && super::tool_by_digest(run, *agent_run_id, &observation.intent.digest())
            .is_ok_and(|tool| tool.permission_verdict.is_some());
    if configured
        || definition_mode.is_some()
        || unguarded
        || first_use_verdict
        || observation.intent.call.is_host()
    {
        validate_permission_verdict(run, *agent_run_id, observation, restrictions, unguarded)?;
    }
    Ok(())
}

fn declared_intents_pending(
    run: &RunView,
    agent_run_id: AgentRunId,
    call: &crate::tools::ToolCall,
) -> bool {
    if run
        .primary()
        .is_some_and(|primary| primary.id == agent_run_id)
    {
        !call.is_internal()
            && !matches!(call, crate::tools::ToolCall::ApplyChangeset { .. })
            && pending_original_tool_requests(run) != Some(0)
    } else {
        run.agents
            .iter()
            .find(|agent| agent.id == agent_run_id && agent.assignment.is_none())
            .is_some_and(|agent| {
                super::super::agent::child_tool_requests(agent) != agent.tools.len()
            })
    }
}

fn validate_permission_verdict(
    run: &mut RunView,
    agent_run_id: AgentRunId,
    observation: &crate::tools::ToolObservation,
    restrictions: Vec<PermissionMode>,
    unguarded: bool,
) -> Result<(), ReplayError> {
    let tool = super::tool_by_digest(run, agent_run_id, &observation.intent.digest())?;
    let verdict = tool
        .permission_verdict
        .as_ref()
        .ok_or(ReplayError::InvalidTransition)?;
    super::permission::validate_modes(&observation.intent.call, verdict, restrictions)?;
    if unguarded
        && !matches!(
            observation.attestation,
            None | Some(crate::tools::Attestation::Unguarded(_))
        )
    {
        return Err(ReplayError::InvalidTransition);
    }
    if !verdict.approved
        && (observation.attestation.is_some()
            || observation.cache_update != crate::tools::CacheUpdate::NotApplicable
            || !matches!(
                observation.disposition,
                crate::tools::ToolDisposition::Denied | crate::tools::ToolDisposition::Limit
            ))
    {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(())
}

pub(super) fn finished(
    current: &mut SessionView,
    event: &Event,
    prior: u64,
) -> Result<(), ReplayError> {
    let Event::ToolFinished {
        run_id,
        agent_run_id,
        observation,
        ..
    } = event
    else {
        return Err(ReplayError::InvalidTransition);
    };
    if let Some(spill) = &observation.spill {
        let run = current
            .runs
            .iter()
            .find(|run| run.id == *run_id)
            .ok_or(ReplayError::InvalidTransition)?;
        let used: u64 = run
            .tools
            .iter()
            .chain(run.agents.iter().flat_map(|agent| &agent.tools))
            .filter_map(|tool| tool.observation.as_ref()?.spill.as_ref())
            .map(|spill| spill.bytes)
            .sum();
        if used.saturating_add(spill.bytes) > crate::tools::spill::MAX_SPILL_RUN_BYTES {
            return Err(ReplayError::InvalidTransition);
        }
    }
    let child = names_child(current, *run_id, *agent_run_id);
    if child {
        current.finish_child_tool(event, prior)?;
    } else {
        validate_primary_finished(current, *run_id, observation)?;
        apply_primary_finished(current, event, prior)?;
    }
    super::super::goal::observe_tool(current, *run_id, observation)
}

fn validate_primary_finished(
    current: &SessionView,
    run_id: RunId,
    observation: &crate::tools::ToolObservation,
) -> Result<(), ReplayError> {
    let goal = active_run_goal(current, run_id).ok_or(ReplayError::InvalidTransition)?;
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
    validate_timeout_limit(current, run_id, goal, observation)
}

fn validate_timeout_limit(
    current: &SessionView,
    run_id: RunId,
    goal: &super::super::GoalView,
    observation: &crate::tools::ToolObservation,
) -> Result<(), ReplayError> {
    let timeout_limit = match &observation.intent.call {
        crate::tools::ToolCall::Command { .. } => {
            let run = current
                .runs
                .iter()
                .find(|run| run.id == run_id)
                .ok_or(ReplayError::InvalidTransition)?;
            let contract_timeout = goal
                .plan_contract
                .as_ref()
                .and_then(|contract| contract.command_timeout_ms);
            let requested =
                contract_timeout.unwrap_or(goal.resource_settings.default_command_timeout_ms);
            (requested > goal.resource_settings.max_command_timeout_ms)
                .then_some((requested, goal.resource_settings.max_command_timeout_ms))
                .map(|limit| {
                    (
                        limit,
                        run.tools
                            .iter()
                            .find(|tool| tool.intent == observation.intent)
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
            || observation.limit_cause != Some(crate::tools::LimitCause::RequestedTime)
            || observation.attestation.is_some()
            || observation.cache_update != crate::tools::CacheUpdate::NotApplicable
            || observation.output
                != crate::tools::types::command_timeout_limit_message(requested, maximum))
    {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(())
}

fn apply_primary_finished(
    current: &mut SessionView,
    event: &Event,
    prior: u64,
) -> Result<(), ReplayError> {
    let Event::ToolFinished {
        checkpoint,
        run_id,
        agent_run_id,
        observation,
    } = event
    else {
        return Err(ReplayError::InvalidTransition);
    };
    {
        let run = active_run(current, *run_id)?;
        if run
            .primary()
            .is_none_or(|agent| agent.id != *agent_run_id || agent.status != AgentStatus::Active)
        {
            return Err(ReplayError::InvalidTransition);
        }
        let tool = super::tool_by_digest(run, *agent_run_id, &observation.intent.digest())?;
        if tool.intent != observation.intent || tool.observation.is_some() {
            return Err(ReplayError::InvalidTransition);
        }
        tool.checkpoint = checkpoint.clone();
        tool.observation = Some(observation.clone());
        tool.finished_sequence = Some(prior);
        if matches!(
            observation.intent.call,
            crate::tools::ToolCall::ParseAgentDefinition { .. }
        ) {
            run.definitions = super::super::DefinitionCache::default();
            let computed = super::super::compute_definition_registry(run).ok();
            run.definitions = super::super::DefinitionCache(computed);
        }
    }
    Ok(())
}

pub(super) fn proposed(
    current: &mut SessionView,
    run_id: RunId,
    agent_run_id: AgentRunId,
    intent: &EffectIntent,
    active_ceiling_ms: u64,
) -> Result<(), ReplayError> {
    if intent.limits.resource_class == crate::tools::ToolResourceClass::Build {
        let goal = active_run_goal(current, run_id).ok_or(ReplayError::InvalidTransition)?;
        if !matches!(intent.call, crate::tools::ToolCall::Command { .. })
            || goal.resource_settings.build_jobs != Some(intent.limits.build_jobs)
            || goal.resource_settings.build_cpu_cores != Some(intent.limits.build_cpu_cores)
        {
            return Err(ReplayError::InvalidTransition);
        }
    }
    let child = names_child(current, run_id, agent_run_id);
    if super::search_allowance_spent(current, &intent.call) {
        return Err(ReplayError::InvalidTransition);
    }
    if child
        && intent.limits.network == crate::tools::NetworkGrant::Proxy
        && !intent.call.is_web()
    {
        return Err(ReplayError::InvalidTransition);
    }
    if child {
        current.start_child_tool(run_id, agent_run_id, intent)?;
    } else {
        start_primary_tool(current, run_id, agent_run_id, intent)?;
    }
    super::record_tool_ceiling(
        current,
        run_id,
        agent_run_id,
        &intent.digest(),
        active_ceiling_ms,
    )
}

fn start_primary_tool(
    current: &mut SessionView,
    run_id: RunId,
    agent_run_id: AgentRunId,
    intent: &EffectIntent,
) -> Result<(), ReplayError> {
    let goal = active_run_goal(current, run_id).ok_or(ReplayError::InvalidTransition)?;
    super::validate_primary_tool_scope(current, run_id, &intent.call)
        .map_err(|_| ReplayError::InvalidTransition)?;
    if goal
        .effective_budget()
        .max_tokens
        .is_some_and(|limit| goal.token_usage_unknown || goal.known_tokens >= limit)
    {
        return Err(ReplayError::InvalidTransition);
    }
    let run = active_run(current, run_id)?;
    let config = run.config.as_ref().ok_or(ReplayError::InvalidTransition)?;
    let primary = run.primary().ok_or(ReplayError::InvalidTransition)?;
    let valid_origin = valid_origin(run, primary, config, intent);
    if config
        .tool_policy
        .as_ref()
        .is_none_or(|policy| policy.config_digest != intent.policy_digest)
        || primary.id != agent_run_id
        || primary.status != AgentStatus::Active
        || !valid_origin
        || !run.hooks_settled()
        || run.tools.iter().any(|tool| tool.intent.id == intent.id)
        || intent.workspace_device != config.workspace_device
        || intent.workspace_inode != config.workspace_inode
    {
        return Err(ReplayError::InvalidTransition);
    }
    let after = super::pending_narrowing_source(run)
        .filter(|(_, source)| source.narrowed_call() == Some(&intent.call))
        .map(|(index, _)| index)
        .or_else(|| run.tools.iter().position(|tool| matches!(&intent.call,
            crate::tools::ToolCall::ApplyChangeset {
                source: crate::session::worker::ChangesetSource::CommandReceipt { source_intent_digest, .. }
            } if *source_intent_digest == tool.intent.digest())));
    let narrowing_source = after.and_then(|index| {
        let source = &run.tools[index];
        source.narrowed_call().map(|_| source.intent.digest())
    });
    let index = after.map_or(run.tools.len(), |index| index + 1);
    run.tools.insert(
        index,
        ToolView {
            checkpoint: None,
            intent: intent.clone(),
            observation: None,
            review_call_index: None,
            active_time_reserved: false,
            narrowing_source,
            permission_verdict: None,
            started_sequence: None,
            hooks: Vec::new(),
            hook_sequence: None,
            finished_sequence: None,
        },
    );
    Ok(())
}

fn valid_origin(
    run: &RunView,
    primary: &AgentView,
    config: &RunConfig,
    intent: &EffectIntent,
) -> bool {
    let integration = matches!(&intent.call, crate::tools::ToolCall::ApplyChangeset { .. });
    if let crate::tools::ToolCall::ParseAgentDefinition { index, raw_digest } = &intent.call {
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
    } else if run.is_invoked_skill_call(&intent.call) && primary.provider_calls.is_empty() {
        run.agents.len() == 1
            && run.tools.len() == run.parse_count()
            && super::super::definition_registry(run).is_ok()
            && run.provider_sequences.is_empty()
    } else if integration {
        super::valid_integration_source(run, intent)
    } else {
        primary
            .provider_calls
            .iter()
            .rev()
            .find(|call| call.phase != crate::provider::AgentPhase::ToolReview)
            .is_some_and(|call| call.disposition == ProviderCallDisposition::ToolRequested)
            && pending_tool_requests(run).is_some_and(|pending| pending >= 1)
            && super::valid_narrowing_source(run, intent)
            && run
                .tools
                .iter()
                .filter_map(|tool| tool.observation.as_ref())
                .all(|observation| !observation.stops_run())
    }
}

pub(super) fn started(
    current: &mut SessionView,
    event: &Event,
    prior: u64,
) -> Result<(), ReplayError> {
    let Event::ToolStarted {
        run_id,
        agent_run_id,
        intent,
        active_ceiling_ms,
        verdict,
    } = event
    else {
        return Err(ReplayError::InvalidTransition);
    };
    let pending = super::tool_by_digest(
        active_run(current, *run_id)?,
        *agent_run_id,
        &EffectIntent::from(intent.clone()).digest(),
    )
    .is_ok_and(|tool| tool.intent.id == intent.id && tool.started_sequence.is_none());
    if active_run(current, *run_id)?.hook_stop_reason().is_some()
        && (!pending || verdict.as_ref().is_none_or(|verdict| verdict.approved))
    {
        return Err(ReplayError::InvalidTransition);
    }
    if !pending
        && verdict
            .as_ref()
            .is_some_and(|verdict| verdict.source == PermissionSource::Human)
    {
        return Err(ReplayError::InvalidTransition);
    }
    if !pending {
        proposed(
            current,
            *run_id,
            *agent_run_id,
            &intent.clone().into(),
            *active_ceiling_ms,
        )?;
    }
    if declared_intents_pending(active_run(current, *run_id)?, *agent_run_id, &intent.call) {
        return Err(ReplayError::InvalidTransition);
    }
    let tool = super::tool_by_digest(
        active_run(current, *run_id)?,
        *agent_run_id,
        &EffectIntent::from(intent.clone()).digest(),
    )?;
    if tool.intent != intent.clone().into() || tool.started_sequence.is_some() {
        return Err(ReplayError::InvalidTransition);
    }
    if verdict.is_some() {
        super::permission::apply(current, event)?;
    } else {
        let run = active_run(current, *run_id)?;
        if super::permissions_pinned(run)
            || super::definition_mode(run, *agent_run_id)?.is_some()
            || intent.call.is_host()
            || intent.call.is_unguarded()
            || intent.call.is_git() && intent.call.facts().mutates
        {
            return Err(ReplayError::InvalidTransition);
        }
    }
    super::tool_by_digest(
        active_run(current, *run_id)?,
        *agent_run_id,
        &EffectIntent::from(intent.clone()).digest(),
    )?
    .started_sequence = Some(prior);
    super::record_tool_ceiling(
        current,
        *run_id,
        *agent_run_id,
        &EffectIntent::from(intent.clone()).digest(),
        *active_ceiling_ms,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;

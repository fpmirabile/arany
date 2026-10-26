use super::super::{
    AgentStatus, AgentView, RunView, SessionView, active_run, active_run_goal, definition_registry,
    worker,
};
use crate::session::events::{
    AgentRole, ChildGrant, Event, ProviderCallDisposition, ReplayError, RunConfig, RunId,
};

pub(super) fn apply(current: &mut SessionView, event: &Event) -> Result<(), ReplayError> {
    let Event::AgentSpawned {
        run_id,
        agent_run_id,
        role,
        ordinal,
        objective,
        grant,
        definition,
        profile,
        assignment,
    } = event
    else {
        return Err(ReplayError::InvalidTransition);
    };
    worker::validate_spawn(current, event)?;
    validate_child_budget(current, *run_id, *role)?;
    let run = active_run(current, *run_id)?;
    let config = run.config.as_ref().ok_or(ReplayError::InvalidTransition)?;
    let valid_profile = profile_fits(config, role, profile, assignment.is_some());
    if !valid_profile {
        return Err(ReplayError::InvalidTransition);
    }
    if grant.is_some_and(|grant| grant.unguarded() != crate::tools::UnguardedGrant::Denied) {
        return Err(ReplayError::InvalidTransition);
    }
    let child_grant = if config.tool_policy.is_some() {
        ChildGrant::ReadOnly {
            unguarded: grant.map_or(crate::tools::UnguardedGrant::Denied, ChildGrant::unguarded),
        }
    } else {
        ChildGrant::NoTools
    };
    match role {
        AgentRole::Primary if run.agents.is_empty() && *ordinal == 0 => {}
        AgentRole::Child
            if !run.agents.is_empty()
                && *ordinal as usize == run.agents.len()
                && *ordinal <= config.policy.max_children()
                && (*grant == Some(child_grant)
                    || assignment.is_some()
                        && matches!(*grant, Some(ChildGrant::EditOnly { .. }))) => {}
        _ => return Err(ReplayError::InvalidTransition),
    }
    if run.agents.iter().any(|agent| agent.id == *agent_run_id) {
        return Err(ReplayError::InvalidTransition);
    }
    if *role == AgentRole::Child
        && run.agents[0]
            .provider_calls
            .iter()
            .rev()
            .find(|call| call.delegation.is_some())
            .is_none_or(|call| call.disposition != ProviderCallDisposition::Delegated)
    {
        return Err(ReplayError::InvalidTransition);
    }
    validate_definition(run, *role, definition, assignment.is_some())?;
    run.agents.push(AgentView {
        tool_refusal: None,
        tool_refusal_sequence: None,
        profile: profile.clone(),
        assignment: assignment.clone().map(worker::WorkerView::new),
        definition: definition.clone(),
        id: *agent_run_id,
        role: *role,
        ordinal: *ordinal,
        objective: objective.clone(),
        summary: None,
        result: None,
        provider_calls: Vec::new(),
        steering_notes: Vec::new(),
        grant: *grant,
        tools: Vec::new(),
        status: AgentStatus::Active,
    });
    Ok(())
}

fn validate_child_budget(
    current: &SessionView,
    run_id: RunId,
    role: AgentRole,
) -> Result<(), ReplayError> {
    if role == AgentRole::Child {
        let goal = active_run_goal(current, run_id).ok_or(ReplayError::InvalidTransition)?;
        let budget = goal.effective_budget();
        if budget
            .max_provider_calls
            .is_some_and(|limit| goal.provider_calls >= limit)
            || budget
                .max_tokens
                .is_some_and(|limit| goal.token_usage_unknown || goal.known_tokens >= limit)
        {
            return Err(ReplayError::InvalidTransition);
        }
    }
    Ok(())
}

fn profile_fits(
    config: &RunConfig,
    role: &AgentRole,
    profile: &Option<String>,
    assigned: bool,
) -> bool {
    if assigned {
        profile.is_none()
    } else {
        match &config.roster {
            Some(roster) => match role {
                AgentRole::Primary => profile.as_ref() == Some(&roster.providers.primary),
                AgentRole::Child => profile
                    .as_ref()
                    .is_some_and(|name| roster.child_profiles.contains(name)),
            },
            None => profile.is_none(),
        }
    }
}

fn validate_definition(
    run: &RunView,
    role: AgentRole,
    definition: &Option<String>,
    assigned: bool,
) -> Result<(), ReplayError> {
    if let Some(name) = definition {
        let definitions = definition_registry(run)?;
        if role != AgentRole::Child
            || definitions
                .get(name)
                .is_none_or(|definition| definition.editor() != assigned)
        {
            return Err(ReplayError::InvalidTransition);
        }
    }
    Ok(())
}

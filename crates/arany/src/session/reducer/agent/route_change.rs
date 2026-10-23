use super::super::{
    AgentStatus, AgentView, GoalView, RunStatus, RunView, SessionView, current_goal_mut,
};
use crate::provider::{
    MAX_PROVIDER_RETRIES, MAX_PROVIDER_RETRY_DELAY_MS, MAX_PROVIDER_SWITCHES, ProviderRole,
};
use crate::session::events::{
    AgentRole, AgentRunId, Event, GoalId, GoalStatus, ReplayError, RunId,
};

struct RouteChange<'a> {
    run: &'a RunView,
    goal: Option<&'a GoalView>,
    active: bool,
    charge_goal: bool,
}

fn route_change_admission(
    current: &SessionView,
    run_id: RunId,
    agent_run_id: AgentRunId,
    role: ProviderRole,
) -> Result<RouteChange<'_>, ReplayError> {
    let compaction = role == ProviderRole::Compaction;
    let run = current
        .runs
        .last()
        .filter(|run| run.id == run_id)
        .ok_or(ReplayError::InvalidTransition)?;
    let active = run.status == RunStatus::Active;
    if !active && !(compaction && run.status.is_terminal()) {
        return Err(ReplayError::InvalidTransition);
    }
    // Only a between-Run compaction outside a running Goal is uncharged.
    let goal = current
        .goals
        .iter()
        .find(|goal| goal.run_ids.contains(&run_id));
    if goal.is_none() && !compaction {
        return Err(ReplayError::InvalidTransition);
    }
    let charge_goal = goal.is_some_and(|goal| goal.status == GoalStatus::Running);
    let reserved = match goal {
        None => true,
        Some(goal) if compaction => goal.compaction_call_reserved == Some(run_id),
        Some(goal) => goal.unobserved_provider_calls.contains(&agent_run_id),
    };
    // Unknown failed-attempt usage cannot fit a finite token budget.
    if (active || charge_goal)
        && (!reserved
            || !charge_goal
            || goal.is_none_or(|goal| {
                goal.effective_budget().max_tokens.is_some()
                    || goal
                        .effective_budget()
                        .max_provider_calls
                        .is_some_and(|limit| goal.provider_calls >= limit)
            }))
    {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(RouteChange {
        run,
        goal,
        active,
        charge_goal,
    })
}

fn attempt_role_fits(run: &RunView, agent: &AgentView, role: ProviderRole) -> bool {
    match role {
        // A child's own pending Tool makes its open call a review, never a child turn.
        ProviderRole::Child => {
            agent.role == AgentRole::Child
                && agent
                    .tools
                    .last()
                    .is_none_or(|tool| tool.observation.is_some())
        }
        ProviderRole::Review => {
            let own_tool = match agent.role {
                AgentRole::Child => agent.tools.last(),
                AgentRole::Primary => run.tools.last(),
            };
            own_tool.is_some_and(|tool| tool.observation.is_none())
        }
        ProviderRole::Primary => {
            agent.role == AgentRole::Primary
                && run
                    .tools
                    .last()
                    .is_none_or(|tool| tool.observation.is_some())
        }
        ProviderRole::Compaction => agent.role == AgentRole::Primary,
    }
}

pub(super) fn retry(current: &mut SessionView, event: &Event) -> Result<(), ReplayError> {
    let Event::ProviderRetryScheduled {
        run_id,
        agent_run_id,
        retry,
    } = event
    else {
        return Err(ReplayError::InvalidTransition);
    };
    let compaction = retry.role == ProviderRole::Compaction;
    let RouteChange {
        run,
        goal,
        active,
        charge_goal,
    } = route_change_admission(current, *run_id, *agent_run_id, retry.role)?;
    let goal_id = goal.map(|goal| goal.id);
    let agent = run
        .agents
        .iter()
        .find(|agent| agent.id == *agent_run_id)
        .ok_or(ReplayError::InvalidTransition)?;
    let index = route_call_index(current, run, agent, compaction, active);
    // One bound per semantic call: compaction numbers its calls apart, every other role shares the actor's.
    let earlier: Vec<_> = run
        .provider_retries
        .iter()
        .filter(|(actor, prior)| {
            actor == agent_run_id
                && (prior.role == ProviderRole::Compaction) == compaction
                && prior.call_index == retry.call_index
        })
        .collect();
    if earlier.iter().any(|(_, prior)| prior.role != retry.role) {
        return Err(ReplayError::InvalidTransition);
    }
    // Every wait of the call fits the active time its reservation admitted.
    let waited = earlier
        .iter()
        .map(|(_, prior)| u64::from(prior.delay_ms))
        .sum::<u64>()
        .saturating_add(u64::from(retry.delay_ms));
    let allowance = match goal {
        Some(goal) if charge_goal => goal.unsettled_active_charge_ms(),
        _ => u64::from(MAX_PROVIDER_RETRY_DELAY_MS),
    };
    if !attempt_role_fits(run, agent, retry.role)
        || active && agent.status != AgentStatus::Active
        || usize::try_from(retry.call_index).ok() != Some(index)
        || usize::from(retry.attempt) != earlier.len() + 1
        || retry.attempt > MAX_PROVIDER_RETRIES
        || waited > allowance
        || retry.role == ProviderRole::Primary
            && run
                .display
                .iter()
                .any(|chunk| chunk.call_index == retry.call_index)
    {
        return Err(ReplayError::InvalidTransition);
    }
    current
        .runs
        .last_mut()
        .ok_or(ReplayError::InvalidTransition)?
        .provider_retries
        .push((*agent_run_id, retry.clone()));
    charge_route_change(current, charge_goal, goal_id)?;
    Ok(())
}

pub(super) fn fallback(current: &mut SessionView, event: &Event) -> Result<(), ReplayError> {
    let Event::ProviderFallbackUsed {
        run_id,
        agent_run_id,
        fallback,
    } = event
    else {
        return Err(ReplayError::InvalidTransition);
    };
    let compaction = fallback.role == ProviderRole::Compaction;
    let RouteChange {
        run,
        goal,
        active,
        charge_goal,
    } = route_change_admission(current, *run_id, *agent_run_id, fallback.role)?;
    let goal_id = goal.map(|goal| goal.id);
    let agent = run
        .agents
        .iter()
        .find(|agent| agent.id == *agent_run_id)
        .ok_or(ReplayError::InvalidTransition)?;
    let valid_role = attempt_role_fits(run, agent, fallback.role);
    let index = route_call_index(current, run, agent, compaction, active);
    let chain = run
        .provider_chain(*agent_run_id, fallback.role)
        .ok_or(ReplayError::InvalidTransition)?;
    let previous = run
        .provider_fallbacks
        .iter()
        .rev()
        .find(|(actor, prior)| actor == agent_run_id && prior.role == fallback.role)
        .map(|(_, prior)| &prior.to)
        .unwrap_or(&chain[0]);
    let next = chain
        .iter()
        .position(|identity| identity == previous)
        .and_then(|index| chain.get(index + 1));
    if !valid_role
        || active && agent.status != AgentStatus::Active
        || usize::try_from(fallback.call_index).ok() != Some(index)
        || previous != &fallback.from
        || next != Some(&fallback.to)
        || run.provider_fallbacks.len() >= MAX_PROVIDER_SWITCHES
        || !compaction
            && agent.role == AgentRole::Primary
            && run
                .display
                .iter()
                .any(|chunk| chunk.call_index == fallback.call_index)
    {
        return Err(ReplayError::InvalidTransition);
    }
    current
        .runs
        .last_mut()
        .ok_or(ReplayError::InvalidTransition)?
        .provider_fallbacks
        .push((*agent_run_id, fallback.clone()));
    charge_route_change(current, charge_goal, goal_id)?;
    Ok(())
}

fn route_call_index(
    current: &SessionView,
    run: &RunView,
    agent: &AgentView,
    compaction: bool,
    active: bool,
) -> usize {
    if compaction {
        if active {
            run.tools.len()
        } else {
            current.compactions.len()
        }
    } else {
        agent.provider_calls.len()
    }
}

fn charge_route_change(
    current: &mut SessionView,
    charge_goal: bool,
    goal_id: Option<GoalId>,
) -> Result<(), ReplayError> {
    if let (true, Some(goal_id)) = (charge_goal, goal_id) {
        let goal = current_goal_mut(current, goal_id)?;
        goal.provider_calls = goal
            .provider_calls
            .checked_add(1)
            .ok_or(ReplayError::InvalidTransition)?;
        goal.token_usage_unknown = true;
    }
    Ok(())
}

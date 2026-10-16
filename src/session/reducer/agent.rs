use super::{
    AgentStatus, AgentView, RunView, SessionView, ToolView, active_run, active_run_goal,
    all_children_finished, current_goal_mut, is_delegated_synthesis, is_tool_continuation,
};
use crate::provider::AgentPhase;
use crate::session::events::{
    AgentDisposition, AgentRole, AgentRunId, ChildGrant, Event, GoalStatus,
    ProviderCallDisposition, ProviderCallRecord, ReplayError, RunId,
};
use crate::tools::{CacheUpdate, EffectIntent, ToolDisposition, ToolObservation};

pub(super) fn child_tool_requests(agent: &AgentView) -> usize {
    agent
        .provider_calls
        .iter()
        .filter(|call| call.disposition == ProviderCallDisposition::ToolRequested)
        .count()
}

/// A child step follows the previous step's finished Tool, or is the child's first step.
pub(super) fn is_child_step(agent: &AgentView, record: &ProviderCallRecord) -> bool {
    agent.role == AgentRole::Child
        && record.phase == AgentPhase::ChildWork
        && (agent.provider_calls.is_empty()
            || (agent
                .provider_calls
                .last()
                .is_some_and(|call| call.disposition == ProviderCallDisposition::ToolRequested)
                && agent.tools.len() == child_tool_requests(agent)
                && agent
                    .tools
                    .last()
                    .and_then(|tool| tool.observation.as_ref())
                    .is_some_and(|observation| !observation.stops_run())))
}

pub(super) fn names_child(view: &SessionView, run_id: RunId, agent_run_id: AgentRunId) -> bool {
    view.runs
        .last()
        .filter(|run| run.id == run_id)
        .and_then(|run| run.agents.iter().find(|agent| agent.id == agent_run_id))
        .is_some_and(|agent| agent.role == AgentRole::Child)
}

fn child_tool_is_admissible(
    run: &RunView,
    agent_run_id: AgentRunId,
    intent: &EffectIntent,
) -> bool {
    let Some(config) = run.config.as_ref() else {
        return false;
    };
    let Some(child) = run.agents.iter().find(|agent| agent.id == agent_run_id) else {
        return false;
    };
    child.status == AgentStatus::Active
        && child.grant == Some(ChildGrant::ReadOnly)
        && child
            .provider_calls
            .last()
            .is_some_and(|call| call.disposition == ProviderCallDisposition::ToolRequested)
        && child_tool_requests(child) == child.tools.len() + 1
        && config
            .tool_policy
            .as_ref()
            .is_some_and(|policy| policy.config_digest == intent.policy_digest)
        && intent.workspace_device == config.workspace_device
        && intent.workspace_inode == config.workspace_inode
        && intent.call.is_child_research()
        && run
            .tools
            .iter()
            .chain(run.agents.iter().flat_map(|agent| agent.tools.iter()))
            .all(|tool| tool.observation.is_some() && tool.intent.id != intent.id)
}

impl SessionView {
    pub(super) fn start_child_tool(
        &mut self,
        run_id: RunId,
        agent_run_id: AgentRunId,
        intent: &EffectIntent,
    ) -> Result<(), ReplayError> {
        let goal = active_run_goal(self, run_id).ok_or(ReplayError::InvalidTransition)?;
        if goal.status != GoalStatus::Running
            || goal
                .effective_budget()
                .max_tokens
                .is_some_and(|limit| goal.token_usage_unknown || goal.known_tokens >= limit)
        {
            return Err(ReplayError::InvalidTransition);
        }
        let run = active_run(self, run_id)?;
        if !child_tool_is_admissible(run, agent_run_id, intent) {
            return Err(ReplayError::InvalidTransition);
        }
        let child = run
            .agents
            .iter_mut()
            .find(|agent| agent.id == agent_run_id)
            .ok_or(ReplayError::InvalidTransition)?;
        child.tools.push(ToolView {
            intent: intent.clone(),
            observation: None,
            review_call_index: None,
            finished_sequence: None,
        });
        Ok(())
    }

    /// A child's Tool that the Goal plan contract refuses is journaled as `Denied` without dispatch.
    pub(super) fn finish_child_tool(
        &mut self,
        run_id: RunId,
        agent_run_id: AgentRunId,
        observation: &ToolObservation,
        prior: u64,
    ) -> Result<(), ReplayError> {
        let goal = active_run_goal(self, run_id).ok_or(ReplayError::InvalidTransition)?;
        let out_of_plan = goal
            .plan_contract
            .as_ref()
            .is_some_and(|contract| !contract.allows_call(&observation.intent.call));
        if out_of_plan
            && (observation.disposition != ToolDisposition::Denied
                || observation.attestation.is_some()
                || observation.cache_update != CacheUpdate::NotApplicable)
        {
            return Err(ReplayError::InvalidTransition);
        }
        let run = active_run(self, run_id)?;
        let child = run
            .agents
            .iter_mut()
            .find(|agent| agent.id == agent_run_id)
            .ok_or(ReplayError::InvalidTransition)?;
        if child.role != AgentRole::Child || child.status != AgentStatus::Active {
            return Err(ReplayError::InvalidTransition);
        }
        let tool = child
            .tools
            .last_mut()
            .ok_or(ReplayError::InvalidTransition)?;
        if tool.intent != observation.intent || tool.observation.is_some() {
            return Err(ReplayError::InvalidTransition);
        }
        tool.observation = Some(observation.clone());
        tool.finished_sequence = Some(prior);
        Ok(())
    }
}

pub(super) fn apply(current: &mut SessionView, event: &Event) -> Result<(), ReplayError> {
    match event {
        Event::AgentSpawned {
            run_id,
            agent_run_id,
            role,
            ordinal,
            objective,
            grant,
        } => {
            if *role == AgentRole::Child {
                let goal =
                    active_run_goal(current, *run_id).ok_or(ReplayError::InvalidTransition)?;
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
            let run = active_run(current, *run_id)?;
            let config = run.config.as_ref().ok_or(ReplayError::InvalidTransition)?;
            let child_grant = if config.tool_policy.is_some() {
                ChildGrant::ReadOnly
            } else {
                ChildGrant::NoTools
            };
            match role {
                AgentRole::Primary if run.agents.is_empty() && *ordinal == 0 => {}
                AgentRole::Child
                    if !run.agents.is_empty()
                        && *ordinal as usize == run.agents.len()
                        && *ordinal <= config.policy.max_children()
                        && *grant == Some(child_grant) => {}
                _ => return Err(ReplayError::InvalidTransition),
            }
            if run.agents.iter().any(|agent| agent.id == *agent_run_id) {
                return Err(ReplayError::InvalidTransition);
            }
            if *role == AgentRole::Child
                && run.agents[0]
                    .provider_calls
                    .last()
                    .is_none_or(|call| call.disposition != ProviderCallDisposition::Delegated)
            {
                return Err(ReplayError::InvalidTransition);
            }
            run.agents.push(AgentView {
                id: *agent_run_id,
                role: *role,
                ordinal: *ordinal,
                objective: objective.clone(),
                summary: None,
                result: None,
                provider_calls: Vec::new(),
                grant: *grant,
                tools: Vec::new(),
                status: AgentStatus::Active,
            });
        }
        Event::ProviderCallRecorded {
            run_id,
            agent_run_id,
            record,
        } => {
            let goal = active_run_goal(current, *run_id).ok_or(ReplayError::InvalidTransition)?;
            if !goal.unobserved_provider_calls.contains(agent_run_id) {
                return Err(ReplayError::InvalidTransition);
            }
            let goal_id = goal.id;
            let run = active_run(current, *run_id)?;
            let index = run
                .agents
                .iter()
                .position(|agent| agent.id == *agent_run_id)
                .ok_or(ReplayError::InvalidTransition)?;
            let agent = &run.agents[index];
            let tools_enabled = run
                .config
                .as_ref()
                .is_some_and(|config| config.tool_policy.is_some());
            let tool_continuation = tools_enabled && is_tool_continuation(run, agent, record);
            let delegated = tools_enabled && is_delegated_synthesis(run, agent, record);
            let tool_review = tools_enabled && is_tool_review(run, agent, record);
            let valid = tool_continuation
                || tool_review
                || delegated
                || match (agent.role, agent.provider_calls.as_slice(), record.phase) {
                    (AgentRole::Primary, [], AgentPhase::RootPlan) => true,
                    (AgentRole::Primary, [first], AgentPhase::RootSynthesis)
                        if first.disposition == ProviderCallDisposition::Delegated =>
                    {
                        all_children_finished(run)
                    }
                    (AgentRole::Child, _, AgentPhase::ChildWork) => is_child_step(agent, record),
                    _ => false,
                };
            if agent.status != AgentStatus::Active
                || !valid
                || record.disposition == ProviderCallDisposition::ToolRequested
                    && !(tools_enabled
                        && match agent.role {
                            AgentRole::Primary => record.phase != AgentPhase::ToolReview,
                            AgentRole::Child => {
                                record.phase == AgentPhase::ChildWork
                                    && agent.grant == Some(ChildGrant::ReadOnly)
                                    && record.requested_tools == 1
                                    && child_tool_requests(agent)
                                        < crate::session::MAX_CHILD_TOOL_CALLS
                            }
                        })
                || (record.phase != AgentPhase::RootPlan
                    && record.disposition == ProviderCallDisposition::Delegated)
            {
                return Err(ReplayError::InvalidTransition);
            }
            if run.agents[index].role == AgentRole::Primary
                && matches!(
                    record.disposition,
                    ProviderCallDisposition::Delegated | ProviderCallDisposition::ToolRequested
                )
                && run.display.iter().any(|chunk| {
                    chunk.category == crate::session::events::DisplayCategory::Answer
                        && usize::try_from(chunk.call_index).ok()
                            == Some(run.agents[index].provider_calls.len())
                })
            {
                return Err(ReplayError::InvalidTransition);
            }
            if record.phase == AgentPhase::ToolReview {
                run.tools
                    .last_mut()
                    .ok_or(ReplayError::InvalidTransition)?
                    .review_call_index = Some(
                    u32::try_from(run.agents[index].provider_calls.len())
                        .map_err(|_| ReplayError::InvalidTransition)?,
                );
            }
            let primary_call = run.agents[index].role == AgentRole::Primary;
            run.agents[index].provider_calls.push(record.clone());
            let goal = current_goal_mut(current, goal_id)?;
            let reservation_index = goal
                .unobserved_provider_calls
                .iter()
                .position(|reserved| reserved == agent_run_id)
                .ok_or(ReplayError::InvalidTransition)?;
            goal.unobserved_provider_calls.remove(reservation_index);
            goal.known_tokens = goal
                .known_tokens
                .checked_add(u64::from(record.input_tokens.unwrap_or_default()))
                .and_then(|tokens| {
                    tokens.checked_add(u64::from(record.output_tokens.unwrap_or_default()))
                })
                .ok_or(ReplayError::InvalidTransition)?;
            goal.known_cached_tokens = goal
                .known_cached_tokens
                .checked_add(u64::from(
                    record.cache_read_input_tokens.unwrap_or_default(),
                ))
                .ok_or(ReplayError::InvalidTransition)?;
            goal.token_usage_unknown |=
                record.input_tokens.is_none() || record.output_tokens.is_none();
            if primary_call
                && record.disposition == ProviderCallDisposition::ToolRequested
                && goal.plan_contract.is_none()
            {
                if goal.awaiting_plan_contract {
                    return Err(ReplayError::InvalidTransition);
                }
                goal.awaiting_plan_contract = true;
            }
        }
        Event::AgentFinished {
            run_id,
            agent_run_id,
            disposition,
            summary,
            result,
        } => {
            // Unknown usage blocks Tool and Delegate outcomes and Provider calls, not a Finish.
            let over_token_cap = match active_run_goal(current, *run_id) {
                Some(goal) => goal
                    .effective_budget()
                    .max_tokens
                    .is_some_and(|limit| goal.known_tokens > limit),
                None => return Err(ReplayError::InvalidTransition),
            };
            let run = active_run(current, *run_id)?;
            let index = run
                .agents
                .iter()
                .position(|agent| agent.id == *agent_run_id)
                .ok_or(ReplayError::InvalidTransition)?;
            if run.agents[index].status != AgentStatus::Active {
                return Err(ReplayError::InvalidTransition);
            }
            if run.agents[index].role == AgentRole::Primary
                && *disposition == AgentDisposition::Finished
                && run
                    .agents
                    .iter()
                    .skip(1)
                    .any(|agent| agent.status != AgentStatus::Finished)
            {
                return Err(ReplayError::InvalidTransition);
            }
            if run.agents[index].role == AgentRole::Child
                && result.as_ref().is_some_and(|text| text.len() > 16 * 1024)
            {
                return Err(ReplayError::InvalidTransition);
            }
            if run.agents[index].role == AgentRole::Child
                && (run.agents[index]
                    .tools
                    .iter()
                    .any(|tool| tool.observation.is_none())
                    || *disposition == AgentDisposition::Finished
                        && run.agents[index].tools.iter().any(|tool| {
                            tool.observation
                                .as_ref()
                                .is_some_and(|observation| observation.stops_run())
                        }))
            {
                return Err(ReplayError::InvalidTransition);
            }
            if *disposition == AgentDisposition::Finished
                && run.tools.iter().any(|tool| {
                    tool.observation.is_none()
                        || tool
                            .observation
                            .as_ref()
                            .is_some_and(|observation| observation.stops_run())
                })
            {
                return Err(ReplayError::InvalidTransition);
            }
            if *disposition == AgentDisposition::Finished
                && run.agents[index].provider_calls.last().is_none_or(|call| {
                    call.disposition != ProviderCallDisposition::Finished
                        || call.phase == crate::provider::AgentPhase::ToolReview
                })
            {
                return Err(ReplayError::InvalidTransition);
            }
            if *disposition == AgentDisposition::Finished && over_token_cap {
                return Err(ReplayError::InvalidTransition);
            }
            if *disposition == AgentDisposition::Finished
                && run.agents[index].role == AgentRole::Primary
            {
                let call = run.agents[index].provider_calls.len().saturating_sub(1);
                for chunk in run.display.iter().filter(|chunk| {
                    chunk.category == crate::session::events::DisplayCategory::Answer
                        && usize::try_from(chunk.call_index).ok() == Some(call)
                }) {
                    let start = chunk.offset as usize;
                    if result.as_ref().is_none_or(|text| {
                        text.get(start..start + chunk.text.len()) != Some(chunk.text.as_str())
                    }) {
                        return Err(ReplayError::InvalidTransition);
                    }
                }
            }
            let agent = &mut run.agents[index];
            agent.status = match disposition {
                AgentDisposition::Finished => AgentStatus::Finished,
                AgentDisposition::Failed => AgentStatus::Failed,
                AgentDisposition::Cancelled => AgentStatus::Cancelled,
            };
            agent.summary = summary.clone();
            agent.result = result.clone();
        }
        _ => return Err(ReplayError::InvalidTransition),
    }
    Ok(())
}

fn is_tool_review(run: &RunView, agent: &AgentView, record: &ProviderCallRecord) -> bool {
    agent.role == AgentRole::Primary
        && record.phase == AgentPhase::ToolReview
        && agent.provider_calls.last().is_some_and(|call| {
            call.disposition == ProviderCallDisposition::ToolRequested
                || call.phase == AgentPhase::ToolReview
                    && run.tools.last().is_some_and(|tool| {
                        matches!(
                            tool.intent.call,
                            crate::tools::ToolCall::ApplyChangeset { .. }
                        )
                    })
        })
        && run
            .tools
            .last()
            .is_some_and(|tool| tool.observation.is_none() && tool.review_call_index.is_none())
}

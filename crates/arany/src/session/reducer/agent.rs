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

pub(super) fn is_child_step(agent: &AgentView, record: &ProviderCallRecord) -> bool {
    agent.role == AgentRole::Child
        && record.phase == AgentPhase::ChildWork
        && (agent.provider_calls.is_empty()
            || (agent
                .provider_calls
                .iter()
                .rev()
                .find(|call| call.phase != AgentPhase::ToolReview)
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
            permission_verdict: None,
            finished_sequence: None,
        });
        Ok(())
    }

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
        Event::ProviderFallbackUsed {
            run_id,
            agent_run_id,
            fallback,
        } => {
            use crate::provider::{MAX_PROVIDER_SWITCHES, ProviderRole};
            let compaction = fallback.role == ProviderRole::Compaction;
            let run = current
                .runs
                .last()
                .filter(|run| run.id == *run_id)
                .ok_or(ReplayError::InvalidTransition)?;
            let active = run.status == super::RunStatus::Active;
            if !active && !(compaction && run.status.is_terminal()) {
                return Err(ReplayError::InvalidTransition);
            }
            // A forked Session inherits Runs but not their Goal; only a Compaction may switch there.
            let goal = current
                .goals
                .iter()
                .find(|goal| goal.run_ids.contains(run_id));
            if goal.is_none() && !compaction {
                return Err(ReplayError::InvalidTransition);
            }
            let charge_goal = goal.is_some_and(|goal| goal.status == GoalStatus::Running);
            let goal_id = goal.map(|goal| goal.id);
            let reserved = match goal {
                None => true,
                Some(goal) if compaction => goal.compaction_call_reserved == Some(*run_id),
                Some(goal) => goal.unobserved_provider_calls.contains(agent_run_id),
            };
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
            let agent = run
                .agents
                .iter()
                .find(|agent| agent.id == *agent_run_id)
                .ok_or(ReplayError::InvalidTransition)?;
            let valid_role = match fallback.role {
                ProviderRole::Child => agent.role == AgentRole::Child,
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
            };
            let index = if compaction {
                if active {
                    run.tools.len()
                } else {
                    current.compactions.len()
                }
            } else {
                agent.provider_calls.len()
            };
            let chain = run
                .config
                .as_ref()
                .and_then(|config| config.providers.as_ref())
                .map(|providers| providers.get(fallback.role))
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
            if let (true, Some(goal_id)) = (charge_goal, goal_id) {
                let goal = current_goal_mut(current, goal_id)?;
                goal.provider_calls = goal
                    .provider_calls
                    .checked_add(1)
                    .ok_or(ReplayError::InvalidTransition)?;
                goal.token_usage_unknown = true;
            }
        }
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
            let call_index = u32::try_from(agent.provider_calls.len())
                .map_err(|_| ReplayError::InvalidTransition)?;
            if run
                .provider_fallbacks
                .iter()
                .rev()
                .find(|(actor, fallback)| {
                    *actor == *agent_run_id
                        && fallback.role != crate::provider::ProviderRole::Compaction
                        && fallback.call_index == call_index
                })
                .is_some_and(|(_, fallback)| {
                    fallback.role != crate::provider::ProviderRole::for_phase(record.phase)
                })
            {
                return Err(ReplayError::InvalidTransition);
            }
            if let Some(routes) = run
                .config
                .as_ref()
                .and_then(|config| config.providers.as_ref())
            {
                let role = crate::provider::ProviderRole::for_phase(record.phase);
                let identity = run
                    .provider_fallbacks
                    .iter()
                    .rev()
                    .find(|(actor, fallback)| *actor == *agent_run_id && fallback.role == role)
                    .map(|(_, fallback)| &fallback.to)
                    .unwrap_or(&routes.get(role)[0]);
                let cap = match role {
                    crate::provider::ProviderRole::Primary => identity.limits.max_output_tokens,
                    crate::provider::ProviderRole::Child => identity
                        .limits
                        .max_output_tokens
                        .min(crate::provider::DEFAULT_OUTPUT_TOKEN_CAP),
                    crate::provider::ProviderRole::Review => {
                        identity.limits.max_output_tokens.min(256)
                    }
                    crate::provider::ProviderRole::Compaction => {
                        return Err(ReplayError::InvalidTransition);
                    }
                };
                if record.output_tokens.is_some_and(|tokens| tokens > cap) {
                    return Err(ReplayError::InvalidTransition);
                }
            }
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
                let review_index = u32::try_from(agent.provider_calls.len())
                    .map_err(|_| ReplayError::InvalidTransition)?;
                let tools = if run.agents[index].role == AgentRole::Primary {
                    &mut run.tools
                } else {
                    &mut run.agents[index].tools
                };
                tools
                    .last_mut()
                    .ok_or(ReplayError::InvalidTransition)?
                    .review_call_index = Some(review_index);
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
    if agent.role == AgentRole::Child {
        return record.phase == AgentPhase::ToolReview
            && agent.grant == Some(ChildGrant::ReadOnly)
            && agent
                .provider_calls
                .last()
                .is_some_and(|call| call.disposition == ProviderCallDisposition::ToolRequested)
            && agent.tools.last().is_some_and(|tool| {
                tool.observation.is_none()
                    && tool.review_call_index.is_none()
                    && tool.permission_verdict.is_none()
            });
    }
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
        && run.tools.last().is_some_and(|tool| {
            tool.observation.is_none()
                && tool.review_call_index.is_none()
                && tool.permission_verdict.is_none()
        })
}

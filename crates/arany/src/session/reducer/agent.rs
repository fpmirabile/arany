use super::{AgentStatus, AgentView, RunView, SessionView, ToolView, active_run, active_run_goal};
use crate::provider::AgentPhase;
use crate::session::events::{
    AgentRole, AgentRunId, ChildGrant, Event, GoalStatus, ProviderCallDisposition,
    ProviderCallRecord, ReplayError, RunId,
};
use crate::tools::{CacheUpdate, EffectIntent, ToolDisposition, ToolObservation};

mod call;
mod finish;
mod route_change;
mod spawn;

pub(super) fn child_turn_limit(run: &RunView, agent: &AgentView) -> Result<u8, ReplayError> {
    match &agent.definition {
        Some(name) => super::definition_registry(run)?
            .get(name)
            .map(|definition| definition.max_turns)
            .ok_or(ReplayError::InvalidTransition),
        None => Ok(crate::session::MAX_CHILD_TURNS),
    }
}

pub(super) fn child_tool_requests(agent: &AgentView) -> usize {
    agent
        .provider_calls
        .iter()
        .filter(|call| call.disposition == ProviderCallDisposition::ToolRequested)
        .map(|call| usize::from(call.requested_tools))
        .sum()
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
                && agent
                    .tools
                    .iter()
                    .filter(|t| {
                        !matches!(t.intent.call, crate::tools::ToolCall::PrepareWorker { .. })
                    })
                    .count()
                    == child_tool_requests(agent)
                && agent.tools.iter().all(ToolView::settled_without_stop)))
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
    if child.assignment.is_some() {
        return super::worker::child_tool_admissible(run, agent_run_id, intent);
    }
    child.status == AgentStatus::Active
        && matches!(child.grant, Some(ChildGrant::ReadOnly { .. }))
        && child
            .provider_calls
            .iter()
            .rev()
            .find(|call| call.phase != AgentPhase::ToolReview)
            .is_some_and(|call| call.disposition == ProviderCallDisposition::ToolRequested)
        && child_tool_requests(child) > child.tools.len()
        && config
            .tool_policy
            .as_ref()
            .is_some_and(|policy| policy.config_digest == intent.policy_digest)
        && intent.workspace_device == config.workspace_device
        && intent.workspace_inode == config.workspace_inode
        && intent.call.is_child_research()
        && child.definition.as_ref().is_none_or(|name| {
            super::definition_registry(run)
                .ok()
                .and_then(|registry| registry.get(name).cloned())
                .is_some_and(|definition| definition.allows(&intent.call))
        })
        && run
            .tools
            .iter()
            .chain(run.agents.iter().flat_map(|agent| agent.tools.iter()))
            .all(|tool| {
                tool.intent.id != intent.id
                    && (tool.intent.agent_run_id == agent_run_id || tool.observation.is_some())
            })
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
            checkpoint: None,
            intent: intent.clone(),
            observation: None,
            review_call_index: None,
            active_time_reserved: false,
            narrowing_source: None,
            permission_verdict: None,
            hooks: Vec::new(),
            hook_sequence: None,
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
            && !matches!(
                observation.intent.call,
                crate::tools::ToolCall::PrepareWorker { .. }
            )
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
            .iter_mut()
            .find(|tool| {
                tool.observation.is_none() && tool.intent.digest() == observation.intent.digest()
            })
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
        Event::ProviderRetryScheduled { .. } => route_change::retry(current, event),
        Event::ProviderFallbackUsed { .. } => route_change::fallback(current, event),
        Event::AgentSpawned { .. } => spawn::apply(current, event),
        Event::ProviderCallRecorded { .. } => call::apply(current, event),
        Event::AgentFinished { .. } => finish::apply(current, event),
        _ => Err(ReplayError::InvalidTransition),
    }
}

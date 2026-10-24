use super::super::agent::names_child;
use super::super::{
    AgentStatus, AgentView, RunView, SessionView, ToolView, active_run, active_run_goal,
    pending_tool_requests,
};
use crate::session::events::{GoalPhase, ProviderCallDisposition, ReplayError, RunConfig};
use crate::session::{AgentRunId, RunId};
use crate::tools::EffectIntent;

pub(super) fn started(
    current: &mut SessionView,
    run_id: RunId,
    agent_run_id: AgentRunId,
    intent: &EffectIntent,
    active_ceiling_ms: u64,
) -> Result<(), ReplayError> {
    if names_child(current, run_id, agent_run_id) {
        current.start_child_tool(run_id, agent_run_id, intent)?;
    } else {
        start_primary_tool(current, run_id, agent_run_id, intent)?;
    }
    super::record_run_ceiling(current, run_id, active_ceiling_ms)
}

fn start_primary_tool(
    current: &mut SessionView,
    run_id: RunId,
    agent_run_id: AgentRunId,
    intent: &EffectIntent,
) -> Result<(), ReplayError> {
    let goal = active_run_goal(current, run_id).ok_or(ReplayError::InvalidTransition)?;
    let phase_allows_call =
        goal.phase == GoalPhase::Implementation || intent.call.is_goal_plan_prelude();
    let plan_allows_call = if matches!(
        intent.call,
        crate::tools::ToolCall::ParseAgentDefinition { .. }
    ) {
        true
    } else if let Some(contract) = &goal.plan_contract {
        !goal.awaiting_plan_contract
            && (matches!(intent.call, crate::tools::ToolCall::PrepareWorker { .. })
                || contract.allows_call(&intent.call)
                || matches!(&intent.call, crate::tools::ToolCall::ApplyChangeset { .. }))
    } else {
        true
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
        checkpoint: None,
        intent: intent.clone(),
        observation: None,
        review_call_index: None,
        permission_verdict: None,
        hooks: Vec::new(),
        hook_sequence: None,
        finished_sequence: None,
    });
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
    } else if let crate::tools::ToolCall::PrepareWorker { preparation } = &intent.call {
        super::super::worker::base_preparation(run, preparation)
    } else if integration {
        super::valid_integration_source(run, intent)
    } else {
        primary
            .provider_calls
            .last()
            .is_some_and(|call| call.disposition == ProviderCallDisposition::ToolRequested)
            && pending_tool_requests(run).is_some_and(|pending| pending >= 1)
            && super::valid_narrowing_source(run, intent)
            && run
                .tools
                .last()
                .and_then(|tool| tool.observation.as_ref())
                .is_none_or(|observation| !observation.stops_run())
    }
}

use super::super::{AgentStatus, RunView, SessionView, active_run, active_run_goal, worker};
use crate::session::events::{
    AgentDisposition, AgentRole, Event, ProviderCallDisposition, ReplayError,
};

pub(super) fn apply(current: &mut SessionView, event: &Event) -> Result<(), ReplayError> {
    let Event::AgentFinished {
        run_id,
        agent_run_id,
        disposition,
        summary,
        result,
        worker: _,
    } = event
    else {
        return Err(ReplayError::InvalidTransition);
    };
    // Unknown usage blocks Tool and Delegate outcomes and Provider calls, not an Answer.
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
    validate_finish(run, index, event, over_token_cap)?;
    let agent = &mut run.agents[index];
    agent.status = match disposition {
        AgentDisposition::StoppedByHook => AgentStatus::StoppedByHook,
        AgentDisposition::Finished => AgentStatus::Finished,
        AgentDisposition::Failed => AgentStatus::Failed,
        AgentDisposition::Cancelled => AgentStatus::Cancelled,
    };
    agent.summary = summary.clone();
    agent.result = result.clone();
    Ok(())
}

fn validate_finish(
    run: &RunView,
    index: usize,
    event: &Event,
    over_token_cap: bool,
) -> Result<(), ReplayError> {
    let Event::AgentFinished {
        disposition,
        summary,
        result,
        ..
    } = event
    else {
        return Err(ReplayError::InvalidTransition);
    };
    if run.agents[index].status != AgentStatus::Active {
        return Err(ReplayError::InvalidTransition);
    }
    if run.agents[index].role == AgentRole::Primary
        && *disposition == AgentDisposition::Finished
        && run
            .agents
            .iter()
            .skip(1)
            .any(|agent| !worker::child_terminal(agent))
    {
        return Err(ReplayError::InvalidTransition);
    }
    if result.as_deref().is_some_and(|text| {
        summary.as_deref() != Some(crate::session::answer_summary(text).as_str())
    }) {
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
    validate_completion(run, index, event, over_token_cap)?;
    Ok(())
}

fn validate_completion(
    run: &RunView,
    index: usize,
    event: &Event,
    over_token_cap: bool,
) -> Result<(), ReplayError> {
    let Event::AgentFinished {
        agent_run_id,
        disposition,
        result,
        ..
    } = event
    else {
        return Err(ReplayError::InvalidTransition);
    };
    if *disposition == AgentDisposition::Finished
        && run.agents[index].provider_calls.last().is_none_or(|call| {
            call.disposition != ProviderCallDisposition::Finished
                || call.phase == crate::provider::AgentPhase::ToolReview
        })
    {
        return Err(ReplayError::InvalidTransition);
    }
    if *disposition == AgentDisposition::Finished
        && (!run.hooks_settled()
            || run.stop_blocked(*agent_run_id)
            || run.hook_stop_reason().is_some())
    {
        return Err(ReplayError::InvalidTransition);
    }
    if *disposition == AgentDisposition::Finished && over_token_cap {
        return Err(ReplayError::InvalidTransition);
    }
    if *disposition == AgentDisposition::Finished && run.agents[index].role == AgentRole::Primary {
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
    if *disposition == AgentDisposition::StoppedByHook
        && (!run.hooks_settled()
            || (!run.actor_stopped_by_hook(*agent_run_id)
                && run.agents[index].role != AgentRole::Primary)
            || run.hook_stop_reason().is_none())
    {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(())
}

use super::*;

pub(super) fn check(
    run: &RunView,
    event: &Event,
    actor: Option<AgentRunId>,
) -> Result<(), ReplayError> {
    if let Event::ProviderCallRecorded {
        agent_run_id,
        record,
        ..
    } = event
        && record.failure_reason
            == Some(crate::provider::ProviderFailureReason::CriterionNotMetInView)
        && (record.phase != AgentPhase::ChildWork || worker(run, *agent_run_id).is_err())
    {
        return Err(ReplayError::InvalidTransition);
    }
    if let Event::ProviderCallRecorded {
        agent_run_id,
        record,
        ..
    } = event
        && matches!(
            record.failure_reason,
            Some(
                crate::provider::ProviderFailureReason::WorkerAnswerDeferred
                    | crate::provider::ProviderFailureReason::WorkerDelegateRefused
                    | crate::provider::ProviderFailureReason::WorkerToolRefused
            )
        )
        && (!primary(run, *agent_run_id)
            || !coordination_ready(run)
            || !run.agents.iter().any(|a| {
                a.assignment
                    .as_ref()
                    .is_some_and(|w| w.pins.request.mode == WorkerMode::Background)
            }))
    {
        return Err(ReplayError::InvalidTransition);
    }
    if (run.workers.pending.is_some() || pending_command_capture(run).is_some())
        && (starts_team_work(event) || matches!(event, Event::HookStarted { .. }))
    {
        return Err(ReplayError::InvalidTransition);
    }
    if unrecorded_integration(run) && starts_team_work(event) {
        return Err(ReplayError::InvalidTransition);
    }
    let workspace_stopped = run
        .tools
        .iter()
        .any(|t| t.observation.as_ref().is_some_and(|o| o.stops_run()));
    if (run.workers.stop.is_some() || workspace_stopped)
        && matches!(
            event,
            Event::AgentSpawned { .. }
                | Event::ToolStarted { .. }
                | Event::GoalProviderCallReserved { .. }
                | Event::ProviderFallbackUsed { .. }
                | Event::ProviderRetryScheduled { .. }
                | Event::WorkerHandoverRecorded { .. }
                | Event::WorkerVerificationRecorded { .. }
        )
    {
        return Err(ReplayError::InvalidTransition);
    }
    if let Some(id) = actor
        && let Ok(w) = worker(run, id)
        && matches!(
            event,
            Event::GoalProviderCallReserved { .. }
                | Event::ProviderCallRecorded { .. }
                | Event::ProviderFallbackUsed { .. }
                | Event::ProviderRetryScheduled { .. }
        )
    {
        let a = run
            .agents
            .iter()
            .find(|a| a.id == id)
            .ok_or(ReplayError::InvalidTransition)?;
        let reviewing = matches!(event, Event::ProviderCallRecorded { record, .. } if record.phase == AgentPhase::ToolReview)
            || matches!(event, Event::GoalProviderCallReserved { .. })
                && a.tools.last().is_some_and(|t| t.observation.is_none());
        if w.outcome.is_some()
            || matches!(event, Event::GoalProviderCallReserved { .. })
                && !reviewing
                && w.gate_continuations >= 3
                && a.provider_calls.last().is_some_and(|c| {
                    c.failure_reason
                        == Some(crate::provider::ProviderFailureReason::CriterionNotMetInView)
                })
            || a.tools.iter().any(private_failure)
            || !reviewing
                && !matches!(event, Event::ProviderCallRecorded { .. })
                && a.provider_calls
                    .iter()
                    .filter(|r| r.phase == AgentPhase::ChildWork)
                    .count()
                    >= usize::from(w.pins.request.max_turns)
        {
            return Err(ReplayError::InvalidTransition);
        }
    }
    Ok(())
}

use super::*;

pub(super) fn provider_call(
    run: &RunView,
    agent_run_id: &AgentRunId,
    record: &ProviderCallRecord,
) -> Result<(), ReplayError> {
    let w = worker(run, *agent_run_id)?;
    let a = run
        .agents
        .iter()
        .find(|a| a.id == *agent_run_id)
        .ok_or(ReplayError::InvalidTransition)?;
    let max_tools = run
        .config
        .as_ref()
        .and_then(|c| c.worker_policy.as_ref())
        .ok_or(ReplayError::InvalidTransition)?
        .max_tools;
    if record.failure_reason == Some(crate::provider::ProviderFailureReason::CriterionNotMetInView)
        && !editor_criterion_evidence(run, *agent_run_id)?.is_empty()
    {
        return Err(ReplayError::InvalidTransition);
    }
    if record.disposition == ProviderCallDisposition::ToolRequested
        && (super::agent::child_tool_requests(a) >= usize::from(max_tools)
            || a.provider_calls
                .iter()
                .filter(|r| r.phase == AgentPhase::ChildWork)
                .count()
                + 1
                >= usize::from(w.pins.request.max_turns))
    {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(())
}

pub(super) fn agent_finished(
    run: &RunView,
    agent_run_id: &AgentRunId,
    disposition: &AgentDisposition,
    outcome: Option<&WorkerOutcome>,
) -> Result<(), ReplayError> {
    if let Ok(w) = worker(run, *agent_run_id) {
        let o = outcome.ok_or(ReplayError::InvalidTransition)?;
        worker_outcome(run, w, agent_run_id, disposition, o)?;
    } else if outcome.is_some() {
        return Err(ReplayError::InvalidTransition);
    }
    if primary(run, *agent_run_id)
        && *disposition == AgentDisposition::Finished
        && !wave_settled(run)
    {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(())
}

fn worker_outcome(
    run: &RunView,
    w: &WorkerView,
    id: &AgentRunId,
    disposition: &AgentDisposition,
    o: &WorkerOutcome,
) -> Result<(), ReplayError> {
    let a = run
        .agents
        .iter()
        .find(|a| a.id == *id)
        .ok_or(ReplayError::InvalidTransition)?;
    let success = matches!(
        o.disposition,
        WorkerDisposition::Ready | WorkerDisposition::NoChange
    );
    let changed = a.tools.iter().any(|t| {
        t.intent.call.mutates()
            && t.observation.as_ref().is_some_and(|o| {
                o.disposition == ToolDisposition::Succeeded && o.attested_for(&t.intent)
            })
    });
    if w.outcome.is_some()
        || !o.valid()
        || (o.cause == WorkerCause::StoppedByHook)
            != (*disposition == AgentDisposition::StoppedByHook)
        || (*disposition == AgentDisposition::Finished) != success
        || (*disposition == AgentDisposition::Cancelled)
            != (o.disposition == WorkerDisposition::Cancelled)
        || a.tools.iter().any(ToolView::outcome_unknown)
        || success
            && (editor_criterion_evidence(run, *id)?.is_empty()
                || (o.disposition == WorkerDisposition::NoChange) == changed)
    {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(())
}
pub(super) fn primary_provider_call(
    view: &SessionView,
    run: &RunView,
    record: &ProviderCallRecord,
) -> Result<(), ReplayError> {
    if record.disposition == ProviderCallDisposition::Delegated {
        if !wave_settled(run) {
            return Err(ReplayError::InvalidTransition);
        }
        let config = run.config.as_ref().ok_or(ReplayError::InvalidTransition)?;
        let assignments = record
            .delegation
            .as_ref()
            .ok_or(ReplayError::InvalidTransition)?;
        if assignments.len()
            + run
                .primary()
                .into_iter()
                .flat_map(|a| &a.provider_calls)
                .filter_map(|c| c.delegation.as_ref())
                .map(Vec::len)
                .sum::<usize>()
            > usize::from(config.policy.max_children())
        {
            return Err(ReplayError::InvalidTransition);
        }
        let editors: Vec<_> = assignments
            .iter()
            .filter_map(|a| {
                if let ChildAssignment::Editor { assignment } = a {
                    Some(assignment)
                } else {
                    None
                }
            })
            .collect();
        for request in &editors {
            validate_request(view, run, request, None)
                .map_err(|_| ReplayError::InvalidTransition)?;
        }
        if !editors.is_empty() && config.worker_policy.is_none() {
            return Err(ReplayError::InvalidTransition);
        }
        if editors.iter().enumerate().any(|(i, a)| {
            editors[..i].iter().any(|b| {
                a.lease
                    .iter()
                    .any(|p| b.lease.iter().any(|q| overlaps(p, q)))
            })
        }) {
            return Err(ReplayError::InvalidTransition);
        }
    }
    Ok(())
}

pub(super) fn run_finished(run: &RunView) -> Result<(), ReplayError> {
    if !wave_settled(run) {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(())
}

fn primary_intents_idle(run: &RunView) -> bool {
    quiescent(run)
        || run.primary().is_some_and(|primary| {
            primary
                .provider_calls
                .iter()
                .rev()
                .find(|call| call.phase != AgentPhase::ToolReview)
                .is_some_and(|call| {
                    call.disposition == ProviderCallDisposition::ToolRequested
                        && call.requested_tools > 1
                })
        }) && run.tools.iter().all(|tool| {
            tool.observation.is_some()
                || !tool.dispatch_in_progress()
                    && tool.narrowing_source.is_none()
                    && !tool.intent.call.is_internal()
                    && !matches!(tool.intent.call, ToolCall::ApplyChangeset { .. })
        }) && run
            .agents
            .iter()
            .flat_map(|agent| &agent.tools)
            .all(|tool| tool.observation.is_some())
}

pub(super) fn tool_started(
    run: &RunView,
    agent_run_id: &AgentRunId,
    intent: &crate::tools::EffectIntent,
) -> Result<(), ReplayError> {
    let config = run.config.as_ref().ok_or(ReplayError::InvalidTransition)?;
    if intent.workspace_device != config.workspace_device
        || intent.workspace_inode != config.workspace_inode
    {
        return Err(ReplayError::InvalidTransition);
    }
    if primary(run, *agent_run_id) {
        if intent.target != EffectTarget::Workspace || !primary_intents_idle(run) {
            return Err(ReplayError::InvalidTransition);
        }
        super::super::tool::validate_tool_lease(run, *agent_run_id, &intent.call)
            .map_err(|_| ReplayError::InvalidTransition)?;
    } else if worker(run, *agent_run_id).is_ok() {
        if !child_tool_admissible(run, *agent_run_id, intent) {
            return Err(ReplayError::InvalidTransition);
        }
    } else if intent.target != EffectTarget::Workspace {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(())
}

pub(super) fn tool_finished(
    _view: &SessionView,
    _run: &RunView,
    _actor: &AgentRunId,
    o: &crate::tools::ToolObservation,
) -> Result<(), ReplayError> {
    if o.intent.target != EffectTarget::Workspace {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(())
}
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
    let stop_sequence = run
        .tools
        .iter()
        .chain(run.agents.iter().flat_map(|a| &a.tools))
        .filter(|tool| tool.observation.as_ref().is_some_and(|o| o.stops_run()))
        .filter_map(|tool| tool.finished_sequence)
        .chain(run.workers.stop.as_ref().map(|(sequence, _)| *sequence))
        .min();
    if let Some(sequence) = stop_sequence {
        check_after_stop(run, event, sequence)?;
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
                && w.gate_continuations >= MAX_EDITOR_CONTINUATIONS
                && a.provider_calls.last().is_some_and(|c| {
                    c.failure_reason
                        == Some(crate::provider::ProviderFailureReason::CriterionNotMetInView)
                })
            || a.tools.iter().any(editor_tool_failed)
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

fn check_after_stop(run: &RunView, event: &Event, sequence: u64) -> Result<(), ReplayError> {
    let tools = || {
        run.tools
            .iter()
            .chain(run.agents.iter().flat_map(|agent| &agent.tools))
    };
    match event {
        Event::ToolStarted {
            agent_run_id,
            intent,
            verdict,
            ..
        } => {
            let batch = run.agents.iter().any(|agent| {
                agent.id == *agent_run_id
                    && agent.assignment.is_none()
                    && agent
                        .provider_calls
                        .iter()
                        .rev()
                        .find(|call| call.phase != AgentPhase::ToolReview)
                        .is_some_and(|call| {
                            call.disposition == ProviderCallDisposition::ToolRequested
                                && call.requested_tools > 1
                        })
            });
            if verdict.as_ref().is_some_and(|verdict| verdict.approved)
                || verdict.is_none() && !batch
                || !tools().any(|tool| {
                    tool.intent.id == intent.id
                        && tool.intent.agent_run_id == *agent_run_id
                        && tool.started_sequence.is_none()
                        && tool.observation.is_none()
                })
            {
                return Err(ReplayError::InvalidTransition);
            }
        }
        Event::ToolFinished {
            observation,
            checkpoint,
            ..
        } => {
            let started_after_stop = tools().any(|tool| {
                tool.intent.id == observation.intent.id
                    && tool
                        .started_sequence
                        .is_some_and(|started| started > sequence)
            });
            if started_after_stop
                && (!matches!(
                    observation.disposition,
                    ToolDisposition::Denied | ToolDisposition::Limit
                ) || observation.attestation.is_some()
                    || observation.cache_update != crate::tools::CacheUpdate::NotApplicable
                    || checkpoint.is_some())
            {
                return Err(ReplayError::InvalidTransition);
            }
        }
        Event::AgentSpawned { .. }
        | Event::ToolProposed { .. }
        | Event::ToolActiveTimeReserved { .. }
        | Event::GoalProviderCallReserved { .. }
        | Event::ProviderFallbackUsed { .. }
        | Event::ProviderRetryScheduled { .. }
        | Event::WorkerVerificationRecorded { .. } => return Err(ReplayError::InvalidTransition),
        Event::ToolPreflightRefused { .. }
        | Event::WorkspaceRewindStarted { .. }
        | Event::WorkspaceRewindFinished { .. }
        | Event::SessionStarted { .. }
        | Event::SessionDefaultChanged { .. }
        | Event::SessionForked { .. }
        | Event::ContextCompacted { .. }
        | Event::SessionRenamed { .. }
        | Event::CommandToolProbeStarted { .. }
        | Event::CommandToolProbeFinished { .. }
        | Event::ProviderSetupEditStarted { .. }
        | Event::ProviderSetupEditFinished { .. }
        | Event::GoalStarted { .. }
        | Event::GoalRunLinked { .. }
        | Event::GoalResumed { .. }
        | Event::GoalPlanContractRecorded { .. }
        | Event::GoalFinished { .. }
        | Event::MessageAccepted { .. }
        | Event::RunStarted { .. }
        | Event::ProviderCallRecorded { .. }
        | Event::GoalCompactionCallReserved { .. }
        | Event::ToolResultsCleared { .. }
        | Event::GoalContextCompacted { .. }
        | Event::GoalSteeringNoteRecorded { .. }
        | Event::AgentSteeringNoteRecorded { .. }
        | Event::AssistantDisplayRecorded { .. }
        | Event::SupervisorVerdictRecorded { .. }
        | Event::HookStarted { .. }
        | Event::HookFinished { .. }
        | Event::AgentFinished { .. }
        | Event::WorkerLeaseReleased { .. }
        | Event::WorkerStopObserved { .. }
        | Event::MessageCommitted { .. }
        | Event::RunFinished { .. } => {}
    }
    Ok(())
}

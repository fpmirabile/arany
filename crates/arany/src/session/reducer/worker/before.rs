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
        && !private_criterion_evidence(run, *agent_run_id)?.is_empty()
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
        if intent.target != EffectTarget::Workspace || !quiescent(run) {
            return Err(ReplayError::InvalidTransition);
        }
        if let ToolCall::ApplyChangeset {
            source: source @ ChangesetSource::WorkerHandover { .. },
        } = &intent.call
        {
            if !integration_source(run, source) {
                return Err(ReplayError::InvalidTransition);
            }
        } else if intent.call.mutates() || intent.call.is_process() {
            let leases: Vec<_> = run
                .agents
                .iter()
                .filter_map(|a| a.assignment.as_ref())
                .filter(|w| w.released.is_none())
                .flat_map(|w| &w.pins.request.lease)
                .collect();
            let paths: Vec<_> = match &intent.call {
                ToolCall::Write { path, .. }
                | ToolCall::Edit { path, .. }
                | ToolCall::MultiEdit {
                    file_path: path, ..
                }
                | ToolCall::Mkdir { path } => vec![path],
                ToolCall::ApplyChangeset {
                    source: ChangesetSource::CommandReceipt { changeset, .. },
                } => changeset.files.iter().map(|f| &f.path).collect(),
                _ if !leases.is_empty() => return Err(ReplayError::InvalidTransition),
                _ => Vec::new(),
            };
            if paths.iter().any(|p| leases.iter().any(|l| overlaps(l, p))) {
                return Err(ReplayError::InvalidTransition);
            }
        }
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
    view: &SessionView,
    run: &RunView,
    agent_run_id: &AgentRunId,
    o: &crate::tools::ToolObservation,
) -> Result<(), ReplayError> {
    if matches!(o.intent.target, EffectTarget::WorkerView { .. })
        && matches!(o.intent.call, ToolCall::Command { .. })
    {
        if o.attestation.is_some()
            && !matches!(o.attestation, Some(Attestation::WorkerCommand { .. }))
        {
            return Err(ReplayError::InvalidTransition);
        }
        if let Some(result) = WorkerCommandResult::parse(&o.output) {
            if let PrivateCapture::Sealed { data } = result.private_capture {
                let w = worker(run, *agent_run_id)?;
                let used: u64 = run
                    .workers
                    .data
                    .iter()
                    .filter(|d| d.reference.assignment_id == *agent_run_id)
                    .map(|d| u64::from(d.reference.bytes))
                    .sum();
                if w.revision >= 16
                    || used + u64::from(data.bytes) > u64::from(w.pins.data_bytes)
                    || o.disposition != ToolDisposition::Succeeded
                    || !o.attested_for(&o.intent)
                    || data.assignment_id != *agent_run_id
                    || data.first_sequence <= view.last_sequence + 1
                    || data.seal_sequence <= data.first_sequence
                    || run.workers.data.iter().any(|d| d.reference == data)
                {
                    return Err(ReplayError::InvalidTransition);
                }
            }
        } else if o.attestation.is_some() {
            return Err(ReplayError::InvalidTransition);
        }
    }
    if let Some(Attestation::WorkerData { data, .. }) = &o.attestation {
        let sealed = run.workers.sealed(data)?;
        if sealed.tool != o.intent.id
            && !matches!(&o.intent.call, ToolCall::PrepareWorker { preparation } if preparation.purpose == WorkerDataPurpose::Handover && data.assignment_id == *agent_run_id)
        {
            return Err(ReplayError::InvalidTransition);
        }
        if matches!(&o.intent.call, ToolCall::PrepareWorker { preparation } if preparation.purpose == WorkerDataPurpose::Handover)
        {
            valid_delta(run, *agent_run_id, data, true)?;
        }
    }
    if let Some(Attestation::WorkerMutation { mutation, .. }) = &o.attestation {
        if o.disposition != ToolDisposition::Succeeded
            || mutation.data.assignment_id != *agent_run_id
            || run.workers.sealed(&mutation.data)?.tool != o.intent.id
        {
            return Err(ReplayError::InvalidTransition);
        }
        let changes = valid_delta(run, *agent_run_id, &mutation.data, false)?;
        let expected_path = match &o.intent.call {
            ToolCall::Write { path, .. }
            | ToolCall::Edit { path, .. }
            | ToolCall::MultiEdit {
                file_path: path, ..
            }
            | ToolCall::Mkdir { path } => path,
            _ => return Err(ReplayError::InvalidTransition),
        };
        if changes
            .iter()
            .any(|f| &f.path != expected_path || f.after == WorkerBaseVersion::Absent)
        {
            return Err(ReplayError::InvalidTransition);
        }
        for change in &changes {
            let expected_digest = match &o.intent.call {
                ToolCall::Write {
                    expected_digest, ..
                } => expected_digest.as_deref(),
                ToolCall::Edit {
                    expected_digest, ..
                }
                | ToolCall::MultiEdit {
                    expected_digest, ..
                } => Some(expected_digest.as_str()),
                _ => None,
            };
            let before_digest = match &change.before {
                WorkerBaseVersion::File { sha256, .. } => Some(sha256.as_str()),
                _ => None,
            };
            if expected_digest != before_digest
                || matches!(
                    (&change.before, &change.after),
                    (
                        WorkerBaseVersion::Absent,
                        WorkerBaseVersion::File {
                            executable: true,
                            ..
                        }
                    )
                )
            {
                return Err(ReplayError::InvalidTransition);
            }
        }
        if let ToolCall::Write { content, .. } = &o.intent.call
            && !changes.iter().all(|f| matches!(&f.after, WorkerBaseVersion::File { sha256, .. } if *sha256 == crate::validate::hex_digest(content.as_bytes()))) { return Err(ReplayError::InvalidTransition); }
    }
    if o.disposition == ToolDisposition::Succeeded
        && matches!(o.intent.target, EffectTarget::WorkerView { .. })
        && o.intent.call.mutates()
        && !matches!(o.intent.call, ToolCall::Command { .. })
        && !matches!(o.attestation, Some(Attestation::WorkerMutation { .. }))
    {
        return Err(ReplayError::InvalidTransition);
    }
    if run.workers.pending.is_some() && o.disposition == ToolDisposition::Succeeded {
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

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
    agent_run_id: &AgentRunId,
    disposition: &AgentDisposition,
    o: &WorkerOutcome,
) -> Result<(), ReplayError> {
    let a = run
        .agents
        .iter()
        .find(|a| a.id == *agent_run_id)
        .ok_or(ReplayError::InvalidTransition)?;
    if let Some(cleanup) = &o.cleanup
        && (cleanup.assignment_id != *agent_run_id
            || cleanup.base_digest != w.pins.base.sha256
            || cleanup.revision != w.revision
            || a.tools
                .last()
                .is_some_and(|t| cleanup.enforcement_digest != t.intent.enforcement_digest)
            || cleanup.enforcement_digest == [0; 32]
            || cleanup.last_intent_digest != a.tools.last().map(|t| t.intent.digest()))
    {
        return Err(ReplayError::InvalidTransition);
    }
    if !matches!(
        o.disposition,
        WorkerDisposition::Ready | WorkerDisposition::NoChange
    ) && o.cleanup.is_none()
    {
        return Err(ReplayError::InvalidTransition);
    }
    if (o.cause == WorkerCause::StoppedByHook) != (*disposition == AgentDisposition::StoppedByHook)
        || w.outcome.is_some()
        || (*disposition == AgentDisposition::Finished)
            != matches!(
                o.disposition,
                WorkerDisposition::Ready | WorkerDisposition::NoChange
            )
        || (*disposition == AgentDisposition::Cancelled)
            != (o.disposition == WorkerDisposition::Cancelled)
    {
        return Err(ReplayError::InvalidTransition);
    }
    if matches!(
        o.disposition,
        WorkerDisposition::Ready | WorkerDisposition::NoChange
    ) {
        if private_criterion_evidence(run, *agent_run_id)?.is_empty() {
            return Err(ReplayError::InvalidTransition);
        }
        let data = o.data.as_ref().ok_or(ReplayError::InvalidTransition)?;
        if data.assignment_id != *agent_run_id || !matches!(data.purpose, WorkerDataPurpose::Handover | WorkerDataPurpose::Mutation)
            || !a.tools.last().is_some_and(|t| matches!(t.observation.as_ref().and_then(|o| o.attestation.as_ref()), Some(Attestation::WorkerData { data: d, .. }) if d == data)) { return Err(ReplayError::InvalidTransition); }
        let changes = valid_delta(run, *agent_run_id, data, true)?;
        if (o.disposition == WorkerDisposition::NoChange) != changes.is_empty()
            || o.cause != WorkerCause::Finished
        {
            return Err(ReplayError::InvalidTransition);
        }
    } else if o.data.is_some() {
        return Err(ReplayError::InvalidTransition);
    }
    let unknown = a.tools.iter().any(|t| {
        t.observation
            .as_ref()
            .is_some_and(|o| o.disposition == ToolDisposition::Uncertain)
    });
    if unknown != (o.disposition == WorkerDisposition::PrivateUncertain) {
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
        || run.workers.capture.is_none()
            && run.primary().is_some_and(|primary| {
                primary
                    .provider_calls
                    .iter()
                    .rev()
                    .find(|call| call.phase != AgentPhase::ToolReview)
                    .is_some_and(|call| {
                        call.disposition == ProviderCallDisposition::ToolRequested
                            && call.requested_tools > 1
                    })
            })
            && run.tools.iter().all(|tool| {
                tool.observation.is_some()
                    || !tool.dispatch_in_progress()
                        && tool.narrowing_source.is_none()
                        && !tool.intent.call.is_internal()
                        && !matches!(tool.intent.call, ToolCall::ApplyChangeset { .. })
            })
            && run
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
                | ToolCall::Delete { path, .. }
                | ToolCall::MultiEdit {
                    file_path: path, ..
                }
                | ToolCall::Mkdir { path } => vec![path],
                ToolCall::Move {
                    source,
                    destination,
                    ..
                } => vec![source, destination],
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

fn command_data_reference(
    view: &SessionView,
    run: &RunView,
    observation: &crate::tools::ToolObservation,
    data: &WorkerDataRef,
) -> Result<(), ReplayError> {
    if data.seal_sequence <= view.last_sequence {
        if run.workers.sealed(data)?.tool != observation.intent.id {
            return Err(ReplayError::InvalidTransition);
        }
    } else {
        let w = worker(run, observation.intent.agent_run_id)?;
        let used: u64 = run
            .workers
            .data
            .iter()
            .filter(|d| d.reference.assignment_id == observation.intent.agent_run_id)
            .map(|d| u64::from(d.reference.bytes))
            .sum();
        if data.first_sequence <= view.last_sequence + 1
            || used + u64::from(data.bytes) > u64::from(w.pins.data_bytes)
        {
            return Err(ReplayError::InvalidTransition);
        }
    }
    Ok(())
}

fn command_finished(
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
                command_data_reference(view, run, o, &data)?;
                if w.revision >= 16
                    || o.disposition != ToolDisposition::Succeeded
                    || !o.attested_for(&o.intent)
                    || data.assignment_id != *agent_run_id
                {
                    return Err(ReplayError::InvalidTransition);
                }
            }
        } else if o.attestation.is_some() {
            return Err(ReplayError::InvalidTransition);
        }
    }
    Ok(())
}

pub(super) fn tool_finished(
    view: &SessionView,
    run: &RunView,
    agent_run_id: &AgentRunId,
    o: &crate::tools::ToolObservation,
) -> Result<(), ReplayError> {
    command_finished(view, run, agent_run_id, o)?;
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
        if matches!(
            o.intent.call,
            ToolCall::Delete { .. } | ToolCall::Move { .. }
        ) {
            validate_removal(&changes, o)?;
        } else {
            validate_literal_mutation(&changes, &o.intent.call)?;
        }
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
    if (run.workers.pending.is_some() || run.workers.capture.is_some())
        && (starts_team_work(event) || matches!(event, Event::HookStarted { .. }))
    {
        return Err(ReplayError::InvalidTransition);
    }
    if unrecorded_integration(run) && starts_team_work(event) {
        return Err(ReplayError::InvalidTransition);
    }
    let stop_sequence = run
        .tools
        .iter()
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
            if verdict.as_ref().is_some_and(|verdict| verdict.approved)
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
        | Event::WorkerHandoverRecorded { .. }
        | Event::WorkerVerificationRecorded { .. } => return Err(ReplayError::InvalidTransition),
        _ => {}
    }
    Ok(())
}

fn validate_literal_mutation(
    changes: &[WorkerFileFact],
    call: &ToolCall,
) -> Result<(), ReplayError> {
    let expected_path = match call {
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
    for change in changes {
        let expected_digest = match call {
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
    if let ToolCall::Write { content, .. } = call
            && !changes.iter().all(|f| matches!(&f.after, WorkerBaseVersion::File { sha256, .. } if *sha256 == crate::validate::hex_digest(content.as_bytes()))) { return Err(ReplayError::InvalidTransition); }
    Ok(())
}

fn validate_removal(
    changes: &[WorkerFileFact],
    o: &crate::tools::ToolObservation,
) -> Result<(), ReplayError> {
    let (source, destination, expected) = match &o.intent.call {
        ToolCall::Delete {
            path,
            expected_digest,
        } => (path, None, expected_digest),
        ToolCall::Move {
            source,
            destination,
            expected_digest,
        } => (source, Some(destination), expected_digest),
        _ => return Err(ReplayError::InvalidTransition),
    };
    let removed = changes
        .iter()
        .find(|f| &f.path == source)
        .ok_or(ReplayError::InvalidTransition)?;
    let result: crate::session::rewind::RemovedFileResult =
        serde_json::from_str(&o.output).map_err(|_| ReplayError::InvalidPayload)?;
    if !matches!(&removed.before, WorkerBaseVersion::File { sha256, bytes, executable }
        if sha256 == expected && sha256 == &result.removed.sha256 && *bytes == result.removed.bytes && *executable == result.removed.executable)
        || removed.after != WorkerBaseVersion::Absent
        || changes.len() != if destination.is_some() { 2 } else { 1 }
    {
        return Err(ReplayError::InvalidTransition);
    }
    if let Some(destination) = destination {
        let created = changes
            .iter()
            .find(|f| &f.path == destination)
            .ok_or(ReplayError::InvalidTransition)?;
        if created.before != WorkerBaseVersion::Absent || created.after != removed.before {
            return Err(ReplayError::InvalidTransition);
        }
    }
    Ok(())
}

use super::{
    AgentRunId, PendingData, ReplayError, RunView, SealedWorkerData, apply_command_capture,
    integration_in_flight, primary, private_criterion_evidence, tool, valid_delta, valid_discard,
    verification_passed, worker, worker_mut,
};
use crate::session::GoalPlanContract;
use crate::session::worker::{
    ChangesetSource, EffectTarget, WORKER_CHUNK_BYTES, WorkerDataChunk, WorkerDataPurpose,
    WorkerDataSealed, WorkerDisposition, WorkerHandover, WorkerIntegration,
    WorkerIntegrationOutcome, WorkerSettlement, WorkerSettlementReason, WorkerStopRecord,
    WorkerVerification, decode_data,
};
use crate::tools::{ToolCall, ToolDisposition};

pub(super) fn stop_observed(
    run: &mut RunView,
    record: &WorkerStopRecord,
    actor: AgentRunId,
    sequence: u64,
) -> Result<(), ReplayError> {
    if !primary(run, actor)
        || run.workers.stop.is_some()
        || run
            .config
            .as_ref()
            .is_none_or(|c| c.worker_policy.is_none())
    {
        return Err(ReplayError::InvalidTransition);
    }
    run.workers.stop = Some((sequence, record.reason));
    Ok(())
}

pub(super) fn data_chunk(
    run: &mut RunView,
    c: &WorkerDataChunk,
    actor: AgentRunId,
    sequence: u64,
) -> Result<(), ReplayError> {
    let (max_bytes, max_chunks) = chunk_limits(run, c, actor)?;
    if run.workers.data.iter().any(|d| d.tool == c.tool_id) {
        return Err(ReplayError::InvalidTransition);
    }
    if let Ok(w) = worker(run, c.assignment_id) {
        let used = run
            .workers
            .data
            .iter()
            .filter(|d| d.reference.assignment_id == c.assignment_id)
            .try_fold(0u32, |sum, d| sum.checked_add(d.reference.bytes))
            .ok_or(ReplayError::InvalidTransition)?;
        let pending = run
            .workers
            .pending
            .as_ref()
            .map_or(0usize, |p| p.bytes.len());
        if (used as usize)
            .checked_add(pending)
            .and_then(|n| n.checked_add(c.bytes as usize))
            .is_none_or(|n| n > w.pins.data_bytes as usize)
        {
            return Err(ReplayError::InvalidTransition);
        }
    }
    let bytes = c.decoded().ok_or(ReplayError::InvalidPayload)?;
    let pending = run.workers.pending.get_or_insert_with(|| PendingData {
        actor,
        assignment: c.assignment_id,
        purpose: c.purpose,
        tool: c.tool_id,
        first: sequence,
        chunks: 0,
        bytes: Vec::new(),
    });
    if pending.actor != actor
        || pending.assignment != c.assignment_id
        || pending.purpose != c.purpose
        || pending.tool != c.tool_id
        || pending.chunks != c.ordinal
        || usize::from(c.ordinal) >= max_chunks
        || pending
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > max_bytes)
        || c.ordinal > 0 && pending.bytes.len() != usize::from(c.ordinal) * WORKER_CHUNK_BYTES
    {
        return Err(ReplayError::InvalidTransition);
    }
    pending.bytes.extend(bytes);
    pending.chunks += 1;
    Ok(())
}

fn chunk_limits(
    run: &RunView,
    c: &WorkerDataChunk,
    actor: AgentRunId,
) -> Result<(usize, usize), ReplayError> {
    let t = tool(run, actor)
        .filter(|t| {
            t.started_sequence.is_some()
                && t.permission_verdict
                    .as_ref()
                    .is_none_or(|verdict| verdict.approved)
                && (t.observation.is_none()
                    || run.workers.capture.as_ref().is_some_and(|(a, id, data)| {
                        *a == actor && *id == t.intent.id && data.assignment_id == c.assignment_id
                    }))
        })
        .ok_or(ReplayError::InvalidTransition)?;
    if t.intent.id != c.tool_id {
        return Err(ReplayError::InvalidTransition);
    }
    let limits = if let ToolCall::PrepareWorker { preparation } = &t.intent.call {
        if preparation.assignment_id != c.assignment_id || preparation.purpose != c.purpose {
            return Err(ReplayError::InvalidTransition);
        }
        (
            preparation.max_bytes as usize,
            usize::from(preparation.max_chunks),
        )
    } else {
        let w = worker(run, actor)?;
        if c.assignment_id != actor
            || c.purpose != WorkerDataPurpose::Mutation
            || !(t.intent.call.mutates() || matches!(t.intent.call, ToolCall::Command { .. }))
        {
            return Err(ReplayError::InvalidTransition);
        }
        (
            w.pins.data_bytes as usize,
            (w.pins.data_bytes as usize).div_ceil(WORKER_CHUNK_BYTES),
        )
    };
    Ok(limits)
}

pub(super) fn data_sealed(
    run: &mut RunView,
    s: &WorkerDataSealed,
    actor: AgentRunId,
    sequence: u64,
) -> Result<(), ReplayError> {
    let p = run
        .workers
        .pending
        .as_ref()
        .ok_or(ReplayError::InvalidTransition)?;
    if p.actor != actor
        || p.tool != s.tool_id
        || p.assignment != s.data.assignment_id
        || p.purpose != s.data.purpose
        || p.first != s.data.first_sequence
        || p.chunks != s.data.chunks
        || sequence != s.data.seal_sequence
        || p.bytes.len() != s.data.bytes as usize
        || crate::validate::digest(&p.bytes) != s.data.sha256
    {
        return Err(ReplayError::InvalidTransition);
    }
    let command = run.workers.capture.clone();
    if let Some((a, id, data)) = &command
        && (*a != actor || *id != s.tool_id || data != &s.data)
    {
        return Err(ReplayError::InvalidTransition);
    }
    let facts = decode_data(&p.bytes, p.purpose)?;
    run.workers.data.push(SealedWorkerData {
        reference: s.data.clone(),
        tool: s.tool_id,
        facts,
    });
    run.workers.pending = None;
    if let Some(capture) = command {
        apply_command_capture(run, &capture)?;
        run.workers.capture = None;
    }
    Ok(())
}

pub(super) fn handover(
    run: &mut RunView,
    h: &WorkerHandover,
    actor: AgentRunId,
    sequence: u64,
) -> Result<(), ReplayError> {
    let w = worker(run, h.assignment_id)?;
    if actor != h.assignment_id
        || run.workers.stop.is_some()
        || w.handover.is_some()
        || w.released.is_some()
        || w.discarded.is_some()
        || w.pins.base.sha256 != h.base_digest
        || h.criterion_evidence != private_criterion_evidence(run, h.assignment_id)?
        || w.outcome.as_ref().is_none_or(|(_, outcome)| {
            outcome.disposition != WorkerDisposition::Ready
                || outcome.data.as_ref() != Some(&h.data)
        })
    {
        return Err(ReplayError::InvalidTransition);
    }
    valid_delta(run, h.assignment_id, &h.data, true)?;
    worker_mut(run, h.assignment_id)?.handover = Some((sequence, h.clone()));
    Ok(())
}

pub(super) fn integration(
    run: &mut RunView,
    i: &WorkerIntegration,
    actor: AgentRunId,
    sequence: u64,
) -> Result<(), ReplayError> {
    if !primary(run, actor) {
        return Err(ReplayError::InvalidTransition);
    }
    let w = worker(run, i.assignment_id)?;
    let (handover_sequence, handover) =
        w.handover.as_ref().ok_or(ReplayError::InvalidTransition)?;
    let t = run
        .tools
        .iter()
        .find(|t| t.intent.id == i.tool_id && t.finished_sequence == Some(i.finished_sequence))
        .ok_or(ReplayError::InvalidTransition)?;
    let o = t
        .observation
        .as_ref()
        .ok_or(ReplayError::InvalidTransition)?;
    let expected = ChangesetSource::WorkerHandover {
        assignment_id: i.assignment_id,
        handover_sequence: *handover_sequence,
        data: handover.data.clone(),
    };
    let outcome = match o.disposition {
        ToolDisposition::Succeeded if o.attested_for(&t.intent) => {
            WorkerIntegrationOutcome::Integrated
        }
        ToolDisposition::Conflict => WorkerIntegrationOutcome::Conflict,
        ToolDisposition::Denied => WorkerIntegrationOutcome::Denied,
        ToolDisposition::Failed | ToolDisposition::Limit | ToolDisposition::Cancelled
            if o.attestation.is_none() =>
        {
            WorkerIntegrationOutcome::Denied
        }
        ToolDisposition::Uncertain => WorkerIntegrationOutcome::Uncertain,
        _ => return Err(ReplayError::InvalidTransition),
    };
    if *handover_sequence != i.handover_sequence
        || w.integration.is_some()
        || w.discarded.is_some()
        || w.released.is_some()
        || i.outcome != outcome
        || !matches!(&t.intent.call, ToolCall::ApplyChangeset { source } if source == &expected)
        || t.intent.target != EffectTarget::Workspace
    {
        return Err(ReplayError::InvalidTransition);
    }
    worker_mut(run, i.assignment_id)?.integration = Some((sequence, i.clone()));
    Ok(())
}

pub(super) fn discard(
    run: &mut RunView,
    d: &WorkerSettlement,
    actor: AgentRunId,
    sequence: u64,
) -> Result<(), ReplayError> {
    if !primary(run, actor) {
        return Err(ReplayError::InvalidTransition);
    }
    let w = worker(run, d.assignment_id)?;
    if w.integration.as_ref().is_some_and(|(_, integration)| {
        matches!(
            integration.outcome,
            WorkerIntegrationOutcome::Integrated | WorkerIntegrationOutcome::Uncertain
        )
    }) || w.discarded.is_some()
        || w.released.is_some()
        || integration_in_flight(run, d.assignment_id)
        || d.handover_sequence != w.handover.as_ref().map(|(sequence, _)| *sequence)
        || !valid_discard(run, w, d)
    {
        return Err(ReplayError::InvalidTransition);
    }
    worker_mut(run, d.assignment_id)?.discarded = Some((sequence, d.clone()));
    Ok(())
}

pub(super) fn lease_release(
    run: &mut RunView,
    r: &WorkerSettlement,
    actor: AgentRunId,
    sequence: u64,
) -> Result<(), ReplayError> {
    if !primary(run, actor) {
        return Err(ReplayError::InvalidTransition);
    }
    let w = worker(run, r.assignment_id)?;
    let integrated = w
        .integration
        .as_ref()
        .is_some_and(|(integration_sequence, i)| {
            i.outcome == WorkerIntegrationOutcome::Integrated
                && r.reason == WorkerSettlementReason::Integrated
                && *integration_sequence == r.cause_sequence
        });
    let disposed = w.discarded.as_ref().is_some_and(|(discard_sequence, d)| {
        *discard_sequence == r.cause_sequence && d.reason == r.reason
    });
    if w.released.is_some()
        || w.outcome.is_none()
        || integration_in_flight(run, r.assignment_id)
        || r.handover_sequence != w.handover.as_ref().map(|(sequence, _)| *sequence)
        || !(integrated || disposed)
    {
        return Err(ReplayError::InvalidTransition);
    }
    worker_mut(run, r.assignment_id)?.released = Some(sequence);
    Ok(())
}

pub(super) fn verification(
    run: &mut RunView,
    v: &WorkerVerification,
    actor: AgentRunId,
    sequence: u64,
    verification_contract: Option<GoalPlanContract>,
) -> Result<(), ReplayError> {
    if !primary(run, actor) || run.workers.stop.is_some() {
        return Err(ReplayError::InvalidTransition);
    }
    let w = worker(run, v.assignment_id)?;
    let released = w.released.ok_or(ReplayError::InvalidTransition)?;
    let criterion = &w.pins.request.criterion;
    if v.criterion_digest
        != crate::validate::digest(
            &serde_json::to_vec(criterion).map_err(|_| ReplayError::InvalidPayload)?,
        )
        || w.verification
            .as_ref()
            .is_some_and(|(_, prior)| prior.passed)
        || w.outcome.as_ref().is_none_or(|(_, outcome)| {
            !matches!(
                outcome.disposition,
                WorkerDisposition::Ready | WorkerDisposition::NoChange
            )
        })
        || w.integration.as_ref().is_some_and(|(_, integration)| {
            integration.outcome != WorkerIntegrationOutcome::Integrated
        })
    {
        return Err(ReplayError::InvalidTransition);
    }
    let mut contract = verification_contract.ok_or(ReplayError::InvalidTransition)?;
    contract.success_criterion = criterion.clone();
    if v.passed != verification_passed(&contract, run, released, &v.evidence)? {
        return Err(ReplayError::InvalidTransition);
    }
    worker_mut(run, v.assignment_id)?.verification = Some((sequence, v.clone()));
    Ok(())
}

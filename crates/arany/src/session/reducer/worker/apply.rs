use super::{
    AgentRunId, PendingData, ReplayError, RunView, SealedWorkerData, pending_command_capture,
    primary, tool, valid_delta, worker, worker_mut,
};
use crate::session::worker::{
    WORKER_CHUNK_BYTES, WORKER_FILES, WorkerDataChunk, WorkerDataPurpose, WorkerDataSealed,
    WorkerStopRecord, decode_data,
};
use crate::tools::ToolCall;

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
    let command = pending_command_capture(run);
    let t = tool(run, actor)
        .filter(|t| {
            t.observation.is_none()
                || command.as_ref().is_some_and(|(a, id, data)| {
                    *a == actor && *id == t.intent.id && data.assignment_id == c.assignment_id
                })
        })
        .ok_or(ReplayError::InvalidTransition)?;
    if t.intent.id != c.tool_id
        || matches!(t.intent.call, ToolCall::Command { .. }) && command.is_none()
    {
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
    let command = pending_command_capture(run);
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
    if command.is_some() {
        run.workers.capture = None;
        let changes = valid_delta(run, actor, &s.data, false)?;
        if changes.is_empty() {
            return Err(ReplayError::InvalidTransition);
        }
        let w = worker_mut(run, actor)?;
        w.revision = w
            .revision
            .checked_add(1)
            .filter(|r| *r <= 16)
            .ok_or(ReplayError::InvalidTransition)?;
        for change in changes {
            if let Some(old) = w.overlay.iter_mut().find(|f| f.path == change.path) {
                old.after = change.after;
            } else {
                w.overlay.push(change);
            }
        }
        w.overlay.sort_by(|a, b| a.path.cmp(&b.path));
        if w.overlay.len() > WORKER_FILES {
            return Err(ReplayError::InvalidTransition);
        }
    }
    Ok(())
}

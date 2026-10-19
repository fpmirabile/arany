use super::*;
use crate::session::rewind::*;

pub(super) fn seal(view: &mut SessionView) {
    if let Some(last) = view.rewinds.operations.last_mut()
        && last.finished.is_none()
    {
        last.sealed = true;
    }
}

pub(super) fn before_event(view: &mut SessionView, event: &Event) {
    if view
        .rewinds
        .operations
        .last()
        .is_some_and(|r| r.finished.is_none() && !r.sealed)
        && !matches!(event, Event::WorkspaceRewindFinished { .. })
    {
        seal(view);
    }
}

pub(super) fn start(
    view: &mut SessionView,
    record: &RewindStarted,
    sequence: u64,
) -> Result<(), ReplayError> {
    if view
        .rewinds
        .operations
        .iter()
        .any(|r| r.started.rewind_id == record.rewind_id)
        || view
            .rewinds
            .operations
            .last()
            .is_some_and(|r| r.finished.is_none() && !r.sealed)
        || record.head_sequence != view.last_sequence
        || record.prefix_digest != view.rewinds.prefix.digest()
        || view.workspace_identity != Some(workspace(record))
    {
        return Err(ReplayError::InvalidTransition);
    }
    let plan = plan(view, &record.target)?.select(&record.kept, &record.overwrite)?;
    if plan.files.is_empty()
        || !plan.blockers.is_empty()
        || plan.files.iter().any(|f| f.history_drift || f.unavailable)
        || plan.digest() != record.plan_digest
        || plan.files.len() != record.file_count as usize
        || plan.restored_bytes() != record.restored_bytes
    {
        return Err(ReplayError::InvalidTransition);
    }
    // Like an idle rename, this seals a crash-left predecessor without inventing a receipt.
    if let Some(run) = view.runs.last_mut() {
        run.interrupt();
    }
    for goal in view
        .goals
        .iter_mut()
        .filter(|g| g.status == GoalStatus::Running)
    {
        goal.status = GoalStatus::Interrupted;
        goal.active_run = None;
    }
    view.rewinds.checkpoints.push(CheckpointTarget::Rewind {
        rewind_id: record.rewind_id,
        sequence,
    });
    view.rewinds.operations.push(RewindView {
        started: record.clone(),
        started_sequence: sequence,
        finished: None,
        finished_sequence: None,
        files: plan.files,
        sealed: false,
    });
    Ok(())
}

pub(super) fn finish(
    view: &mut SessionView,
    record: &RewindFinished,
    sequence: u64,
) -> Result<(), ReplayError> {
    let r = view
        .rewinds
        .operations
        .last()
        .ok_or(ReplayError::InvalidTransition)?;
    if r.sealed
        || r.finished.is_some()
        || r.started.rewind_id != record.rewind_id
        || view.last_sequence != r.started_sequence
        || record.conflict_count as usize > r.files.len()
        || record
            .conflicts
            .iter()
            .any(|p| !r.files.iter().any(|f| &f.path == p))
    {
        return Err(ReplayError::InvalidTransition);
    }
    if let Some(a) = &record.attestation
        && (a.plan_digest != r.started.plan_digest
            || a.workspace_device != r.started.workspace_device
            || a.workspace_inode != r.started.workspace_inode
            || a.enforcement_digest != r.started.enforcement_digest)
    {
        return Err(ReplayError::InvalidTransition);
    }
    let reached_go = record.attestation.is_some();
    if matches!(
        record.outcome,
        RewindOutcome::Restored | RewindOutcome::Uncertain
    ) {
        if reached_go || record.outcome == RewindOutcome::Restored {
            let c = record
                .checkpoint
                .as_ref()
                .ok_or(ReplayError::InvalidTransition)?;
            let needed = r.files.iter().any(|f| f.expected_current.is_some());
            if (c.status == super::super::CheckpointStatus::NotNeeded) == needed
                || c.pruned_before_run
                    .is_some_and(|id| !view.runs.iter().any(|run| run.id == id))
            {
                return Err(ReplayError::InvalidTransition);
            }
        } else if record.checkpoint.is_some() {
            return Err(ReplayError::InvalidTransition);
        }
    }
    if record.outcome == RewindOutcome::Restored {
        let paths: Vec<_> = r.files.iter().map(|f| f.path.as_str()).collect();
        for goal in &mut view.goals {
            if let Some(contract) = &goal.plan_contract
                && invalidates(contract, &r.files)
            {
                goal.success_state = contract.initial_success_state();
                goal.success_criterion_satisfied = false;
            }
        }
        for run in &mut view.runs {
            for agent in &mut run.agents {
                if let Some(w) = &mut agent.assignment
                    && w.pins.request.lease.iter().any(|p| {
                        paths
                            .iter()
                            .any(|q| *q == p || q.starts_with(&format!("{p}/")))
                    })
                {
                    w.verification = None;
                }
            }
        }
    }
    let r = view
        .rewinds
        .operations
        .last_mut()
        .expect("validated rewind");
    r.finished = Some(record.clone());
    r.finished_sequence = Some(sequence);
    Ok(())
}

use super::*;

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

pub(super) fn lease_release(
    run: &mut RunView,
    r: &WorkerLeaseRelease,
    actor: AgentRunId,
    sequence: u64,
) -> Result<(), ReplayError> {
    if !primary(run, actor) {
        return Err(ReplayError::InvalidTransition);
    }
    let w = worker(run, r.assignment_id)?;
    if w.released.is_some()
        || w.outcome
            .as_ref()
            .is_none_or(|(seq, _)| *seq != r.cause_sequence)
        || run
            .agents
            .iter()
            .find(|a| a.id == r.assignment_id)
            .is_none_or(|a| {
                a.status == AgentStatus::Active || a.tools.iter().any(ToolView::outcome_unknown)
            })
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

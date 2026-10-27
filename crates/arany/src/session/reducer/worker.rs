use super::*;
use crate::session::worker::*;
use crate::session::{ChildGrant, GoalSuccessCriterion};
use crate::tools::{Attestation, ToolCall, ToolDisposition};

mod apply;
mod before;
mod command;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkerState {
    pub stop: Option<(u64, WorkerStopReason)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerView {
    pub pins: WorkerAssignment,
    pub outcome: Option<(u64, WorkerOutcome)>,
    pub released: Option<u64>,
    pub verification: Option<(u64, WorkerVerification)>,
    pub interrupted: bool,
    pub gate_continuations: u8,
}
impl WorkerView {
    pub(super) fn new(pins: WorkerAssignment) -> Self {
        Self {
            pins,
            outcome: None,
            released: None,
            verification: None,
            interrupted: false,
            gate_continuations: 0,
        }
    }
    pub(crate) fn settled(&self) -> bool {
        self.released.is_some()
            && (self.outcome.as_ref().is_some_and(|(_, o)| {
                !matches!(
                    o.disposition,
                    WorkerDisposition::Ready | WorkerDisposition::NoChange
                )
            }) || self.verification.as_ref().is_some_and(|(_, v)| v.passed))
    }
}
impl RunView {
    pub fn worker_history(&self) -> Vec<WorkerHistory> {
        self.agents
            .iter()
            .filter_map(|a| {
                let w = a.assignment.as_ref()?;
                let unknown = a.tools.iter().any(ToolView::outcome_unknown);
                let publication = if unknown {
                    WorkerPublication::Unknown
                } else if a.tools.iter().any(|t| {
                    t.intent.call.mutates()
                        && t.observation.as_ref().is_some_and(|o| {
                            o.disposition == ToolDisposition::Succeeded && o.attested_for(&t.intent)
                        })
                }) {
                    WorkerPublication::Confirmed
                } else {
                    WorkerPublication::Unpublished
                };
                let outcome = w.outcome.as_ref().map(|(_, o)| o);
                let mut report = a
                    .result
                    .clone()
                    .or_else(|| a.summary.clone())
                    .unwrap_or_default();
                report.truncate(crate::tools::types::truncate_utf8(&report, 2048).len());
                Some(WorkerHistory {
                    run_id: self.id,
                    assignment_id: a.id,
                    resumes: w.pins.request.resumes,
                    supersedes: w.pins.request.supersedes,
                    status: if w.interrupted {
                        "interrupted; Workspace effects retained".into()
                    } else {
                        outcome.map_or_else(
                            || a.status.to_string(),
                            |o| format!("{:?}", o.disposition),
                        )
                    },
                    cause: outcome.map(|o| o.cause),
                    publication,
                    lease_released: w.released.is_some(),
                    verified: w.verification.as_ref().is_some_and(|(_, v)| v.passed),
                    changed_paths: crate::session::worker::changed_paths(self, a),
                    report,
                })
            })
            .collect()
    }
}
fn worker(run: &RunView, id: AgentRunId) -> Result<&WorkerView, ReplayError> {
    run.agents
        .iter()
        .find(|a| a.id == id)
        .and_then(|a| a.assignment.as_ref())
        .ok_or(ReplayError::InvalidTransition)
}
fn worker_mut(run: &mut RunView, id: AgentRunId) -> Result<&mut WorkerView, ReplayError> {
    run.agents
        .iter_mut()
        .find(|a| a.id == id)
        .and_then(|a| a.assignment.as_mut())
        .ok_or(ReplayError::InvalidTransition)
}
fn primary(run: &RunView, actor: AgentRunId) -> bool {
    run.primary()
        .is_some_and(|a| a.id == actor && a.status == AgentStatus::Active)
}
fn quiescent(run: &RunView) -> bool {
    run.tools
        .iter()
        .chain(run.agents.iter().flat_map(|a| &a.tools))
        .all(|t| t.observation.is_some())
}
pub(crate) fn coordination_ready(run: &RunView) -> bool {
    let proposed: usize = run
        .primary()
        .into_iter()
        .flat_map(|agent| &agent.provider_calls)
        .filter_map(|record| record.delegation.as_ref())
        .map(Vec::len)
        .sum();
    proposed == run.agents.len().saturating_sub(1)
        && run.agents.iter().skip(1).all(|agent| {
            agent
                .assignment
                .as_ref()
                .map_or(agent.status == AgentStatus::Finished, |worker| {
                    (worker.pins.request.mode == WorkerMode::Background
                        && agent.status == AgentStatus::Active
                        || agent.status != AgentStatus::Active
                            && worker.released.is_some()
                            && worker.outcome.is_some())
                        && !agent.tools.iter().any(ToolView::outcome_unknown)
                })
        })
}
pub(crate) fn wave_settled(run: &RunView) -> bool {
    coordination_ready(run)
        && run
            .agents
            .iter()
            .filter_map(|agent| agent.assignment.as_ref())
            .all(WorkerView::settled)
}
pub(super) fn child_terminal(a: &AgentView) -> bool {
    a.assignment
        .as_ref()
        .map_or(a.status == AgentStatus::Finished, |w| {
            a.status != AgentStatus::Active && w.settled()
        })
}
fn proposal(run: &RunView, ordinal: u8) -> Result<&ChildAssignment, ReplayError> {
    let plan = run
        .primary()
        .and_then(|a| {
            a.provider_calls
                .iter()
                .rev()
                .find(|c| c.delegation.is_some())
        })
        .and_then(|r| r.delegation.as_ref())
        .ok_or(ReplayError::InvalidTransition)?;
    let before = run
        .agents
        .iter()
        .skip(1)
        .filter(|a| a.ordinal < ordinal)
        .count();
    let earlier: usize = run
        .primary()
        .into_iter()
        .flat_map(|a| a.provider_calls.iter().rev())
        .filter_map(|r| r.delegation.as_ref())
        .skip(1)
        .map(Vec::len)
        .sum();
    plan.get(
        before
            .checked_sub(earlier)
            .ok_or(ReplayError::InvalidTransition)?,
    )
    .ok_or(ReplayError::InvalidTransition)
}

pub(crate) fn validate_request(
    view: &SessionView,
    run: &RunView,
    request: &EditorAssignment,
    spawning: Option<AgentRunId>,
) -> Result<(), crate::session::ProposalRefusalCause> {
    use crate::session::ProposalRefusalCause::*;
    let config = run.config.as_ref().ok_or(Phase)?;
    let policy = config.worker_policy.as_ref().ok_or(EditorGrant)?;
    let goal = active_run_goal(view, run.id).ok_or(Phase)?;
    let plan = goal.plan_contract.as_ref().ok_or(Phase)?;
    if goal.phase != GoalPhase::Implementation || goal.awaiting_plan_contract {
        return Err(Phase);
    }
    let registry = super::definition_registry(run).map_err(|_| EditorDefinition)?;
    let definition = registry.get(&request.definition).ok_or(EditorDefinition)?;
    if !definition.editor() || definition.background && request.mode != WorkerMode::Background {
        return Err(EditorDefinition);
    }
    if request.definition_digest
        != crate::validate::digest(&serde_json::to_vec(definition).map_err(|_| EditorDigest)?)
    {
        return Err(EditorDigest);
    }
    if request.max_turns > definition.max_turns || request.max_turns > policy.max_turns {
        return Err(EditorTurns);
    }
    if request
        .lease
        .iter()
        .any(|p| !within(p, &plan.paths) || !within(p, &policy.parent_grant.write_paths))
    {
        return Err(EditorLease);
    }
    let grant = policy
        .parent_grant
        .clone()
        .narrowed_for_editor(request, &plan.commands, |tool| {
            definition.allows_worker_tool(tool)
        });
    if !grant.valid() {
        return Err(EditorGrant);
    }
    validate_editor_criterion(request, &grant, plan)?;
    for source in request.resumes.iter().chain(request.supersedes.iter()) {
        if Some(*source) == spawning || view.worker_resume_source(goal.id, *source).is_none() {
            return Err(EditorResume);
        }
    }
    Ok(())
}

fn validate_editor_criterion(
    request: &EditorAssignment,
    grant: &WorkerGrant,
    plan: &GoalPlanContract,
) -> Result<(), crate::session::ProposalRefusalCause> {
    use crate::session::ProposalRefusalCause::EditorCriterion;
    if match &request.criterion {
        GoalSuccessCriterion::ToolCallSucceeded { call } => {
            !plan.allows_call(call)
                || matches!(call.as_ref(), ToolCall::Mkdir { .. })
                || !grant.allows(call)
                || matches!(call.as_ref(), ToolCall::Command { program, .. } if !request.commands.contains(program))
        }
        GoalSuccessCriterion::WorkspaceFileContent { path, .. }
        | GoalSuccessCriterion::WorkspaceFileDigest { path, .. } => {
            !within(path, &plan.paths) || !within(path, &request.lease)
        }
    } {
        return Err(EditorCriterion);
    }
    Ok(())
}

impl SessionView {
    pub(crate) fn worker_resume_source(
        &self,
        goal: GoalId,
        source: AgentRunId,
    ) -> Option<(&RunView, &AgentView)> {
        let goal = self.goal(goal)?;
        self.runs
            .iter()
            .filter(|r| goal.run_ids.contains(&r.id))
            .find_map(|r| {
                let a = r.agents.iter().find(|a| a.id == source)?;
                (a.role == AgentRole::Child
                    && a.status != AgentStatus::Active
                    && a.assignment
                        .as_ref()
                        .is_none_or(|w| w.released.is_some() || r.status != RunStatus::Active))
                .then_some((r, a))
            })
    }
}

pub(super) fn validate_spawn(view: &SessionView, event: &Event) -> Result<(), ReplayError> {
    let Event::AgentSpawned {
        run_id,
        agent_run_id: id,
        ordinal,
        objective,
        definition,
        profile,
        grant,
        assignment: pins,
        ..
    } = event
    else {
        return Err(ReplayError::InvalidTransition);
    };
    let (run_id, id, ordinal, grant) = (*run_id, *id, *ordinal, *grant);
    let run = view
        .runs
        .last()
        .filter(|r| r.id == run_id)
        .ok_or(ReplayError::InvalidTransition)?;
    if pins.is_some() && view.runs.iter().flat_map(|r| &r.agents).any(|a| a.id == id) {
        return Err(ReplayError::InvalidTransition);
    }
    if ordinal == 0 {
        return if pins.is_none() {
            Ok(())
        } else {
            Err(ReplayError::InvalidTransition)
        };
    }
    match (proposal(run, ordinal)?, pins) {
        (
            ChildAssignment::Researcher {
                objective: expected,
                definition: selected,
                profile: selected_profile,
            },
            None,
        ) if objective.as_ref() == Some(expected)
            && definition == selected
            && profile == selected_profile
            && !matches!(grant, Some(ChildGrant::EditOnly { .. })) =>
        {
            Ok(())
        }
        (ChildAssignment::Editor { assignment }, Some(p))
            if assignment.as_ref() == &p.request
                && objective.as_ref() == Some(&p.request.mission)
                && definition.as_ref() == Some(&p.request.definition)
                && profile.is_none()
                && matches!(grant, Some(ChildGrant::EditOnly { .. })) =>
        {
            let goal = active_run_goal(view, run_id).ok_or(ReplayError::InvalidTransition)?;
            let config = run.config.as_ref().ok_or(ReplayError::InvalidTransition)?;
            let policy = config
                .worker_policy
                .as_ref()
                .ok_or(ReplayError::InvalidTransition)?;
            let receipt = config
                .tool_policy
                .as_ref()
                .ok_or(ReplayError::InvalidTransition)?;
            validate_request(view, run, &p.request, Some(id))
                .map_err(|_| ReplayError::InvalidTransition)?;
            let plan = goal
                .plan_contract
                .as_ref()
                .ok_or(ReplayError::InvalidTransition)?;
            let registry = super::definition_registry(run)?;
            let def = registry
                .get(&p.request.definition)
                .ok_or(ReplayError::InvalidTransition)?;
            if run.workers.stop.is_some()
                || !p.grant.within(&policy.parent_grant)
                || p.grant.write_paths != p.request.lease
                || p.grant.commands != p.request.commands
                || !p.grant.read_paths.iter().all(|l| within(l, &plan.paths))
                || p.policy_digest != receipt.config_digest
                || p.permissions_digest != receipt.permissions_digest
                || run
                    .agents
                    .iter()
                    .filter(|a| a.assignment.as_ref().is_some_and(|w| w.released.is_none()))
                    .count()
                    >= usize::from(policy.concurrency)
                || run
                    .agents
                    .iter()
                    .filter_map(|a| a.assignment.as_ref())
                    .filter(|w| w.released.is_none())
                    .any(|w| {
                        w.pins
                            .request
                            .lease
                            .iter()
                            .any(|a| p.request.lease.iter().any(|b| overlaps(a, b)))
                    })
            {
                return Err(ReplayError::InvalidTransition);
            }
            for op in &p.grant.tools {
                if !def.allows_worker_tool(*op) {
                    return Err(ReplayError::InvalidTransition);
                }
            }
            let wave = run
                .primary()
                .into_iter()
                .flat_map(|a| a.provider_calls.iter())
                .filter(|r| r.delegation.is_some())
                .count();
            if usize::from(p.request.wave) != wave {
                return Err(ReplayError::InvalidTransition);
            }
            Ok(())
        }
        _ => Err(ReplayError::InvalidTransition),
    }
}
pub(super) fn child_tool_admissible(
    run: &RunView,
    id: AgentRunId,
    intent: &crate::tools::EffectIntent,
) -> bool {
    let Ok(w) = worker(run, id) else {
        return false;
    };
    let Some(a) = run.agents.iter().find(|a| a.id == id) else {
        return false;
    };
    intent.target == EffectTarget::Workspace
        && w.outcome.is_none()
        && run.workers.stop.is_none()
        && quiescent(run)
        && intent.policy_digest == w.pins.policy_digest
        && a.status == AgentStatus::Active
        && !a
            .tools
            .iter()
            .any(|t| t.intent.id == intent.id || editor_tool_failed(t))
        && a.provider_calls
            .iter()
            .rev()
            .find(|record| record.phase != AgentPhase::ToolReview)
            .is_some_and(|r| r.disposition == ProviderCallDisposition::ToolRequested)
        && command::admissible(run, a, intent)
        && super::tool::validate_tool_lease(run, id, &intent.call).is_ok()
}

pub(super) fn before_event(view: &SessionView, event: &Event) -> Result<(), ReplayError> {
    let Some(run) = view.runs.last().filter(|r| r.status == RunStatus::Active) else {
        return Ok(());
    };
    let (scope, actor) = event.scope();
    if scope != Some(run.id) {
        return Ok(());
    }
    if let Event::ToolStarted {
        intent, verdict, ..
    } = event
        && run
            .tools
            .iter()
            .chain(run.agents.iter().flat_map(|agent| &agent.tools))
            .any(|tool| tool.intent == intent.clone().into() && tool.started_sequence.is_none())
    {
        if verdict.as_ref().is_none_or(|verdict| verdict.approved) {
            before::check(run, event, actor)?;
        }
        return Ok(());
    }
    before::check(run, event, actor)?;
    match event {
        Event::ProviderCallRecorded {
            agent_run_id,
            record,
            ..
        } if worker(run, *agent_run_id).is_ok() => {
            before::provider_call(run, agent_run_id, record)?
        }
        Event::ToolProposed {
            agent_run_id,
            intent,
            ..
        }
        | Event::ToolStarted {
            agent_run_id,
            intent,
            ..
        } => before::tool_started(run, agent_run_id, &intent.clone().into())?,
        Event::ToolFinished {
            agent_run_id,
            observation: o,
            ..
        } => before::tool_finished(view, run, agent_run_id, o)?,
        Event::AgentFinished {
            agent_run_id,
            disposition,
            worker: outcome,
            ..
        } => before::agent_finished(run, agent_run_id, disposition, outcome.as_ref())?,
        Event::ProviderCallRecorded {
            agent_run_id,
            record,
            ..
        } if primary(run, *agent_run_id) => before::primary_provider_call(view, run, record)?,
        Event::RunFinished {
            disposition: RunDisposition::Finished,
            ..
        } => before::run_finished(run)?,
        _ => {}
    }
    Ok(())
}

pub(super) fn after_event(
    view: &mut SessionView,
    event: &Event,
    sequence: u64,
) -> Result<(), ReplayError> {
    let Some(run) = view.runs.last_mut() else {
        return Ok(());
    };
    match event {
        Event::GoalProviderCallReserved { agent_run_id, .. }
            if run
                .agents
                .iter()
                .find(|a| a.id == *agent_run_id)
                .is_some_and(|a| {
                    a.assignment.is_some()
                        && a.provider_calls.last().is_some_and(|c| {
                            c.failure_reason
                                == Some(
                                    crate::provider::ProviderFailureReason::CriterionNotMetInView,
                                )
                        })
                }) =>
        {
            worker_mut(run, *agent_run_id)?.gate_continuations += 1;
        }
        Event::ToolFinished { observation: o, .. } => {
            if o.intent.target == EffectTarget::Workspace
                && (o.intent.call.mutates() || o.intent.call.is_process())
            {
                for a in &mut run.agents {
                    if let Some(w) = &mut a.assignment {
                        w.verification = None;
                    }
                }
            }
        }
        Event::AgentFinished {
            agent_run_id,
            worker: Some(outcome),
            ..
        } => {
            worker_mut(run, *agent_run_id)?.outcome = Some((sequence, outcome.clone()));
        }
        Event::RunFinished { .. } => {
            for a in &mut run.agents {
                if let Some(w) = &mut a.assignment
                    && !w.settled()
                {
                    w.interrupted = true;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn apply(
    current: &mut SessionView,
    event: &Event,
    sequence: u64,
) -> Result<(), ReplayError> {
    let (run_id, actor) = event.scope();
    let verification_contract = if matches!(event, Event::WorkerVerificationRecorded { .. }) {
        active_run_goal(current, run_id.ok_or(ReplayError::InvalidTransition)?)
            .and_then(|g| g.plan_contract.clone())
    } else {
        None
    };
    let run = active_run(current, run_id.ok_or(ReplayError::InvalidTransition)?)?;
    let actor = actor.ok_or(ReplayError::InvalidTransition)?;
    match event {
        Event::WorkerStopObserved { record, .. } => {
            apply::stop_observed(run, record, actor, sequence)?
        }
        Event::WorkerDataChunk { .. }
        | Event::WorkerDataSealed { .. }
        | Event::WorkerHandoverRecorded { .. }
        | Event::WorkerIntegrationRecorded { .. }
        | Event::WorkerHandoverDiscarded { .. } => return Err(ReplayError::InvalidTransition),
        Event::WorkerLeaseReleased { record, .. } => {
            apply::lease_release(run, record, actor, sequence)?
        }
        Event::WorkerVerificationRecorded { record, .. } => {
            apply::verification(run, record, actor, sequence, verification_contract)?
        }
        _ => return Err(ReplayError::InvalidTransition),
    }
    Ok(())
}
pub(crate) fn evidence_admitted(
    contract: &crate::session::GoalPlanContract,
    o: &crate::tools::ToolObservation,
) -> bool {
    o.intent.target == EffectTarget::Workspace
        && !matches!(o.intent.call, ToolCall::PrepareWorker { .. })
        && (!matches!(o.intent.call, ToolCall::ApplyChangeset { .. })
            || matches!((&contract.success_criterion, &o.intent.call), (GoalSuccessCriterion::ToolCallSucceeded { call }, ToolCall::ApplyChangeset { source: ChangesetSource::CommandReceipt { .. } }) if matches!(call.as_ref(), ToolCall::Command { .. })))
}

pub(crate) fn verification_passed(
    contract: &crate::session::GoalPlanContract,
    run: &RunView,
    released: u64,
    evidence: &[u64],
) -> Result<bool, ReplayError> {
    for seq in evidence {
        let o = run
            .tools
            .iter()
            .find(|t| t.finished_sequence == Some(*seq))
            .and_then(|t| t.observation.as_ref())
            .ok_or(ReplayError::InvalidTransition)?;
        if *seq <= released || !evidence_admitted(contract, o) {
            return Err(ReplayError::InvalidTransition);
        }
    }
    let mut observations = run
        .tools
        .iter()
        .chain(run.agents.iter().flat_map(|agent| &agent.tools))
        .filter_map(|tool| Some((tool.finished_sequence.filter(|seq| *seq > released)?, tool)))
        .collect::<Vec<_>>();
    observations.sort_unstable_by_key(|(sequence, _)| *sequence);
    let mut state = contract.initial_success_state();
    for (seq, t) in observations {
        let o = t
            .observation
            .as_ref()
            .ok_or(ReplayError::InvalidTransition)?;
        if evidence.contains(&seq) {
            contract.observe_success(&mut state, o);
        } else if o.intent.call.mutates() || o.intent.call.is_process() {
            state = contract.initial_success_state();
        } else {
            let prior = state.clone();
            contract.observe_success(&mut state, o);
            if contract.success_state_satisfied(&state) && !contract.success_state_satisfied(&prior)
            {
                state = prior;
            }
        }
    }
    Ok(contract.success_state_satisfied(&state))
}

pub(crate) fn editor_tool_failed(t: &ToolView) -> bool {
    let Some(o) = t.observation.as_ref() else {
        return false;
    };
    let command = matches!(o.intent.call, ToolCall::Command { .. });
    let recoverable_refusal = recoverable_editor_refusal(t);
    o.disposition != ToolDisposition::Succeeded
        && !(recoverable_refusal && !command)
        && !(command
            && !matches!(
                o.disposition,
                ToolDisposition::Uncertain | ToolDisposition::Cancelled
            )
            && o.cache_update != crate::tools::CacheUpdate::Uncertain
            && (o.disposition != ToolDisposition::Denied
                || (t.permission_verdict.is_some() && recoverable_refusal)))
}

fn recoverable_editor_refusal(t: &ToolView) -> bool {
    use crate::tools::ApprovalCause;

    let Some(observation) = t.observation.as_ref() else {
        return false;
    };
    let cause_is_recoverable = matches!(
        observation.approval_cause,
        None | Some(ApprovalCause::Declined | ApprovalCause::ChannelUnavailable)
    );
    cause_is_recoverable && t.model_denial_output().is_some()
}

pub(crate) fn criterion_unmet_in_view(run: &RunView, id: AgentRunId) -> Result<bool, ReplayError> {
    Ok(editor_criterion_evidence(run, id)?.is_empty())
}
pub(crate) fn editor_criterion_evidence(
    run: &RunView,
    id: AgentRunId,
) -> Result<Vec<u64>, ReplayError> {
    let w = worker(run, id)?;
    let a = run
        .agents
        .iter()
        .find(|a| a.id == id)
        .ok_or(ReplayError::InvalidTransition)?;
    for t in a.tools.iter().rev() {
        let Some(o) = &t.observation else {
            return Ok(Vec::new());
        };
        if o.disposition == ToolDisposition::Succeeded && o.attested_for(&o.intent) {
            if let GoalSuccessCriterion::ToolCallSucceeded { call } = &w.pins.request.criterion
                && let Some(source) = command::criterion_command(a, t, call)
            {
                return Ok(vec![
                    source,
                    t.finished_sequence.ok_or(ReplayError::InvalidTransition)?,
                ]);
            }
            let passed = match &w.pins.request.criterion {
                GoalSuccessCriterion::ToolCallSucceeded { call } => {
                    o.intent.call == **call
                        && (!matches!(call.as_ref(), ToolCall::Command { .. })
                            || command::succeeded(o, true))
                }
                GoalSuccessCriterion::WorkspaceFileDigest { path, sha256 } => {
                    file_digest(o, path).as_ref() == Some(sha256)
                }
                GoalSuccessCriterion::WorkspaceFileContent { path, content } => {
                    file_digest(o, path).as_ref()
                        == Some(&crate::validate::hex_digest(content.as_bytes()))
                }
            };
            if passed {
                return Ok(vec![
                    t.finished_sequence.ok_or(ReplayError::InvalidTransition)?,
                ]);
            }
        }
        if o.intent.call.mutates() || o.intent.call.is_process() {
            break;
        }
    }
    Ok(Vec::new())
}
fn file_digest(o: &crate::tools::ToolObservation, path: &str) -> Option<String> {
    let same = match &o.intent.call {
        ToolCall::Read { path: p, .. }
        | ToolCall::Write { path: p, .. }
        | ToolCall::Edit { path: p, .. }
        | ToolCall::MultiEdit { file_path: p, .. } => p == path,
        _ => false,
    };
    if !same {
        return None;
    }
    let output: serde_json::Value = serde_json::from_str(&o.output).ok()?;
    output.get("sha256")?.as_str().map(str::to_owned)
}

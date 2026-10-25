use super::*;
use crate::session::worker::*;
use crate::session::{ChildGrant, GoalSuccessCriterion};
use crate::tools::{Attestation, ToolCall, ToolDisposition};

mod apply;
mod before;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkerState {
    pub stop: Option<(u64, WorkerStopReason)>,
    pub data: Vec<SealedWorkerData>,
    pending: Option<PendingData>,
    capture: Option<CommandCapture>,
}
type CommandCapture = (AgentRunId, uuid::Uuid, WorkerDataRef);
#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingData {
    actor: AgentRunId,
    assignment: AgentRunId,
    purpose: WorkerDataPurpose,
    tool: uuid::Uuid,
    first: u64,
    chunks: u16,
    bytes: Vec<u8>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SealedWorkerData {
    pub reference: WorkerDataRef,
    pub tool: uuid::Uuid,
    pub facts: WorkerDataFacts,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerView {
    pub pins: WorkerAssignment,
    pub revision: u32,
    pub overlay: Vec<WorkerFileFact>,
    pub outcome: Option<(u64, WorkerOutcome)>,
    pub handover: Option<(u64, WorkerHandover)>,
    pub integration: Option<(u64, WorkerIntegration)>,
    pub discarded: Option<(u64, WorkerSettlement)>,
    pub released: Option<u64>,
    pub verification: Option<(u64, WorkerVerification)>,
    pub interrupted: bool,
    pub gate_continuations: u8,
}
impl WorkerView {
    pub(super) fn new(pins: WorkerAssignment) -> Self {
        Self {
            pins,
            revision: 0,
            overlay: Vec::new(),
            outcome: None,
            handover: None,
            integration: None,
            discarded: None,
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
            }) || self.verification.as_ref().is_some_and(|(_, v)| v.passed)
                || self
                    .discarded
                    .as_ref()
                    .is_some_and(|(_, d)| d.reason != WorkerSettlementReason::NoChange))
    }
}

impl RunView {
    pub fn worker_history(&self) -> Vec<WorkerHistory> {
        let mut history: Vec<_> = self
            .agents
            .iter()
            .filter_map(|a| {
                let w = a.assignment.as_ref()?;
                let publication = self
                    .tools
                    .iter()
                    .find_map(|t| {
                        if !matches!(&t.intent.call, ToolCall::ApplyChangeset {
                    source: ChangesetSource::WorkerHandover { assignment_id, .. }
                } if *assignment_id == a.id)
                        {
                            return None;
                        }
                        Some(match &t.observation {
                            Some(o)
                                if o.disposition == ToolDisposition::Succeeded
                                    && o.attested_for(&t.intent) =>
                            {
                                WorkerPublication::Confirmed
                            }
                            None => WorkerPublication::Unknown,
                            Some(o) if o.disposition == ToolDisposition::Uncertain => {
                                WorkerPublication::Unknown
                            }
                            _ => WorkerPublication::Unpublished,
                        })
                    })
                    .unwrap_or(WorkerPublication::Unpublished);
                let outcome = w.outcome.as_ref().map(|(_, o)| o);
                let interrupted = w.interrupted || self.status != RunStatus::Active && !w.settled();
                let status = if interrupted && publication == WorkerPublication::Unpublished {
                    "interrupted; unpublished private work discarded".to_owned()
                } else {
                    outcome.map_or_else(|| a.status.to_string(), |o| format!("{:?}", o.disposition))
                };
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
                    status,
                    cause: outcome.map(|o| o.cause),
                    publication,
                    integration: w.integration.as_ref().map(|(_, i)| i.outcome),
                    discarded: w.discarded.as_ref().map(|(_, d)| d.reason),
                    private_outcome_unknown: a.tools.iter().any(|t| {
                        t.observation
                            .as_ref()
                            .is_none_or(|o| o.disposition == ToolDisposition::Uncertain)
                    }),
                    cleanup_confirmed: outcome.is_some_and(|o| o.cleanup.is_some()),
                    lease_released: w.released.is_some(),
                    verified: w.verification.as_ref().is_some_and(|(_, v)| v.passed),
                    report,
                })
            })
            .collect();
        for (index, t) in self.tools.iter().enumerate() {
            if let ToolCall::PrepareWorker { preparation } = &t.intent.call
                && preparation.purpose == WorkerDataPurpose::Base
                && !self
                    .agents
                    .iter()
                    .any(|a| a.id == preparation.assignment_id)
            {
                let ordinal = self.tools[..index].iter().filter(|t| matches!(&t.intent.call, ToolCall::PrepareWorker { preparation } if preparation.purpose == WorkerDataPurpose::Base)).count();
                let request = self
                    .primary()
                    .into_iter()
                    .flat_map(|a| &a.provider_calls)
                    .filter_map(|c| c.delegation.as_ref())
                    .flatten()
                    .filter_map(|p| match p {
                        ChildAssignment::Editor { assignment } => Some(assignment),
                        _ => None,
                    })
                    .nth(ordinal);
                history.push(WorkerHistory {
                    run_id: self.id,
                    assignment_id: preparation.assignment_id,
                    resumes: request.and_then(|r| r.resumes),
                    supersedes: request.and_then(|r| r.supersedes),
                    status: if self.status == RunStatus::Active {
                        "preparing"
                    } else {
                        "unspawned; private preparation discarded"
                    }
                    .into(),
                    cause: None,
                    publication: WorkerPublication::Unpublished,
                    integration: None,
                    discarded: None,
                    private_outcome_unknown: false,
                    cleanup_confirmed: false,
                    lease_released: false,
                    verified: false,
                    report: String::new(),
                });
            }
        }
        history
    }

    pub(crate) fn private_recovery_tools(&self) -> impl Iterator<Item = &ToolView> {
        self.agents
            .iter()
            .filter(|a| a.assignment.is_some())
            .flat_map(|a| &a.tools)
            .filter(|t| !t.settled_without_stop())
    }
}
impl WorkerState {
    pub(super) fn clear_pending(&mut self) {
        self.pending = None;
        self.capture = None;
    }
    fn sealed(&self, reference: &WorkerDataRef) -> Result<&SealedWorkerData, ReplayError> {
        self.data
            .iter()
            .find(|d| &d.reference == reference)
            .ok_or(ReplayError::InvalidTransition)
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
fn tool(run: &RunView, actor: AgentRunId) -> Option<&ToolView> {
    if primary(run, actor) {
        run.tools.last()
    } else {
        run.agents.iter().find(|a| a.id == actor)?.tools.last()
    }
}
fn quiescent(run: &RunView) -> bool {
    if run.workers.capture.is_some() {
        return false;
    }
    run.tools
        .iter()
        .chain(run.agents.iter().flat_map(|a| &a.tools))
        .all(|t| t.observation.is_some())
}

fn sealed_changes<'a>(
    run: &'a RunView,
    reference: &WorkerDataRef,
) -> Result<&'a [WorkerFileFact], ReplayError> {
    match &run.workers.sealed(reference)?.facts {
        WorkerDataFacts::Changes(changes) => Ok(changes),
        _ => Err(ReplayError::InvalidTransition),
    }
}
fn manifest<'a>(
    run: &'a RunView,
    w: &WorkerView,
) -> Result<&'a [WorkerManifestEntry], ReplayError> {
    match &run.workers.sealed(&w.pins.base)?.facts {
        WorkerDataFacts::Manifest(entries) => Ok(entries),
        _ => Err(ReplayError::InvalidTransition),
    }
}
fn base_version(entries: &[WorkerManifestEntry], path: &str) -> Option<WorkerBaseVersion> {
    entries
        .iter()
        .find(|e| e.path == path)
        .map(|e| e.version.clone())
        .or_else(|| {
            entries
                .iter()
                .any(|e| {
                    matches!(
                        e.version,
                        WorkerBaseVersion::Directory | WorkerBaseVersion::Absent
                    ) && contains(&e.path, path)
                })
                .then_some(WorkerBaseVersion::Absent)
        })
}
fn retained_image(entries: &[WorkerManifestEntry], changes: &[WorkerFileFact]) -> usize {
    let mut files: std::collections::BTreeMap<&str, &WorkerBaseVersion> = entries
        .iter()
        .map(|e| (e.path.as_str(), &e.version))
        .collect();
    for change in changes {
        files.insert(&change.path, &change.after);
    }
    files
        .into_iter()
        .map(|(path, version)| {
            path.len()
                + 128
                + match version {
                    WorkerBaseVersion::File { bytes, .. } => *bytes as usize,
                    _ => 0,
                }
        })
        .sum()
}

fn valid_delta(
    run: &RunView,
    id: AgentRunId,
    reference: &WorkerDataRef,
    cumulative: bool,
) -> Result<Vec<WorkerFileFact>, ReplayError> {
    let w = worker(run, id)?;
    let entries = manifest(run, w)?;
    let changes = sealed_changes(run, reference)?;
    for change in changes {
        let expected = if cumulative {
            base_version(entries, &change.path)
        } else {
            w.overlay
                .iter()
                .find(|f| f.path == change.path)
                .map(|f| f.after.clone())
                .or_else(|| base_version(entries, &change.path))
        };
        if !within(&change.path, &w.pins.request.lease) || expected.as_ref() != Some(&change.before)
        {
            return Err(ReplayError::InvalidTransition);
        }
        if let (
            WorkerBaseVersion::File { executable: a, .. },
            WorkerBaseVersion::File { executable: b, .. },
        ) = (&change.before, &change.after)
            && a != b
        {
            return Err(ReplayError::InvalidTransition);
        }
    }
    let mut overlay = if cumulative {
        Vec::new()
    } else {
        w.overlay.clone()
    };
    for change in changes {
        if let Some(old) = overlay.iter_mut().find(|f| f.path == change.path) {
            old.after = change.after.clone();
        } else {
            overlay.push(change.clone());
        }
    }
    let policy = run
        .config
        .as_ref()
        .and_then(|c| c.worker_policy.as_ref())
        .ok_or(ReplayError::InvalidTransition)?;
    if retained_image(entries, &[]) + retained_image(entries, &overlay) > policy.retained_bytes() {
        return Err(ReplayError::InvalidTransition);
    }
    if cumulative {
        let nonempty: Vec<_> = w
            .overlay
            .iter()
            .filter(|f| f.before != f.after)
            .cloned()
            .collect();
        if nonempty != changes {
            return Err(ReplayError::InvalidTransition);
        }
    }
    Ok(changes.to_vec())
}

pub(crate) fn coordination_ready(run: &RunView) -> bool {
    let admitted = run.agents.len().saturating_sub(1)
        + run.tools.iter().filter(|t| refused_capture(run, t)).count();
    run.primary().into_iter().flat_map(|a| &a.provider_calls).filter_map(|r| r.delegation.as_ref()).flatten().skip(admitted).all(|p| matches!(p, ChildAssignment::Editor { assignment } if assignment.mode == WorkerMode::Background))
    && run.agents.iter().skip(1).all(|a| {
        a.assignment
            .as_ref()
            .map_or(a.status == AgentStatus::Finished, |w| {
                a.status != AgentStatus::Active && w.released.is_some()
                    || w.pins.request.mode == WorkerMode::Background
            })
    })
}

pub(crate) fn wave_settled(run: &RunView) -> bool {
    let proposed: usize = run
        .primary()
        .into_iter()
        .flat_map(|a| &a.provider_calls)
        .filter_map(|r| r.delegation.as_ref())
        .map(Vec::len)
        .sum();
    proposed
        == run.agents.len().saturating_sub(1)
            + run.tools.iter().filter(|t| refused_capture(run, t)).count()
        && run.agents.iter().skip(1).all(|a| {
            a.assignment
                .as_ref()
                .map_or(a.status == AgentStatus::Finished, |w| {
                    a.status != AgentStatus::Active && w.settled()
                })
        })
}
pub(super) fn child_terminal(a: &AgentView) -> bool {
    a.assignment
        .as_ref()
        .map_or(a.status == AgentStatus::Finished, |w| {
            a.status != AgentStatus::Active && w.settled()
        })
}
fn unspawned_capture(run: &RunView, tool: &ToolView) -> bool {
    matches!(&tool.intent.call, ToolCall::PrepareWorker { preparation: p }
        if p.purpose == WorkerDataPurpose::Base && !run.agents.iter().any(|a| a.id == p.assignment_id))
}

// A refused base capture spends its proposal slot without spawning; the other proposals proceed.
fn refused_capture(run: &RunView, tool: &ToolView) -> bool {
    unspawned_capture(run, tool)
        && tool
            .observation
            .as_ref()
            .is_some_and(|o| o.disposition != ToolDisposition::Succeeded)
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
        .count()
        + run.tools.iter().filter(|t| refused_capture(run, t)).count();
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

// The one admission of an editor proposal, at delegation and again when its assignment spawns.
fn validate_request(
    view: &SessionView,
    run: &RunView,
    request: &EditorAssignment,
    spawning: Option<AgentRunId>,
) -> Result<(), ReplayError> {
    let config = run.config.as_ref().ok_or(ReplayError::InvalidTransition)?;
    let policy = config
        .worker_policy
        .as_ref()
        .ok_or(ReplayError::InvalidTransition)?;
    let goal = active_run_goal(view, run.id).ok_or(ReplayError::InvalidTransition)?;
    let plan = goal
        .plan_contract
        .as_ref()
        .ok_or(ReplayError::InvalidTransition)?;
    let registry = super::definition_registry(run)?;
    let definition = registry
        .get(&request.definition)
        .ok_or(ReplayError::InvalidTransition)?;
    if goal.phase != GoalPhase::Implementation
        || goal.awaiting_plan_contract
        || !definition.editor()
        || definition.background && request.mode != WorkerMode::Background
        || request.definition_digest
            != crate::validate::digest(
                &serde_json::to_vec(definition).map_err(|_| ReplayError::InvalidPayload)?,
            )
        || request.max_turns > definition.max_turns
        || request.max_turns > policy.max_turns
        || !valid_commands(&request.commands)
        || request.commands.iter().any(|c| {
            !policy.parent_grant.commands.contains(c)
                || !plan.commands.contains(c)
                || !definition.allows_worker_tool(WorkerTool::Command)
        })
        || request
            .lease
            .iter()
            .any(|p| !within(p, &plan.paths) || !within(p, &policy.parent_grant.write_paths))
        || ![
            WorkerTool::Write,
            WorkerTool::Edit,
            WorkerTool::MultiEdit,
            WorkerTool::Delete,
            WorkerTool::Move,
        ]
        .iter()
        .any(|op| policy.parent_grant.tools.contains(op) && definition.allows_worker_tool(*op))
        || match &request.criterion {
            GoalSuccessCriterion::ToolCallSucceeded { call } => {
                !plan.allows_call(call)
                    || matches!(call.as_ref(), ToolCall::Mkdir { .. })
                    || !policy
                        .parent_grant
                        .clone()
                        .narrowed_for_editor(request, &plan.commands, |tool| {
                            definition.allows_worker_tool(tool)
                        })
                        .allows(call)
                    || matches!(call.as_ref(), ToolCall::Command { program, .. } if !request.commands.contains(program))
            }
            GoalSuccessCriterion::WorkspaceFileContent { path, .. }
            | GoalSuccessCriterion::WorkspaceFileDigest { path, .. } => {
                !within(path, &plan.paths) || !within(path, &request.lease)
            }
        }
    {
        return Err(ReplayError::InvalidTransition);
    }
    for source in request.resumes.iter().chain(request.supersedes.iter()) {
        if Some(*source) == spawning || view.worker_resume_source(goal.id, *source).is_none() {
            return Err(ReplayError::InvalidTransition);
        }
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
            validate_request(view, run, &p.request, Some(id))?;
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
                || p.cache_generation == Some([0; 32])
                || p.grant.commands != p.request.commands
                || !p.grant.read_paths.iter().all(|l| within(l, &plan.paths))
                || p.policy_digest != receipt.config_digest
                || p.permissions_digest != receipt.permissions_digest
                || p.data_bytes > policy.data_bytes
                || p.data_bytes == 0
                || p.base.assignment_id != id
                || p.base.purpose != WorkerDataPurpose::Base
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
            if p.base.bytes > p.data_bytes {
                return Err(ReplayError::InvalidTransition);
            }
            for op in &p.grant.tools {
                if !def.allows_worker_tool(*op) {
                    return Err(ReplayError::InvalidTransition);
                }
            }
            let sealed = run.workers.sealed(&p.base)?;
            if let WorkerDataFacts::Manifest(entries) = &sealed.facts {
                if retained_image(entries, &[]) * 2 > policy.retained_bytes() {
                    return Err(ReplayError::InvalidTransition);
                }
                if entries
                    .iter()
                    .any(|e| !within(&e.path, &p.grant.read_paths))
                {
                    return Err(ReplayError::InvalidTransition);
                }
            } else {
                return Err(ReplayError::InvalidTransition);
            }

            if !run.tools.last().is_some_and(|t| {
                t.intent.id == sealed.tool
                    && t.observation.as_ref().is_some_and(|o| {
                        o.disposition == ToolDisposition::Succeeded && o.attested_for(&t.intent)
                    })
            }) {
                return Err(ReplayError::InvalidTransition);
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
    let target = EffectTarget::WorkerView {
        cache_generation: w.pins.cache_generation,
        assignment_id: id,
        base_digest: w.pins.base.sha256,
        revision: w.revision,
    };
    if intent.target != target
        || w.outcome.is_some()
        || run.workers.stop.is_some()
        || !quiescent(run)
        || intent.policy_digest != w.pins.policy_digest
        || a.status != AgentStatus::Active
        || a.tools
            .iter()
            .any(|t| t.intent.id == intent.id || private_failure(t))
    {
        return false;
    }
    if let ToolCall::PrepareWorker { preparation } = &intent.call {
        return preparation.assignment_id == id
            && preparation.purpose == WorkerDataPurpose::Handover
            && preparation.max_bytes <= w.pins.data_bytes
            && a.provider_calls
                .last()
                .is_some_and(|r| r.disposition == ProviderCallDisposition::Finished)
            && !a
                .tools
                .iter()
                .any(|t| matches!(t.intent.call, ToolCall::PrepareWorker { .. }));
    }
    a.provider_calls
        .last()
        .is_some_and(|r| r.disposition == ProviderCallDisposition::ToolRequested)
        && super::agent::child_tool_requests(a)
            == a.tools
                .iter()
                .filter(|t| !matches!(t.intent.call, ToolCall::PrepareWorker { .. }))
                .count()
                + 1
        && w.pins.grant.allows(&intent.call)
}

pub(super) fn base_preparation(run: &RunView, preparation: &WorkerPreparation) -> bool {
    run.workers.stop.is_none()
        && preparation.purpose == WorkerDataPurpose::Base
        && quiescent(run)
        && !run
            .tools
            .iter()
            .any(|t| unspawned_capture(run, t) && !refused_capture(run, t))
        && !run.agents.iter().any(|a| a.id == preparation.assignment_id)
        && !run
            .workers
            .data
            .iter()
            .any(|d| d.reference.assignment_id == preparation.assignment_id)
        && matches!(
            u8::try_from(run.agents.len())
                .ok()
                .and_then(|o| proposal(run, o).ok()),
            Some(ChildAssignment::Editor { .. })
        )
}

// A publication that is started or observed but not yet recorded must settle before its lease can.
fn integration_in_flight(run: &RunView, assignment_id: AgentRunId) -> bool {
    run.tools.iter().any(|t| {
        matches!(
            &t.intent.call,
            ToolCall::ApplyChangeset {
                source: ChangesetSource::WorkerHandover { assignment_id: id, .. }
            } if *id == assignment_id
        )
    }) && worker(run, assignment_id).is_ok_and(|w| w.integration.is_none())
}

fn integration_started(run: &RunView, sequence: u64) -> bool {
    run.tools.iter().any(|t| {
        matches!(&t.intent.call, ToolCall::ApplyChangeset {
            source: ChangesetSource::WorkerHandover { handover_sequence, .. }
        } if *handover_sequence == sequence)
    })
}

// The newest Tool is an observed integration whose record has not been committed yet.
fn unrecorded_integration(run: &RunView) -> bool {
    run.tools.last().is_some_and(|t| {
        t.observation.is_some()
            && matches!(&t.intent.call, ToolCall::ApplyChangeset {
                source: ChangesetSource::WorkerHandover { assignment_id, .. }
            } if worker(run, *assignment_id).is_ok_and(|w| w.integration.is_none()))
    })
}

fn starts_team_work(event: &Event) -> bool {
    matches!(
        event,
        Event::ToolStarted { .. }
            | Event::GoalProviderCallReserved { .. }
            | Event::AgentSpawned { .. }
            | Event::WorkerHandoverRecorded { .. }
    )
}

pub(super) fn integration_source(run: &RunView, source: &ChangesetSource) -> bool {
    let ChangesetSource::WorkerHandover {
        assignment_id,
        handover_sequence,
        data,
    } = source
    else {
        return false;
    };
    let Ok(w) = worker(run, *assignment_id) else {
        return false;
    };
    run.workers.stop.is_none()
        && quiescent(run)
        && !integration_started(run, *handover_sequence)
        && w.handover
            .as_ref()
            .is_some_and(|(s, h)| s == handover_sequence && &h.data == data)
        && w.integration.is_none()
        && w.discarded.is_none()
        && w.released.is_none()
        && run
            .agents
            .iter()
            .filter_map(|a| a.assignment.as_ref())
            .filter(|w| w.integration.is_none() && w.discarded.is_none())
            .filter_map(|w| w.handover.as_ref().map(|(s, _)| *s))
            .min()
            == Some(*handover_sequence)
}

pub(super) fn before_event(view: &SessionView, event: &Event) -> Result<(), ReplayError> {
    let Some(run) = view.runs.last().filter(|r| r.status == RunStatus::Active) else {
        return Ok(());
    };
    let (scope, actor) = event.scope();
    if scope != Some(run.id) {
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
        Event::ToolStarted {
            agent_run_id,
            intent,
            ..
        } => before::tool_started(run, agent_run_id, intent)?,
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
        Event::ToolFinished {
            agent_run_id,
            observation: o,
            ..
        } => {
            if o.intent.target == EffectTarget::Workspace
                && (o.intent.call.mutates() || o.intent.call.is_process())
            {
                for a in &mut run.agents {
                    if let Some(w) = &mut a.assignment {
                        w.verification = None;
                    }
                }
            }
            if o.disposition != ToolDisposition::Succeeded {
                run.workers.pending = None;
            }
            record_command_capture(run, o)?;
            if let Some(Attestation::WorkerMutation { mutation, .. }) = &o.attestation {
                let changes = sealed_changes(run, &mutation.data)?.to_vec();
                let w = worker_mut(run, *agent_run_id)?;
                w.revision = mutation.next_revision;
                for change in changes {
                    if let Some(prior) = w.overlay.iter_mut().find(|f| f.path == change.path) {
                        prior.after = change.after;
                    } else {
                        w.overlay.push(change);
                    }
                }
                w.overlay.sort_by(|a, b| a.path.cmp(&b.path));
                if w.overlay.len() > WORKER_FILES {
                    return Err(ReplayError::InvalidTransition);
                }
                for data in run.workers.data.iter_mut().filter(|d| {
                    d.reference.assignment_id == *agent_run_id
                        && d.reference.purpose == WorkerDataPurpose::Mutation
                        && d.reference != mutation.data
                }) {
                    data.facts = WorkerDataFacts::Changes(Vec::new());
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
            run.workers.clear_pending();
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
        Event::WorkerDataChunk { record, .. } => apply::data_chunk(run, record, actor, sequence)?,
        Event::WorkerDataSealed { record, .. } => apply::data_sealed(run, record, actor, sequence)?,
        Event::WorkerHandoverRecorded { record, .. } => {
            apply::handover(run, record, actor, sequence)?
        }
        Event::WorkerIntegrationRecorded { record, .. } => {
            apply::integration(run, record, actor, sequence)?
        }
        Event::WorkerHandoverDiscarded { record, .. } => {
            apply::discard(run, record, actor, sequence)?
        }
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
    let mut state = contract.initial_success_state();
    for t in &run.tools {
        let Some(seq) = t.finished_sequence.filter(|s| *s > released) else {
            continue;
        };
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

fn valid_discard(run: &RunView, w: &WorkerView, d: &WorkerSettlement) -> bool {
    match d.reason {
        WorkerSettlementReason::Stopped => {
            run.workers.stop.is_some_and(|(s, _)| s == d.cause_sequence)
        }
        WorkerSettlementReason::Conflict | WorkerSettlementReason::Denied => {
            w.integration.as_ref().is_some_and(|(s, i)| {
                *s == d.cause_sequence
                    && matches!(
                        (d.reason, i.outcome),
                        (
                            WorkerSettlementReason::Conflict,
                            WorkerIntegrationOutcome::Conflict
                        ) | (
                            WorkerSettlementReason::Denied,
                            WorkerIntegrationOutcome::Denied
                        )
                    )
            })
        }
        WorkerSettlementReason::Integrated => false,
        reason => w.outcome.as_ref().is_some_and(|(s, o)| {
            *s == d.cause_sequence
                && matches!(
                    (reason, o.disposition),
                    (
                        WorkerSettlementReason::Failed,
                        WorkerDisposition::Failed | WorkerDisposition::Lost
                    ) | (
                        WorkerSettlementReason::Cancelled,
                        WorkerDisposition::Cancelled
                    ) | (
                        WorkerSettlementReason::PrivateUncertain,
                        WorkerDisposition::PrivateUncertain
                    ) | (
                        WorkerSettlementReason::NoChange,
                        WorkerDisposition::NoChange
                    )
                )
        }),
    }
}

pub(crate) fn private_failure(t: &ToolView) -> bool {
    let Some(o) = t.observation.as_ref() else {
        return false;
    };
    let command = matches!(o.intent.call, ToolCall::Command { .. });
    let recoverable_refusal = recoverable_private_refusal(t);
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

fn recoverable_private_refusal(t: &ToolView) -> bool {
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

fn record_command_capture(
    run: &mut RunView,
    observation: &crate::tools::ToolObservation,
) -> Result<(), ReplayError> {
    if !matches!(observation.intent.target, EffectTarget::WorkerView { .. })
        || !matches!(observation.intent.call, ToolCall::Command { .. })
        || observation.disposition != ToolDisposition::Succeeded
    {
        return Ok(());
    }
    let Some(WorkerCommandResult {
        private_capture: PrivateCapture::Sealed { data },
        ..
    }) = WorkerCommandResult::parse(&observation.output)
    else {
        return Ok(());
    };
    let capture = (observation.intent.agent_run_id, observation.intent.id, data);
    if run
        .workers
        .data
        .iter()
        .any(|sealed| sealed.reference == capture.2)
    {
        apply_command_capture(run, &capture)?;
    } else {
        run.workers.capture = Some(capture);
    }
    Ok(())
}

fn apply_command_capture(run: &mut RunView, capture: &CommandCapture) -> Result<(), ReplayError> {
    let (actor, _, data) = capture;
    let changes = valid_delta(run, *actor, data, false)?;
    if changes.is_empty() {
        return Err(ReplayError::InvalidTransition);
    }
    let w = worker_mut(run, *actor)?;
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
    Ok(())
}

pub(crate) fn criterion_unmet_in_view(run: &RunView, id: AgentRunId) -> Result<bool, ReplayError> {
    Ok(private_criterion_evidence(run, id)?.is_empty())
}

pub(crate) fn private_criterion_evidence(
    run: &RunView,
    id: AgentRunId,
) -> Result<Vec<u64>, ReplayError> {
    let w = worker(run, id)?;
    let a = run
        .agents
        .iter()
        .find(|a| a.id == id)
        .ok_or(ReplayError::InvalidTransition)?;
    if run.workers.capture.is_some() {
        return Ok(Vec::new());
    }
    for t in a.tools.iter().rev() {
        let Some(o) = t.observation.as_ref() else {
            return Ok(Vec::new());
        };
        let EffectTarget::WorkerView { revision, .. } = o.intent.target else {
            continue;
        };
        let effective_revision = match &o.attestation {
            Some(Attestation::WorkerMutation { mutation, .. }) => mutation.next_revision,
            Some(Attestation::WorkerCommand { .. }) => {
                let result: WorkerCommandResult =
                    serde_json::from_str(&o.output).map_err(|_| ReplayError::InvalidTransition)?;
                revision
                    + u32::from(matches!(
                        result.private_capture,
                        PrivateCapture::Sealed { .. }
                    ))
            }
            _ => revision,
        };
        if effective_revision != w.revision {
            continue;
        }
        if o.disposition == ToolDisposition::Succeeded && o.attested_for(&o.intent) {
            let passed = match &w.pins.request.criterion {
                GoalSuccessCriterion::ToolCallSucceeded { call } if o.intent.call == **call => {
                    if matches!(o.intent.call, ToolCall::Command { .. }) {
                        WorkerCommandResult::parse(&o.output).is_some_and(|r| {
                            r.exit_code == Some(0)
                                && !matches!(r.private_capture, PrivateCapture::Rejected { .. })
                        })
                    } else {
                        true
                    }
                }
                GoalSuccessCriterion::WorkspaceFileDigest { path, sha256 } => {
                    private_file_digest(run, w, o, path).as_ref() == Some(sha256)
                }
                GoalSuccessCriterion::WorkspaceFileContent { path, content } => {
                    private_file_digest(run, w, o, path).as_ref()
                        == Some(&crate::validate::hex_digest(content.as_bytes()))
                }
                _ => false,
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

fn private_file_digest(
    run: &RunView,
    w: &WorkerView,
    o: &crate::tools::ToolObservation,
    path: &str,
) -> Option<String> {
    if matches!(&o.intent.call, ToolCall::PrepareWorker { preparation } if preparation.purpose == WorkerDataPurpose::Handover)
        || matches!(o.attestation, Some(Attestation::WorkerMutation { .. }))
        || matches!(o.attestation, Some(Attestation::WorkerCommand { .. }))
            && WorkerCommandResult::parse(&o.output).is_some_and(|r| {
                r.exit_code == Some(0) && matches!(r.private_capture, PrivateCapture::Sealed { .. })
            })
    {
        let version = w
            .overlay
            .iter()
            .find(|f| f.path == path)
            .map(|f| f.after.clone())
            .or_else(|| base_version(manifest(run, w).ok()?, path))?;
        return match version {
            WorkerBaseVersion::File { sha256, .. } => Some(sha256),
            _ => None,
        };
    }
    if !matches!(&o.intent.call, ToolCall::Read { path: p, .. } if p == path) {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(&o.output)
        .ok()?
        .get("sha256")?
        .as_str()
        .map(str::to_owned)
}

mod call;
mod decide;
mod editor;
mod primary;
mod researcher;
mod settle;
mod spawn;
mod tool;
mod wave;

use super::stop::{PrimaryEnd, RunFacts, RunProgress, RunRefusal, StepEnd, StopCause};
use super::worker::{WorkerAdmission, WorkerCompletion, WorkerEnd};
use super::{
    Action, ActiveSegment, Admission, CallFailure, CallResult, Grant, HookRefusal, LifecycleHook,
    PrimaryDecision, PrimarySite, RunLoop, call_record, decide_primary_call, primary_admitted,
    valid_answer, wave_deferred,
};
use crate::engine::EngineError;
use crate::engine::ledger::{
    ActiveDeadline, ActiveDeadlineCause, ActiveReservationLimit, duration_ms,
};
use crate::engine::routing::{CallScope, decide_attempt};
use crate::provider::{
    AgentPhase, Answer, ChildAssignment, ChildResult, DEFAULT_OUTPUT_TOKEN_CAP,
    MAX_OUTPUT_TOKEN_CAP, PromptRole, ProviderFailureReason, ProviderOutcome, ProviderRequest,
    ProviderResponse, ProviderTurn, RunInput,
};
use crate::session::{
    AgentDisposition, AgentRole, AgentRunId, ChildGrant, CollaborationPolicy, Event,
    MAX_CHILD_RESULT_BYTES, MAX_CHILD_TOOL_CALLS, MAX_CHILD_TURNS,
};
use crate::tools::{ToolCall, ToolContext, ToolRequest, ToolStep};
use crate::worker::WorkerCause;
use std::collections::{HashMap, VecDeque};
use std::ops::ControlFlow;
use std::time::Duration;
use tokio::task::{Id, JoinError, JoinSet};
use tokio::time::Instant;

const MAX_CHILD_TOOL_FACTS: usize = 8;
const CANCELLATION_DRAIN: Duration = Duration::from_secs(2);
pub(crate) const MAX_DELEGATION_DEADLINE: Duration = Duration::from_secs(20 * 60);
const CHILD_INTENT_RESERVE_BYTES: usize = 4 * 1024;
// Keep enough room for a full Read page and its metadata before offering child Tools.
const MIN_CHILD_RESULT_BYTES: usize = crate::tools::types::MAX_TOOL_RESULT_BYTES;

#[derive(Clone, Copy)]
pub(super) enum ChildKind {
    Researcher,
    Editor,
}

impl ChildKind {
    pub(super) fn is_editor(self) -> bool {
        match self {
            Self::Researcher => false,
            Self::Editor => true,
        }
    }
}

pub(super) struct ChildWork {
    pub(super) agent_run_id: AgentRunId,
    pub(super) objective: String,
    pub(super) definition: Option<crate::tools::definitions::AgentDefinition>,
    pub(super) turns_remaining: u8,
    pub(super) kind: ChildKind,
    pub(super) source_context: Option<ChildResult>,
    pub(super) steering_notes: Vec<String>,
    pub(super) stop_hook_feedback: Vec<crate::provider::StopHookFeedback>,
    pub(super) tools: Option<ToolContext>,
    pub(super) tool_calls_remaining: usize,
    pub(super) active_deadline: Option<ActiveDeadline>,
}

impl ChildWork {
    fn hook_subject(&self) -> &str {
        self.definition.as_ref().map_or(
            if self.kind.is_editor() {
                "editor"
            } else {
                "researcher"
            },
            |definition| definition.name.as_str(),
        )
    }
}

struct Spawned {
    children: Vec<ChildWork>,
    parallel: usize,
    active_segment: Option<ActiveSegment>,
    bound: Duration,
}

struct Drained {
    abort: Option<WaveStop>,
    pending: VecDeque<usize>,
}

struct DelegationSettlement {
    children: Vec<ChildWork>,
    active_segment: Option<ActiveSegment>,
    drained: Drained,
    primary_step: Option<(Vec<ToolRequest>, ProviderTurn)>,
}

struct ChildQueue {
    pending: VecDeque<usize>,
    calls: JoinSet<crate::engine::routing::AttemptResult>,
    active: HashMap<Id, ActiveChild>,
    parallel: usize,
    unspawned: VecDeque<ChildAssignment>,
    definition: Option<String>,
    tool_allowance: usize,
    primary_step: Option<(Vec<ToolRequest>, ProviderTurn)>,
    primary_pending: bool,
    // A deferred wave primary proposal waits until more agents settle than the count.
    deferred: Option<(RepromptReason, usize)>,
    // Once set, the wave starts no call, spawn or primary turn; it ends as a budget stop.
    stop: Option<StopCause>,
    capacity_error: Option<EngineError>,
    active_deadline: Option<ActiveDeadline>,
}

struct ChildWave<'a> {
    children: &'a mut Vec<ChildWork>,
    root_request: &'a mut ProviderRequest,
    queue: &'a mut ChildQueue,
    segments: &'a mut Option<ActiveSegment>,
    deadline: Instant,
}

impl ChildWave<'_> {
    fn deadline(&self) -> Instant {
        self.queue
            .active_deadline
            .as_ref()
            .map_or(self.deadline, |deadline| deadline.wall_deadline())
    }
}

#[must_use]
enum ChildProgress {
    Refilled,
    Settled(Option<AgentDisposition>),
}

pub(super) enum DelegationProgress {
    Continue,
    Settled,
}

enum ChildSlot {
    Ready(usize),
    Retry,
    Stopped,
}

enum ResearchSpawn {
    Ready,
    Cancelled,
}

enum SlotDispatch {
    Started,
    Stopped,
}

struct ChildCall {
    result: CallResult,
    limits: Option<crate::provider::RateLimits>,
    valid_response: bool,
}

type ChildAttempt = ControlFlow<CompletedChildAttempt>;

struct CompletedChildAttempt {
    index: Option<usize>,
    active: ActiveChild,
    result: crate::engine::routing::AttemptResult,
}

struct EditorCompletion {
    answer: Option<Answer>,
    failure: WorkerCause,
    step_stop: Option<StopCause>,
}

struct ActiveChild {
    limits: crate::provider::RateLimitSlot,
    index: Option<usize>,
    request: ProviderRequest,
}

// A boundary stop lets in-flight work settle on its own terms; an abort tears the wave down.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WaveStop {
    Boundary(StopCause),
    Abort(StopCause),
}

impl WaveStop {
    const BUDGET: Self = Self::Boundary(StopCause::Budget);

    const fn cause(self) -> StopCause {
        match self {
            Self::Boundary(cause) | Self::Abort(cause) => cause,
        }
    }

    const fn from_queue(cause: StopCause) -> Self {
        match cause {
            StopCause::ToolRefused(_) | StopCause::Uncertain => Self::Abort(cause),
            StopCause::ContextFull
            | StopCause::Cancelled
            | StopCause::HookStop
            | StopCause::ActiveTime
            | StopCause::DelegationDeadline
            | StopCause::Capacity => Self::Boundary(cause),
            StopCause::Budget
            | StopCause::Paused
            | StopCause::Refused(_)
            | StopCause::ToolStopped
            | StopCause::InvalidCommandResult => Self::BUDGET,
        }
    }

    // A budget boundary lets workers end on their budget cause; every other stop cancels them.
    pub(super) const fn tears_down(self) -> bool {
        !matches!(
            self,
            Self::Boundary(StopCause::Budget | StopCause::Capacity | StopCause::ActiveTime)
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RepromptReason {
    AnswerDeferred,
    DelegateRefused,
    ToolRefused,
    CriterionNotMetInView,
}

impl RepromptReason {
    // Why a wave primary response waits for its workers.
    fn deferral(result: &CallResult) -> Option<Self> {
        let CallResult::Response(response) = result else {
            return None;
        };
        Some(match response.outcome {
            ProviderOutcome::Answer(_) => Self::AnswerDeferred,
            ProviderOutcome::Delegate(_) => Self::DelegateRefused,
            ProviderOutcome::Plan(_) | ProviderOutcome::Tools(_) | ProviderOutcome::Review(_) => {
                Self::ToolRefused
            }
        })
    }

    const fn failure_reason(self) -> ProviderFailureReason {
        match self {
            Self::AnswerDeferred => ProviderFailureReason::WorkerAnswerDeferred,
            Self::DelegateRefused => ProviderFailureReason::WorkerDelegateRefused,
            Self::ToolRefused => ProviderFailureReason::WorkerToolRefused,
            Self::CriterionNotMetInView => ProviderFailureReason::CriterionNotMetInView,
        }
    }

    const fn text(self) -> &'static str {
        match self {
            Self::AnswerDeferred => {
                "Answer deferred: workers are still active; answer again after they settle."
            }
            Self::DelegateRefused => "Delegate refused: workers are still active.",
            Self::ToolRefused => {
                "Tool request refused: it was malformed or edits inside a live worker lease."
            }
            Self::CriterionNotMetInView => {
                "Answer refused: criterion_not_met_in_view. Obtain current passing Workspace Tool evidence before answering."
            }
        }
    }
}

// Hook state of the Run, read once per decision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct HookFacts {
    pub(super) uncertain: bool,
    // A settled hook journaled `stopped`.
    pub(super) stop_requested: bool,
    pub(super) settled: bool,
}

impl HookFacts {
    fn stop(self) -> Option<WaveStop> {
        if self.uncertain {
            Some(WaveStop::Abort(StopCause::Uncertain))
        } else if self.stop_requested {
            Some(WaveStop::Boundary(StopCause::HookStop))
        } else if !self.settled {
            Some(WaveStop::Abort(StopCause::ToolStopped))
        } else {
            None
        }
    }
}

// Observations read once after a child's call record or SubagentStop run commits.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ChildFacts {
    pub(super) has_tools: bool,
    pub(super) refusal_limit_reached: bool,
    pub(super) turns_left: bool,
    pub(super) tool_calls_left: bool,
    pub(super) before_deadline: bool,
    pub(super) cancelled: bool,
    pub(super) paused: bool,
    pub(super) tokens_open: bool,
    pub(super) calls_left: bool,
    pub(super) hook: HookFacts,
    // A settled hook stop requested by this child.
    pub(super) actor_hook_stop: bool,
}

// The in-view criterion gate of an editor's call.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct GateFacts {
    // An otherwise valid answer failed the gate.
    pub(super) unmet: bool,
    pub(super) worker_stopped: bool,
    pub(super) stop_room: bool,
    pub(super) continuations_left: bool,
}

#[must_use]
#[derive(Debug)]
pub(super) enum ChildDecision {
    ToolStep(Vec<ToolRequest>, ProviderTurn),
    RefusedTools(Vec<ToolRequest>, ProviderTurn),
    // `stop_hook`: the shell runs SubagentStop, then `decide_child_stop`.
    Finish {
        answer: Answer,
        turn: ProviderTurn,
        stop_hook: bool,
    },
    Reprompt(RepromptReason, ProviderTurn),
    End(ChildEnd),
}

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChildEnd {
    // Stronger stop facts take precedence over the editor's own failure cause.
    Editor(WorkerCause),
    // A researcher ends while its call reservation is held.
    Researcher(AgentDisposition),
    // A researcher's Tool request is released unrun; `budget` once the Goal budget refused it.
    Skipped {
        disposition: AgentDisposition,
        budget: bool,
    },
}

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StopHookRun {
    Passed,
    Blocked,
    Cancelled,
    CutOffByActiveTime,
    CutOffByContext,
    Uncertain,
}

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChildStop {
    Proceed,
    Reprompt,
    // `stops_wave`: the wave stops for budget unless it is already stopping.
    End {
        disposition: AgentDisposition,
        cause: WorkerCause,
        stops_wave: bool,
    },
}

impl ChildQueue {
    fn new(count: usize, parallel: usize) -> Self {
        Self {
            pending: (0..count).collect(),
            calls: JoinSet::new(),
            active: HashMap::new(),
            parallel,
            unspawned: VecDeque::new(),
            definition: None,
            tool_allowance: 0,
            primary_step: None,
            primary_pending: false,
            deferred: None,
            stop: None,
            capacity_error: None,
            active_deadline: None,
        }
    }

    // Preserve the original abort cause if draining also fails.
    async fn abort_with<T>(&mut self, outcome: T) -> T {
        self.abort_and_drain().await;
        outcome
    }

    async fn abort_and_drain(&mut self) {
        let _ = self
            .try_abort_and_drain()
            .await
            .inspect_err(crate::diagnostics::engine_failure);
    }

    fn begin_drain(&mut self) -> Instant {
        self.calls.abort_all();
        Instant::now() + CANCELLATION_DRAIN
    }

    async fn try_abort_and_drain(&mut self) -> Result<(), EngineError> {
        let deadline = self.begin_drain();
        while !self.calls.is_empty() {
            let _ = tokio::time::timeout_at(deadline, self.calls.join_next_with_id())
                .await
                .map_err(|_| EngineError::CancellationDrainTimeout)?
                .ok_or(EngineError::CoordinatorFailed)?;
        }
        self.active.clear();
        Ok(())
    }
}

impl RunLoop<'_> {
    pub(super) fn well_formed_delegation(
        &self,
        children: &[ChildAssignment],
        agent: Option<&str>,
    ) -> Result<Result<(), super::ProposalRefusalCause>, EngineError> {
        if children.iter().any(|child| child.editor.is_some()) {
            return self.editor_delegation(children, agent);
        }
        let valid = {
            self.wave_settled()?
                && !matches!(self.controls.policy, CollaborationPolicy::Single)
                && crate::provider::valid_assignments(children, agent)
                && self.ledger().view(|v| {
                    v.runs.last().is_some_and(|r| {
                        let attempts: usize = r
                            .primary()
                            .into_iter()
                            .flat_map(|a| &a.provider_calls)
                            .filter_map(|c| c.delegation.as_ref())
                            .map(Vec::len)
                            .sum();
                        attempts + children.len()
                            <= usize::from(self.controls.policy.max_children())
                    })
                })?
                && children
                    .iter()
                    .all(|a| self.valid_research_assignment(a, agent))
        };
        Ok(if valid {
            Ok(())
        } else {
            Err(super::ProposalRefusalCause::DelegateInvalid)
        })
    }

    pub(super) fn valid_research_assignment(
        &self,
        assignment: &ChildAssignment,
        agent: Option<&str>,
    ) -> bool {
        agent.is_none_or(|name| {
            self.definitions
                .get()
                .and_then(|r| r.get(name))
                .is_some_and(|d| !d.editor())
        }) && assignment.profile.as_ref().is_none_or(|name| {
            self.routing
                .providers
                .roster
                .as_ref()
                .is_some_and(|r| r.snapshot.child_profiles.contains(name))
        })
    }

    pub(super) fn run_children<'a>(
        &'a self,
        root_request: &'a mut ProviderRequest,
        objectives: Vec<ChildAssignment>,
        agent: Option<String>,
        delegate_turn: ProviderTurn,
    ) -> super::step::BoxedStep<'a, RunProgress<DelegationProgress>> {
        Box::pin(self.run_children_inner(root_request, objectives, agent, delegate_turn))
    }

    async fn run_children_inner(
        &self,
        root_request: &mut ProviderRequest,
        objectives: Vec<ChildAssignment>,
        agent: Option<String>,
        delegate_turn: ProviderTurn,
    ) -> Result<RunProgress<DelegationProgress>, EngineError> {
        let count = objectives.len();
        let Some(Spawned {
            mut children,
            parallel,
            active_segment: mut segment,
            bound,
        }) = self
            .spawn_children(count, objectives.iter().any(|a| a.editor.is_some()))
            .await?
        else {
            return self
                .settle_primary(PrimaryEnd::Stopped(StopCause::Budget))
                .await
                .map(|()| RunProgress::Ready(DelegationProgress::Settled));
        };
        let segment_ref = segment.as_ref().expect("delegation segment");
        let deadline = segment_ref.deadline();
        let (allowance_ms, cause) = active_deadline_allowance(segment_ref, bound, Instant::now());
        let active_deadline = self.ledger().active_deadline(allowance_ms, cause);
        let mut queue = self.child_queue(objectives, agent, parallel);
        queue.active_deadline = Some(active_deadline);
        if queue.unspawned.iter().all(|a| a.editor.is_none()) {
            match self
                .spawn_researchers(
                    &mut queue,
                    &mut children,
                    root_request,
                    &mut segment,
                    deadline,
                )
                .await?
            {
                RunProgress::Ready(ResearchSpawn::Ready) => {}
                RunProgress::Stopped(cause) => return Ok(RunProgress::Stopped(cause)),
                RunProgress::Ready(ResearchSpawn::Cancelled) => {
                    return self
                        .settle_primary(PrimaryEnd::Stopped(StopCause::Cancelled))
                        .await
                        .map(|()| RunProgress::Ready(DelegationProgress::Settled));
                }
            }
        }
        Self::append_delegation_turn(root_request, &delegate_turn);
        let drained = match self
            .drive_children(
                &mut children,
                root_request,
                &mut queue,
                &mut segment,
                deadline,
            )
            .await
        {
            Ok(RunProgress::Ready(drained)) => drained,
            Ok(RunProgress::Stopped(cause)) => {
                return Ok(queue.abort_with(RunProgress::Stopped(cause)).await);
            }
            Err(error) => return Err(queue.abort_with(error).await),
        };
        self.finish_children(
            root_request,
            DelegationSettlement {
                children,
                active_segment: segment,
                drained,
                primary_step: queue.primary_step.take(),
            },
            queue.capacity_error,
        )
        .await
    }

    async fn finish_children(
        &self,
        root_request: &mut ProviderRequest,
        settlement: DelegationSettlement,
        capacity_error: Option<EngineError>,
    ) -> Result<RunProgress<DelegationProgress>, EngineError> {
        let progress = self.settle_delegation(root_request, settlement).await?;
        let progress = match progress {
            Admission::Reserved(progress) => progress,
            Admission::Exhausted => return Ok(RunProgress::Stopped(RunRefusal::Budget)),
        };
        match capacity_error {
            Some(error) => Err(error),
            None => Ok(RunProgress::Ready(progress)),
        }
    }

    fn append_delegation_turn(root_request: &mut ProviderRequest, delegate_turn: &ProviderTurn) {
        if let Some(tools) = root_request.input.tools.as_mut() {
            tools.steps.push(ToolStep {
                narrowed_observations: Default::default(),
                refusal_index: None,
                refusal: None,
                plan: None,
                turn: delegate_turn.clone(),
                requests: Vec::new(),
                observations: Vec::new(),
                facts: None,
                steering_notes: Vec::new(),
                calls_remaining: None,
                cleared: false,
                hook_feedback: Default::default(),
            });
        }
    }

    fn child_queue(
        &self,
        objectives: Vec<ChildAssignment>,
        agent: Option<String>,
        parallel: usize,
    ) -> ChildQueue {
        let count = objectives.len();
        let mut queue = ChildQueue::new(0, parallel);
        queue.unspawned = objectives.into();
        queue.definition = agent;
        queue.tool_allowance =
            self.ledger()
                .remaining_calls()
                .map_or(MAX_CHILD_TOOL_CALLS, |remaining| {
                    (usize::try_from(remaining)
                        .unwrap_or(usize::MAX)
                        .saturating_sub(count + 1)
                        / 2
                        / count)
                        .min(MAX_CHILD_TOOL_CALLS)
                });
        queue
    }

    pub(super) async fn begin_synthesis(
        &self,
        root_request: &mut ProviderRequest,
    ) -> Result<bool, EngineError> {
        if !self.ledger().can_start_provider_call() {
            return self
                .settle_primary(PrimaryEnd::Stopped(StopCause::Budget))
                .await
                .map(|()| false);
        }
        root_request.phase = AgentPhase::RootSynthesis;
        self.refresh_team_context(root_request)?;
        Ok(true)
    }

    fn refresh_team_context(&self, root: &mut ProviderRequest) -> Result<(), EngineError> {
        let (reports, pending) = self
            .ledger()
            .view(|v| team_reports(v.runs.last().expect("Run")))?;
        root.input.child_results = reports;
        if let Some(tools) = root.input.tools.as_mut() {
            let mut catalog: serde_json::Value =
                serde_json::from_str(tools.catalog()).map_err(|_| EngineError::InvalidRequest)?;
            if let Some(fields) = catalog.as_object_mut() {
                fields.remove("last_answer_refused");
            }
            catalog["pending_workers"] = serde_json::json!(pending);
            catalog["worker_coordination"] = serde_json::json!(
                "While workers are pending, inspect or edit only paths outside their leases. Commands wait for lease quiescence. Answer or propose the next wave only after settlement and fresh primary verification."
            );
            tools.set_catalog(&catalog);
        }
        Ok(())
    }
}

// Settled children report in their proposal order; active ones are listed as pending.
fn team_reports(run: &crate::session::RunView) -> (Vec<ChildResult>, Vec<String>) {
    let order = proposal_order(run);
    let workers = run.worker_history();
    let mut reports = run
        .agents
        .iter()
        .skip(1)
        .filter(|a| a.status != crate::session::AgentStatus::Active)
        .map(|a| (order[&a.id], child_report(a, &workers)))
        .collect::<Vec<_>>();
    let pending = run
        .agents
        .iter()
        .skip(1)
        .filter(|a| a.status == crate::session::AgentStatus::Active)
        .map(|a| a.id.to_string())
        .collect::<Vec<_>>();
    reports.sort_by_key(|(index, _)| *index);
    (
        reports
            .into_iter()
            .map(|(_, report)| report)
            .collect::<Vec<_>>(),
        pending,
    )
}

fn proposal_order(run: &crate::session::RunView) -> HashMap<AgentRunId, usize> {
    let proposals: Vec<_> = run
        .primary()
        .into_iter()
        .flat_map(|a| &a.provider_calls)
        .filter_map(|c| c.delegation.as_ref())
        .flatten()
        .collect();
    let mut order = HashMap::new();
    for agent in run.agents.iter().skip(1) {
        let index = proposals
            .iter()
            .enumerate()
            .find(|(i, p)| !order.values().any(|v| v == i) && spawned_from(agent, p))
            .map(|(index, _)| index)
            .expect("spawn matches its proposal");
        order.insert(agent.id, index);
    }
    order
}

fn spawned_from(
    agent: &crate::session::AgentView,
    proposal: &crate::worker::ChildAssignment,
) -> bool {
    match proposal {
        crate::worker::ChildAssignment::Editor { assignment } => agent
            .assignment
            .as_ref()
            .is_some_and(|w| &w.pins.request == assignment.as_ref()),
        crate::worker::ChildAssignment::Researcher {
            objective,
            definition,
            profile,
        } => {
            agent.assignment.is_none()
                && agent.objective.as_ref() == Some(objective)
                && &agent.definition == definition
                && &agent.profile == profile
        }
    }
}

fn child_report(
    agent: &crate::session::AgentView,
    workers: &[crate::session::worker::WorkerHistory],
) -> ChildResult {
    let history = workers.iter().find(|w| w.assignment_id == agent.id);
    let tool_facts = child_tool_facts(&agent.tools);
    let result = agent.result.as_deref().unwrap_or(if history.is_some() {
        "Editor stopped; landed Workspace effects retained"
    } else {
        ""
    });
    let mut report = [
        history
            .map(|history| history.description())
            .unwrap_or_default(),
        history
            .map(|history| history.changed_paths.clone())
            .unwrap_or_default(),
        tool_facts,
        result.to_owned(),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join("\n");
    let mut end = report.len().min(MAX_CHILD_RESULT_BYTES);
    while !report.is_char_boundary(end) {
        end -= 1;
    }
    report.truncate(end);
    ChildResult {
        objective: agent.objective.clone().unwrap_or_default(),
        summary: agent
            .summary
            .clone()
            .unwrap_or_else(|| "Worker did not finish".into()),
        result: report,
    }
}

fn child_tool_facts(tools: &[crate::session::ToolView]) -> String {
    use crate::tools::ToolDisposition;

    let facts: Vec<_> = tools
        .iter()
        .filter_map(|tool| {
            let observation = tool.observation.as_ref()?;
            let cause = match observation.disposition {
                ToolDisposition::Succeeded => return None,
                ToolDisposition::Denied => tool
                    .model_denial_output()
                    .or_else(|| {
                        observation
                            .enforcement_cause
                            .map(|cause| cause.diagnostic())
                    })
                    .unwrap_or("denied; cause unavailable"),
                ToolDisposition::Limit => match observation.limit_cause {
                    Some(crate::tools::LimitCause::Timeout) => "limit: timeout",
                    Some(crate::tools::LimitCause::OutputCeiling) => "limit: output ceiling",
                    Some(crate::tools::LimitCause::ContextFull) => "limit: context full",
                    Some(crate::tools::LimitCause::ReadCapacity) => "limit: read capacity",
                    Some(crate::tools::LimitCause::RequestedTime) => "limit: requested time",
                    Some(crate::tools::LimitCause::Resource) => "limit: resource",
                    None => "limit reached",
                },
                ToolDisposition::Conflict => "conflict",
                ToolDisposition::Failed => observation
                    .enforcement_cause
                    .map_or("failed", |cause| cause.diagnostic()),
                ToolDisposition::Cancelled => "cancelled",
                ToolDisposition::Uncertain => "uncertain; outcome unknown and not retried",
            };
            let fact = format!("Tool {}: {cause}", tool.intent.call.label());
            Some(fact)
        })
        .collect();
    if facts.is_empty() {
        return String::new();
    }
    let omitted = facts.len().saturating_sub(MAX_CHILD_TOOL_FACTS);
    let mut report = String::from("Recent child Tool outcomes:");
    for fact in facts.iter().rev().take(MAX_CHILD_TOOL_FACTS) {
        report.push_str("\n- ");
        report.push_str(fact);
    }
    if omitted > 0 {
        report.push_str(&format!("\n- {omitted} earlier Tool outcome(s) omitted."));
    }
    report
}

fn delegation_active_bound(research: bool, call_deadline: Duration, waves: usize) -> Duration {
    if research {
        MAX_DELEGATION_DEADLINE
    } else {
        call_deadline
            .saturating_mul(u32::try_from(waves).unwrap_or(u32::MAX))
            .min(MAX_DELEGATION_DEADLINE)
    }
}

fn active_deadline_allowance(
    segment: &ActiveSegment,
    bound: Duration,
    sampled_at: Instant,
) -> (u64, ActiveDeadlineCause) {
    let bound_ms = duration_ms(bound);
    let allowance_ms = segment
        .reserved_ms
        .saturating_sub(segment.active_ms(sampled_at))
        .saturating_sub(duration_ms(crate::engine::STORE_ACTION_OVERHEAD))
        .min(bound_ms);
    let cause = match segment.limit {
        ActiveReservationLimit::Goal => ActiveDeadlineCause::Goal,
        ActiveReservationLimit::ActionBound => ActiveDeadlineCause::Delegation,
    };
    (allowance_ms, cause)
}

#[cfg(test)]
mod child_tool_report_tests {
    use super::*;

    #[test]
    fn approved_execution_denial_is_reported_without_inventing_a_permission_refusal() {
        use crate::tools::permissions::{Decision, PermissionSource, PermissionVerdict};
        use crate::tools::{
            Attestation, CacheUpdate, EffectIntent, EnforcementCause, GuardReceipt, ToolCall,
            ToolDisposition, ToolLimits, ToolObservation,
        };

        let intent = EffectIntent {
            host: None,
            wrap: None,
            rewrite: Vec::new(),
            host_command: None,
            target: crate::session::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: crate::session::RunId::new(),
            agent_run_id: crate::session::AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: ToolCall::Command {
                program: "ls".into(),
                args: vec!["notes".into()],
                cwd: "".into(),
                resource_class: None,
                timeout_ms: None,
            },
            limits: ToolLimits::default(),
            use_count: 1,
        };
        let denied = crate::session::ToolView {
            started_sequence: Some(0),
            checkpoint: None,
            intent: intent.clone(),
            observation: Some(ToolObservation {
                spill: None,
                intent: intent.clone(),
                disposition: ToolDisposition::Denied,
                enforcement_cause: Some(EnforcementCause::FilesystemDenied),
                limit_cause: None,
                output: "Guard enforcement: protected filesystem path refused".into(),
                attestation: Some(Attestation::Guard(GuardReceipt {
                    contract_version: 1,
                    intent_digest: intent.digest(),
                    enforcement_digest: intent.enforcement_digest,
                    limits: intent.limits.clone(),
                })),
                approval_cause: None,
                guard_stage: None,
                cache_update: CacheUpdate::NotApplicable,
            }),
            review_call_index: None,
            active_time_reserved: false,
            narrowing_source: None,
            permission_verdict: Some(PermissionVerdict {
                host: None,
                intent_digest: intent.digest(),
                decision: Decision::Allow,
                mode: None,
                basis: PermissionSource::Human,
                source: PermissionSource::Human,
                approved: true,
                hook_restriction: None,
                hooks: 0,
                narrowed: false,
            }),
            hooks: Vec::new(),
            hook_sequence: None,
            finished_sequence: Some(1),
        };

        for (cause, disposition) in [
            (EnforcementCause::FilesystemDenied, ToolDisposition::Denied),
            (EnforcementCause::GuardBootstrap, ToolDisposition::Failed),
        ] {
            let mut tool = denied.clone();
            let observation = tool.observation.as_mut().unwrap();
            observation.enforcement_cause = Some(cause);
            observation.disposition = disposition;
            if cause == EnforcementCause::GuardBootstrap {
                observation.attestation = None;
            }
            assert!(observation.valid());
            assert_eq!(tool.model_denial_output(), None);
            let report = child_tool_facts(&[tool]);
            assert_eq!(
                report,
                format!(
                    "Recent child Tool outcomes:\n- Tool command: {}",
                    cause.diagnostic()
                )
            );
            assert!(!report.contains("no operation was dispatched"));
            assert!(!report.contains("protected filesystem path"));
        }
    }
}

fn restrict_catalog(
    tools: &mut ToolContext,
    definition: &crate::tools::definitions::AgentDefinition,
) -> Result<(), EngineError> {
    let mut catalog: serde_json::Value =
        serde_json::from_str(tools.catalog()).map_err(|_| EngineError::InvalidRequest)?;
    let operations: Vec<_> = [
        ToolCall::List { path: ".".into() },
        ToolCall::Read {
            path: "file".into(),
            offset: None,
            limit: Some(1),
        },
        ToolCall::Search {
            path: ".".into(),
            query: "x".into(),
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: 100,
        },
        ToolCall::Skill {
            name: "skill".into(),
            resource: None,
        },
        ToolCall::McpList {
            server: "server".into(),
        },
    ]
    .into_iter()
    .filter(|call| definition.allows(call))
    .map(|call| call.label())
    .collect();
    catalog["operations"] = serde_json::json!(operations);
    tools.set_catalog(&catalog);
    Ok(())
}

#[cfg(test)]
mod delegation_bound_tests {
    use super::*;

    #[tokio::test]
    async fn child_tool_room_includes_the_dispatch_feedback_reserve() {
        use crate::engine::run_loop::{LoopControls, RunIdentity};
        use crate::test_support::{engine as engine_support, scenario::*};
        use crate::tools::ToolRuntime;
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let workspace = crate::test_support::goals::facts_workspace(temp.path(), "Fact");
        let state = crate::StateRoot::admit(&temp.path().join("state")).unwrap();
        let engine = engine_support::open(state, crate::EngineConfig::default()).unwrap();
        let provider = std::sync::Arc::new(ScriptedProvider::new(vec![
            crate::test_support::goals::finish_with("Seed context"),
        ]));
        let mut input = request(&workspace, "Inspect notes");
        input.include_paths.clear();
        let outcome = engine_support::run_to_end(&engine, &provider, input)
            .await
            .unwrap();
        let view = engine
            .journal
            .store()
            .load_view(outcome.session_id)
            .await
            .unwrap()
            .unwrap();
        let ledger = crate::engine::ledger::GoalLedger::new(view, Some(outcome.goal_id));

        for hooks in [false, true] {
            let settings = if hooks {
                br#"{"hooks":{"PreToolUse":[{"matcher":"Read","hooks":[{"type":"command","command":"unpinned","args":[]}]}]}}"#.as_slice()
            } else {
                b"{}".as_slice()
            };
            let path = engine.state.path().join("permissions.json");
            std::fs::write(&path, settings).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let config = serde_json::from_value(serde_json::json!({
                "workspace_paths":["notes"],"write":false,
                "commands":[],"skills":[],"mcp":[]
            }))
            .unwrap();
            let mut runtime = ToolRuntime::admit(
                config,
                crate::tools::load_permissions(&engine.state).unwrap(),
                None,
                &workspace,
                &engine.state,
                std::env::current_exe().unwrap(),
            )
            .unwrap();
            let reserve = ToolRuntime::step_context_reserve(
                false,
                RunLoop::maximum_feedback_reserve(&runtime),
            );
            for (steering, (headroom, answer_only)) in [None, Some(crate::RunSteering::new())]
                .into_iter()
                .flat_map(|steering| {
                    [(0, true), (reserve - 1, true), (reserve, false)]
                        .into_iter()
                        .map(move |case| (steering.clone(), case))
                })
            {
                let tools = runtime.research_context(16).unwrap();
                runtime
                    .set_context_limit(
                        tools.rendered_len()
                            + CHILD_INTENT_RESERVE_BYTES
                            + if steering.is_some() {
                                crate::session::MAX_STEERING_CONTEXT_BYTES
                            } else {
                                0
                            }
                            + MIN_CHILD_RESULT_BYTES
                            + headroom,
                    )
                    .unwrap();
                let runner = RunLoop::new(
                    &engine.journal,
                    crate::AdmittedProviders::single(provider.clone()),
                    RunIdentity {
                        session_id: outcome.session_id,
                        run_id: outcome.run.id,
                        goal_id: Some(outcome.goal_id),
                        primary_id: outcome.run.agents[0].id,
                    },
                    LoopControls {
                        host_settings: &engine.host_permissions,
                        policy: CollaborationPolicy::Single,
                        concurrency: 1,
                        goal_phase: crate::GoalPhase::Implementation,
                        cancellation: Default::default(),
                        tools: Some(&runtime),
                        approvals: None,
                        steering,
                    },
                    &ledger,
                    None,
                )
                .unwrap();
                let mut child = ChildWork {
                    agent_run_id: AgentRunId::new(),
                    objective: "Inspect notes".into(),
                    definition: None,
                    turns_remaining: 2,
                    kind: ChildKind::Researcher,
                    source_context: None,
                    steering_notes: Vec::new(),
                    stop_hook_feedback: Vec::new(),
                    tools: Some(tools),
                    tool_calls_remaining: 16,
                    active_deadline: None,
                };
                let root = crate::provider::test_support::primary_request("test-model");
                let offered = runner.child_request(&mut child, &root).unwrap();
                assert_eq!(
                    offered.input.tools.unwrap().answer_only(),
                    answer_only,
                    "hooks={hooks}, headroom={headroom}"
                );
            }
        }
        engine.close().await.unwrap();
    }

    #[test]
    fn missing_editor_definition_matches_editor_stop_hooks_only() {
        use crate::tools::permissions::{HookEvent, PermissionConfig};
        let config = PermissionConfig::parse(
            br#"{"hooks":{"SubagentStop":[
            {"matcher":"editor","hooks":[{"type":"command","command":"editor-check"}]},
            {"matcher":"researcher","hooks":[{"type":"command","command":"researcher-check"}]}
        ]}}"#,
        )
        .unwrap();
        for kind in [ChildKind::Researcher, ChildKind::Editor] {
            let child = ChildWork {
                agent_run_id: AgentRunId::new(),
                objective: "Inspect assignment".into(),
                definition: None,
                turns_remaining: 0,
                kind,
                source_context: None,
                steering_notes: Vec::new(),
                stop_hook_feedback: Vec::new(),
                tools: None,
                tool_calls_remaining: 0,
                active_deadline: None,
            };
            let matched = config
                .matched_hooks(HookEvent::SubagentStop, |group| {
                    group.matches(child.hook_subject())
                })
                .unwrap();
            assert_eq!(matched.len(), 1);
            assert_eq!(
                matched[0].2.command(),
                if kind.is_editor() {
                    "editor-check"
                } else {
                    "researcher-check"
                }
            );
        }
    }

    #[test]
    fn tool_free_delegation_is_capped_at_the_delegation_deadline() {
        let call = Duration::from_secs(600);
        assert_eq!(
            delegation_active_bound(false, call, 1),
            Duration::from_secs(600)
        );
        assert_eq!(
            delegation_active_bound(false, call, 2),
            MAX_DELEGATION_DEADLINE
        );
        assert_eq!(
            delegation_active_bound(false, call, 4),
            MAX_DELEGATION_DEADLINE
        );
        assert_eq!(
            delegation_active_bound(true, Duration::from_secs(120), 1),
            MAX_DELEGATION_DEADLINE
        );
        assert_eq!(MAX_DELEGATION_DEADLINE, Duration::from_secs(20 * 60));
    }
}

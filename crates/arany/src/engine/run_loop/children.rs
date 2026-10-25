use super::stop::{PrimaryEnd, RunFacts, RunProgress, RunRefusal, StepEnd, StopCause};
use super::worker::{WorkerAdmission, WorkerEnd, WorkerSettlementProgress};
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
    MAX_CHILD_TOOL_CALLS, MAX_CHILD_TURNS,
};
use crate::tools::{ToolCall, ToolContext, ToolRequest, ToolStep};
use crate::worker::WorkerCause;
use std::collections::{HashMap, VecDeque};
use std::ops::ControlFlow;
use std::time::Duration;
use tokio::task::{Id, JoinError, JoinSet};
use tokio::time::Instant;

const MAX_CHILD_RESULT_BYTES: usize = 16 * 1024;
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
    deadline: Instant,
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
                "Answer refused: criterion_not_met_in_view. Obtain current passing private evidence before answering; at most three continuations are allowed."
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
    // An editor ends before handover capture; the cause applies when no stronger fact does.
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

pub(super) fn decide_child_call(
    result: CallResult,
    valid: bool,
    gate: Option<GateFacts>,
    abort: Option<WaveStop>,
    facts: &ChildFacts,
) -> ChildDecision {
    let Some(gate) = gate else {
        return decide_researcher_call(result, valid, abort, facts);
    };
    let open = abort.is_none();
    if gate.unmet {
        return match result {
            CallResult::Response(ProviderResponse { turn, .. })
                if editor_may_continue_after_gate(gate, abort, facts) =>
            {
                ChildDecision::Reprompt(RepromptReason::CriterionNotMetInView, turn)
            }
            CallResult::Response(_) | CallResult::Failed(_) | CallResult::Cancelled => {
                ChildDecision::End(ChildEnd::Editor(WorkerCause::CriterionUnverified))
            }
        };
    }
    match result {
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Answer(answer),
            turn,
            ..
        }) if valid && open => ChildDecision::Finish {
            answer,
            turn,
            stop_hook: true,
        },
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Tools(requests),
            turn,
            ..
        }) if valid && open && !gate.worker_stopped => ChildDecision::ToolStep(requests, turn),
        other => ChildDecision::End(ChildEnd::Editor(editor_failure(
            &other,
            valid,
            facts.turns_left,
        ))),
    }
}

fn editor_may_continue_after_gate(
    gate: GateFacts,
    abort: Option<WaveStop>,
    facts: &ChildFacts,
) -> bool {
    abort.is_none()
        && !gate.worker_stopped
        && facts.turns_left
        && facts.tool_calls_left
        && gate.stop_room
        && facts.before_deadline
        && facts.tokens_open
        && facts.calls_left
        && gate.continuations_left
}

fn editor_failure(result: &CallResult, valid: bool, turns_left: bool) -> WorkerCause {
    match result {
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Tools(_),
            ..
        }) if !turns_left => WorkerCause::TurnLimit,
        CallResult::Response(_) if !valid => WorkerCause::InvalidOutcome,
        CallResult::Failed(CallFailure::InvalidResponse(_) | CallFailure::OutputLimit) => {
            WorkerCause::InvalidOutcome
        }
        CallResult::Response(_) | CallResult::Failed(_) | CallResult::Cancelled => {
            WorkerCause::ToolFailed
        }
    }
}

fn decide_researcher_call(
    result: CallResult,
    valid: bool,
    abort: Option<WaveStop>,
    facts: &ChildFacts,
) -> ChildDecision {
    if abort == Some(WaveStop::Boundary(StopCause::HookStop)) || facts.hook.stop().is_some() {
        return ChildDecision::End(ChildEnd::Researcher(AgentDisposition::Cancelled));
    }
    match result {
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Answer(answer),
            turn,
            ..
        }) if valid => ChildDecision::Finish {
            answer,
            turn,
            stop_hook: facts.has_tools && !facts.cancelled && abort.is_none(),
        },
        // An exhausted Goal budget stops the child before its Tool starts, like a pause.
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Tools(requests),
            turn,
            ..
        }) if valid => {
            let budget = !facts.tokens_open || !facts.calls_left;
            if facts.cancelled || facts.paused || abort.is_some() || budget {
                ChildDecision::End(ChildEnd::Skipped {
                    disposition: researcher_end(facts.actor_hook_stop, true),
                    budget,
                })
            } else {
                ChildDecision::ToolStep(requests, turn)
            }
        }
        CallResult::Cancelled => {
            ChildDecision::End(ChildEnd::Researcher(AgentDisposition::Cancelled))
        }
        CallResult::Response(_) | CallResult::Failed(_) => {
            ChildDecision::End(ChildEnd::Researcher(AgentDisposition::Failed))
        }
    }
}

// A researcher that ends without an answer.
fn researcher_end(actor_hook_stop: bool, interrupted: bool) -> AgentDisposition {
    if actor_hook_stop {
        AgentDisposition::StoppedByHook
    } else if interrupted {
        AgentDisposition::Cancelled
    } else {
        AgentDisposition::Failed
    }
}

pub(super) fn decide_child_stop(
    run: StopHookRun,
    abort: Option<WaveStop>,
    facts: &ChildFacts,
) -> ChildStop {
    match run {
        StopHookRun::Cancelled => ChildStop::End {
            disposition: AgentDisposition::Cancelled,
            cause: WorkerCause::Cancelled,
            stops_wave: true,
        },
        StopHookRun::Passed if facts.cancelled => ChildStop::End {
            disposition: AgentDisposition::Cancelled,
            cause: WorkerCause::Cancelled,
            stops_wave: true,
        },
        StopHookRun::Passed if facts.actor_hook_stop => ChildStop::End {
            disposition: AgentDisposition::StoppedByHook,
            cause: WorkerCause::StoppedByHook,
            stops_wave: false,
        },
        StopHookRun::Passed => ChildStop::Proceed,
        // A cut-off or cancelled hook ends this child only, unless its outcome stops the Run.
        StopHookRun::CutOffByActiveTime => ChildStop::End {
            disposition: if facts.cancelled {
                AgentDisposition::Cancelled
            } else {
                AgentDisposition::Failed
            },
            cause: WorkerCause::Budget,
            stops_wave: !facts.hook.settled && abort.is_none(),
        },
        StopHookRun::CutOffByContext => ChildStop::End {
            disposition: if facts.cancelled {
                AgentDisposition::Cancelled
            } else {
                AgentDisposition::Failed
            },
            cause: WorkerCause::ToolFailed,
            stops_wave: !facts.hook.settled && abort.is_none(),
        },
        StopHookRun::Uncertain => ChildStop::End {
            disposition: AgentDisposition::Failed,
            cause: WorkerCause::ToolFailed,
            stops_wave: false,
        },
        StopHookRun::Blocked => {
            let room = facts.before_deadline && facts.tokens_open && facts.calls_left;
            if facts.turns_left && room {
                ChildStop::Reprompt
            } else {
                ChildStop::End {
                    disposition: AgentDisposition::Failed,
                    cause: if room {
                        WorkerCause::TurnLimit
                    } else {
                        WorkerCause::Budget
                    },
                    stops_wave: false,
                }
            }
        }
    }
}

// What stops the wave after a child's call settles, when nothing stopped it yet.
fn wave_stop_after(
    queue_stop: Option<StopCause>,
    cancelled: bool,
    paused: bool,
    end: Option<AgentDisposition>,
    hook: HookFacts,
) -> Option<WaveStop> {
    if let Some(cause @ (StopCause::ActiveTime | StopCause::DelegationDeadline)) = queue_stop {
        Some(WaveStop::Boundary(cause))
    } else if queue_stop.is_some() {
        Some(WaveStop::BUDGET)
    } else if cancelled {
        Some(WaveStop::Abort(StopCause::Cancelled))
    } else if paused {
        Some(WaveStop::Boundary(StopCause::Paused))
    } else if end == Some(AgentDisposition::StoppedByHook) {
        Some(WaveStop::Boundary(StopCause::HookStop))
    } else if let Some(stop) = hook.stop() {
        Some(stop)
    } else if end == Some(AgentDisposition::Failed) {
        Some(WaveStop::Abort(StopCause::ToolStopped))
    } else if end == Some(AgentDisposition::Cancelled) {
        Some(WaveStop::Abort(StopCause::Cancelled))
    } else {
        None
    }
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

    async fn try_abort_and_drain(&mut self) -> Result<(), EngineError> {
        self.calls.abort_all();
        let deadline = Instant::now() + CANCELLATION_DRAIN;
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
    ) -> Result<(), super::ProposalRefusalCause> {
        let valid = if children.iter().any(|child| child.editor.is_some()) {
            self.valid_editor_delegation(children, agent)
        } else {
            self.wave_settled().unwrap_or(false)
                && !matches!(self.controls.policy, CollaborationPolicy::Single)
                && crate::provider::valid_assignments(children, agent)
                && self
                    .ledger()
                    .view(|v| {
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
                    })
                    .unwrap_or(false)
                && children
                    .iter()
                    .all(|a| self.valid_research_assignment(a, agent))
        };
        if valid {
            Ok(())
        } else {
            Err(super::ProposalRefusalCause::DelegateInvalid)
        }
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
                deadline,
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

    fn settle_delegation<'b>(
        &'b self,
        root_request: &'b mut ProviderRequest,
        settlement: DelegationSettlement,
    ) -> super::step::BoxedStep<'b, Admission<DelegationProgress>> {
        Box::pin(self.settle_delegation_inner(root_request, settlement))
    }

    async fn settle_delegation_inner(
        &self,
        root_request: &mut ProviderRequest,
        settlement: DelegationSettlement,
    ) -> Result<Admission<DelegationProgress>, EngineError> {
        let DelegationSettlement {
            mut children,
            mut active_segment,
            drained,
            primary_step,
            deadline,
        } = settlement;
        let Drained {
            abort: mut stop,
            pending,
        } = drained;
        if let Some(segment) = active_segment.take() {
            self.settle_active_segment(segment)?;
        }
        if stop == Some(WaveStop::Boundary(StopCause::Paused)) {
            self.pause_at_boundary();
        }

        if self.controls.cancellation.is_cancelled() {
            stop = Some(WaveStop::Abort(StopCause::Cancelled));
        }
        if stop.is_none() && self.ledger().active_time_exhausted() {
            stop = Some(WaveStop::BUDGET);
        }
        if stop.is_none() && self.worker_stopped()? {
            stop = Some(WaveStop::BUDGET);
        }
        if stop.is_none() && !pending.is_empty() {
            stop = Some(WaveStop::Abort(StopCause::ToolStopped));
        }
        if let Some(stop) = stop {
            let Admission::Reserved(()) = self
                .settle_stopped_children(
                    root_request,
                    &mut children,
                    pending,
                    WorkerEnd {
                        answer: None,
                        deadline,
                        torn_down: stop != WaveStop::BUDGET,
                        failure: WorkerCause::ToolFailed,
                        step_stop: None,
                    },
                )
                .await?
            else {
                return Ok(Admission::Exhausted);
            };
            return self
                .settle_primary(PrimaryEnd::Stopped(stop.cause()))
                .await
                .map(|()| Admission::Reserved(DelegationProgress::Settled));
        }
        if let Some(step) = primary_step {
            match self.settle_delegation_tool(root_request, step).await? {
                DelegationProgress::Continue => {}
                DelegationProgress::Settled => {
                    return Ok(Admission::Reserved(DelegationProgress::Settled));
                }
            }
        }
        self.begin_synthesis(root_request).await.map(|continues| {
            if continues {
                Admission::Reserved(DelegationProgress::Continue)
            } else {
                Admission::Reserved(DelegationProgress::Settled)
            }
        })
    }

    async fn settle_delegation_tool(
        &self,
        root_request: &mut ProviderRequest,
        (requests, turn): (Vec<ToolRequest>, ProviderTurn),
    ) -> Result<DelegationProgress, EngineError> {
        let Some(request) = self.begin_tool_step(requests).await? else {
            return self
                .settle_primary(PrimaryEnd::Failed(super::RunFailure::PlanAdmission))
                .await
                .map(|()| DelegationProgress::Settled);
        };
        let end = Box::pin(self.execute_step(
            Grant::Primary {
                request: root_request,
                review_allowed: true,
            },
            request,
            turn,
        ))
        .await?;
        match end {
            StepEnd::Continue => {}
            StepEnd::Stopped(cause) => {
                return self
                    .settle_primary(PrimaryEnd::Stopped(cause))
                    .await
                    .map(|()| DelegationProgress::Settled);
            }
        }
        self.verify_workers().await?;
        Ok(DelegationProgress::Continue)
    }

    async fn settle_stopped_children(
        &self,
        root_request: &mut ProviderRequest,
        children: &mut [ChildWork],
        pending: VecDeque<usize>,
        end: WorkerEnd,
    ) -> Result<Admission<()>, EngineError> {
        if children.iter().any(|child| child.kind.is_editor()) {
            self.observe_worker_stop().await?;
        }
        for index in pending {
            if children[index].kind.is_editor() {
                let completed = {
                    let _active_work = self.ledger().active_work();
                    self.complete_worker(
                        &mut children[index],
                        WorkerEnd {
                            answer: None,
                            ..end
                        },
                    )
                    .await?
                };
                match self
                    .settle_worker_handover(root_request, &children[index], completed)
                    .await?
                {
                    WorkerSettlementProgress::Settled => {}
                    WorkerSettlementProgress::Exhausted => return Ok(Admission::Exhausted),
                }
            } else {
                self.finish_child(
                    children[index].agent_run_id,
                    AgentDisposition::Cancelled,
                    None,
                    false,
                )
                .await?;
            }
        }
        Ok(Admission::Reserved(()))
    }

    // A wave admits only a well-formed Tool step, and only outside live worker leases unless it
    // spawns a process, which waits for lease quiescence.
    fn wave_validity(
        &self,
        result: &CallResult,
    ) -> Result<Result<(), super::ProposalRefusalCause>, EngineError> {
        let CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Tools(requests),
            ..
        }) = result
        else {
            return Ok(Err(super::ProposalRefusalCause::Phase));
        };
        if let Err(cause) = self.well_formed_tools(requests)? {
            return Ok(Err(cause));
        }
        for request in requests {
            if !request.call.is_process()
                && let Err(cause) = self.primary_lease_allows(&request.call)?
            {
                return Ok(Err(cause));
            }
        }
        Ok(Ok(()))
    }

    fn settled_agents(&self) -> Result<usize, EngineError> {
        self.ledger().view(|v| {
            v.runs.last().map_or(0, |r| {
                r.agents
                    .iter()
                    .skip(1)
                    .filter(|a| a.status != crate::session::AgentStatus::Active)
                    .count()
            })
        })
    }

    async fn spawn_researchers(
        &self,
        queue: &mut ChildQueue,
        children: &mut Vec<ChildWork>,
        root_request: &mut ProviderRequest,
        segment: &mut Option<ActiveSegment>,
        deadline: Instant,
    ) -> Result<RunProgress<ResearchSpawn>, EngineError> {
        while !queue.unspawned.is_empty() && !self.controls.cancellation.is_cancelled() {
            let child = match self
                .spawn_next_child(queue, root_request, segment, deadline)
                .await
            {
                Ok(RunProgress::Ready(Some(child))) => child,
                Ok(RunProgress::Ready(None)) => break,
                Ok(RunProgress::Stopped(cause)) => {
                    return Ok(queue.abort_with(RunProgress::Stopped(cause)).await);
                }
                Err(error) => return Err(queue.abort_with(error).await),
            };
            queue.pending.push_back(children.len());
            children.push(child);
        }
        if !self.controls.cancellation.is_cancelled() {
            return Ok(RunProgress::Ready(ResearchSpawn::Ready));
        }
        if let Some(segment) = segment.take() {
            self.settle_active_segment(segment)?;
        }
        for child in children.iter() {
            self.finish_child(child.agent_run_id, AgentDisposition::Cancelled, None, false)
                .await?;
        }
        Ok(RunProgress::Ready(ResearchSpawn::Cancelled))
    }

    async fn spawn_children(
        &self,
        count: usize,
        workers: bool,
    ) -> Result<Option<Spawned>, EngineError> {
        let research = self.controls.tools.is_some();
        let action = Action::Delegation {
            children: count,
            research,
        };
        if !self.ledger().admits(action) {
            return Ok(None);
        }
        let capacity = if workers {
            Action::Worker { chunks: 0 }
        } else {
            action
        };
        self.journal
            .admit(
                self.session_id,
                capacity.max_events_for_admitted(&self.routing.providers)
                    + self.ledger().verdicts(capacity),
                capacity.tool_observations(),
            )
            .await?;
        let parallel = self.ledger().provider_concurrency(
            usize::from(self.controls.concurrency)
                .min(usize::from(self.controls.policy.max_children()))
                .min(self.ledger().view(|v| {
                    v.runs
                        .last()
                        .and_then(|r| r.config.as_ref())
                        .and_then(|c| c.worker_policy.as_ref())
                        .map_or(8, |p| usize::from(p.concurrency))
                })?),
        );
        let bound = delegation_active_bound(
            research,
            self.routing
                .providers
                .child_chains()
                .into_iter()
                .flatten()
                .map(|p| p.identity().limits.deadline)
                .max()
                .expect("child chain"),
            count.div_ceil(parallel.max(1)),
        );
        let Admission::Reserved(segment) = self.reserve_active_segment(
            bound,
            crate::session::BudgetAction::Delegation,
            self.primary_id,
        )?
        else {
            return Ok(None);
        };
        Ok(Some(Spawned {
            children: Vec::with_capacity(count),
            parallel,
            active_segment: Some(segment),
            bound,
        }))
    }

    async fn spawn_next_child(
        &self,
        queue: &mut ChildQueue,
        root: &mut ProviderRequest,
        segments: &mut Option<ActiveSegment>,
        deadline: Instant,
    ) -> Result<RunProgress<Option<ChildWork>>, EngineError> {
        let Some(assignment) = queue.unspawned.pop_front() else {
            return Ok(RunProgress::Ready(None));
        };
        if let Some(editor) = assignment.editor {
            if let Some(s) = segments.take() {
                self.settle_active_segment(s)?;
            }
            let _active_work = self.ledger().active_work();
            let mut child = match self.begin_worker(root, editor).await? {
                WorkerAdmission::Admitted(child) => Some(*child),
                WorkerAdmission::Refused => None,
                WorkerAdmission::Exhausted => {
                    return Ok(RunProgress::Stopped(RunRefusal::Budget));
                }
                WorkerAdmission::CapacityExhausted => {
                    return Ok(RunProgress::Stopped(RunRefusal::Capacity));
                }
            };
            if let Some(child) = &mut child {
                child.tool_calls_remaining = child.tool_calls_remaining.min(queue.tool_allowance);
                child.active_deadline = queue.active_deadline.clone();
            }
            *segments = match self
                .reserve_active_segment(
                    deadline.saturating_duration_since(Instant::now()),
                    crate::session::BudgetAction::Delegation,
                    self.primary_id,
                )?
                .map(Some)
            {
                Admission::Reserved(segment) => segment,
                Admission::Exhausted => {
                    queue.stop.get_or_insert(StopCause::ActiveTime);
                    None
                }
            };
            return Ok(RunProgress::Ready(child));
        }
        self.spawn_researcher(queue, assignment)
            .await
            .map(|child| RunProgress::Ready(Some(child)))
    }

    async fn spawn_researcher(
        &self,
        queue: &ChildQueue,
        assignment: ChildAssignment,
    ) -> Result<ChildWork, EngineError> {
        let definition = queue
            .definition
            .as_ref()
            .and_then(|n| self.definitions.get()?.get(n))
            .cloned();
        let id = AgentRunId::new();
        let research = self.controls.tools.is_some();
        self.append(Event::AgentSpawned {
            run_id: self.run_id,
            agent_run_id: id,
            role: AgentRole::Child,
            ordinal: self
                .ledger()
                .view(|v| v.runs.last().expect("Run").agents.len())?
                .try_into()
                .map_err(|_| EngineError::CoordinatorFailed)?,
            objective: Some(assignment.objective.clone()),
            definition: queue.definition.clone(),
            profile: assignment.profile.or_else(|| self.default_child_profile()),
            grant: Some(if research {
                ChildGrant::ReadOnly {
                    unguarded: crate::tools::UnguardedGrant::Denied,
                }
            } else {
                ChildGrant::NoTools
            }),
            assignment: None,
        })
        .await?;
        Ok(ChildWork {
            agent_run_id: id,
            objective: assignment.objective,
            turns_remaining: definition.as_ref().map_or(MAX_CHILD_TURNS, |d| d.max_turns),
            definition,
            kind: ChildKind::Researcher,
            source_context: None,
            steering_notes: Vec::new(),
            stop_hook_feedback: Vec::new(),
            tools: self
                .controls
                .tools
                .map(|r| r.research_context(queue.tool_allowance))
                .transpose()?,
            tool_calls_remaining: queue.tool_allowance,
            active_deadline: queue.active_deadline.clone(),
        })
    }

    fn drive_children<'a>(
        &'a self,
        children: &'a mut Vec<ChildWork>,
        root_request: &'a mut ProviderRequest,
        queue: &'a mut ChildQueue,
        segments: &'a mut Option<ActiveSegment>,
        deadline: Instant,
    ) -> super::step::BoxedStep<'a, RunProgress<Drained>> {
        Box::pin(self.drive_children_inner(ChildWave {
            children,
            root_request,
            queue,
            segments,
            deadline,
        }))
    }

    // A same-route retry waits out its committed delay inside the task, so cancellation still drains it.
    fn respawn_attempt(
        &self,
        queue: &mut ChildQueue,
        task_id: Id,
        index: Option<usize>,
        delay: Duration,
        deadline: Instant,
    ) -> Result<(), EngineError> {
        let role = if index.is_none() {
            crate::provider::ProviderRole::Primary
        } else {
            crate::provider::ProviderRole::Child
        };
        let mut active = queue
            .active
            .remove(&task_id)
            .ok_or(EngineError::CoordinatorFailed)?;
        let provider = self.routing.bind(
            role,
            active.request.agent_run_id,
            &mut active.request.model,
            &mut active.request.max_output_tokens,
        )?;
        let preview = (index.is_none()
            || self.routing.chain(role, active.request.agent_run_id)?.len() > 1)
            .then(|| crate::provider::ResponsePreview::new().0);
        let request = active.request.clone();
        let limits = active.limits.clone();
        let mut cancellation = self.controls.cancellation.clone();
        let steering = self.controls.steering.clone();
        let session_id = self.session_id;
        let provider_activity = self.ledger().provider_activity();
        let handle = queue.calls.spawn(async move {
            let _provider_work = provider_activity.start();
            // A pause or cancellation in the wait ends the child's call unsent; the wave then drains.
            if !delay.is_zero()
                && crate::engine::routing::back_off(
                    session_id,
                    delay,
                    &mut cancellation,
                    steering.as_ref(),
                )
                .await
                    != crate::engine::routing::Backoff::Elapsed
            {
                return crate::engine::routing::AttemptResult {
                    result: CallResult::Cancelled,
                    transient: None,
                    visible: false,
                };
            }
            crate::engine::routing::invoke_attempt(
                provider.as_ref(),
                request,
                deadline,
                cancellation,
                preview,
                limits,
                crate::fault::LatencyReceipt::default(),
            )
            .await
        });
        queue.active.insert(handle.id(), active);
        Ok(())
    }

    async fn drive_children_inner(
        &self,
        mut wave: ChildWave<'_>,
    ) -> Result<RunProgress<Drained>, EngineError> {
        match self.refill_wave(&mut wave).await {
            Ok(RunProgress::Ready(())) => {}
            Ok(RunProgress::Stopped(cause)) => {
                return Ok(wave.queue.abort_with(RunProgress::Stopped(cause)).await);
            }
            Err(error) => return Err(wave.queue.abort_with(error).await),
        }
        let mut abort = self.unstarted_stop(wave.queue);
        let mut drain_deadline = None;

        while !wave.queue.active.is_empty() {
            let (index, active, result) = match self
                .next_child_attempt(&mut wave, &mut abort, &mut drain_deadline)
                .await?
            {
                ControlFlow::Continue(()) => continue,
                ControlFlow::Break(completed) => {
                    let CompletedChildAttempt {
                        index,
                        active,
                        result,
                    } = completed;
                    (index, active, result)
                }
            };
            let limits = active.limits.take();
            let result = result.result;
            self.settle_result(&result)?;
            if index.is_none() {
                let (decision, deferral) = self
                    .classify_wave_primary(result, limits, wave.queue, abort)
                    .await?;
                self.apply_wave_primary(&mut wave, decision, deferral, &mut abort)
                    .await?;
                if abort.is_some() {
                    wave.queue.calls.abort_all();
                    drain_deadline = Some(Instant::now() + CANCELLATION_DRAIN);
                } else if let RunProgress::Stopped(cause) = self.refill_wave(&mut wave).await? {
                    return Ok(RunProgress::Stopped(cause));
                }
                continue;
            }
            let index = index.ok_or(EngineError::CoordinatorFailed)?;
            let step_disposition = match self
                .settle_wave_child(&mut wave, index, result, limits, &mut abort)
                .await?
            {
                RunProgress::Stopped(cause) => return Ok(RunProgress::Stopped(cause)),
                RunProgress::Ready(ChildProgress::Refilled) => continue,
                RunProgress::Ready(ChildProgress::Settled(disposition)) => disposition,
            };
            if abort.is_none() {
                abort = wave_stop_after(
                    wave.queue.stop,
                    self.controls.cancellation.is_cancelled(),
                    self.pause_requested(),
                    step_disposition,
                    self.hook_facts()?,
                );
            }
            if abort.is_none() {
                match self.refill_wave(&mut wave).await {
                    Ok(RunProgress::Ready(())) => {}
                    Ok(RunProgress::Stopped(cause)) => {
                        return Ok(wave.queue.abort_with(RunProgress::Stopped(cause)).await);
                    }
                    Err(error) => return Err(wave.queue.abort_with(error).await),
                }
                abort = self.unstarted_stop(wave.queue);
            }
        }
        Ok(RunProgress::Ready(Drained {
            abort,
            pending: std::mem::take(&mut wave.queue.pending),
        }))
    }

    async fn next_child_attempt(
        &self,
        wave: &mut ChildWave<'_>,
        abort: &mut Option<WaveStop>,
        drain_deadline: &mut Option<Instant>,
    ) -> Result<ChildAttempt, EngineError> {
        let joined = self
            .join_child_call(wave.queue, abort, drain_deadline)
            .await?;
        let (task_id, result) = classify_join(joined);
        let Some(index) = wave.queue.active.get(&task_id).map(|active| active.index) else {
            return Err(wave.queue.abort_with(EngineError::CoordinatorFailed).await);
        };
        let respawn = self
            .child_attempt_continuation(
                wave.queue,
                task_id,
                index,
                &result,
                *abort,
                wave.deadline(),
            )
            .await;
        match respawn {
            Ok(Some(delay)) => {
                if let Err(error) =
                    self.respawn_attempt(wave.queue, task_id, index, delay, wave.deadline())
                {
                    return Err(wave.queue.abort_with(error).await);
                }
                return Ok(ControlFlow::Continue(()));
            }
            Ok(None) => {}
            Err(error) => return Err(wave.queue.abort_with(error).await),
        }
        let Some(active) = wave.queue.active.remove(&task_id) else {
            return Err(wave.queue.abort_with(EngineError::CoordinatorFailed).await);
        };
        Ok(ControlFlow::Break(CompletedChildAttempt {
            index,
            active,
            result,
        }))
    }

    async fn child_attempt_continuation(
        &self,
        queue: &ChildQueue,
        task_id: Id,
        index: Option<usize>,
        result: &crate::engine::routing::AttemptResult,
        abort: Option<WaveStop>,
        deadline: Instant,
    ) -> Result<Option<Duration>, EngineError> {
        let active = queue
            .active
            .get(&task_id)
            .ok_or(EngineError::CoordinatorFailed)?;
        let role = if index.is_none() {
            crate::provider::ProviderRole::Primary
        } else {
            crate::provider::ProviderRole::Child
        };
        let facts = self.routing.attempt_facts(
            active.request.agent_run_id,
            role,
            deadline,
            abort.is_some() || self.controls.cancellation.is_cancelled() || self.pause_requested(),
            result.visible,
        )?;
        let scope = CallScope {
            session_id: self.session_id,
            run_id: self.run_id,
            agent_run_id: active.request.agent_run_id,
        };
        self.routing
            .continue_attempt(
                scope,
                role,
                decide_attempt(result.transient, facts),
                &active.request,
                &active.limits,
                || self.routing.provider(role, active.request.agent_run_id),
            )
            .await
    }

    async fn classify_wave_primary(
        &self,
        result: CallResult,
        limits: Option<crate::provider::RateLimits>,
        queue: &ChildQueue,
        abort: Option<WaveStop>,
    ) -> Result<(PrimaryDecision, Option<RepromptReason>), EngineError> {
        let deferral = RepromptReason::deferral(&result);
        let validity = self.wave_validity(&result)?;
        let facts = RunFacts::observe(self)?;
        let site = PrimarySite::Wave {
            queue_stopped: abort.is_some() || queue.stop.is_some(),
        };
        let record_admitted = primary_admitted(site, &result, validity, &facts);
        let mut record = call_record(
            AgentPhase::RootSynthesis,
            &result,
            record_admitted,
            None,
            None,
            limits,
        );
        if wave_deferred(&result, record_admitted, &facts) {
            record.failure_reason = deferral.map(RepromptReason::failure_reason);
        }
        self.append(Event::ProviderCallRecorded {
            run_id: self.run_id,
            agent_run_id: self.primary_id,
            record,
        })
        .await?;
        let facts = RunFacts::observe(self)?;
        let decision = decide_primary_call(site, result, validity, &facts);
        Ok((decision, deferral))
    }

    async fn apply_wave_primary(
        &self,
        wave: &mut ChildWave<'_>,
        decision: PrimaryDecision,
        deferral: Option<RepromptReason>,
        abort: &mut Option<WaveStop>,
    ) -> Result<(), EngineError> {
        match decision {
            PrimaryDecision::ToolStep(requests, turn) => {
                let process = requests.iter().any(|r| r.call.is_process());
                if process {
                    wave.queue.primary_step = Some((requests, turn));
                } else {
                    self.apply_wave_primary_tool(wave, requests, turn, abort)
                        .await?;
                }
            }
            PrimaryDecision::Defer => {
                let settled = self.settled_agents()?;
                wave.queue.deferred = deferral.map(|reason| (reason, settled));
                wave.queue.primary_pending = false;
            }
            PrimaryDecision::Stop(cause) => *abort = Some(WaveStop::Abort(cause)),
            PrimaryDecision::Plan(..)
            | PrimaryDecision::Finish(..)
            | PrimaryDecision::Delegate(..)
            | PrimaryDecision::Refused(_)
            | PrimaryDecision::RefusedTool(..)
            | PrimaryDecision::CorrectOutcome
            | PrimaryDecision::Fail(_) => {
                return Err(wave.queue.abort_with(EngineError::CoordinatorFailed).await);
            }
        }
        Ok(())
    }

    async fn apply_wave_primary_tool(
        &self,
        wave: &mut ChildWave<'_>,
        requests: Vec<ToolRequest>,
        turn: ProviderTurn,
        abort: &mut Option<WaveStop>,
    ) -> Result<(), EngineError> {
        if let Some(segment) = wave.segments.take() {
            self.settle_active_segment(segment)?;
        }
        // Wave validity admits exactly one request.
        let request = self
            .begin_tool_step(requests)
            .await?
            .ok_or(EngineError::CoordinatorFailed)?;
        let end = match Box::pin(self.execute_step(
            Grant::Primary {
                request: wave.root_request,
                review_allowed: false,
            },
            request,
            turn,
        ))
        .await?
        {
            StepEnd::Continue => StepEnd::Continue,
            StepEnd::Stopped(StopCause::ActiveTime)
                if self
                    .active_time_floor
                    .load(std::sync::atomic::Ordering::Relaxed) =>
            {
                StepEnd::Stopped(StopCause::ActiveTime)
            }
            StepEnd::Stopped(cause @ (StopCause::ActiveTime | StopCause::Capacity)) => {
                wave.queue.stop.get_or_insert(cause);
                self.observe_worker_stop().await?;
                StepEnd::Continue
            }
            end @ StepEnd::Stopped(
                StopCause::Uncertain
                | StopCause::Cancelled
                | StopCause::HookStop
                | StopCause::Paused
                | StopCause::Budget
                | StopCause::ContextFull
                | StopCause::DelegationDeadline
                | StopCause::Refused(_)
                | StopCause::ToolStopped
                | StopCause::InvalidCommandResult,
            ) => end,
        };
        if let StepEnd::Stopped(cause) = end {
            *abort = self
                .hook_facts()?
                .stop()
                .or(*abort)
                .or(Some(WaveStop::Abort(cause)));
        }
        if wave.queue.stop.is_none() {
            self.verify_workers().await?;
        }
        *wave.segments = match self
            .reserve_active_segment(
                wave.deadline().saturating_duration_since(Instant::now()),
                crate::session::BudgetAction::Delegation,
                self.primary_id,
            )?
            .map(Some)
        {
            Admission::Reserved(segment) => segment,
            Admission::Exhausted => {
                wave.queue.stop.get_or_insert(StopCause::ActiveTime);
                None
            }
        };
        wave.queue.primary_pending = false;
        Ok(())
    }

    async fn classify_editor_call(
        &self,
        child: &ChildWork,
        result: CallResult,
        limits: Option<crate::provider::RateLimits>,
        valid_response: bool,
        abort: Option<WaveStop>,
        deadline: Instant,
    ) -> Result<ChildDecision, EngineError> {
        let gate_failed = valid_response
            && matches!(
                &result,
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Answer(_),
                    ..
                })
            )
            && self.criterion_unmet(child.agent_run_id)?;
        let mut record = call_record(
            AgentPhase::ChildWork,
            &result,
            valid_response && !gate_failed,
            None,
            None,
            limits,
        );
        if gate_failed {
            record.failure_reason = Some(RepromptReason::CriterionNotMetInView.failure_reason());
        }
        self.append(Event::ProviderCallRecorded {
            run_id: self.run_id,
            agent_run_id: child.agent_run_id,
            record,
        })
        .await?;
        self.ledger().child_call_finished()?;
        let tools = valid_response
            && matches!(
                &result,
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Tools(_),
                    ..
                })
            );
        // The worker-stop boundary commits before a continuation or private Tool.
        let worker_stopped =
            abort.is_none() && (gate_failed || tools) && self.observe_worker_stop().await?;
        let gate = GateFacts {
            unmet: gate_failed,
            worker_stopped,
            stop_room: self
                .controls
                .tools
                .zip(child.tools.as_ref())
                .zip(match &result {
                    CallResult::Response(ProviderResponse { turn, .. }) => Some(turn),
                    CallResult::Failed(_) | CallResult::Cancelled => None,
                })
                .is_some_and(|((runtime, tools), turn)| {
                    tools.has_stop_room(turn, runtime.context_limit_bytes())
                }),
            continuations_left: gate_failed
                && self.ledger().view(|v| {
                    v.runs
                        .last()
                        .expect("active Run")
                        .agents
                        .iter()
                        .find(|a| a.id == child.agent_run_id)
                        .and_then(|a| a.assignment.as_ref())
                        .is_some_and(|w| w.gate_continuations < 3)
                })?,
        };
        let facts = self.child_facts(child, deadline)?;
        Ok(decide_child_call(
            result,
            valid_response,
            Some(gate),
            abort,
            &facts,
        ))
    }

    async fn run_editor_tool(
        &self,
        child: &mut ChildWork,
        queue: &mut ChildQueue,
        mut requests: Vec<ToolRequest>,
        turn: ProviderTurn,
        deadline: Instant,
    ) -> Result<(Option<StopCause>, bool), EngineError> {
        let active_deadline = child.active_deadline.clone();
        let step_stop = match Box::pin(self.execute_step(
            Grant::Researcher {
                child,
                deadline,
                active_deadline,
            },
            requests.pop().ok_or(EngineError::CoordinatorFailed)?,
            turn,
        ))
        .await?
        {
            StepEnd::Continue => None,
            StepEnd::Stopped(cause @ (StopCause::ActiveTime | StopCause::DelegationDeadline)) => {
                queue.stop.get_or_insert(cause);
                if cause == StopCause::ActiveTime {
                    self.observe_worker_stop().await?;
                }
                Some(cause)
            }
            StepEnd::Stopped(cause) => Some(cause),
        };
        child.tool_calls_remaining = child.tool_calls_remaining.saturating_sub(1);
        let can_continue = self.ledger().view(|v| {
            v.runs
                .last()
                .and_then(|r| r.agents.iter().find(|a| a.id == child.agent_run_id))
                .and_then(|a| a.tools.last())
                .is_some_and(|t| t.observation.is_some() && !crate::session::private_failure(t))
        })?;
        Ok((step_stop, can_continue))
    }

    async fn apply_editor_call(
        &self,
        wave: &mut ChildWave<'_>,
        index: usize,
        decision: ChildDecision,
        abort: &mut Option<WaveStop>,
    ) -> Result<RunProgress<ChildProgress>, EngineError> {
        let mut step_stop = None;
        let (answer, failure) = match decision {
            ChildDecision::Reprompt(reason, turn) => {
                Self::append_editor_reprompt(&mut wave.children[index], reason, turn);
                return self.refill_editor_child(wave, index).await;
            }
            ChildDecision::Finish { answer, turn, .. } => {
                let deadline = wave.deadline();
                match self
                    .subagent_stop(&mut wave.children[index], &answer, turn, abort, deadline)
                    .await?
                {
                    ChildStop::Proceed => (Some(answer), WorkerCause::ToolFailed),
                    ChildStop::Reprompt => {
                        return self.refill_editor_child(wave, index).await;
                    }
                    ChildStop::End { cause, .. } => (None, cause),
                }
            }
            ChildDecision::ToolStep(requests, turn) => {
                let deadline = wave.deadline();
                let (stop, can_continue) = self
                    .run_editor_tool(
                        &mut wave.children[index],
                        wave.queue,
                        requests,
                        turn,
                        deadline,
                    )
                    .await?;
                step_stop = stop;
                if step_stop.is_none() && can_continue {
                    wave.queue.pending.push_back(index);
                    match self.refill_wave(wave).await {
                        Ok(RunProgress::Ready(())) => {}
                        Ok(RunProgress::Stopped(cause)) => {
                            wave.queue.abort_and_drain().await;
                            return Ok(RunProgress::Stopped(cause));
                        }
                        Err(error) => return Err(wave.queue.abort_with(error).await),
                    }
                    return Ok(RunProgress::Ready(ChildProgress::Refilled));
                }
                (None, WorkerCause::ToolFailed)
            }
            ChildDecision::End(ChildEnd::Editor(cause)) => (None, cause),
            ChildDecision::End(ChildEnd::Researcher(_) | ChildEnd::Skipped { .. }) => {
                return Err(wave.queue.abort_with(EngineError::CoordinatorFailed).await);
            }
        };
        self.settle_editor_call(
            wave,
            index,
            EditorCompletion {
                answer,
                failure,
                step_stop,
            },
            *abort,
        )
        .await
        .map(|admission| {
            RunProgress::from(admission)
                .map(|disposition| ChildProgress::Settled(Some(disposition)))
        })
    }

    async fn refill_editor_child(
        &self,
        wave: &mut ChildWave<'_>,
        index: usize,
    ) -> Result<RunProgress<ChildProgress>, EngineError> {
        wave.queue.pending.push_back(index);
        Ok(self
            .refill_wave(wave)
            .await?
            .map(|()| ChildProgress::Refilled))
    }

    fn append_editor_reprompt(child: &mut ChildWork, reason: RepromptReason, turn: ProviderTurn) {
        let text = reason.text().to_owned();
        if let Some(tools) = child.tools.as_mut() {
            tools.add_stop_feedback(&turn, &text);
        }
        child
            .stop_hook_feedback
            .push(crate::provider::StopHookFeedback {
                after_step: child.tools.as_ref().map_or(0, |t| t.steps.len()),
                turn,
                text,
                facts: None,
                steering_notes: Vec::new(),
            });
    }

    async fn settle_editor_call(
        &self,
        wave: &mut ChildWave<'_>,
        index: usize,
        completion: EditorCompletion,
        abort: Option<WaveStop>,
    ) -> Result<Admission<AgentDisposition>, EngineError> {
        let EditorCompletion {
            answer,
            failure,
            step_stop,
        } = completion;
        let deadline = wave.deadline();
        let completed = {
            let _active_work = self.ledger().active_work();
            self.complete_worker(
                &mut wave.children[index],
                WorkerEnd {
                    answer,
                    deadline,
                    torn_down: matches!(abort, Some(WaveStop::Abort(_))),
                    failure,
                    step_stop,
                },
            )
            .await?
        };
        if let Some(s) = wave.segments.take() {
            self.settle_active_segment(s)?;
        }
        match self
            .settle_worker_handover(wave.root_request, &wave.children[index], completed)
            .await?
        {
            WorkerSettlementProgress::Settled => {}
            WorkerSettlementProgress::Exhausted => return Ok(Admission::Exhausted),
        }
        *wave.segments = match self
            .reserve_active_segment(
                wave.deadline().saturating_duration_since(Instant::now()),
                crate::session::BudgetAction::Delegation,
                self.primary_id,
            )?
            .map(Some)
        {
            Admission::Reserved(segment) => segment,
            Admission::Exhausted => {
                wave.queue.stop.get_or_insert(StopCause::ActiveTime);
                None
            }
        };
        Ok(Admission::Reserved(
            if self.actor_hook_requested_stop(wave.children[index].agent_run_id)? {
                AgentDisposition::StoppedByHook
            } else {
                AgentDisposition::Finished
            },
        ))
    }

    fn refill_wave<'a>(
        &'a self,
        wave: &'a mut ChildWave<'_>,
    ) -> super::step::BoxedStep<'a, RunProgress<()>> {
        Box::pin(self.fill_slots(wave))
    }

    async fn join_child_call(
        &self,
        queue: &mut ChildQueue,
        abort: &mut Option<WaveStop>,
        drain_deadline: &mut Option<Instant>,
    ) -> Result<Result<(Id, crate::engine::routing::AttemptResult), JoinError>, EngineError> {
        if let Some(cause) = queue.stop
            && abort.is_none()
        {
            *abort = Some(match cause {
                cause @ (StopCause::ActiveTime | StopCause::DelegationDeadline) => {
                    WaveStop::Boundary(cause)
                }
                _ => WaveStop::BUDGET,
            });
        }
        // A pause lets in-flight child calls finish and be recorded; every other stop drains them.
        if abort.is_some_and(|stop| stop != WaveStop::Boundary(StopCause::Paused))
            && drain_deadline.is_none()
        {
            queue.calls.abort_all();
            *drain_deadline = Some(Instant::now() + CANCELLATION_DRAIN);
        }
        let joined = match *drain_deadline {
            Some(limit) => tokio::time::timeout_at(limit, queue.calls.join_next_with_id())
                .await
                .map_err(|_| EngineError::CancellationDrainTimeout)?,
            None => queue.calls.join_next_with_id().await,
        }
        .ok_or(EngineError::CoordinatorFailed)?;
        Ok(joined)
    }

    async fn settle_wave_child(
        &self,
        wave: &mut ChildWave<'_>,
        index: usize,
        result: CallResult,
        limits: Option<crate::provider::RateLimits>,
        abort: &mut Option<WaveStop>,
    ) -> Result<RunProgress<ChildProgress>, EngineError> {
        let valid_response = self.ledger().tokens_within_limit()
            && match &result {
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Answer(answer),
                    ..
                }) => valid_answer(answer, MAX_CHILD_RESULT_BYTES),
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Tools(requests),
                    ..
                }) => self.valid_child_tools(&wave.children[index], requests),
                CallResult::Response(ProviderResponse {
                    outcome:
                        ProviderOutcome::Delegate(_)
                        | ProviderOutcome::Plan(_)
                        | ProviderOutcome::Review(_),
                    ..
                })
                | CallResult::Failed(_)
                | CallResult::Cancelled => false,
            };
        if wave.children[index].kind.is_editor() {
            let decision = self
                .classify_editor_call(
                    &wave.children[index],
                    result,
                    limits,
                    valid_response,
                    *abort,
                    wave.deadline(),
                )
                .await?;
            return self.apply_editor_call(wave, index, decision, abort).await;
        }
        let deadline = wave.deadline();
        let admission = match self
            .settle_child_call(
                &mut wave.children[index],
                ChildCall {
                    result,
                    limits,
                    valid_response,
                },
                abort,
                deadline,
            )
            .await
        {
            Ok(admission) => admission,
            Err(error) => return Err(wave.queue.abort_with(error).await),
        };
        if matches!(admission, Admission::Exhausted) {
            wave.queue.abort_and_drain().await;
        }
        Ok(RunProgress::from(admission).map(|disposition| {
            if disposition.is_none() {
                wave.queue.pending.push_back(index);
            }
            ChildProgress::Settled(disposition)
        }))
    }

    async fn settle_child_call(
        &self,
        child: &mut ChildWork,
        call: ChildCall,
        abort: &mut Option<WaveStop>,
        deadline: Instant,
    ) -> Result<Admission<Option<AgentDisposition>>, EngineError> {
        let ChildCall {
            result,
            limits,
            valid_response,
        } = call;
        let agent_run_id = child.agent_run_id;
        self.append(Event::ProviderCallRecorded {
            run_id: self.run_id,
            agent_run_id,
            record: call_record(
                AgentPhase::ChildWork,
                &result,
                valid_response,
                None,
                None,
                limits,
            ),
        })
        .await?;
        let facts = self.child_facts(child, deadline)?;
        let decision = decide_child_call(result, valid_response, None, *abort, &facts);
        self.apply_researcher_call(child, decision, abort, deadline)
            .await
    }

    async fn apply_researcher_call(
        &self,
        child: &mut ChildWork,
        decision: ChildDecision,
        abort: &mut Option<WaveStop>,
        deadline: Instant,
    ) -> Result<Admission<Option<AgentDisposition>>, EngineError> {
        let agent_run_id = child.agent_run_id;
        let disposition = match decision {
            ChildDecision::Finish {
                answer,
                turn,
                stop_hook,
            } => {
                if stop_hook {
                    match self
                        .subagent_stop(child, &answer, turn, abort, deadline)
                        .await?
                    {
                        ChildStop::Proceed => {}
                        ChildStop::Reprompt => return Ok(Admission::Reserved(None)),
                        ChildStop::End { disposition, .. } => {
                            return Ok(Admission::Reserved(Some(disposition)));
                        }
                    }
                }
                self.finish_child(
                    agent_run_id,
                    AgentDisposition::Finished,
                    Some(&answer),
                    true,
                )
                .await?;
                AgentDisposition::Finished
            }
            ChildDecision::ToolStep(requests, turn) => {
                return self
                    .run_researcher_tool(child, requests, turn, deadline)
                    .await;
            }
            ChildDecision::End(ChildEnd::Skipped {
                disposition,
                budget,
            }) => {
                self.ledger().child_call_finished()?;
                if budget {
                    abort.get_or_insert(WaveStop::BUDGET);
                }
                self.finish_child(agent_run_id, disposition, None, false)
                    .await?;
                disposition
            }
            ChildDecision::End(ChildEnd::Researcher(disposition)) => {
                self.finish_child(agent_run_id, disposition, None, true)
                    .await?;
                disposition
            }
            ChildDecision::Reprompt(..) | ChildDecision::End(ChildEnd::Editor(_)) => {
                return Err(EngineError::CoordinatorFailed);
            }
        };
        Ok(Admission::Reserved(Some(disposition)))
    }

    async fn run_researcher_tool(
        &self,
        child: &mut ChildWork,
        mut requests: Vec<ToolRequest>,
        turn: ProviderTurn,
        deadline: Instant,
    ) -> Result<Admission<Option<AgentDisposition>>, EngineError> {
        let agent_run_id = child.agent_run_id;

        self.ledger().child_call_finished()?;
        let request = requests.pop().ok_or(EngineError::CoordinatorFailed)?;
        // Boxed: the Tool future is large and would overflow the caller's stack.
        let active_deadline = child.active_deadline.clone();
        let end = Box::pin(self.execute_step(
            Grant::Researcher {
                child: &mut *child,
                deadline,
                active_deadline,
            },
            request,
            turn,
        ))
        .await?;
        match end {
            StepEnd::Continue => {
                child.tool_calls_remaining = child.tool_calls_remaining.saturating_sub(1);
                return Ok(Admission::Reserved(None));
            }
            StepEnd::Stopped(StopCause::ActiveTime) => {
                return Ok(Admission::Exhausted);
            }
            StepEnd::Stopped(
                StopCause::Uncertain
                | StopCause::Cancelled
                | StopCause::HookStop
                | StopCause::Paused
                | StopCause::Budget
                | StopCause::Capacity
                | StopCause::ContextFull
                | StopCause::DelegationDeadline
                | StopCause::Refused(_)
                | StopCause::ToolStopped
                | StopCause::InvalidCommandResult,
            ) => {}
        }
        let disposition = researcher_end(
            self.actor_hook_requested_stop(agent_run_id)?,
            self.controls.cancellation.is_cancelled() || self.hook_requested_stop()?,
        );
        self.finish_child(agent_run_id, disposition, None, false)
            .await?;
        Ok(Admission::Reserved(Some(disposition)))
    }

    fn subagent_stop<'a>(
        &'a self,
        child: &'a mut ChildWork,
        answer: &'a Answer,
        turn: ProviderTurn,
        abort: &'a mut Option<WaveStop>,
        deadline: Instant,
    ) -> super::step::BoxedStep<'a, ChildStop> {
        Box::pin(self.subagent_stop_inner(child, answer, turn, abort, deadline))
    }

    async fn subagent_stop_inner(
        &self,
        child: &mut ChildWork,
        answer: &Answer,
        turn: ProviderTurn,
        abort: &mut Option<WaveStop>,
        deadline: Instant,
    ) -> Result<ChildStop, EngineError> {
        let run = match self
            .run_subagent_stop(child, &answer.text, turn, deadline)
            .await
        {
            Err(EngineError::Tool(crate::tools::ToolError::Uncertain))
                if child.kind.is_editor() =>
            {
                Ok(StopHookRun::Uncertain)
            }
            run => run,
        }?;
        if run == StopHookRun::Blocked && !child.kind.is_editor() {
            self.ledger().child_call_finished()?;
        }
        let decision = decide_child_stop(run, *abort, &self.child_facts(child, deadline)?);
        if let ChildStop::End {
            disposition,
            stops_wave,
            ..
        } = decision
        {
            if stops_wave {
                abort.get_or_insert(if self.controls.cancellation.is_cancelled() {
                    WaveStop::Abort(StopCause::Cancelled)
                } else {
                    WaveStop::BUDGET
                });
            }
            if !child.kind.is_editor() {
                self.finish_child(
                    child.agent_run_id,
                    disposition,
                    None,
                    run != StopHookRun::Blocked,
                )
                .await?;
            }
        }
        Ok(decision)
    }

    pub(super) async fn run_subagent_stop(
        &self,
        child: &mut ChildWork,
        message: &str,
        turn: ProviderTurn,
        deadline: Instant,
    ) -> Result<StopHookRun, EngineError> {
        let subject = child.hook_subject().to_owned();
        let fits = self
            .controls
            .tools
            .zip(child.tools.as_ref())
            .is_none_or(|(runtime, tools)| {
                tools.has_stop_room(&turn, runtime.context_limit_bytes())
            });
        let result = self
            .lifecycle_hooks(LifecycleHook {
                agent: child.agent_run_id,
                event: crate::tools::permissions::HookEvent::SubagentStop,
                subject: &subject,
                message,
                deadline: Some(deadline),
                feedback_fits: fits,
            })
            .await?;
        if self.controls.cancellation.is_cancelled() {
            return Ok(StopHookRun::Cancelled);
        }
        let feedback = match result {
            Ok(Some(feedback)) => feedback,
            Ok(None) => return Ok(StopHookRun::Passed),
            Err(HookRefusal::ActiveTime) => {
                return Ok(StopHookRun::CutOffByActiveTime);
            }
            Err(HookRefusal::Cancelled) => return Ok(StopHookRun::Cancelled),
            Err(HookRefusal::ContextFull) => return Ok(StopHookRun::CutOffByContext),
        };
        if let Some(tools) = child.tools.as_mut() {
            tools.add_stop_feedback(&turn, &feedback);
        }
        child
            .stop_hook_feedback
            .push(crate::provider::StopHookFeedback {
                after_step: child.tools.as_ref().map_or(0, |tools| tools.steps.len()),
                turn,
                text: feedback,
                facts: None,
                steering_notes: Vec::new(),
            });
        Ok(StopHookRun::Blocked)
    }

    fn hook_facts(&self) -> Result<HookFacts, EngineError> {
        Ok(HookFacts {
            uncertain: self.uncertain_hook()?,
            stop_requested: self.hook_requested_stop()?,
            settled: self.ledger().view(|view| {
                view.runs
                    .last()
                    .is_some_and(crate::session::RunView::hooks_settled)
            })?,
        })
    }

    fn child_facts(&self, child: &ChildWork, deadline: Instant) -> Result<ChildFacts, EngineError> {
        Ok(ChildFacts {
            has_tools: child.tools.is_some(),
            turns_left: child.turns_remaining > 0,
            tool_calls_left: child.tool_calls_remaining > 0,
            before_deadline: Instant::now() < deadline,
            cancelled: self.controls.cancellation.is_cancelled(),
            paused: self.pause_requested(),
            tokens_open: self.ledger().can_start_provider_call(),
            calls_left: self.ledger().remaining_calls() != Some(0),
            hook: self.hook_facts()?,
            actor_hook_stop: self.actor_hook_requested_stop(child.agent_run_id)?,
        })
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
        let (reports, pending, integrations) = self.ledger().view(|v| {
            let run = v.runs.last().expect("Run");
            let proposals: Vec<_> = run.primary().into_iter().flat_map(|a| &a.provider_calls).filter_map(|c| c.delegation.as_ref()).flatten().collect();
            let mut order = std::collections::HashMap::new();
            for agent in run.agents.iter().skip(1) {
                let index = proposals.iter().enumerate().find(|(i, p)| !order.values().any(|v| v == i) && match p {
                    crate::worker::ChildAssignment::Editor { assignment } => agent.assignment.as_ref().is_some_and(|w| &w.pins.request == assignment.as_ref()),
                    crate::worker::ChildAssignment::Researcher { objective, definition, profile } => agent.assignment.is_none() && agent.objective.as_ref() == Some(objective) && &agent.definition == definition && &agent.profile == profile,
                }).map(|(index, _)| index).expect("spawn matches its proposal");
                order.insert(agent.id, index);
            }
            let workers = run.worker_history();
            let mut reports = run
                .agents
                .iter()
                .skip(1)
                .filter(|a| a.status != crate::session::AgentStatus::Active)
                .map(|a| (order[&a.id], ChildResult {
                    objective: a.objective.clone().unwrap_or_default(),
                    summary: a
                        .summary
                        .clone()
                        .unwrap_or_else(|| "Worker did not finish".into()),
                    result: {
                        let history = workers.iter().find(|w| w.assignment_id == a.id);
                        let tool_facts = child_tool_facts(&a.tools);
                        let result = a.result.as_deref().unwrap_or(if history.is_some() {
                            "Unpublished private work discarded"
                        } else {
                            ""
                        });
                        let mut report = [
                            history.map(|history| history.description()).unwrap_or_default(),
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
                        report
                    },
                }))
                .collect::<Vec<_>>();
            for tool in &run.tools {
                if let ToolCall::PrepareWorker { preparation } = &tool.intent.call
                    && preparation.purpose == crate::worker::WorkerDataPurpose::Base
                    && !run.agents.iter().any(|a| a.id == preparation.assignment_id)
                    && let Some(o) = &tool.observation
                    && o.disposition != crate::tools::ToolDisposition::Succeeded {
                    let index = proposals.iter().enumerate().find(|(i, p)| matches!(p, crate::worker::ChildAssignment::Editor { .. }) && !order.values().any(|v| v == i)).map(|(i, _)| i).expect("refused capture matches a proposal");
                    order.insert(preparation.assignment_id, index);
                    reports.push((index, ChildResult { objective: "Editor base preparation".into(), summary: "Assignment did not start".into(), result: format!("Assignment {}: {:?}; no private work or Workspace publication", preparation.assignment_id, o.disposition) }));
                }
            }
            let pending = run
                .agents
                .iter()
                .skip(1)
                .filter(|a| a.status == crate::session::AgentStatus::Active)
                .map(|a| a.id.to_string())
                .collect::<Vec<_>>();
            let wave = run.primary().into_iter().flat_map(|a| &a.provider_calls).filter(|c| c.delegation.is_some()).count();
            let observations = run
                .live_tool_records()
                .map(|(_, tool)| tool)
                .filter(|t| {
                    matches!(
                        &t.intent.call,
                        ToolCall::ApplyChangeset {
                            source: crate::worker::ChangesetSource::WorkerHandover { assignment_id, .. }
                        } if run.agents.iter().find(|a| a.id == *assignment_id).and_then(|a| a.assignment.as_ref()).is_some_and(|w| usize::from(w.pins.request.wave) == wave)
                    )
                })
                .filter(|t| t.observation.is_some())
                .cloned()
                .collect::<Vec<_>>();
            reports.sort_by_key(|(index, _)| *index);
            (reports.into_iter().map(|(_, report)| report).collect::<Vec<_>>(), pending, observations)
        })?;
        root.input.child_results = reports;
        if let Some(tools) = root.input.tools.as_mut() {
            let integrations: Vec<_> = integrations
                .iter()
                .filter(|tool| {
                    !tools
                        .steps
                        .iter()
                        .flat_map(|step| &step.observations)
                        .any(|observation| observation.intent.id == tool.intent.id)
                })
                .collect();
            if !integrations.is_empty() {
                if tools
                    .steps
                    .last()
                    .is_none_or(|step| !step.requests.is_empty() || step.cleared)
                {
                    tools.steps.push(ToolStep {
                        refusal: None,
                        plan: None,
                        turn: ProviderTurn::scripted(),
                        requests: Vec::new(),
                        observations: Vec::new(),
                        facts: None,
                        steering_notes: Vec::new(),
                        calls_remaining: None,
                        cleared: false,
                        hook_feedback: Default::default(),
                    });
                }
                let step = tools.steps.last_mut().ok_or(EngineError::InvalidHistory)?;
                for tool in integrations {
                    let index = step.observations.len();
                    let feedback = tool.hook_feedback();
                    if !feedback.is_empty() {
                        step.hook_feedback.insert(index, feedback);
                    }
                    step.observations.push(
                        tool.observation
                            .clone()
                            .ok_or(EngineError::InvalidHistory)?,
                    );
                }
            }
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

    // Honor pause before reporting an exhausted queued-child call budget.
    fn unstarted_stop(&self, queue: &ChildQueue) -> Option<WaveStop> {
        if self.pause_requested() {
            Some(WaveStop::Boundary(StopCause::Paused))
        } else if let Some(
            cause @ (StopCause::ContextFull
            | StopCause::Cancelled
            | StopCause::HookStop
            | StopCause::ActiveTime
            | StopCause::DelegationDeadline),
        ) = queue.stop
        {
            Some(WaveStop::Boundary(cause))
        } else if queue.stop.is_some() {
            Some(WaveStop::BUDGET)
        } else if queue.pending.is_empty() && queue.unspawned.is_empty() {
            None
        } else if !self.ledger().can_start_provider_call()
            || self.ledger().remaining_calls() == Some(0)
        {
            Some(WaveStop::BUDGET)
        } else {
            None
        }
    }

    async fn fill_slots(&self, wave: &mut ChildWave<'_>) -> Result<RunProgress<()>, EngineError> {
        if let Some(deadline) = &wave.queue.active_deadline
            && deadline.reached()
        {
            wave.queue.stop = Some(match deadline.cause {
                ActiveDeadlineCause::Goal => StopCause::ActiveTime,
                ActiveDeadlineCause::Delegation => StopCause::DelegationDeadline,
            });
        }
        while wave.queue.active.len() < self.ledger().provider_concurrency(wave.queue.parallel)
            && !self.controls.cancellation.is_cancelled()
            && !self.pause_requested()
            && self.ledger().can_start_provider_call()
            && self.ledger().remaining_calls() != Some(0)
            && wave.queue.stop.is_none()
            && !self.worker_stopped()?
        {
            if wave.queue.pending.is_empty() {
                match self
                    .fill_primary_slot(wave.queue, wave.root_request, wave.deadline())
                    .await?
                {
                    Admission::Reserved(SlotDispatch::Started) => continue,
                    Admission::Reserved(SlotDispatch::Stopped) => {}
                    Admission::Exhausted => return Ok(RunProgress::Stopped(RunRefusal::Budget)),
                }
            }
            if wave.queue.stop.is_some() {
                break;
            }
            let index = match self.next_child_slot(wave).await? {
                RunProgress::Ready(ChildSlot::Ready(index)) => index,
                RunProgress::Ready(ChildSlot::Stopped) => break,
                RunProgress::Ready(ChildSlot::Retry) => continue,
                RunProgress::Stopped(cause) => return Ok(RunProgress::Stopped(cause)),
            };
            match self.fill_child_slot(wave, index).await? {
                Admission::Reserved(SlotDispatch::Started) => {}
                Admission::Reserved(SlotDispatch::Stopped) => break,
                Admission::Exhausted => return Ok(RunProgress::Stopped(RunRefusal::Budget)),
            }
        }
        Ok(RunProgress::Ready(()))
    }

    async fn next_child_slot(
        &self,
        wave: &mut ChildWave<'_>,
    ) -> Result<RunProgress<ChildSlot>, EngineError> {
        let index = if let Some(index) = wave.queue.pending.pop_front() {
            index
        } else {
            let child = match self
                .spawn_next_child(
                    wave.queue,
                    wave.root_request,
                    wave.segments,
                    wave.deadline(),
                )
                .await?
            {
                RunProgress::Ready(Some(child)) => child,
                RunProgress::Ready(None) => {
                    return Ok(RunProgress::Ready(if wave.queue.unspawned.is_empty() {
                        ChildSlot::Stopped
                    } else {
                        ChildSlot::Retry
                    }));
                }
                RunProgress::Stopped(RunRefusal::Capacity) => {
                    return Ok(RunProgress::Stopped(RunRefusal::Capacity));
                }
                RunProgress::Stopped(RunRefusal::Budget) => {
                    wave.queue.stop.get_or_insert(StopCause::ActiveTime);
                    return Ok(RunProgress::Ready(ChildSlot::Stopped));
                }
            };
            let index = wave.children.len();
            wave.children.push(child);
            index
        };
        Ok(RunProgress::Ready(ChildSlot::Ready(index)))
    }

    async fn fill_child_slot(
        &self,
        wave: &mut ChildWave<'_>,
        index: usize,
    ) -> Result<Admission<SlotDispatch>, EngineError> {
        let child = &mut wave.children[index];
        let researcher = child.tools.is_some();
        let mut request = self.child_request(child, wave.root_request)?;
        let provider = self.routing.bind(
            crate::provider::ProviderRole::Child,
            child.agent_run_id,
            &mut request.model,
            &mut request.max_output_tokens,
        )?;
        if child.kind.is_editor() && !self.admit_worker_call(wave.queue).await? {
            wave.queue.pending.push_front(index);
            return Ok(Admission::Reserved(SlotDispatch::Stopped));
        }
        let Admission::Reserved(()) = self
            .reserve_provider_call(
                child.agent_run_id,
                Action::Child {
                    in_flight: self.ledger().in_flight_children(),
                    research: researcher,
                },
                0,
            )
            .await?
        else {
            return Ok(Admission::Exhausted);
        };
        if self.controls.cancellation.is_cancelled()
            || self.pause_requested()
            || self.worker_stopped()?
            || self.hook_requested_stop()?
            || wave.queue.stop.is_some()
        {
            let result = CallResult::Cancelled;
            self.append(Event::ProviderCallRecorded {
                run_id: self.run_id,
                agent_run_id: child.agent_run_id,
                record: call_record(AgentPhase::ChildWork, &result, false, None, None, None),
            })
            .await?;
            self.settle_result(&result)?;
            self.ledger().child_call_finished()?;
            wave.queue.pending.push_front(index);
            return Ok(Admission::Reserved(SlotDispatch::Stopped));
        }
        self.child_steering_note(child, &mut request).await?;
        self.spawn_child_attempt(wave.queue, index, request, provider, wave.deadline())?;
        Ok(Admission::Reserved(SlotDispatch::Started))
    }

    async fn child_steering_note(
        &self,
        child: &mut ChildWork,
        request: &mut ProviderRequest,
    ) -> Result<(), EngineError> {
        if let Some(steering) = &self.controls.steering {
            let target = crate::engine::ChildSteeringTarget {
                session_id: self.session_id,
                run_id: self.run_id,
                agent_run_id: child.agent_run_id,
            };
            if let Some(text) = steering.take_child_note(target) {
                self.append(Event::AgentSteeringNoteRecorded {
                    goal_id: self.goal()?,
                    run_id: self.run_id,
                    agent_run_id: child.agent_run_id,
                    text: text.clone(),
                })
                .await?;
                super::place_steering_note(&mut request.input, text);
                child.steering_notes = request.input.steering_notes.clone();
                child.tools = request.input.tools.clone();
                child.stop_hook_feedback = request.input.stop_hook_feedback.clone();
            }
        }
        Ok(())
    }

    fn spawn_child_attempt(
        &self,
        queue: &mut ChildQueue,
        index: usize,
        request: ProviderRequest,
        provider: std::sync::Arc<dyn crate::provider::Provider>,
        deadline: Instant,
    ) -> Result<(), EngineError> {
        let preview = (self
            .routing
            .chain(crate::provider::ProviderRole::Child, request.agent_run_id)?
            .len()
            > 1)
        .then(|| crate::provider::ResponsePreview::new().0);
        self.spawn_attempt(
            queue,
            ActiveChild {
                limits: crate::provider::RateLimitSlot::default(),
                index: Some(index),
                request,
            },
            provider,
            preview,
            deadline,
        );
        Ok(())
    }

    fn spawn_attempt(
        &self,
        queue: &mut ChildQueue,
        active: ActiveChild,
        provider: std::sync::Arc<dyn crate::provider::Provider>,
        preview: Option<crate::provider::ResponsePreview>,
        deadline: Instant,
    ) {
        let cancellation = self.controls.cancellation.clone();
        let request = active.request.clone();
        let limits = active.limits.clone();
        let provider_activity = self.ledger().provider_activity();
        let handle = queue.calls.spawn(async move {
            let _provider_work = provider_activity.start();
            crate::engine::routing::invoke_attempt(
                provider.as_ref(),
                request,
                deadline,
                cancellation,
                preview,
                limits,
                crate::fault::LatencyReceipt::default(),
            )
            .await
        });
        queue.active.insert(handle.id(), active);
    }

    fn child_request(
        &self,
        child: &mut ChildWork,
        root_request: &ProviderRequest,
    ) -> Result<ProviderRequest, EngineError> {
        let mut context: Vec<String> = child
            .definition
            .iter()
            .map(|definition| format!("Agent definition:\n{}", definition.body))
            .collect();
        if child.kind.is_editor() {
            let assignment = self.proposed_editor(child.agent_run_id)?;
            context.push(format!(
                "Worker assignment: {}",
                serde_json::to_string(&assignment).map_err(|_| EngineError::InvalidRequest)?,
            ));
            if let Some(s) = &child.source_context {
                context.push(format!(
                    "Historical source (bounded; no effects or authority resume): {}",
                    serde_json::json!({"objective":s.objective,"summary":s.summary,"result":s.result})
                ));
            }
        }
        let context = (!context.is_empty()).then(|| context.join("\n\n"));
        child.turns_remaining = child.turns_remaining.saturating_sub(1);
        if child.turns_remaining == 0 {
            child.tool_calls_remaining = 0;
        }
        if let Some(tools) = child.tools.as_mut() {
            let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
            let room = runtime
                .result_capacity(
                    tools.rendered_len()
                        + root_request
                            .input
                            .workspace_guidance
                            .iter()
                            .map(|guidance| {
                                crate::provider::instructions_text(&guidance.path, &guidance.text)
                                    .len()
                            })
                            .sum::<usize>()
                        + context.as_ref().map_or(0, String::len)
                        + CHILD_INTENT_RESERVE_BYTES
                        + if self.controls.steering.is_some() {
                            crate::session::MAX_STEERING_CONTEXT_BYTES
                        } else {
                            0
                        },
                    crate::tools::ToolRuntime::step_context_reserve(
                        false,
                        Self::maximum_feedback_reserve(runtime),
                    ),
                )
                .is_some_and(|capacity| capacity >= MIN_CHILD_RESULT_BYTES);
            if !room {
                child.tool_calls_remaining = 0;
            }
            // Keep the request head frozen; later calls carry their remaining budget in the newest step.
            match tools.steps.last_mut() {
                Some(step) => step.calls_remaining = Some(child.tool_calls_remaining),
                None => {
                    if child.kind.is_editor() {
                        let mut catalog: serde_json::Value = serde_json::from_str(tools.catalog())
                            .map_err(|_| EngineError::InvalidRequest)?;
                        catalog["tool_calls_remaining"] =
                            serde_json::json!(child.tool_calls_remaining);
                        tools.set_catalog(&catalog);
                    } else {
                        let refreshed = runtime.research_context(child.tool_calls_remaining)?;
                        tools.set_catalog(
                            &serde_json::from_str(refreshed.catalog())
                                .map_err(|_| EngineError::InvalidRequest)?,
                        );
                    }
                    // Only the first call rebuilds the catalog; later calls keep this restriction.
                    if !child.kind.is_editor()
                        && let Some(definition) = &child.definition
                    {
                        restrict_catalog(tools, definition)?;
                    }
                }
            }
        }
        let researcher = child.tools.is_some();
        let root = &root_request.input;
        let request = ProviderRequest {
            session_id: self.session_id,
            run_id: self.run_id,
            agent_run_id: child.agent_run_id,
            phase: AgentPhase::ChildWork,
            collaboration: CollaborationPolicy::Single,
            model: String::new(),
            max_output_tokens: if child.kind.is_editor() {
                MAX_OUTPUT_TOKEN_CAP
            } else {
                DEFAULT_OUTPUT_TOKEN_CAP
            },
            input: RunInput {
                images: root
                    .images
                    .iter()
                    .filter(|value| value.origin == crate::provider::ImageOrigin::Objective)
                    .cloned()
                    .collect(),
                goal_budget: if self.ledger().autonomous()
                    || self.ledger().plan_contract()?.is_some()
                {
                    Some(self.ledger().effective_budget()?)
                } else {
                    None
                },
                goal_plan_contract: if researcher {
                    self.ledger().plan_contract()?
                } else {
                    None
                },
                steering_notes: child.steering_notes.clone(),
                stop_hook_feedback: child.stop_hook_feedback.clone(),
                workspace_guidance: root.workspace_guidance.clone(),
                assignment_context: context,
                includes: root.includes.clone(),
                tools: child.tools.clone(),
                ..RunInput::new(
                    if child.kind.is_editor() {
                        PromptRole::Editor
                    } else if researcher {
                        PromptRole::Researcher
                    } else {
                        PromptRole::ReadOnly
                    },
                    child.objective.clone(),
                )
            },
        };
        Ok(request)
    }

    async fn fill_primary_slot(
        &self,
        queue: &mut ChildQueue,
        root_request: &mut ProviderRequest,
        deadline: Instant,
    ) -> Result<Admission<SlotDispatch>, EngineError> {
        // Settled agents only grow, so a deferral ends once more agents settle than it saw.
        let deferred = match queue.deferred {
            Some((_, at)) => self.settled_agents()? <= at,
            None => false,
        };
        if !queue.primary_pending
            && !deferred
            && queue.stop.is_none()
            && self.controls.tools.is_some()
            && self.coordination_ready()?
        {
            self.refresh_team_context(root_request)?;
            if let (Some((reason, _)), Some(tools)) =
                (queue.deferred.take(), root_request.input.tools.as_mut())
            {
                let mut catalog: serde_json::Value = serde_json::from_str(tools.catalog())
                    .map_err(|_| EngineError::InvalidRequest)?;
                catalog["last_answer_refused"] = serde_json::json!(reason.text());
                tools.set_catalog(&catalog);
            }
            root_request.phase = AgentPhase::RootSynthesis;
            match self
                .maintain_primary_context(root_request, Some(deadline))
                .await?
            {
                super::tool_compaction::PrimaryContext::Ready => {}
                super::tool_compaction::PrimaryContext::Stop(cause) => {
                    queue.stop.get_or_insert(cause);
                    return Ok(Admission::Reserved(SlotDispatch::Stopped));
                }
            }
            let mut request = root_request.clone();
            self.routing.bind(
                crate::provider::ProviderRole::Primary,
                self.primary_id,
                &mut request.model,
                &mut request.max_output_tokens,
            )?;
            if !self.admit_worker_call(queue).await? {
                return Ok(Admission::Reserved(SlotDispatch::Stopped));
            }
            let Admission::Reserved(()) = self
                .reserve_provider_call(self.primary_id, self.primary_call(), 0)
                .await?
            else {
                return Ok(Admission::Exhausted);
            };
            self.primary_context(root_request).await?;
            request.input = root_request.input.clone();
            self.spawn_wave_primary(queue, request, deadline)?;
            queue.primary_pending = true;
            return Ok(Admission::Reserved(SlotDispatch::Started));
        }
        Ok(Admission::Reserved(SlotDispatch::Stopped))
    }

    fn spawn_wave_primary(
        &self,
        queue: &mut ChildQueue,
        request: ProviderRequest,
        deadline: Instant,
    ) -> Result<(), EngineError> {
        let provider = self
            .routing
            .provider(crate::provider::ProviderRole::Primary, self.primary_id)?;
        self.spawn_attempt(
            queue,
            ActiveChild {
                limits: crate::provider::RateLimitSlot::default(),
                index: None,
                request,
            },
            provider,
            Some(crate::provider::ResponsePreview::new().0),
            deadline,
        );
        Ok(())
    }

    async fn admit_worker_call(&self, queue: &mut ChildQueue) -> Result<bool, EngineError> {
        match self.admit_worker_action(0).await {
            Ok(()) => Ok(true),
            Err(
                error @ EngineError::Store(
                    crate::store::StoreError::ReplayLimit | crate::store::StoreError::StorageFull,
                ),
            ) => {
                self.record_capacity_stop().await?;
                queue.stop.get_or_insert(StopCause::Capacity);
                queue.capacity_error = Some(error);
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn valid_child_tools(&self, child: &ChildWork, requests: &[ToolRequest]) -> bool {
        matches!(requests, [request]
        if child.tools.is_some()
            && child.tool_calls_remaining > 0 && child.turns_remaining > 0
            && request.call.valid()
            && self.model_may_propose(&request.call)
            && (if child.kind.is_editor() {
                self.proposed_editor(child.agent_run_id)
                    .and_then(|a| self.editor_grant(&a))
                    .is_ok_and(|g| g.allows(&request.call))
            } else {
                request.call.is_child_research()
                    && child.definition.as_ref().is_none_or(|definition| definition.allows(&request.call))
            }))
    }

    async fn finish_child(
        &self,
        agent_run_id: AgentRunId,
        disposition: AgentDisposition,
        answer: Option<&Answer>,
        call_reserved: bool,
    ) -> Result<(), EngineError> {
        self.append(Event::AgentFinished {
            worker: None,
            run_id: self.run_id,
            agent_run_id,
            disposition,
            summary: answer.map(|value| crate::session::answer_summary(&value.text)),
            result: answer.map(|value| value.text.clone()),
        })
        .await?;
        if call_reserved {
            self.ledger().child_call_finished()?;
        }
        Ok(())
    }
}

fn child_tool_facts(tools: &[crate::session::ToolView]) -> String {
    use crate::tools::ToolDisposition;

    let facts: Vec<_> = tools
        .iter()
        .filter_map(|tool| {
            let observation = tool.observation.as_ref()?;
            let command_failure = child_command_failure(tool);
            let cause = match observation.disposition {
                ToolDisposition::Succeeded if command_failure.is_none() => return None,
                ToolDisposition::Succeeded => "succeeded",
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
            let mut fact = format!("Tool {}: {cause}", tool.intent.call.label());
            if let Some(details) = command_failure {
                fact.push_str("; ");
                fact.push_str(&details);
            }
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

fn child_command_failure(tool: &crate::session::ToolView) -> Option<String> {
    use crate::worker::{PrivateCapture, PrivateCaptureRejection, WorkerCommandResult};

    let observation = tool.observation.as_ref()?;
    if !matches!(
        observation.attestation,
        Some(crate::tools::Attestation::WorkerCommand { .. })
    ) || !observation.attested_for(&tool.intent)
    {
        return None;
    }
    let result =
        WorkerCommandResult::parse(&observation.output).filter(WorkerCommandResult::valid)?;
    let rejection = match result.private_capture {
        PrivateCapture::Rejected { cause } => Some(match cause {
            PrivateCaptureRejection::OutOfLease => "out_of_lease",
            PrivateCaptureRejection::Overflow => "overflow",
            PrivateCaptureRejection::UnsupportedEntry => "unsupported_entry",
        }),
        PrivateCapture::None {} | PrivateCapture::Sealed { .. } => None,
    };
    if result.exit_code == Some(0) && rejection.is_none() {
        return None;
    }
    let mut details = format!(
        "exit code {}",
        result
            .exit_code
            .map_or_else(|| "unknown".into(), |code| code.to_string())
    );
    if let Some(cause) = rejection {
        details.push_str("; private capture rejected: ");
        details.push_str(cause);
    }
    Some(details)
}

fn classify_join(
    joined: Result<(Id, crate::engine::routing::AttemptResult), JoinError>,
) -> (Id, crate::engine::routing::AttemptResult) {
    match joined {
        Ok((task_id, result)) => (task_id, result),
        Err(error) => (
            error.id(),
            crate::engine::routing::AttemptResult {
                result: if error.is_cancelled() {
                    CallResult::Cancelled
                } else {
                    CallResult::Failed(CallFailure::TaskPanic)
                },
                transient: None,
                visible: false,
            },
        ),
    }
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
            host_command: None,
            target: crate::session::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: crate::session::RunId::new(),
            agent_run_id: crate::session::AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: ToolCall::List {
                path: "notes".into(),
            },
            limits: ToolLimits::default(),
            use_count: 1,
        };
        let denied = crate::session::ToolView {
            checkpoint: None,
            intent: intent.clone(),
            observation: Some(ToolObservation {
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
                cache_update: CacheUpdate::NotApplicable,
            }),
            review_call_index: None,
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
                    "Recent child Tool outcomes:\n- Tool list: {}",
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
                "version":1,"workspace_paths":["notes"],"write":false,
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

#[cfg(test)]
mod decision_tests {
    use super::*;

    #[test]
    fn delayed_pre_wave_time_reduces_the_active_deadline_allowance() {
        let started = Instant::now();
        let carried_ms = duration_ms(Duration::from_secs(20));
        let delayed_ms = duration_ms(Duration::from_secs(21));
        let bound = Duration::from_secs(120);
        let requested_ms = duration_ms(bound)
            .saturating_add(duration_ms(crate::engine::STORE_ACTION_OVERHEAD))
            .saturating_add(carried_ms);
        let sampled_at = started + Duration::from_millis(delayed_ms);
        for (limit, reserved_ms, expected_cause) in [
            (
                ActiveReservationLimit::ActionBound,
                requested_ms,
                ActiveDeadlineCause::Delegation,
            ),
            (
                ActiveReservationLimit::Goal,
                requested_ms.saturating_sub(15_000),
                ActiveDeadlineCause::Goal,
            ),
        ] {
            let segment = ActiveSegment {
                started,
                reserved_ms,
                limit,
            };
            let (allowance_ms, cause) = active_deadline_allowance(&segment, bound, sampled_at);

            assert_eq!(
                allowance_ms,
                reserved_ms
                    .saturating_sub(delayed_ms)
                    .saturating_sub(duration_ms(crate::engine::STORE_ACTION_OVERHEAD))
            );
            assert_eq!(cause, expected_cause);
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Kind {
        ToolStep,
        Finish { stop_hook: bool },
        Reprompt(RepromptReason),
        End(ChildEnd),
    }

    // (call, valid, gate (editors only), wave abort, facts, decision)
    type Row = (
        CallResult,
        bool,
        Option<GateFacts>,
        Option<WaveStop>,
        ChildFacts,
        Kind,
    );

    fn kind(decision: &ChildDecision) -> Kind {
        match decision {
            ChildDecision::ToolStep(..) => Kind::ToolStep,
            ChildDecision::Finish { stop_hook, .. } => Kind::Finish {
                stop_hook: *stop_hook,
            },
            ChildDecision::Reprompt(reason, _) => Kind::Reprompt(*reason),
            ChildDecision::End(end) => Kind::End(*end),
        }
    }

    fn response(outcome: ProviderOutcome) -> CallResult {
        CallResult::Response(ProviderResponse {
            outcome,
            turn: ProviderTurn::new("test", "[{}]", "absent-secret", 4096).expect("minimal turn"),
            response_id: None,
            usage: crate::provider::Usage::default(),
            wire_provenance: None,
        })
    }

    fn answer() -> CallResult {
        response(ProviderOutcome::Answer(Answer {
            text: "done".into(),
        }))
    }

    fn tools() -> CallResult {
        response(ProviderOutcome::Tools(vec![ToolRequest {
            id: crate::provider::ToolUseId::new("call").expect("id"),
            call: ToolCall::List { path: ".".into() },
        }]))
    }

    fn timeout() -> CallResult {
        CallResult::Failed(CallFailure::Timeout)
    }

    const OPEN: ChildFacts = ChildFacts {
        has_tools: true,
        turns_left: true,
        tool_calls_left: true,
        before_deadline: true,
        cancelled: false,
        paused: false,
        tokens_open: true,
        calls_left: true,
        hook: HookFacts {
            uncertain: false,
            stop_requested: false,
            settled: true,
        },
        actor_hook_stop: false,
    };

    const GATE: GateFacts = GateFacts {
        unmet: false,
        worker_stopped: false,
        stop_room: true,
        continuations_left: true,
    };

    fn with(change: fn(&mut ChildFacts)) -> ChildFacts {
        let mut facts = OPEN;
        change(&mut facts);
        facts
    }

    fn gate(change: fn(&mut GateFacts)) -> GateFacts {
        let mut gate = GATE;
        gate.unmet = true;
        change(&mut gate);
        gate
    }

    #[test]
    fn child_calls_decide_by_kind_outcome_and_facts() {
        use AgentDisposition::{Cancelled, Failed, StoppedByHook};
        let editor = |cause| Kind::End(ChildEnd::Editor(cause));
        let researcher = |disposition| Kind::End(ChildEnd::Researcher(disposition));
        let skipped = |disposition, budget| {
            Kind::End(ChildEnd::Skipped {
                disposition,
                budget,
            })
        };
        let unverified = editor(WorkerCause::CriterionUnverified);
        let reprompt = Kind::Reprompt(RepromptReason::CriterionNotMetInView);
        let torn = Some(WaveStop::Abort(StopCause::ToolStopped));
        let hook_stop = Some(WaveStop::Boundary(StopCause::HookStop));
        let invalid = || response(ProviderOutcome::Answer(Answer { text: " ".into() }));
        let rows: Vec<Row> = vec![
            (
                answer(),
                true,
                Some(GATE),
                None,
                OPEN,
                Kind::Finish { stop_hook: true },
            ),
            (
                answer(),
                true,
                Some(GATE),
                torn,
                OPEN,
                editor(WorkerCause::ToolFailed),
            ),
            (answer(), true, Some(gate(|_| {})), None, OPEN, reprompt),
            (
                answer(),
                true,
                Some(gate(|g| g.worker_stopped = true)),
                None,
                OPEN,
                unverified,
            ),
            (
                answer(),
                true,
                Some(gate(|g| g.stop_room = false)),
                None,
                OPEN,
                unverified,
            ),
            (
                answer(),
                true,
                Some(gate(|g| g.continuations_left = false)),
                None,
                OPEN,
                unverified,
            ),
            (answer(), true, Some(gate(|_| {})), torn, OPEN, unverified),
            (
                answer(),
                true,
                Some(gate(|_| {})),
                None,
                with(|f| f.turns_left = false),
                unverified,
            ),
            (
                answer(),
                true,
                Some(gate(|_| {})),
                None,
                with(|f| f.tool_calls_left = false),
                unverified,
            ),
            (
                answer(),
                true,
                Some(gate(|_| {})),
                None,
                with(|f| f.before_deadline = false),
                unverified,
            ),
            (
                answer(),
                true,
                Some(gate(|_| {})),
                None,
                with(|f| f.tokens_open = false),
                unverified,
            ),
            (
                answer(),
                true,
                Some(gate(|_| {})),
                None,
                with(|f| f.calls_left = false),
                unverified,
            ),
            (tools(), true, Some(GATE), None, OPEN, Kind::ToolStep),
            (
                tools(),
                true,
                Some(GateFacts {
                    worker_stopped: true,
                    ..GATE
                }),
                None,
                OPEN,
                editor(WorkerCause::ToolFailed),
            ),
            (
                tools(),
                true,
                Some(GATE),
                hook_stop,
                OPEN,
                editor(WorkerCause::ToolFailed),
            ),
            (
                tools(),
                false,
                Some(GATE),
                None,
                with(|f| f.turns_left = false),
                editor(WorkerCause::TurnLimit),
            ),
            (
                tools(),
                false,
                Some(GATE),
                None,
                OPEN,
                editor(WorkerCause::InvalidOutcome),
            ),
            (
                invalid(),
                false,
                Some(GATE),
                None,
                OPEN,
                editor(WorkerCause::InvalidOutcome),
            ),
            (
                CallResult::Failed(CallFailure::OutputLimit),
                false,
                Some(GATE),
                None,
                OPEN,
                editor(WorkerCause::InvalidOutcome),
            ),
            (
                CallResult::Failed(CallFailure::InvalidResponse(None)),
                false,
                Some(GATE),
                None,
                OPEN,
                editor(WorkerCause::InvalidOutcome),
            ),
            (
                timeout(),
                false,
                Some(GATE),
                None,
                OPEN,
                editor(WorkerCause::ToolFailed),
            ),
            (
                CallResult::Cancelled,
                false,
                Some(GATE),
                None,
                OPEN,
                editor(WorkerCause::ToolFailed),
            ),
            (
                answer(),
                true,
                None,
                None,
                OPEN,
                Kind::Finish { stop_hook: true },
            ),
            (
                answer(),
                true,
                None,
                None,
                with(|f| f.has_tools = false),
                Kind::Finish { stop_hook: false },
            ),
            (
                answer(),
                true,
                None,
                None,
                with(|f| f.cancelled = true),
                Kind::Finish { stop_hook: false },
            ),
            (
                answer(),
                true,
                None,
                Some(WaveStop::BUDGET),
                OPEN,
                Kind::Finish { stop_hook: false },
            ),
            (answer(), true, None, hook_stop, OPEN, researcher(Cancelled)),
            (
                answer(),
                true,
                None,
                None,
                with(|f| f.hook.uncertain = true),
                researcher(Cancelled),
            ),
            (
                answer(),
                true,
                None,
                None,
                with(|f| f.hook.stop_requested = true),
                researcher(Cancelled),
            ),
            (
                tools(),
                true,
                None,
                None,
                with(|f| f.hook.settled = false),
                researcher(Cancelled),
            ),
            (tools(), true, None, None, OPEN, Kind::ToolStep),
            (
                tools(),
                true,
                None,
                None,
                with(|f| f.calls_left = false),
                skipped(Cancelled, true),
            ),
            (
                tools(),
                true,
                None,
                None,
                with(|f| f.tokens_open = false),
                skipped(Cancelled, true),
            ),
            (
                tools(),
                true,
                None,
                None,
                with(|f| f.paused = true),
                skipped(Cancelled, false),
            ),
            (tools(), true, None, torn, OPEN, skipped(Cancelled, false)),
            (
                tools(),
                true,
                None,
                None,
                with(|f| {
                    f.cancelled = true;
                    f.actor_hook_stop = true;
                }),
                skipped(StoppedByHook, false),
            ),
            (invalid(), false, None, None, OPEN, researcher(Failed)),
            (timeout(), false, None, None, OPEN, researcher(Failed)),
            (
                CallResult::Cancelled,
                false,
                None,
                None,
                OPEN,
                researcher(Cancelled),
            ),
        ];
        for (index, (result, valid, gate, abort, facts, expected)) in rows.into_iter().enumerate() {
            assert_eq!(
                kind(&decide_child_call(result, valid, gate, abort, &facts)),
                expected,
                "row {index}: {gate:?} {abort:?} {facts:?}"
            );
        }
    }

    #[test]
    fn subagent_stop_runs_decide_the_child_end() {
        use AgentDisposition::{Cancelled, Failed, StoppedByHook};
        let end = |disposition, cause, stops_wave| ChildStop::End {
            disposition,
            cause,
            stops_wave,
        };
        let unsettled = with(|f| f.hook.settled = false);
        let budget = StopHookRun::CutOffByActiveTime;
        let rows = [
            (
                StopHookRun::Cancelled,
                None,
                with(|f| f.cancelled = true),
                end(Cancelled, WorkerCause::Cancelled, true),
            ),
            (StopHookRun::Passed, None, OPEN, ChildStop::Proceed),
            (
                StopHookRun::Passed,
                None,
                with(|f| f.cancelled = true),
                end(Cancelled, WorkerCause::Cancelled, true),
            ),
            (
                StopHookRun::Passed,
                None,
                with(|f| {
                    f.cancelled = true;
                    f.actor_hook_stop = true;
                }),
                end(Cancelled, WorkerCause::Cancelled, true),
            ),
            (
                StopHookRun::Passed,
                None,
                with(|f| f.actor_hook_stop = true),
                end(StoppedByHook, WorkerCause::StoppedByHook, false),
            ),
            (budget, None, OPEN, end(Failed, WorkerCause::Budget, false)),
            (
                budget,
                None,
                unsettled,
                end(Failed, WorkerCause::Budget, true),
            ),
            (
                budget,
                Some(WaveStop::BUDGET),
                unsettled,
                end(Failed, WorkerCause::Budget, false),
            ),
            (
                StopHookRun::CutOffByContext,
                None,
                with(|f| f.cancelled = true),
                end(Cancelled, WorkerCause::ToolFailed, false),
            ),
            (
                StopHookRun::Uncertain,
                None,
                OPEN,
                end(Failed, WorkerCause::ToolFailed, false),
            ),
            (
                StopHookRun::Uncertain,
                None,
                with(|f| f.cancelled = true),
                end(Failed, WorkerCause::ToolFailed, false),
            ),
            (StopHookRun::Blocked, None, OPEN, ChildStop::Reprompt),
            (
                StopHookRun::Blocked,
                None,
                with(|f| f.turns_left = false),
                end(Failed, WorkerCause::TurnLimit, false),
            ),
            (
                StopHookRun::Blocked,
                None,
                with(|f| f.before_deadline = false),
                end(Failed, WorkerCause::Budget, false),
            ),
            (
                StopHookRun::Blocked,
                None,
                with(|f| f.tokens_open = false),
                end(Failed, WorkerCause::Budget, false),
            ),
            (
                StopHookRun::Blocked,
                None,
                with(|f| {
                    f.turns_left = false;
                    f.calls_left = false;
                }),
                end(Failed, WorkerCause::Budget, false),
            ),
        ];
        for (index, (run, abort, facts, expected)) in rows.into_iter().enumerate() {
            assert_eq!(
                decide_child_stop(run, abort, &facts),
                expected,
                "row {index}: {run:?} {abort:?} {facts:?}"
            );
        }
    }

    #[test]
    fn wave_stops_after_a_child_by_one_order() {
        use AgentDisposition::{Cancelled, Failed, Finished, StoppedByHook};
        let settled = OPEN.hook;
        let uncertain = HookFacts {
            uncertain: true,
            ..settled
        };
        let requested = HookFacts {
            stop_requested: true,
            ..settled
        };
        let unsettled = HookFacts {
            settled: false,
            ..settled
        };
        let abort = |cause| Some(WaveStop::Abort(cause));
        let boundary = |cause| Some(WaveStop::Boundary(cause));
        // (queue stop, cancelled, paused, child end, hook state, wave stop)
        let rows = [
            (
                Some(StopCause::Budget),
                true,
                true,
                Some(Failed),
                uncertain,
                Some(WaveStop::BUDGET),
            ),
            (
                None,
                true,
                true,
                Some(StoppedByHook),
                uncertain,
                abort(StopCause::Cancelled),
            ),
            (
                None,
                false,
                true,
                Some(StoppedByHook),
                uncertain,
                boundary(StopCause::Paused),
            ),
            (
                None,
                false,
                false,
                Some(StoppedByHook),
                uncertain,
                boundary(StopCause::HookStop),
            ),
            (
                None,
                false,
                false,
                Some(Failed),
                uncertain,
                abort(StopCause::Uncertain),
            ),
            (
                None,
                false,
                false,
                Some(Failed),
                requested,
                boundary(StopCause::HookStop),
            ),
            (
                None,
                false,
                false,
                Some(Failed),
                unsettled,
                abort(StopCause::ToolStopped),
            ),
            (
                None,
                false,
                false,
                Some(Failed),
                settled,
                abort(StopCause::ToolStopped),
            ),
            (
                None,
                false,
                false,
                Some(Cancelled),
                settled,
                abort(StopCause::Cancelled),
            ),
            (None, false, false, Some(Finished), settled, None),
            (None, false, false, None, settled, None),
            (
                None,
                false,
                false,
                None,
                unsettled,
                abort(StopCause::ToolStopped),
            ),
        ];
        for (index, (queue_stop, cancelled, paused, end, hook, expected)) in
            rows.into_iter().enumerate()
        {
            assert_eq!(
                wave_stop_after(queue_stop, cancelled, paused, end, hook),
                expected,
                "row {index}"
            );
        }
        for cause in [StopCause::ActiveTime, StopCause::DelegationDeadline] {
            assert_eq!(
                wave_stop_after(Some(cause), false, false, Some(Failed), settled),
                boundary(cause)
            );
        }
        for (result, reason) in [
            (answer(), Some(RepromptReason::AnswerDeferred)),
            (
                response(ProviderOutcome::Delegate(crate::provider::Delegate {
                    agent: None,
                    children: Vec::new(),
                })),
                Some(RepromptReason::DelegateRefused),
            ),
            (tools(), Some(RepromptReason::ToolRefused)),
            (timeout(), None),
            (CallResult::Cancelled, None),
        ] {
            assert_eq!(RepromptReason::deferral(&result), reason);
        }
    }
}

use super::{EngineError, STORE_ACTION_OVERHEAD};
use crate::journal::Journal;
use crate::provider::{GoalContextFacts, ProviderRole, Providers};
use crate::session::{
    AgentRunId, BudgetAction, BudgetChargeBasis, BudgetRefusal, Event, EventEnvelope, GoalBudget,
    GoalId, GoalLimit, GoalPlanContract, GoalUsage, GoalView, MAX_CHILD_TOOL_CALLS,
    MAX_DISPLAY_CHUNKS, SessionId, SessionView,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::{Instant, sleep};

pub(super) const MAX_NARROWINGS_PER_STEP: usize = 1;
pub(super) const NARROWING_EVENTS: usize = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ActiveDeadlineCause {
    Delegation,
    Goal,
}

#[derive(Clone)]
pub(super) struct ActiveDeadline {
    clock: Arc<ActivityClock>,
    started_at: Duration,
    allowance_ms: u64,
    pub(super) cause: ActiveDeadlineCause,
}

impl ActiveDeadline {
    pub(super) fn remaining_ms(&self) -> u64 {
        self.allowance_ms.saturating_sub(duration_ms(
            self.clock.elapsed().saturating_sub(self.started_at),
        ))
    }

    pub(super) fn reached(&self) -> bool {
        self.remaining_ms() == 0
    }

    pub(super) fn wall_deadline(&self) -> Instant {
        Instant::now() + Duration::from_millis(self.remaining_ms())
    }

    pub(super) async fn elapsed(&self) {
        let mut changes = self.clock.changes.subscribe();
        loop {
            let remaining = self.remaining_ms();
            if remaining == 0 {
                return;
            }
            if self.clock.active() {
                tokio::select! {
                    () = sleep(Duration::from_millis(remaining)) => {}
                    result = changes.changed() => if result.is_err() { return; },
                }
            } else if changes.changed().await.is_err() {
                return;
            }
        }
    }
}

#[derive(Default)]
struct ActivityState {
    providers: usize,
    effects: u16,
    elapsed: Duration,
    started: Option<Instant>,
}

struct ActivityClock {
    state: Mutex<ActivityState>,
    changes: tokio::sync::watch::Sender<u64>,
}

impl ActivityClock {
    fn new() -> Self {
        let (changes, _) = tokio::sync::watch::channel(0);
        Self {
            state: Mutex::new(ActivityState::default()),
            changes,
        }
    }

    fn elapsed(&self) -> Duration {
        self.state.lock().map_or(Duration::MAX, |state| {
            state.elapsed.saturating_add(
                state
                    .started
                    .map_or(Duration::ZERO, |started| started.elapsed()),
            )
        })
    }

    fn active(&self) -> bool {
        self.state
            .lock()
            .is_ok_and(|state| state.providers > 0 || state.effects > 0)
    }

    fn update(&self, update: impl FnOnce(&mut ActivityState)) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let was_active = state.providers > 0 || state.effects > 0;
        update(&mut state);
        let is_active = state.providers > 0 || state.effects > 0;
        match (was_active, is_active) {
            (false, true) => state.started = Some(Instant::now()),
            (true, false) => {
                state.elapsed = state.elapsed.saturating_add(
                    state
                        .started
                        .take()
                        .map_or(Duration::ZERO, |started| started.elapsed()),
                );
            }
            _ => {}
        }
        self.changes
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    fn effect(self: &Arc<Self>) -> ActiveWork {
        self.update(|state| state.effects = state.effects.saturating_add(1));
        ActiveWork(self.clone())
    }

    fn provider_completed(&self) {
        self.update(|state| state.providers = state.providers.saturating_sub(1));
    }

    fn provider(self: &Arc<Self>) -> ProviderActivity {
        ProviderActivity(self.clone())
    }
}

pub(super) struct ActiveWork(Arc<ActivityClock>);

impl Drop for ActiveWork {
    fn drop(&mut self) {
        self.0
            .update(|state| state.effects = state.effects.saturating_sub(1));
    }
}

pub(super) struct ProviderWork(Arc<ActivityClock>);

#[derive(Clone)]
pub(super) struct ProviderActivity(Arc<ActivityClock>);

impl ProviderActivity {
    pub(super) fn start(&self) -> ProviderWork {
        self.0
            .update(|state| state.providers = state.providers.saturating_add(1));
        ProviderWork(self.0.clone())
    }
}

impl Drop for ProviderWork {
    fn drop(&mut self) {
        self.0.provider_completed();
    }
}

#[cfg(test)]
mod tests;

#[must_use]
pub(super) enum Admission<T> {
    Reserved(T),
    Exhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActiveReservationLimit {
    Goal,
    ActionBound,
}

impl<T> Admission<T> {
    pub(super) fn map<U>(self, map: impl FnOnce(T) -> U) -> Admission<U> {
        match self {
            Self::Reserved(value) => Admission::Reserved(map(value)),
            Self::Exhausted => Admission::Exhausted,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Action {
    ProviderCall { previews: bool, steering: bool },
    Child { in_flight: u32, research: bool },
    Tool,
    ToolStep { calls: usize },
    Worker { chunks: usize },
    Review,
    Compaction,
    BoundaryCompaction { goal: bool },
    Delegation { children: usize, research: bool },
}

impl Action {
    /// Base Event bound before configured Provider switches and same-route retry facts.
    pub fn max_events(self) -> usize {
        match self {
            // Reserve terminal Events through the Goal receipt even when no further step runs.
            Self::ProviderCall { previews, steering } => {
                6 + usize::from(steering) + if previews { MAX_DISPLAY_CHUNKS } else { 0 }
            }
            Self::Child {
                in_flight,
                research,
            } => (if research { 15_usize } else { 10_usize }).saturating_add(
                usize::try_from(in_flight)
                    .unwrap_or(usize::MAX)
                    .saturating_mul(2),
            ),
            Self::Tool | Self::Review => 8,
            Self::ToolStep { calls } => calls
                .saturating_mul(Self::Tool.max_events().saturating_add(1))
                .saturating_add(MAX_NARROWINGS_PER_STEP.saturating_mul(NARROWING_EVENTS)),
            Self::Worker { chunks } => chunks.saturating_add(12 + 5 * 8 + 6),
            Self::Compaction => 6,
            Self::BoundaryCompaction { goal } => 1 + usize::from(goal),
            Self::Delegation { children, research } => {
                if research {
                    children * (8 * MAX_CHILD_TOOL_CALLS + 5) + 8
                } else {
                    5 * children + 6
                }
            }
        }
    }

    // Shadow verdicts follow their subject Events, and only Runs pinned with a Supervisor reserve them.
    pub fn verdict_slots(self) -> usize {
        match self {
            Self::ProviderCall { .. } => 1,
            Self::Child { in_flight, .. } => usize::try_from(in_flight)
                .unwrap_or(usize::MAX)
                .saturating_add(1),
            Self::Tool | Self::Review => 2,
            Self::ToolStep { calls } => calls.saturating_mul(Self::Tool.verdict_slots()),
            Self::Worker { .. } => 12,
            Self::Compaction | Self::BoundaryCompaction { .. } => 0,
            Self::Delegation { children, research } => {
                if research {
                    children * (MAX_CHILD_TOOL_CALLS + 1)
                } else {
                    children + 1
                }
            }
        }
    }

    pub fn max_events_for<T>(self, providers: &Providers<Vec<T>>) -> usize {
        self.max_events_with_lengths(Providers {
            primary: providers.primary.len(),
            child: providers.child.len(),
            review: providers.review.len(),
            compaction: providers.compaction.len(),
        })
    }

    pub fn max_events_for_admitted(self, providers: &crate::AdmittedProviders) -> usize {
        self.max_events_with_lengths(Providers {
            primary: providers.primary.len(),
            child: providers
                .child_chains()
                .into_iter()
                .map(Vec::len)
                .max()
                .unwrap_or(1),
            review: providers.review.len(),
            compaction: providers.compaction.len(),
        })
    }

    // Each call in the action may append every switch of its chain and three same-route retry facts.
    fn max_events_with_lengths(self, lengths: Providers<usize>) -> usize {
        let attempt_facts = |role| {
            lengths
                .get(role)
                .saturating_sub(1)
                .saturating_add(usize::from(crate::provider::MAX_PROVIDER_RETRIES))
        };
        self.max_events().saturating_add(match self {
            Self::ProviderCall { .. } => attempt_facts(ProviderRole::Primary),
            Self::Child {
                in_flight,
                research,
            } => usize::try_from(in_flight)
                .unwrap_or(usize::MAX)
                .saturating_add(1)
                .saturating_mul(attempt_facts(ProviderRole::Child))
                .saturating_add(if research {
                    attempt_facts(ProviderRole::Review)
                } else {
                    0
                }),
            Self::Tool | Self::Review => attempt_facts(ProviderRole::Review),
            Self::ToolStep { calls } => calls.saturating_mul(attempt_facts(ProviderRole::Review)),
            Self::Worker { .. } => attempt_facts(ProviderRole::Review)
                .saturating_add(8 * attempt_facts(ProviderRole::Child))
                .saturating_add(attempt_facts(ProviderRole::Primary)),
            Self::Compaction | Self::BoundaryCompaction { .. } => {
                attempt_facts(ProviderRole::Compaction)
            }
            Self::Delegation { children, research } => children
                .saturating_mul(if research {
                    MAX_CHILD_TOOL_CALLS + 1
                } else {
                    1
                })
                .saturating_mul(attempt_facts(ProviderRole::Child))
                .saturating_add(if research {
                    children
                        .saturating_mul(MAX_CHILD_TOOL_CALLS)
                        .saturating_mul(attempt_facts(ProviderRole::Review))
                } else {
                    0
                })
                .saturating_add(attempt_facts(ProviderRole::Primary)),
        })
    }

    pub(super) fn tool_observations(self) -> usize {
        match self {
            // A command can need a separate integration observation, so reserve two observations per Tool step.
            Self::Tool | Self::Review | Self::Worker { .. } => 2,
            Self::ToolStep { calls } => calls
                .saturating_mul(Self::Tool.tool_observations())
                .saturating_add(
                    MAX_NARROWINGS_PER_STEP.saturating_mul(Self::Tool.tool_observations()),
                ),
            Self::Child {
                in_flight,
                research: true,
            } => usize::try_from(in_flight)
                .unwrap_or(usize::MAX)
                .saturating_add(1),
            Self::Delegation {
                children,
                research: true,
            } => children.saturating_mul(MAX_CHILD_TOOL_CALLS),
            Self::ProviderCall { .. }
            | Self::Child {
                research: false, ..
            }
            | Self::Compaction
            | Self::BoundaryCompaction { .. }
            | Self::Delegation { .. } => 0,
        }
    }

    pub(super) fn remaining<T>(self, appended: usize, providers: &Providers<Vec<T>>) -> usize {
        self.max_events_for(providers).saturating_sub(appended)
    }
}

// ProviderCallRecorded or ToolFinished, AgentFinished, WorkerHandoverDiscarded and WorkerLeaseReleased.
const DRAIN_EVENTS_PER_EDITOR: usize = 4;

pub(super) fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

pub(super) struct GoalLedger {
    goal_id: Option<GoalId>,
    state: Mutex<State>,
    admission: tokio::sync::Mutex<()>,
    activity: Arc<ActivityClock>,
}

struct State {
    view: SessionView,
    in_flight: InFlight,
}

#[derive(Default)]
struct InFlight {
    children: u32,
    // Settled Provider results whose ProviderCallRecorded is not appended yet.
    unrecorded_tokens: u64,
    unrecorded_unknown: u32,
    // This Run's settled active time, journaled once by RunFinished.
    run_active_ms: u64,
    idle_approval_ms: u64,
    // The open action's active-time reservation.
    reserved_ms: Option<u64>,
    // Start of the active time since the last settlement.
    origin: Option<Instant>,
    // The Run ended because the Goal budget ran out.
    budget_stopped: bool,
    budget_refusal: Option<BudgetRefusal>,
}

impl InFlight {
    fn pending_ms(&self) -> u64 {
        self.origin.map_or(0, |origin| {
            duration_ms(Instant::now().saturating_duration_since(origin))
        })
    }

    fn spent_ms(&self, goal: &GoalView) -> u64 {
        goal.active_ms
            .saturating_add(self.run_active_ms)
            .saturating_add(self.pending_ms())
    }

    fn consumed_ms(&self, goal: &GoalView) -> u64 {
        self.spent_ms(goal)
            .saturating_add(self.reserved_ms.unwrap_or_default())
    }
}

pub(super) struct Reservation<'a> {
    ledger: &'a GoalLedger,
    child: bool,
    _admission: tokio::sync::MutexGuard<'a, ()>,
}

impl Reservation<'_> {
    // Commit comes after the append, so a refused append leaves no child counted.
    pub(super) fn commit(self) -> Result<(), EngineError> {
        if self.child {
            self.ledger.in_flight(|in_flight| {
                in_flight.children = in_flight.children.saturating_add(1);
            })?;
        }
        Ok(())
    }
}

impl GoalLedger {
    pub(super) fn new(view: SessionView, goal_id: Option<GoalId>) -> Self {
        Self {
            goal_id,
            state: Mutex::new(State {
                view,
                in_flight: InFlight::default(),
            }),
            admission: tokio::sync::Mutex::new(()),
            activity: Arc::new(ActivityClock::new()),
        }
    }

    pub(super) fn verdicts(&self, action: Action) -> usize {
        match self.view(|v| {
            v.runs
                .last()
                .is_some_and(|r| r.config.as_ref().is_some_and(|c| c.supervisor.is_some()))
        }) {
            Ok(false) => 0,
            _ => action.verdict_slots(),
        }
    }

    pub(super) fn view<T>(&self, read: impl FnOnce(&SessionView) -> T) -> Result<T, EngineError> {
        let state = self
            .state
            .lock()
            .map_err(|_| EngineError::CoordinatorFailed)?;
        Ok(read(&state.view))
    }

    pub(super) fn append<'a>(
        &'a self,
        journal: &'a Journal,
        session_id: SessionId,
        event: Event,
    ) -> super::run_loop::step::BoxedStep<'a, u64> {
        Box::pin(async move {
            self.append_committed(journal, session_id, event)
                .await
                .map(|committed| committed.sequence)
        })
    }

    // The ledger is fed before any subscriber can see the Event.
    pub(super) async fn append_committed(
        &self,
        journal: &Journal,
        session_id: SessionId,
        event: Event,
    ) -> Result<Arc<EventEnvelope>, EngineError> {
        let (committed, fed) = journal
            .append_then(session_id, event, |envelope| self.committed(envelope))
            .await?;
        fed?;
        Ok(committed)
    }

    fn committed(&self, envelope: &EventEnvelope) -> Result<(), EngineError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| EngineError::CoordinatorFailed)?;
        state
            .view
            .apply_committed(envelope)
            .map_err(|_| EngineError::InvalidHistory)?;
        if let Event::RunFinished { .. } = &envelope.event {
            let in_flight = &mut state.in_flight;
            in_flight.run_active_ms = 0;
            in_flight.idle_approval_ms = 0;
            in_flight.reserved_ms = None;
            in_flight.origin = None;
        }
        if let Event::ProviderCallRecorded { record, .. } = &envelope.event {
            let in_flight = &mut state.in_flight;
            in_flight.unrecorded_tokens = in_flight
                .unrecorded_tokens
                .saturating_sub(tokens(record.input_tokens, record.output_tokens));
            if record.input_tokens.is_none() || record.output_tokens.is_none() {
                in_flight.unrecorded_unknown = in_flight.unrecorded_unknown.saturating_sub(1);
            }
        }
        Ok(())
    }

    pub(super) fn active_work(&self) -> ActiveWork {
        self.activity.effect()
    }

    pub(super) fn provider_activity(&self) -> ProviderActivity {
        self.activity.provider()
    }

    pub(super) fn active_elapsed(&self) -> Duration {
        self.activity.elapsed()
    }

    pub(super) fn active_deadline(
        &self,
        allowance_ms: u64,
        cause: ActiveDeadlineCause,
    ) -> ActiveDeadline {
        ActiveDeadline {
            started_at: self.activity.elapsed(),
            clock: self.activity.clone(),
            allowance_ms,
            cause,
        }
    }

    fn read<T>(&self, read: impl FnOnce(&GoalView, &InFlight) -> T) -> Result<T, EngineError> {
        let state = self
            .state
            .lock()
            .map_err(|_| EngineError::CoordinatorFailed)?;
        let goal = self
            .goal_id
            .and_then(|goal_id| state.view.goal(goal_id))
            .ok_or(EngineError::InvalidHistory)?;
        Ok(read(goal, &state.in_flight))
    }

    fn in_flight<T>(&self, update: impl FnOnce(&mut InFlight) -> T) -> Result<T, EngineError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| EngineError::CoordinatorFailed)?;
        Ok(update(&mut state.in_flight))
    }

    fn update<T>(
        &self,
        update: impl FnOnce(&GoalView, &mut InFlight) -> T,
    ) -> Result<T, EngineError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| EngineError::CoordinatorFailed)?;
        let State {
            view, in_flight, ..
        } = &mut *state;
        let goal = self
            .goal_id
            .and_then(|goal_id| view.goal(goal_id))
            .ok_or(EngineError::InvalidHistory)?;
        Ok(update(goal, in_flight))
    }

    pub(super) fn in_flight_children(&self) -> u32 {
        self.read(|_, in_flight| in_flight.children).unwrap_or(0)
    }

    // Admission holds nothing back, so a primary step reserves every lease-holding editor's drain.
    pub(super) fn drain_headroom(&self) -> usize {
        self.view(|view| {
            let Some(run) = view.runs.last() else {
                return 0;
            };
            let editors = run
                .agents
                .iter()
                .filter(|agent| {
                    agent
                        .assignment
                        .as_ref()
                        .is_some_and(|worker| worker.released.is_none())
                })
                .count();
            let supervised = run.config.as_ref().is_some_and(|c| c.supervisor.is_some());
            editors.saturating_mul(DRAIN_EVENTS_PER_EDITOR + usize::from(supervised))
        })
        .unwrap_or(0)
    }

    pub(super) fn admits(&self, action: Action) -> bool {
        match action {
            Action::ProviderCall { .. }
            | Action::Child { .. }
            | Action::Tool
            | Action::ToolStep { .. }
            | Action::Worker { .. } => {
                self.can_start_provider_call() && self.remaining_calls() != Some(0)
            }
            Action::Review | Action::Compaction => {
                self.can_start_provider_call()
                    && self.remaining_calls().is_none_or(|remaining| remaining > 1)
            }
            Action::Delegation { children, .. } => {
                let needed = u32::try_from(children.saturating_add(1)).unwrap_or(u32::MAX);
                self.remaining_calls()
                    .is_none_or(|remaining| remaining >= needed)
            }
            Action::BoundaryCompaction { .. } => true,
        }
    }

    // The admission guard is held until the reservation Event is appended.
    pub(super) async fn reserve(
        &self,
        action: Action,
        actor: AgentRunId,
    ) -> Result<Admission<Reservation<'_>>, EngineError> {
        let admission = self.admission.lock().await;
        if !(self.can_start_provider_call() && self.remaining_calls() != Some(0)) {
            self.capture_budget_refusal(
                match action {
                    Action::Compaction | Action::BoundaryCompaction { .. } => {
                        BudgetAction::Compaction
                    }
                    Action::Delegation { .. } => BudgetAction::Delegation,
                    Action::ProviderCall { .. }
                    | Action::Child { .. }
                    | Action::Tool
                    | Action::ToolStep { .. }
                    | Action::Worker { .. }
                    | Action::Review => BudgetAction::Provider,
                },
                Some(actor),
            )?;
            return Ok(Admission::Exhausted);
        }
        Ok(Admission::Reserved(Reservation {
            ledger: self,
            child: matches!(action, Action::Child { .. }),
            _admission: admission,
        }))
    }

    pub(super) fn settle(
        &self,
        input: Option<u32>,
        output: Option<u32>,
    ) -> Result<(), EngineError> {
        self.in_flight(|in_flight| {
            in_flight.unrecorded_tokens = in_flight
                .unrecorded_tokens
                .saturating_add(tokens(input, output));
            if input.is_none() || output.is_none() {
                in_flight.unrecorded_unknown = in_flight.unrecorded_unknown.saturating_add(1);
            }
        })
    }

    pub(super) fn child_call_finished(&self) -> Result<(), EngineError> {
        self.in_flight(|in_flight| {
            in_flight.children = in_flight.children.checked_sub(1)?;
            Some(())
        })?
        .ok_or(EngineError::CoordinatorFailed)
    }

    pub(super) fn remaining_calls(&self) -> Option<u32> {
        self.read(|goal, _| {
            goal.effective_budget()
                .max_provider_calls
                .map(|limit| limit.saturating_sub(goal.provider_calls))
        })
        .unwrap_or(Some(0))
    }

    pub(super) fn plan_contract(&self) -> Result<Option<GoalPlanContract>, EngineError> {
        self.read(|goal, _| goal.plan_contract.clone())
    }

    pub(super) fn effective_budget(&self) -> Result<GoalBudget, EngineError> {
        self.read(|goal, _| goal.effective_budget())
    }

    pub(super) fn allows_tool(&self, call: &crate::tools::ToolCall) -> bool {
        self.read(|goal, _| {
            goal.plan_contract
                .as_ref()
                .is_none_or(|contract| contract.allows_call(call))
        })
        .unwrap_or(false)
    }

    pub(super) fn requested_command_resources(
        &self,
    ) -> Result<
        (
            crate::tools::ToolResourceClass,
            u32,
            crate::tools::ToolResourceSettings,
        ),
        EngineError,
    > {
        self.read(|goal, _| {
            let contract = goal.plan_contract.as_ref();
            (
                contract.map_or(crate::tools::ToolResourceClass::Default, |plan| {
                    plan.resource_class
                }),
                contract
                    .and_then(|plan| plan.command_timeout_ms)
                    .unwrap_or(goal.resource_settings.default_command_timeout_ms),
                goal.resource_settings,
            )
        })
    }

    pub(super) fn command_timeout_limit(
        &self,
        call: &crate::tools::ToolCall,
    ) -> Result<Option<(u32, u32)>, EngineError> {
        if !matches!(call, crate::tools::ToolCall::Command { .. }) {
            return Ok(None);
        }
        let (_, requested, settings) = self.requested_command_resources()?;
        let maximum = settings.max_command_timeout_ms;
        Ok((requested > maximum).then_some((requested, maximum)))
    }

    pub(super) fn autonomous(&self) -> bool {
        self.read(|goal, _| goal.autonomous).unwrap_or(false)
    }

    pub(super) fn allows_next_run(&self) -> bool {
        self.read(|goal, in_flight| {
            goal.autonomous
                && goal.effective_budget().admits_next_run(admission_usage(
                    goal,
                    in_flight,
                    goal.run_ids.len(),
                ))
        })
        .unwrap_or(false)
    }

    pub(super) fn success_criterion_satisfied(&self) -> bool {
        self.read(|goal, _| goal.plan_contract.is_none() || goal.success_criterion_satisfied)
            .unwrap_or(false)
    }

    pub(super) fn can_narrow(&self, contract: &GoalPlanContract) -> bool {
        self.read(|goal, in_flight| {
            let usage = admission_usage(goal, in_flight, goal.run_ids.len().saturating_sub(1));
            contract.budget.admits_next_run(GoalUsage {
                active_ms: in_flight.spent_ms(goal),
                ..usage
            })
        })
        .unwrap_or(false)
    }

    pub(super) fn run_active_deadline(&self) -> Result<Option<u64>, EngineError> {
        self.read(|goal, _| {
            goal.effective_budget()
                .max_active_ms
                .map(|limit| limit.saturating_sub(goal.active_ms))
        })
    }

    pub(super) fn reserve_active(
        &self,
        action_ms: u64,
        action: BudgetAction,
        actor: Option<AgentRunId>,
    ) -> Result<Admission<(Instant, u64, ActiveReservationLimit)>, EngineError> {
        self.update(|goal, in_flight| {
            let now = Instant::now();
            let started = in_flight.origin.unwrap_or(now);
            let carried_ms = duration_ms(now.saturating_duration_since(started));
            let requested_ms = action_ms
                .saturating_add(duration_ms(STORE_ACTION_OVERHEAD))
                .saturating_add(carried_ms);
            let reservation = goal
                .effective_budget()
                .max_active_ms
                .map_or(requested_ms, |limit| {
                    requested_ms.min(
                        limit
                            .saturating_sub(goal.active_ms)
                            .saturating_sub(in_flight.run_active_ms),
                    )
                });
            let limit = if reservation < requested_ms {
                ActiveReservationLimit::Goal
            } else {
                ActiveReservationLimit::ActionBound
            };
            let minimum_reservation = carried_ms.saturating_add(if action_ms == 0 {
                0
            } else {
                duration_ms(STORE_ACTION_OVERHEAD)
            });
            if in_flight.reserved_ms.is_some() {
                return Err(EngineError::CoordinatorFailed);
            }
            if reservation <= minimum_reservation {
                if let Some(mut refusal) = refusal_at(goal, in_flight, action, actor) {
                    if refusal.dimension == GoalLimit::ActiveTime {
                        refusal.requested = Some(requested_ms);
                        refusal.required_minimum = Some(minimum_reservation.saturating_add(1));
                    }
                    in_flight.budget_refusal.get_or_insert(refusal);
                }
                return Ok(Admission::Exhausted);
            }
            in_flight.reserved_ms = Some(reservation);
            in_flight.origin = None;
            Ok(Admission::Reserved((started, reservation, limit)))
        })?
    }

    pub(super) fn settle_active(
        &self,
        active_ms: u64,
        origin: Option<Instant>,
    ) -> Result<(), EngineError> {
        self.in_flight(|in_flight| {
            in_flight.run_active_ms = in_flight.run_active_ms.saturating_add(
                active_ms.saturating_sub(std::mem::take(&mut in_flight.idle_approval_ms)),
            );
            in_flight.reserved_ms = None;
            in_flight.origin = origin;
        })
    }

    pub(super) fn exclude_idle_approval(&self, elapsed: Duration) -> Result<(), EngineError> {
        self.in_flight(|in_flight| {
            in_flight.idle_approval_ms = in_flight
                .idle_approval_ms
                .saturating_add(duration_ms(elapsed));
        })
    }

    pub(super) fn active_ceiling(&self, pending_ms: u64) -> Result<u64, EngineError> {
        self.in_flight(|in_flight| {
            // With no reservation open, the Store tail allows for work before the next one starts.
            let open_ms = in_flight.reserved_ms.unwrap_or_else(|| {
                in_flight
                    .pending_ms()
                    .saturating_add(duration_ms(STORE_ACTION_OVERHEAD))
            });
            in_flight
                .run_active_ms
                .saturating_add(open_ms)
                .saturating_add(pending_ms)
        })
    }

    pub(super) fn mark_budget_stopped(&self) {
        let _ = self.in_flight(|in_flight| in_flight.budget_stopped = true);
    }

    pub(super) fn capture_budget_refusal(
        &self,
        action: BudgetAction,
        actor: Option<AgentRunId>,
    ) -> Result<(), EngineError> {
        self.update(|goal, in_flight| {
            if in_flight.budget_refusal.is_none() {
                in_flight.budget_refusal = refusal_at(goal, in_flight, action, actor);
            }
        })
    }

    pub(super) fn budget_refusal(&self) -> Result<Option<BudgetRefusal>, EngineError> {
        self.in_flight(|in_flight| in_flight.budget_refusal)
    }

    pub(super) fn budget_stopped(&self) -> bool {
        self.in_flight(|in_flight| in_flight.budget_stopped)
            .unwrap_or(false)
    }

    pub(super) fn active_reserved(&self) -> bool {
        self.in_flight(|in_flight| in_flight.reserved_ms.is_some())
            .unwrap_or(true)
    }

    pub(super) fn run_active_ms(&self) -> Result<u64, EngineError> {
        self.in_flight(|in_flight| {
            in_flight
                .run_active_ms
                .saturating_add(in_flight.pending_ms())
        })
    }

    pub(super) fn providers_idle(&self) -> Result<bool, EngineError> {
        self.read(|goal, _| {
            goal.unobserved_provider_calls.is_empty() && goal.compaction_call_reserved.is_none()
        })
    }

    pub(super) fn active_remaining_ms(&self) -> Option<u64> {
        self.read(|goal, in_flight| {
            goal.effective_budget()
                .max_active_ms
                .map(|limit| limit.saturating_sub(in_flight.consumed_ms(goal)))
        })
        .unwrap_or(Some(0))
    }

    pub(super) fn spent_active_time_exhausted(&self) -> bool {
        self.read(|goal, in_flight| {
            goal.effective_budget()
                .max_active_ms
                .is_some_and(|limit| in_flight.spent_ms(goal) >= limit)
        })
        .unwrap_or(false)
    }

    pub(super) fn active_time_exhausted(&self) -> bool {
        self.read(|goal, in_flight| {
            let Some(limit) = goal.effective_budget().max_active_ms else {
                return false;
            };
            let consumed_ms = in_flight.consumed_ms(goal);
            consumed_ms >= limit
                || limit.saturating_sub(consumed_ms) <= duration_ms(STORE_ACTION_OVERHEAD)
        })
        .unwrap_or(false)
    }

    pub(super) fn context_facts(&self) -> Result<GoalContextFacts, EngineError> {
        self.read(|goal, in_flight| GoalContextFacts {
            provider_calls: goal.provider_calls,
            known_tokens: known_tokens(goal, in_flight),
            token_usage_unknown: usage_unknown(goal, in_flight),
            active_ms: goal.active_ms.saturating_add(in_flight.run_active_ms),
            reserved_active_ms: in_flight.reserved_ms.unwrap_or_default(),
            uncertain_active_ms: goal.uncertain_active_ms,
            success_criterion_satisfied: goal
                .plan_contract
                .as_ref()
                .map(|_| goal.success_criterion_satisfied),
        })
    }

    pub(super) fn tokens_within_limit(&self) -> bool {
        self.read(|goal, in_flight| {
            goal.effective_budget().max_tokens.is_none_or(|limit| {
                !usage_unknown(goal, in_flight) && known_tokens(goal, in_flight) <= limit
            })
        })
        .unwrap_or(false)
    }

    pub(super) fn can_start_provider_call(&self) -> bool {
        self.read(|goal, in_flight| {
            goal.effective_budget().max_tokens.is_none_or(|limit| {
                !usage_unknown(goal, in_flight) && known_tokens(goal, in_flight) < limit
            })
        })
        .unwrap_or(false)
    }

    pub(super) fn provider_concurrency(&self, admitted: usize) -> usize {
        match self.read(|goal, _| goal.effective_budget().max_tokens) {
            Ok(None) => admitted,
            _ => admitted.min(1),
        }
    }
}

fn refusal_at(
    goal: &GoalView,
    in_flight: &InFlight,
    action: BudgetAction,
    actor: Option<AgentRunId>,
) -> Option<BudgetRefusal> {
    let mut refusal = goal.effective_budget().refusal(
        GoalUsage {
            active_ms: in_flight.spent_ms(goal),
            ..admission_usage(goal, in_flight, goal.run_ids.len())
        },
        action,
    )?;
    refusal.run_id = goal.active_run;
    refusal.agent_run_id = actor.filter(|_| refusal.run_id.is_some());
    if goal.uncertain_active_ms > 0 {
        refusal.charge_basis = BudgetChargeBasis::Conservative;
    }
    Some(refusal)
}

fn tokens(input: Option<u32>, output: Option<u32>) -> u64 {
    u64::from(input.unwrap_or_default()) + u64::from(output.unwrap_or_default())
}

fn known_tokens(goal: &GoalView, in_flight: &InFlight) -> u64 {
    goal.known_tokens
        .saturating_add(in_flight.unrecorded_tokens)
}

fn usage_unknown(goal: &GoalView, in_flight: &InFlight) -> bool {
    goal.token_usage_unknown || in_flight.unrecorded_unknown > 0
}

fn admission_usage(goal: &GoalView, in_flight: &InFlight, runs: usize) -> GoalUsage {
    GoalUsage {
        runs,
        provider_calls: goal.provider_calls,
        active_ms: in_flight.consumed_ms(goal),
        known_tokens: known_tokens(goal, in_flight),
        token_usage_unknown: usage_unknown(goal, in_flight),
    }
}

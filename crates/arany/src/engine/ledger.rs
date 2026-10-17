use super::{EngineError, STORE_ACTION_OVERHEAD};
use crate::journal::Journal;
use crate::provider::{GoalContextFacts, ProviderRole, Providers};
use crate::session::{
    Event, EventEnvelope, GoalBudget, GoalId, GoalPlanContract, GoalUsage, GoalView,
    MAX_CHILD_TOOL_CALLS, MAX_DISPLAY_CHUNKS, SessionId, SessionView,
};
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::Instant;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug)]
pub enum Action {
    ProviderCall { previews: bool, steering: bool },
    Child { in_flight: u32, research: bool },
    Tool,
    Review,
    Compaction,
    BoundaryCompaction { goal: bool },
    Delegation { children: usize, research: bool },
}

impl Action {
    /// Base Event bound before configured Provider switches.
    pub fn max_events(self) -> usize {
        match self {
            // A finishing call: its reservation, record, an optional steering note and display chunks,
            // then the terminal Events through the Goal receipt.
            Self::ProviderCall { previews, steering } => {
                6 + usize::from(steering) + if previews { MAX_DISPLAY_CHUNKS } else { 0 }
            }
            Self::Child {
                in_flight,
                research,
            } => (if research { 14_usize } else { 9_usize }).saturating_add(
                usize::try_from(in_flight)
                    .unwrap_or(usize::MAX)
                    .saturating_mul(2),
            ),
            Self::Tool | Self::Review => 8,
            Self::Compaction => 6,
            Self::BoundaryCompaction { goal } => 1 + usize::from(goal),
            Self::Delegation { children, research } => {
                if research {
                    children * (7 * MAX_CHILD_TOOL_CALLS + 4) + 8
                } else {
                    4 * children + 6
                }
            }
        }
    }

    pub fn max_events_for<T>(self, providers: &Providers<Vec<T>>) -> usize {
        let switches = |role| providers.get(role).len().saturating_sub(1);
        self.max_events().saturating_add(match self {
            Self::ProviderCall { .. } => switches(ProviderRole::Primary),
            Self::Child {
                in_flight,
                research,
            } => usize::try_from(in_flight)
                .unwrap_or(usize::MAX)
                .saturating_add(1)
                .saturating_mul(switches(ProviderRole::Child))
                .saturating_add(if research {
                    switches(ProviderRole::Review)
                } else {
                    0
                }),
            Self::Tool | Self::Review => switches(ProviderRole::Review),
            Self::Compaction | Self::BoundaryCompaction { .. } => {
                switches(ProviderRole::Compaction)
            }
            Self::Delegation { children, research } => children
                .saturating_mul(if research {
                    MAX_CHILD_TOOL_CALLS + 1
                } else {
                    1
                })
                .saturating_mul(switches(ProviderRole::Child))
                .saturating_add(if research {
                    children
                        .saturating_mul(MAX_CHILD_TOOL_CALLS)
                        .saturating_mul(switches(ProviderRole::Review))
                } else {
                    0
                })
                .saturating_add(switches(ProviderRole::Primary)),
        })
    }

    /// `ToolFinished` Events among `max_events`, which Store admission sizes at their larger bound.
    pub(super) fn tool_observations(self) -> usize {
        match self {
            // A Tool step, and the review inside it, ends with its observation and any integration
            // observation.
            Self::Tool | Self::Review => 2,
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

    /// What is left of the bound once `appended` of the action's Events are committed.
    pub(super) fn remaining<T>(self, appended: usize, providers: &Providers<Vec<T>>) -> usize {
        self.max_events_for(providers).saturating_sub(appended)
    }
}

pub(super) fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

/// Goal bookkeeping read from the reducer's view. Only what has no Event yet lives outside it.
pub(super) struct GoalLedger {
    goal_id: GoalId,
    state: Mutex<State>,
    admission: tokio::sync::Mutex<()>,
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
}

impl InFlight {
    fn pending_ms(&self) -> u64 {
        self.origin.map_or(0, |origin| {
            duration_ms(Instant::now().saturating_duration_since(origin))
        })
    }

    /// The Goal's active time with this Run's settled and carried time, excluding an open reservation.
    fn spent_ms(&self, goal: &GoalView) -> u64 {
        goal.active_ms
            .saturating_add(self.run_active_ms)
            .saturating_add(self.pending_ms())
    }

    /// Active time spent or held, counting an open reservation as spent.
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
    /// Call after the reservation Event is appended.
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
    pub(super) fn new(view: SessionView, goal_id: GoalId) -> Self {
        Self {
            goal_id,
            state: Mutex::new(State {
                view,
                in_flight: InFlight::default(),
            }),
            admission: tokio::sync::Mutex::new(()),
        }
    }

    pub(super) fn view<T>(&self, read: impl FnOnce(&SessionView) -> T) -> Result<T, EngineError> {
        let state = self
            .state
            .lock()
            .map_err(|_| EngineError::CoordinatorFailed)?;
        Ok(read(&state.view))
    }

    /// Appends a Run's Event and feeds it to the ledger before any subscriber can see it, so budget
    /// decisions never wait on an observer.
    pub(super) async fn append(
        &self,
        journal: &Journal,
        session_id: SessionId,
        event: Event,
    ) -> Result<u64, EngineError> {
        let (committed, fed) = journal
            .append_then(session_id, event, |envelope| self.committed(envelope))
            .await?;
        fed?;
        Ok(committed.sequence)
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

    fn read<T>(&self, read: impl FnOnce(&GoalView, &InFlight) -> T) -> Result<T, EngineError> {
        let state = self
            .state
            .lock()
            .map_err(|_| EngineError::CoordinatorFailed)?;
        let goal = state
            .view
            .goal(self.goal_id)
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
        let State { view, in_flight } = &mut *state;
        let goal = view.goal(self.goal_id).ok_or(EngineError::InvalidHistory)?;
        Ok(update(goal, in_flight))
    }

    pub(super) fn in_flight_children(&self) -> u32 {
        self.read(|_, in_flight| in_flight.children).unwrap_or(0)
    }

    /// Whether the action may begin under the Goal's effective budget.
    pub(super) fn admits(&self, action: Action) -> bool {
        match action {
            Action::ProviderCall { .. } | Action::Child { .. } | Action::Tool => {
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

    /// Serializes Provider-call admission; the guard is held until the reservation Event is appended.
    pub(super) async fn reserve(&self, action: Action) -> Result<Reservation<'_>, EngineError> {
        let admission = self.admission.lock().await;
        if !(self.can_start_provider_call() && self.remaining_calls() != Some(0)) {
            return Err(EngineError::GoalBudgetExhausted);
        }
        Ok(Reservation {
            ledger: self,
            child: matches!(action, Action::Child { .. }),
            _admission: admission,
        })
    }

    /// Charges a settled Provider result until its call record is appended.
    pub(super) fn settle(&self, input: Option<u32>, output: Option<u32>) {
        let _ = self.in_flight(|in_flight| {
            in_flight.unrecorded_tokens = in_flight
                .unrecorded_tokens
                .saturating_add(tokens(input, output));
            if input.is_none() || output.is_none() {
                in_flight.unrecorded_unknown = in_flight.unrecorded_unknown.saturating_add(1);
            }
        });
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

    /// The pinned budget, narrowed to the recorded plan contract.
    pub(super) fn effective_budget(&self) -> Result<GoalBudget, EngineError> {
        self.read(|goal, _| goal.effective_budget())
    }

    pub(super) fn allows_tool(&self, call: &crate::tools::ToolCall) -> bool {
        self.read(|goal, _| {
            goal.plan_contract.as_ref().map_or_else(
                || call.is_goal_plan_prelude(),
                |contract| contract.allows_call(call),
            )
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

    /// Whether an autonomous Goal can admit another Run after this unsatisfied Run.
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

    /// Whether the contract admits this Run along with the earlier ones. The in-segment check is
    /// provisional; the reducer rechecks after settlement.
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

    /// The active time a starting Run may spend: what remains of the effective budget.
    pub(super) fn run_active_deadline(&self) -> Result<Option<u64>, EngineError> {
        self.read(|goal, _| {
            goal.effective_budget()
                .max_active_ms
                .map(|limit| limit.saturating_sub(goal.active_ms))
        })
    }

    /// Reserves an action's active interval in memory, with the time carried since the last settlement.
    pub(super) fn reserve_active(&self, action_ms: u64) -> Result<(Instant, u64), EngineError> {
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
            let minimum_reservation = carried_ms.saturating_add(if action_ms == 0 {
                0
            } else {
                duration_ms(STORE_ACTION_OVERHEAD)
            });
            if in_flight.reserved_ms.is_some() {
                return Err(EngineError::CoordinatorFailed);
            }
            if reservation <= minimum_reservation {
                return Err(EngineError::GoalBudgetExhausted);
            }
            in_flight.reserved_ms = Some(reservation);
            in_flight.origin = None;
            Ok((started, reservation))
        })?
    }

    /// Settles the open reservation; `origin` starts the next carried time.
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

    /// The Run's active-time bound through the open reservation, or carried time, plus `pending_ms`.
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

    pub(super) fn active_reserved(&self) -> bool {
        self.in_flight(|in_flight| in_flight.reserved_ms.is_some())
            .unwrap_or(true)
    }

    /// This Run's active time so far, carried time included, as RunFinished records it.
    pub(super) fn run_active_ms(&self) -> Result<u64, EngineError> {
        self.in_flight(|in_flight| {
            in_flight
                .run_active_ms
                .saturating_add(in_flight.pending_ms())
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

    /// Like `active_time_exhausted`, but an open delegation reservation, already capped to the
    /// remaining budget, does not count as spent.
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

/// Usage the admission rule judges, with the unsettled active segment counted as spent.
fn admission_usage(goal: &GoalView, in_flight: &InFlight, runs: usize) -> GoalUsage {
    GoalUsage {
        runs,
        provider_calls: goal.provider_calls,
        active_ms: in_flight.consumed_ms(goal),
        known_tokens: known_tokens(goal, in_flight),
        token_usage_unknown: usage_unknown(goal, in_flight),
    }
}

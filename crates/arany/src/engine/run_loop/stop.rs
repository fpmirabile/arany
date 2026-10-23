use super::RunLoop;
use crate::engine::EngineError;
use crate::provider::Answer;
use crate::session::{AgentStatus, GoalStatus, ProposalRefusalCause, RunFailure, RunStatus};
use crate::tools::ToolDisposition;

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StopCause {
    Uncertain,
    Cancelled,
    HookStop,
    Paused,
    ActiveTime,
    Budget,
    Capacity,
    ContextFull,
    DelegationDeadline,
    Refused(ProposalRefusalCause),
    ToolStopped,
    InvalidCommandResult,
}

impl StopCause {
    const fn rank(self) -> u8 {
        match self {
            Self::Uncertain => 12,
            Self::Cancelled => 11,
            Self::HookStop => 10,
            Self::Paused => 9,
            Self::ActiveTime => 8,
            Self::Budget => 7,
            Self::Capacity => 6,
            Self::ContextFull => 5,
            Self::DelegationDeadline => 4,
            Self::Refused(_) => 3,
            Self::ToolStopped => 2,
            Self::InvalidCommandResult => 1,
        }
    }

    pub(super) fn merge(self, later: Self) -> Self {
        if later.rank() > self.rank() {
            later
        } else {
            self
        }
    }
}

#[must_use]
pub(super) enum StepEnd {
    Continue,
    Stopped(StopCause),
}

#[must_use]
pub(super) enum SizingRefusal {
    DelegationDeadline,
    ActiveTime,
    ContextFull,
}

impl SizingRefusal {
    pub(super) fn from_error(error: EngineError) -> Result<Self, EngineError> {
        match error {
            EngineError::GoalBudgetExhausted => Ok(Self::ActiveTime),
            EngineError::ContextTooLarge => Ok(Self::ContextFull),
            error => Err(error),
        }
    }
}

impl From<SizingRefusal> for StopCause {
    fn from(refusal: SizingRefusal) -> Self {
        match refusal {
            SizingRefusal::DelegationDeadline => Self::DelegationDeadline,
            SizingRefusal::ActiveTime => Self::ActiveTime,
            SizingRefusal::ContextFull => Self::ContextFull,
        }
    }
}

// Observations read once per decision from one ledger view and the token; never decision state.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct RunFacts {
    pub(super) cancel_requested: bool,
    // A primary Tool or a lifecycle hook of this Run was journaled Cancelled.
    pub(super) action_cancelled: bool,
    pub(super) hook_stop: bool,
    pub(super) uncertain: bool,
    // The Goal is open for this Run with no reservation, no other agent is active, and every
    // primary Tool is acknowledged unless the Run is cancelled.
    pub(super) settleable: bool,
    pub(super) active_time_exhausted: bool,
    pub(super) tokens_within_limit: bool,
    pub(super) tokens_open: bool,
    pub(super) calls_left: bool,
    pub(super) workers_settled: bool,
    // The plan criterion is unmet and the Goal admits no next Run.
    pub(super) criterion_closed: bool,
    pub(super) autonomous: bool,
    pub(super) outcome_correction_pending: bool,
}

impl RunFacts {
    pub(super) fn observe(run: &RunLoop<'_>) -> Result<Self, EngineError> {
        let ledger = run.ledger();
        let active_reserved = ledger.active_reserved();
        let goal_id = run.goal()?;
        let (mut facts, acknowledged) = ledger.view(|view| -> Result<_, EngineError> {
            let goal = view.goal(goal_id).ok_or(EngineError::InvalidHistory)?;
            let current = view
                .runs
                .iter()
                .find(|current| current.id == run.run_id)
                .ok_or(EngineError::InvalidHistory)?;
            let primary_active = current
                .agents
                .iter()
                .any(|agent| agent.id == run.primary_id && agent.status == AgentStatus::Active);
            let others_closed = current
                .agents
                .iter()
                .all(|agent| agent.id == run.primary_id || agent.status != AgentStatus::Active);
            let uncertain = current.lifecycle_hooks.iter().any(|hook| {
                hook.outcome.as_ref().is_some_and(|outcome| {
                    outcome.observation.stops_run()
                        && outcome.observation.disposition != ToolDisposition::Cancelled
                })
            }) || current.tools.iter().any(|tool| {
                tool.observation
                    .iter()
                    .chain(tool.hooks.iter().filter_map(|hook| {
                        hook.outcome.as_ref().map(|outcome| &outcome.observation)
                    }))
                    .any(|observation| observation.disposition == ToolDisposition::Uncertain)
            }) || run.uncertain_hook_in(current);
            let action_cancelled = current.lifecycle_hooks.iter().any(|hook| {
                hook.outcome.as_ref().is_some_and(|outcome| {
                    outcome.observation.disposition == ToolDisposition::Cancelled
                })
            }) || current.tools.iter().any(|tool| {
                tool.observation.as_ref().is_some_and(|observation| {
                    observation.disposition == ToolDisposition::Cancelled
                })
            });
            let acknowledged = current.tools.iter().all(|tool| {
                tool.observation.as_ref().is_some_and(|observation| {
                    observation.disposition != ToolDisposition::Uncertain
                })
            });
            let goal_open = goal.status == GoalStatus::Running
                && goal.active_run == Some(run.run_id)
                && goal.unobserved_provider_calls.is_empty()
                && goal.compaction_call_reserved.is_none();
            let facts = Self {
                action_cancelled,
                outcome_correction_pending: current
                    .primary()
                    .is_some_and(|agent| agent.outcome_correction_pending()),
                hook_stop: current.hooks_settled() && current.hook_stop_reason().is_some(),
                uncertain,
                settleable: goal_open
                    && !active_reserved
                    && current.status == RunStatus::Active
                    && primary_active
                    && others_closed,
                workers_settled: view.runs.last().is_some_and(|last| {
                    last.agents
                        .iter()
                        .filter_map(|agent| agent.assignment.as_ref())
                        .all(|worker| worker.settled())
                }),
                ..Self::default()
            };
            Ok((facts, acknowledged))
        })??;
        facts.cancel_requested = run.controls.cancellation.is_cancelled();
        facts.settleable &= acknowledged || facts.cancelled();
        facts.active_time_exhausted = ledger.active_time_exhausted();
        facts.tokens_within_limit = ledger.tokens_within_limit();
        facts.tokens_open = ledger.can_start_provider_call();
        facts.calls_left = ledger.remaining_calls() != Some(0);
        facts.criterion_closed = !ledger.success_criterion_satisfied() && !ledger.allows_next_run();
        facts.autonomous = ledger.autonomous();
        Ok(facts)
    }

    pub(super) fn cancelled(&self) -> bool {
        self.cancel_requested || self.action_cancelled
    }

    pub(super) fn tool_admitted(&self) -> bool {
        self.tokens_open && self.calls_left
    }

    // The interruptions the facts show, merged by rank.
    pub(super) fn observed(&self) -> Option<StopCause> {
        [
            (self.uncertain, StopCause::Uncertain),
            (self.cancelled(), StopCause::Cancelled),
            (self.hook_stop, StopCause::HookStop),
        ]
        .into_iter()
        .filter_map(|(seen, cause)| seen.then_some(cause))
        .reduce(StopCause::merge)
    }
}

#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum PrimaryEnd {
    Finished(Answer),
    Stopped(StopCause),
    // A definite failure no stop cause names: a failed Provider call, a plan step that cannot be
    // admitted, a refused agent definition or a failed Skill read.
    Failed(RunFailure),
}

#[must_use]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Settlement {
    Finish(Answer),
    // `exhausted`: settlement agrees the Goal admits no next Run.
    Fail {
        failure: RunFailure,
        exhausted: bool,
    },
    Cancel,
    HookStop,
    Unknown,
    Unsettleable,
}

impl Settlement {
    const BUDGET: Self = Self::Fail {
        failure: RunFailure::BudgetExhausted,
        exhausted: true,
    };

    const fn failed(failure: RunFailure) -> Self {
        Self::Fail {
            failure,
            exhausted: false,
        }
    }

    fn or_unsettleable(self, facts: &RunFacts) -> Self {
        if facts.settleable {
            self
        } else {
            Self::Unsettleable
        }
    }
}

// An accepted answer stands once given; active time spent after it does not undo it.
pub(super) fn settle(end: PrimaryEnd, facts: &RunFacts) -> Settlement {
    let interrupted = facts.observed();
    let lapsed = facts.active_time_exhausted.then_some(StopCause::ActiveTime);
    match end {
        PrimaryEnd::Stopped(cause) => stopped(
            [interrupted, lapsed]
                .into_iter()
                .flatten()
                .fold(cause, StopCause::merge),
            facts,
        ),
        PrimaryEnd::Finished(answer) => match interrupted {
            Some(cause) => stopped(cause, facts),
            None if !facts.workers_settled => {
                Settlement::failed(RunFailure::WorkersUnverified).or_unsettleable(facts)
            }
            // Only an autonomous Goal runs out of Runs; a single-Run Goal with an unmet plan simply fails.
            None if facts.criterion_closed => Settlement::Fail {
                failure: RunFailure::CriterionUnmet,
                exhausted: facts.autonomous,
            }
            .or_unsettleable(facts),
            None => Settlement::Finish(answer).or_unsettleable(facts),
        },
        PrimaryEnd::Failed(failure) => {
            match [interrupted, lapsed]
                .into_iter()
                .flatten()
                .reduce(StopCause::merge)
            {
                Some(cause) => stopped(cause, facts),
                // A definition parse refused once no Provider call can start is the token budget's refusal.
                None if failure == RunFailure::DefinitionInvalid && !facts.tokens_open => {
                    Settlement::BUDGET.or_unsettleable(facts)
                }
                None => Settlement::Fail {
                    failure,
                    exhausted: failure == RunFailure::BudgetExhausted,
                }
                .or_unsettleable(facts),
            }
        }
    }
}

fn stopped(cause: StopCause, facts: &RunFacts) -> Settlement {
    let settlement = match cause {
        StopCause::Uncertain => return Settlement::Unknown,
        StopCause::Cancelled | StopCause::Paused => Settlement::Cancel,
        StopCause::HookStop => Settlement::HookStop,
        StopCause::ActiveTime | StopCause::Budget => Settlement::BUDGET,
        StopCause::ContextFull => Settlement::failed(RunFailure::ToolContextFull),
        StopCause::Refused(_) => Settlement::failed(RunFailure::RefusalLimit),
        StopCause::Capacity
        | StopCause::DelegationDeadline
        | StopCause::ToolStopped
        | StopCause::InvalidCommandResult => Settlement::failed(RunFailure::ToolStopped),
    };
    settlement.or_unsettleable(facts)
}

#[cfg(test)]
impl RunFacts {
    pub(super) fn ready() -> Self {
        Self {
            settleable: true,
            tokens_within_limit: true,
            tokens_open: true,
            calls_left: true,
            workers_settled: true,
            ..Self::default()
        }
    }

    pub(super) fn with(mut self, update: impl FnOnce(&mut Self)) -> Self {
        update(&mut self);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAUSES: [StopCause; 13] = [
        StopCause::Uncertain,
        StopCause::Cancelled,
        StopCause::HookStop,
        StopCause::Paused,
        StopCause::ActiveTime,
        StopCause::Budget,
        StopCause::Capacity,
        StopCause::ContextFull,
        StopCause::DelegationDeadline,
        StopCause::Refused(ProposalRefusalCause::Phase),
        StopCause::Refused(ProposalRefusalCause::InvalidArguments),
        StopCause::ToolStopped,
        StopCause::InvalidCommandResult,
    ];

    #[test]
    fn sizing_and_stop_precedence() {
        for (refusal, expected) in [
            (SizingRefusal::ActiveTime, StopCause::ActiveTime),
            (SizingRefusal::ContextFull, StopCause::ContextFull),
            (
                SizingRefusal::DelegationDeadline,
                StopCause::DelegationDeadline,
            ),
        ] {
            assert_eq!(StopCause::from(refusal), expected);
        }
        for (index, cause) in CAUSES.into_iter().enumerate() {
            assert_eq!(cause.merge(cause), cause);
            for (later_index, later) in CAUSES.into_iter().enumerate() {
                let merged = cause.merge(later);
                assert!(merged.rank() >= cause.rank() && merged.rank() >= later.rank());
                assert_eq!(
                    merged,
                    if index <= later_index || cause.rank() == later.rank() {
                        cause
                    } else {
                        later
                    }
                );
                for last in CAUSES {
                    assert_eq!(
                        cause.merge(later).merge(last),
                        cause.merge(later.merge(last))
                    );
                }
            }
        }
    }

    fn facts(bits: [bool; 5]) -> RunFacts {
        let [cancelled, hook_stop, uncertain, budget_closed, settleable] = bits;
        RunFacts::ready().with(|facts| {
            facts.cancel_requested = cancelled;
            facts.hook_stop = hook_stop;
            facts.uncertain = uncertain;
            facts.settleable = settleable;
            facts.tokens_open = !budget_closed;
        })
    }

    fn combinations() -> impl Iterator<Item = [bool; 5]> {
        (0..32_u8).map(|bits| std::array::from_fn(|index| bits & (1 << index) != 0))
    }

    #[test]
    fn every_stop_cause_settles_by_the_one_precedence() {
        let tool = || Settlement::failed(RunFailure::ToolStopped);
        // (cause, settlement with no interruption, whether a settled hook stop outranks it)
        let rows = [
            (StopCause::Uncertain, Settlement::Unknown, false),
            (StopCause::Cancelled, Settlement::Cancel, false),
            (StopCause::HookStop, Settlement::HookStop, false),
            (StopCause::Paused, Settlement::Cancel, true),
            (StopCause::ActiveTime, Settlement::BUDGET, true),
            (StopCause::Budget, Settlement::BUDGET, true),
            (StopCause::Capacity, tool(), true),
            (
                StopCause::ContextFull,
                Settlement::failed(RunFailure::ToolContextFull),
                true,
            ),
            (StopCause::DelegationDeadline, tool(), true),
            (
                StopCause::Refused(ProposalRefusalCause::Phase),
                Settlement::failed(RunFailure::RefusalLimit),
                true,
            ),
            (StopCause::ToolStopped, tool(), true),
            (StopCause::InvalidCommandResult, tool(), true),
        ];
        for (cause, base, below_hook) in rows {
            for bits in combinations() {
                let [cancelled, hook_stop, uncertain, _, settleable] = bits;
                let expected = if uncertain || cause == StopCause::Uncertain {
                    Settlement::Unknown
                } else if !settleable {
                    Settlement::Unsettleable
                } else if cancelled {
                    Settlement::Cancel
                } else if hook_stop && below_hook {
                    Settlement::HookStop
                } else {
                    base.clone()
                };
                assert_eq!(
                    settle(PrimaryEnd::Stopped(cause), &facts(bits)),
                    expected,
                    "{cause:?} with [cancelled, hook stop, uncertain, budget closed, settleable] = {bits:?}"
                );
            }
        }
    }

    #[test]
    fn answers_failures_and_lapsed_active_time_settle_by_their_rows() {
        let open = RunFacts::ready();
        let answer = || Answer {
            text: "done".into(),
        };
        let lapsed = open.with(|facts| facts.active_time_exhausted = true);
        let rows: [(PrimaryEnd, RunFacts, Settlement); 22] = [
            (
                PrimaryEnd::Finished(answer()),
                open,
                Settlement::Finish(answer()),
            ),
            (
                PrimaryEnd::Finished(answer()),
                lapsed,
                Settlement::Finish(answer()),
            ),
            (
                PrimaryEnd::Finished(answer()),
                open.with(|facts| facts.action_cancelled = true),
                Settlement::Cancel,
            ),
            (
                PrimaryEnd::Finished(answer()),
                open.with(|facts| facts.hook_stop = true),
                Settlement::HookStop,
            ),
            (
                PrimaryEnd::Finished(answer()),
                open.with(|facts| facts.uncertain = true),
                Settlement::Unknown,
            ),
            (
                PrimaryEnd::Finished(answer()),
                open.with(|facts| facts.settleable = false),
                Settlement::Unsettleable,
            ),
            (
                PrimaryEnd::Finished(answer()),
                open.with(|facts| facts.workers_settled = false),
                Settlement::failed(RunFailure::WorkersUnverified),
            ),
            (
                PrimaryEnd::Finished(answer()),
                open.with(|facts| facts.criterion_closed = true),
                Settlement::failed(RunFailure::CriterionUnmet),
            ),
            (
                PrimaryEnd::Finished(answer()),
                open.with(|facts| {
                    facts.criterion_closed = true;
                    facts.autonomous = true;
                }),
                Settlement::Fail {
                    failure: RunFailure::CriterionUnmet,
                    exhausted: true,
                },
            ),
            (
                PrimaryEnd::Failed(RunFailure::ProviderCall),
                open,
                Settlement::failed(RunFailure::ProviderCall),
            ),
            (
                PrimaryEnd::Failed(RunFailure::ProviderCall),
                open.with(|facts| facts.tokens_open = false),
                Settlement::failed(RunFailure::ProviderCall),
            ),
            (
                PrimaryEnd::Failed(RunFailure::ProviderCall),
                lapsed,
                Settlement::BUDGET,
            ),
            (
                PrimaryEnd::Failed(RunFailure::ProviderCall),
                open.with(|facts| facts.cancel_requested = true),
                Settlement::Cancel,
            ),
            (
                PrimaryEnd::Failed(RunFailure::DefinitionInvalid),
                open,
                Settlement::failed(RunFailure::DefinitionInvalid),
            ),
            (
                PrimaryEnd::Failed(RunFailure::DefinitionInvalid),
                open.with(|facts| facts.tokens_open = false),
                Settlement::BUDGET,
            ),
            (
                PrimaryEnd::Failed(RunFailure::DefinitionInvalid),
                open.with(|facts| facts.calls_left = false),
                Settlement::failed(RunFailure::DefinitionInvalid),
            ),
            (
                PrimaryEnd::Failed(RunFailure::DefinitionInvalid),
                open.with(|facts| facts.action_cancelled = true),
                Settlement::Cancel,
            ),
            (
                PrimaryEnd::Failed(RunFailure::DefinitionInvalid),
                lapsed,
                Settlement::BUDGET,
            ),
            (
                PrimaryEnd::Stopped(StopCause::ContextFull),
                lapsed,
                Settlement::BUDGET,
            ),
            (
                PrimaryEnd::Stopped(StopCause::HookStop),
                lapsed,
                Settlement::HookStop,
            ),
            (
                PrimaryEnd::Stopped(StopCause::Paused),
                lapsed,
                Settlement::Cancel,
            ),
            (
                PrimaryEnd::Stopped(StopCause::ToolStopped),
                open.with(|facts| facts.action_cancelled = true),
                Settlement::Cancel,
            ),
        ];
        for (end, facts, expected) in rows {
            let case = format!("{end:?} with {facts:?}");
            assert_eq!(settle(end, &facts), expected, "{case}");
        }
    }
}

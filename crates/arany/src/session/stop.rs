use super::{
    GoalStatus, GoalView, RunFailure, RunStatus, RunView, STORE_ACTION_OVERHEAD_MS, SessionView,
};

/// The Goal budget dimension that admitted no next Run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoalLimit {
    Calls,
    Runs,
    ActiveTime,
    Tokens,
}

/// Why a Run stopped short of completing its Goal, projected from committed facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunStop {
    GoalExhausted(GoalLimit),
    /// The Goal waits at this Run for an explicit resume.
    Paused,
    Cancelled,
    StoppedByHook,
    /// The Run has no terminal receipt, so its outcome is unknown.
    Uncertain,
    Failed(RunFailure),
}

impl SessionView {
    /// The one stop projection for live results and history; `None` while `run` is open or when
    /// it finished without stopping its Goal.
    #[must_use]
    pub fn run_stop(&self, run: &RunView) -> Option<RunStop> {
        let goal = self
            .goals
            .iter()
            .find(|goal| goal.run_ids.contains(&run.id))?;
        // An unaccepted link has no Run view, so the latest accepted Run is where the Goal stopped.
        let latest = self
            .runs
            .iter()
            .rev()
            .find(|candidate| goal.run_ids.contains(&candidate.id))
            .is_some_and(|candidate| candidate.id == run.id);
        project(goal, run, latest)
    }
}

fn project(goal: &GoalView, run: &RunView, latest: bool) -> Option<RunStop> {
    if matches!(run.status, RunStatus::Pending | RunStatus::Active) {
        return None;
    }
    if latest && goal.budget_closed {
        return Some(RunStop::GoalExhausted(exhausted_limit(goal)));
    }
    let paused = latest && matches!(goal.status, GoalStatus::Running | GoalStatus::Interrupted);
    match run.status {
        RunStatus::Pending | RunStatus::Active => None,
        RunStatus::Interrupted => Some(RunStop::Uncertain),
        RunStatus::Finished | RunStatus::Cancelled if paused => Some(RunStop::Paused),
        RunStatus::Finished => None,
        RunStatus::Cancelled => Some(RunStop::Cancelled),
        RunStatus::StoppedByHook => Some(RunStop::StoppedByHook),
        RunStatus::Failed => run.failure.map(RunStop::Failed),
    }
}

// Replay accepts `budget_closed` only on a spent budget, so the last dimension is the one left.
fn exhausted_limit(goal: &GoalView) -> GoalLimit {
    let budget = goal.effective_budget();
    if budget
        .max_tokens
        .is_some_and(|limit| goal.token_usage_unknown || goal.known_tokens >= limit)
    {
        GoalLimit::Tokens
    } else if budget
        .max_provider_calls
        .is_some_and(|limit| goal.provider_calls >= limit)
    {
        GoalLimit::Calls
    } else if budget
        .max_active_ms
        .is_some_and(|limit| limit.saturating_sub(goal.active_ms) <= STORE_ACTION_OVERHEAD_MS)
    {
        GoalLimit::ActiveTime
    } else {
        GoalLimit::Runs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::unanswered_run;
    use crate::session::{GoalBudget, GoalId, GoalPhase, GoalSuccessState, WorkspaceIdentity};

    fn goal(status: GoalStatus, budget_closed: bool) -> GoalView {
        GoalView {
            id: GoalId::new(),
            objective: "Stop".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
            budget: GoalBudget {
                max_provider_calls: Some(4),
                max_tokens: Some(100),
                max_active_ms: Some(60_000),
                max_runs: 2,
            },
            resource_settings: crate::tools::ToolResourceSettings::default(),
            phase: GoalPhase::Implementation,
            autonomous: true,
            status,
            run_ids: Vec::new(),
            active_run: None,
            plan_contract: None,
            success_state: GoalSuccessState::NoContract,
            success_criterion_satisfied: false,
            awaiting_plan_contract: false,
            active_ms: 0,
            uncertain_active_ms: 0,
            active_deadline: None,
            active_ceiling_ms: 0,
            provider_calls: 1,
            unobserved_provider_calls: Vec::new(),
            compaction_call_reserved: None,
            uncertain_compaction_calls: 0,
            known_tokens: 10,
            known_cached_tokens: 0,
            token_usage_unknown: false,
            report: None,
            started_sequence: 1,
            finished_sequence: None,
            continues: None,
            budget_closed,
        }
    }

    #[test]
    fn committed_run_and_goal_facts_project_one_closed_stop() {
        use GoalStatus::{Completed, Failed, Interrupted, Running};
        let failure = |status| (status == RunStatus::Failed).then_some(RunFailure::ToolStopped);
        // (Run status, Goal status, budget closed, latest accepted Run, expected stop)
        let rows = [
            (RunStatus::Active, Running, false, true, None),
            (RunStatus::Pending, Running, false, true, None),
            (RunStatus::Finished, Completed, false, true, None),
            (RunStatus::Finished, Running, false, false, None),
            (
                RunStatus::Finished,
                Running,
                false,
                true,
                Some(RunStop::Paused),
            ),
            (
                RunStatus::Finished,
                Interrupted,
                false,
                true,
                Some(RunStop::Paused),
            ),
            (
                RunStatus::Cancelled,
                Interrupted,
                false,
                true,
                Some(RunStop::Paused),
            ),
            (
                RunStatus::Cancelled,
                Running,
                false,
                false,
                Some(RunStop::Cancelled),
            ),
            (
                RunStatus::Cancelled,
                Failed,
                false,
                true,
                Some(RunStop::Cancelled),
            ),
            (
                RunStatus::StoppedByHook,
                Interrupted,
                false,
                true,
                Some(RunStop::StoppedByHook),
            ),
            (
                RunStatus::Interrupted,
                Interrupted,
                false,
                true,
                Some(RunStop::Uncertain),
            ),
            (
                RunStatus::Interrupted,
                Failed,
                false,
                false,
                Some(RunStop::Uncertain),
            ),
            (
                RunStatus::Failed,
                Failed,
                false,
                true,
                Some(RunStop::Failed(RunFailure::ToolStopped)),
            ),
            (
                RunStatus::Failed,
                Failed,
                true,
                true,
                Some(RunStop::GoalExhausted(GoalLimit::Runs)),
            ),
            (
                RunStatus::Finished,
                Failed,
                true,
                true,
                Some(RunStop::GoalExhausted(GoalLimit::Runs)),
            ),
            // A crash-left Run that a later resume closed for budget reports the Goal's exhaustion.
            (
                RunStatus::Interrupted,
                Failed,
                true,
                true,
                Some(RunStop::GoalExhausted(GoalLimit::Runs)),
            ),
            // Only the latest accepted Run carries the Goal's exhaustion.
            (RunStatus::Finished, Failed, true, false, None),
            (RunStatus::Active, Failed, true, true, None),
        ];
        for (status, goal_status, closed, latest, expected) in rows {
            let mut run = unanswered_run(status);
            run.failure = failure(status);
            assert_eq!(
                project(&goal(goal_status, closed), &run, latest),
                expected,
                "{status:?} in a {goal_status:?} Goal, closed {closed}, latest {latest}"
            );
        }
    }

    #[test]
    fn exhaustion_names_tokens_then_calls_then_active_time_then_runs() {
        let spent = |change: fn(&mut GoalView)| {
            let mut goal = goal(GoalStatus::Failed, true);
            change(&mut goal);
            exhausted_limit(&goal)
        };
        type Change = fn(&mut GoalView);
        let rows: [(Change, GoalLimit); 7] = [
            (|_| {}, GoalLimit::Runs),
            (|goal| goal.active_ms = 55_000, GoalLimit::ActiveTime),
            (|goal| goal.provider_calls = 4, GoalLimit::Calls),
            (|goal| goal.known_tokens = 100, GoalLimit::Tokens),
            (|goal| goal.token_usage_unknown = true, GoalLimit::Tokens),
            (
                |goal| {
                    goal.provider_calls = 4;
                    goal.active_ms = 60_000;
                },
                GoalLimit::Calls,
            ),
            (
                |goal| {
                    goal.budget.max_tokens = None;
                    goal.token_usage_unknown = true;
                },
                GoalLimit::Runs,
            ),
        ];
        for (index, (change, expected)) in rows.into_iter().enumerate() {
            assert_eq!(spent(change), expected, "row {index}");
        }
    }

    #[test]
    fn the_session_projection_judges_the_latest_accepted_run_of_the_runs_goal() {
        let first = unanswered_run(RunStatus::Finished);
        let second = unanswered_run(RunStatus::Interrupted);
        let mut open = goal(GoalStatus::Interrupted, false);
        // The trailing link was never accepted, so the second Run is still the latest.
        open.run_ids = vec![first.id, second.id, crate::session::RunId::new()];
        let session = crate::session::SessionId::new();
        let mut view = SessionView::replay(
            session,
            &[crate::session::EventEnvelope {
                sequence: 1,
                session_id: session,
                run_id: None,
                agent_run_id: None,
                event: crate::session::Event::SessionStarted {
                    title: "Stops".into(),
                    workspace_identity: open.workspace_identity,
                },
                created_at_ms: 0,
            }],
        )
        .expect("Session start")
        .expect("Session view");
        view.runs = vec![first.clone(), second.clone()];
        view.goals = vec![open];
        assert_eq!(view.run_stop(&first), None);
        assert_eq!(view.run_stop(&second), Some(RunStop::Uncertain));
        assert_eq!(view.run_stop(&unanswered_run(RunStatus::Failed)), None);
    }
}

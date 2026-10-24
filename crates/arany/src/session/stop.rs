use super::{GoalStatus, GoalUsage, GoalView, RunFailure, RunStatus, RunView, SessionView};
use crate::tools::{CacheUpdate, ToolDisposition};

/// The Goal budget dimension that admitted no next Run.
#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalLimit {
    Calls,
    Runs,
    ActiveTime,
    Tokens,
}

/// Why a Run stopped short of completing its Goal, projected from committed facts.
#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStop {
    GoalExhausted(GoalLimit),
    /// The Goal waits at this Run for an explicit resume.
    Paused,
    Cancelled,
    StoppedByHook,
    /// The Run has no terminal receipt and committed an effect whose outcome is unknown.
    Uncertain,
    /// The Run has no terminal receipt and no open or unknown effect.
    Interrupted,
    Failed(RunFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub struct RunFailureFeedback {
    pub stop: RunStop,
    pub provider_disposition: Option<super::ProviderCallDisposition>,
    pub provider_failure: Option<crate::provider::ProviderFailureReason>,
    pub refusal_cause: Option<super::ProposalRefusalCause>,
}

impl SessionView {
    pub(crate) fn previous_run_failure(&self) -> Option<RunFailureFeedback> {
        let run = self.runs.last()?;
        let stop = self
            .run_stop(run)
            .or_else(|| run.failure.map(RunStop::Failed))?;
        if run.status == RunStatus::Finished
            && stop == RunStop::Paused
            && self.goals.iter().any(|goal| {
                goal.autonomous
                    && goal.status == GoalStatus::Running
                    && goal.run_ids.contains(&run.id)
            })
        {
            return None;
        }
        let call = run.primary().and_then(|primary| {
            primary
                .provider_calls
                .iter()
                .rev()
                .find(|call| call.phase != crate::provider::AgentPhase::ToolReview)
        });
        Some(RunFailureFeedback {
            stop,
            provider_disposition: call.map(|call| call.disposition),
            provider_failure: call.and_then(|call| call.failure_reason),
            refusal_cause: call.and_then(|call| call.refusal.as_ref().map(|refusal| refusal.cause)),
        })
    }

    pub fn budget_stop_goal(&self, run: &RunView) -> Option<&GoalView> {
        let goal = self
            .goals
            .iter()
            .find(|goal| goal.run_ids.contains(&run.id))?;
        match self.run_stop(run)? {
            RunStop::GoalExhausted(_) => Some(goal),
            RunStop::Failed(RunFailure::BudgetExhausted)
                if goal.effective_budget().spent(GoalUsage {
                    runs: self
                        .runs
                        .iter()
                        .filter(|candidate| goal.run_ids.contains(&candidate.id))
                        .count(),
                    active_ms: goal.active_ms,
                    provider_calls: goal.provider_calls,
                    known_tokens: goal.known_tokens,
                    token_usage_unknown: goal.token_usage_unknown,
                }) =>
            {
                Some(goal)
            }
            RunStop::Paused
            | RunStop::Cancelled
            | RunStop::StoppedByHook
            | RunStop::Uncertain
            | RunStop::Interrupted
            | RunStop::Failed(_) => None,
        }
    }

    pub fn budget_stop_limit(&self, run: &RunView) -> Option<GoalLimit> {
        self.budget_stop_goal(run).map(exhausted_limit)
    }

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
        project(goal, run, latest, goal.resume_usage(self).runs)
    }
}

fn project(goal: &GoalView, run: &RunView, latest: bool, accepted_runs: usize) -> Option<RunStop> {
    match run.status {
        RunStatus::Pending | RunStatus::Active => return None,
        RunStatus::Interrupted if unknown_effect(run) => return Some(RunStop::Uncertain),
        RunStatus::Interrupted
        | RunStatus::Finished
        | RunStatus::Failed
        | RunStatus::Cancelled
        | RunStatus::StoppedByHook => {}
    }
    // A Run left without a terminal receipt by a spent budget proves the exhaustion from its counts.
    let spent = goal.effective_budget().spent(GoalUsage {
        runs: accepted_runs,
        provider_calls: goal.provider_calls,
        active_ms: goal.active_ms,
        known_tokens: goal.known_tokens,
        token_usage_unknown: goal.token_usage_unknown,
    });
    if latest && (goal.budget_closed || (run.status == RunStatus::Interrupted && spent)) {
        return Some(RunStop::GoalExhausted(exhausted_limit(goal)));
    }
    let paused = latest && matches!(goal.status, GoalStatus::Running | GoalStatus::Interrupted);
    match run.status {
        RunStatus::Pending | RunStatus::Active => None,
        RunStatus::Interrupted => Some(RunStop::Interrupted),
        RunStatus::Finished | RunStatus::Cancelled if paused => Some(RunStop::Paused),
        RunStatus::Finished => None,
        RunStatus::Cancelled => Some(RunStop::Cancelled),
        RunStatus::StoppedByHook => Some(RunStop::StoppedByHook),
        RunStatus::Failed => run.failure.map(RunStop::Failed),
    }
}

// A started effect, Tool or hook, whose outcome is not journaled, or is journaled uncertain.
fn unknown_effect(run: &RunView) -> bool {
    run.tools
        .iter()
        .chain(run.agents.iter().flat_map(|agent| &agent.tools))
        .any(|tool| {
            tool.observation.as_ref().is_none_or(|observation| {
                observation.disposition == ToolDisposition::Uncertain
                    || observation.cache_update == CacheUpdate::Uncertain
            }) || tool.hooks.iter().any(hook_unknown)
        })
        || run.lifecycle_hooks.iter().any(hook_unknown)
}

fn hook_unknown(hook: &super::reducer::HookView) -> bool {
    hook.outcome
        .as_ref()
        .is_none_or(|outcome| outcome.observation.disposition == ToolDisposition::Uncertain)
}

// Replay accepts `budget_closed` only on a spent budget, so the last dimension is the one left.
fn exhausted_limit(goal: &GoalView) -> GoalLimit {
    let budget = goal.effective_budget();
    goal.budget_refusal
        .map(|refusal| refusal.dimension)
        .unwrap_or_else(|| {
            budget
                .exhausted_dimension(usage_of(goal))
                .unwrap_or(GoalLimit::Runs)
        })
}

fn usage_of(goal: &GoalView) -> GoalUsage {
    GoalUsage {
        runs: goal.run_ids.len(),
        provider_calls: goal.provider_calls,
        active_ms: goal.active_ms,
        known_tokens: goal.known_tokens,
        token_usage_unknown: goal.token_usage_unknown,
    }
}

pub fn escape_status_text(value: &str) -> std::borrow::Cow<'_, str> {
    if !value.chars().any(status_character_needs_escape) {
        return std::borrow::Cow::Borrowed(value);
    }
    let mut safe = String::with_capacity(value.len());
    for character in value.chars() {
        let code = character as u32;
        if status_character_needs_escape(character) {
            safe.push_str(&format!("\\u{{{code:04x}}}"));
        } else {
            safe.push(character);
        }
    }
    std::borrow::Cow::Owned(safe)
}

pub(crate) fn status_character_needs_escape(character: char) -> bool {
    character.is_control()
        || matches!(
            character as u32,
            0x00ad
                | 0x061c
                | 0x180e
                // Zero-width joiners stay: emoji and many scripts need them.
                | 0x200b
                | 0x200e
                | 0x200f
                | 0x2028..=0x202e
                | 0x2060..=0x2069
                | 0xfeff
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::unanswered_run;
    use crate::session::{GoalBudget, GoalId, GoalPhase, GoalSuccessState, WorkspaceIdentity};

    fn goal(status: GoalStatus, budget_closed: bool) -> GoalView {
        GoalView {
            include_paths: Some(Vec::new()),
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
            budget_refusal: None,
            run_start_usage: Default::default(),
        }
    }

    // A Read Tool with the given observation, `None` when only its intent is journaled.
    fn tool(observed: Option<(ToolDisposition, CacheUpdate)>) -> crate::session::ToolView {
        let intent = crate::tools::EffectIntent {
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
            call: crate::tools::ToolCall::Read {
                path: "facts.txt".into(),
                offset: None,
                limit: Some(32),
            },
            limits: crate::tools::ToolLimits::default(),
            use_count: 1,
        };
        crate::session::ToolView {
            checkpoint: None,
            permission_verdict: None,
            hooks: Vec::new(),
            hook_sequence: None,
            intent: intent.clone(),
            observation: observed.map(|(disposition, cache_update)| {
                crate::tools::ToolObservation {
                    enforcement_cause: None,
                    limit_cause: None,
                    intent,
                    disposition,
                    output: String::new(),
                    attestation: None,
                    approval_cause: None,
                    cache_update,
                }
            }),
            review_call_index: None,
            finished_sequence: observed.map(|_| 2),
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
            // A crash before any effect is an ordinary interruption, never an unknown effect.
            (
                RunStatus::Interrupted,
                Interrupted,
                false,
                true,
                Some(RunStop::Interrupted),
            ),
            (
                RunStatus::Interrupted,
                Failed,
                false,
                false,
                Some(RunStop::Interrupted),
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
                project(&goal(goal_status, closed), &run, latest, 0),
                expected,
                "{status:?} in a {goal_status:?} Goal, closed {closed}, latest {latest}"
            );
        }
    }

    #[test]
    fn only_committed_effect_evidence_makes_an_interrupted_run_uncertain() {
        let open = goal(GoalStatus::Interrupted, false);
        let mut spent = goal(GoalStatus::Interrupted, false);
        spent.provider_calls = 4;
        let known = Some((ToolDisposition::Succeeded, CacheUpdate::NotApplicable));
        // (Tool evidence, Goal, expected stop of the latest interrupted Run)
        let rows = [
            (Vec::new(), &open, RunStop::Interrupted),
            (vec![tool(known)], &open, RunStop::Interrupted),
            (vec![tool(None)], &open, RunStop::Uncertain),
            (
                vec![
                    tool(known),
                    tool(Some((
                        ToolDisposition::Uncertain,
                        CacheUpdate::NotApplicable,
                    ))),
                ],
                &open,
                RunStop::Uncertain,
            ),
            (
                vec![tool(Some((
                    ToolDisposition::Succeeded,
                    CacheUpdate::Uncertain,
                )))],
                &open,
                RunStop::Uncertain,
            ),
            // A spent budget proves the exhaustion of a Run that could not settle.
            (Vec::new(), &spent, RunStop::GoalExhausted(GoalLimit::Calls)),
            (
                vec![tool(known)],
                &spent,
                RunStop::GoalExhausted(GoalLimit::Calls),
            ),
            // An unknown effect outranks the exhaustion.
            (vec![tool(None)], &spent, RunStop::Uncertain),
        ];
        for (index, (tools, goal, expected)) in rows.into_iter().enumerate() {
            let mut run = unanswered_run(RunStatus::Interrupted);
            run.tools = tools;
            assert_eq!(project(goal, &run, true, 0), Some(expected), "row {index}");
        }
        let mut finished = unanswered_run(RunStatus::Failed);
        finished.tools = vec![tool(None)];
        assert_eq!(
            project(&goal(GoalStatus::Failed, false), &finished, true, 0),
            Some(RunStop::Failed(RunFailure::ProposalRefused)),
            "a terminal receipt keeps its own outcome"
        );
    }

    #[test]
    fn previous_run_feedback_preserves_closed_stops_even_without_a_primary_call() {
        let fixture = crate::session::worker_protocol_fixture();
        let mut view = SessionView::replay(fixture.session, &fixture.envelopes())
            .unwrap()
            .unwrap();
        for (status, goal_status, unknown, expected) in [
            (
                RunStatus::Interrupted,
                GoalStatus::Interrupted,
                false,
                Some(RunStop::Interrupted),
            ),
            (
                RunStatus::Interrupted,
                GoalStatus::Interrupted,
                true,
                Some(RunStop::Uncertain),
            ),
            (
                RunStatus::Cancelled,
                GoalStatus::Failed,
                false,
                Some(RunStop::Cancelled),
            ),
            (
                RunStatus::Cancelled,
                GoalStatus::Interrupted,
                false,
                Some(RunStop::Paused),
            ),
            (
                RunStatus::StoppedByHook,
                GoalStatus::Interrupted,
                false,
                Some(RunStop::StoppedByHook),
            ),
            (
                RunStatus::Failed,
                GoalStatus::Failed,
                false,
                Some(RunStop::Failed(RunFailure::ProposalRefused)),
            ),
            (RunStatus::Finished, GoalStatus::Completed, false, None),
            (RunStatus::Finished, GoalStatus::Running, false, None),
            (
                RunStatus::Finished,
                GoalStatus::Interrupted,
                false,
                Some(RunStop::Paused),
            ),
        ] {
            let mut run = unanswered_run(status);
            run.objective = "PRIVATE_OBJECTIVE".into();
            if unknown {
                run.tools.push(tool(None));
            }
            let mut goal = goal(goal_status, false);
            goal.run_ids.push(run.id);
            view.runs = vec![run];
            view.goals = vec![goal];
            let feedback = view.previous_run_failure();
            assert_eq!(
                feedback.map(|feedback| feedback.stop),
                expected,
                "{status:?}"
            );
            if let Some(feedback) = feedback {
                assert_eq!(feedback.provider_disposition, None);
                assert!(
                    !serde_json::to_string(&feedback)
                        .unwrap()
                        .contains("PRIVATE_OBJECTIVE")
                );
            }
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
        let mut second = unanswered_run(RunStatus::Interrupted);
        second.tools = vec![tool(None)];
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
        // The unaccepted link consumes no Run budget; unsettled time stays out of this projection.
        view.goals[0].active_ceiling_ms = 60_000;
        for (max_runs, expected) in [
            (3, RunStop::Interrupted),
            (2, RunStop::GoalExhausted(GoalLimit::Runs)),
        ] {
            view.goals[0].budget.max_runs = max_runs;
            view.runs[1].tools.clear();
            assert_eq!(view.run_stop(&view.runs[1]), Some(expected));
            assert_eq!(view.goals[0].run_ids.len(), 3);
        }
        view.goals[0].budget.max_runs = 3;
        view.goals[0].provider_calls = 4;
        view.runs[1].status = RunStatus::Failed;
        view.runs[1].failure = Some(RunFailure::BudgetExhausted);
        assert_eq!(
            view.run_stop(&view.runs[1]),
            Some(RunStop::Failed(RunFailure::BudgetExhausted))
        );
        assert_eq!(
            view.budget_stop_limit(&view.runs[1]),
            Some(GoalLimit::Calls)
        );
        view.goals[0].provider_calls = 3;
        assert_eq!(
            view.budget_stop_limit(&view.runs[1]),
            None,
            "an unaccepted link never proves exhaustion"
        );
    }
}

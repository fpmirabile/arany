use super::{
    AgentStatus, GoalView, RunStatus, RunView, SessionView, current_goal_mut, current_run,
    model_tool_count,
};
use crate::provider::AgentPhase;
use crate::session::events::{
    AgentRole, AgentRunId, GoalBudget, GoalId, GoalPhase, GoalPlanContract, GoalReport, GoalStatus,
    ProviderCallDisposition, ReplayError, RunId,
};

impl SessionView {
    pub(super) fn start_goal(
        &mut self,
        goal_id: &GoalId,
        objective: &str,
        workspace_identity: &(u64, u64),
        budget: &GoalBudget,
        phase: &GoalPhase,
        prior: u64,
    ) -> Result<(), ReplayError> {
        if self.goals.iter().any(|goal| goal.id == *goal_id)
            || self
                .workspace_identity
                .is_some_and(|identity| identity != *workspace_identity)
        {
            return Err(ReplayError::InvalidTransition);
        }
        // A new Goal pauses an unfinished predecessor, such as one a crash left Running.
        for goal in self
            .goals
            .iter_mut()
            .filter(|goal| goal.status == GoalStatus::Running)
        {
            seal_unfinished_goal(goal, &mut self.runs);
        }
        self.workspace_identity = Some(*workspace_identity);
        self.goals.push(GoalView {
            id: *goal_id,
            objective: objective.to_owned(),
            workspace_identity: *workspace_identity,
            budget: *budget,
            resource_settings: crate::tools::ToolResourceSettings::default(),
            phase: *phase,
            autonomous: false,
            status: GoalStatus::Running,
            run_ids: Vec::new(),
            active_run: None,
            plan_contract: None,
            success_state: crate::session::GoalSuccessState::NoContract,
            success_criterion_satisfied: false,
            awaiting_plan_contract: false,
            active_ms: 0,
            uncertain_active_ms: 0,
            reserved_active_run: None,
            reserved_active_ms: None,
            provider_calls: 0,
            unobserved_provider_calls: Vec::new(),
            compaction_call_reserved: None,
            uncertain_compaction_calls: 0,
            known_tokens: 0,
            token_usage_unknown: false,
            report: None,
            started_sequence: prior,
            finished_sequence: None,
        });
        Ok(())
    }

    pub(super) fn link_goal_run(
        &mut self,
        goal_id: &GoalId,
        run_id: &RunId,
        phase: &GoalPhase,
    ) -> Result<(), ReplayError> {
        if self.runs.iter().any(|run| run.id == *run_id) {
            return Err(ReplayError::InvalidTransition);
        }
        let goal = current_goal_mut(self, *goal_id)?;
        if goal.status != GoalStatus::Running
            || goal.active_run.is_some()
            || goal.reserved_active_ms.is_some()
            || !goal.admits_next_run()
        {
            return Err(ReplayError::InvalidTransition);
        }
        if goal.phase != *phase {
            return Err(ReplayError::InvalidTransition);
        }
        goal.active_run = Some(*run_id);
        goal.run_ids.push(*run_id);
        Ok(())
    }

    pub(super) fn resume_goal(&mut self, goal_id: &GoalId) -> Result<(), ReplayError> {
        let index = self
            .resumable_goal_index()
            .filter(|index| self.goals[*index].id == *goal_id)
            .ok_or(ReplayError::InvalidTransition)?;
        // A crashed predecessor is still Running in the raw journal; this explicit resume seals it.
        if self.goals[index].status == GoalStatus::Running {
            seal_unfinished_goal(&mut self.goals[index], &mut self.runs);
        }
        let recoverable = self.goals[index].can_resume(self);
        let has_unaccepted_run = self.goals[index]
            .run_ids
            .last()
            .is_some_and(|run_id| !self.runs.iter().any(|run| run.id == *run_id));
        let goal = &mut self.goals[index];
        if goal.status != GoalStatus::Interrupted || goal.active_run.is_some() || !recoverable {
            return Err(ReplayError::InvalidTransition);
        }
        settle_unfinished_facts(goal, has_unaccepted_run)?;
        let active_limit = goal.effective_budget().max_active_ms;
        if active_limit.is_some_and(|limit| goal.active_ms > limit) {
            return Err(ReplayError::InvalidTransition);
        }
        goal.awaiting_plan_contract = false;
        goal.status = GoalStatus::Running;
        Ok(())
    }

    pub(super) fn reserve_goal_active_time(
        &mut self,
        goal_id: &GoalId,
        run_id: &RunId,
        reserved_ms: &u64,
    ) -> Result<(), ReplayError> {
        let run_status = current_run(self, *run_id)?.status;
        let goal = current_goal_mut(self, *goal_id)?;
        let consumed = goal
            .active_ms
            .checked_add(*reserved_ms)
            .ok_or(ReplayError::InvalidTransition)?;
        let active_limit = goal.effective_budget().max_active_ms;
        // A compaction call runs after its Run has ended, so its reservation follows the Run's terminal status.
        let active_run_matches = goal.active_run == Some(*run_id)
            || goal.active_run.is_none()
                && run_status.is_terminal()
                && goal.run_ids.last() == Some(run_id)
                && goal.compaction_call_reserved == Some(*run_id);
        if goal.status != GoalStatus::Running
            || !active_run_matches
            || goal.reserved_active_ms.is_some()
            || active_limit.is_some_and(|limit| consumed > limit)
        {
            return Err(ReplayError::InvalidTransition);
        }
        goal.reserved_active_run = Some(*run_id);
        goal.reserved_active_ms = Some(*reserved_ms);
        Ok(())
    }

    pub(super) fn settle_goal_active_time(
        &mut self,
        goal_id: &GoalId,
        run_id: &RunId,
        active_ms: &u64,
    ) -> Result<(), ReplayError> {
        let run_status = current_run(self, *run_id)?.status;
        let compaction_committed = self
            .compactions
            .last()
            .is_some_and(|compaction| compaction.record.covered_run_id == *run_id);
        let goal = current_goal_mut(self, *goal_id)?;
        goal.reserved_active_ms
            .ok_or(ReplayError::InvalidTransition)?;
        let total = goal
            .active_ms
            .checked_add(*active_ms)
            .ok_or(ReplayError::InvalidTransition)?;
        let settles_active_run =
            goal.active_run == Some(*run_id) && run_status == RunStatus::Active;
        let settles_finished_run = goal.active_run.is_none()
            && run_status.is_terminal()
            && goal.run_ids.last() == Some(run_id)
            && (goal.compaction_call_reserved == Some(*run_id)
                || goal.compaction_call_reserved.is_none() && compaction_committed);
        if goal.status != GoalStatus::Running
            || !(settles_active_run || settles_finished_run)
            || goal.reserved_active_run != Some(*run_id)
        {
            return Err(ReplayError::InvalidTransition);
        }
        goal.active_ms = total;
        goal.reserved_active_run = None;
        goal.reserved_active_ms = None;
        Ok(())
    }

    pub(super) fn record_goal_plan_contract(
        &mut self,
        goal_id: &GoalId,
        run_id: &RunId,
        contract: &GoalPlanContract,
    ) -> Result<(), ReplayError> {
        let run = self
            .runs
            .last()
            .filter(|run| run.id == *run_id)
            .ok_or(ReplayError::InvalidTransition)?;
        let primary = run.primary().ok_or(ReplayError::InvalidTransition)?;
        let valid_previous_tools = run.tools.iter().all(|tool| {
            tool.intent.call.is_goal_plan_prelude()
                && tool
                    .observation
                    .as_ref()
                    .is_some_and(|observation| !observation.stops_run())
        });
        let requested_tools = primary
            .provider_calls
            .iter()
            .filter(|call| call.disposition == ProviderCallDisposition::ToolRequested)
            .count();
        let valid_contract_call = run.status == RunStatus::Active
            && primary.role == AgentRole::Primary
            && primary.provider_calls.last().is_some_and(|call| {
                call.disposition == ProviderCallDisposition::ToolRequested
                    && matches!(call.phase, AgentPhase::RootPlan | AgentPhase::RootSynthesis)
            })
            && valid_previous_tools
            && requested_tools == model_tool_count(run) + 1;
        // Pre-plan inspections count: folding every Run's observations decides the criterion.
        let mut success_state = contract.initial_success_state();
        for observation in self
            .runs
            .iter()
            .filter(|run| {
                self.goal(*goal_id)
                    .is_some_and(|goal| goal.run_ids.contains(&run.id))
            })
            .flat_map(|run| run.tools.iter())
            .filter_map(|tool| tool.observation.as_ref())
        {
            contract.observe_success(&mut success_state, observation);
        }
        let satisfied = contract.success_state_satisfied(&success_state);
        let goal = current_goal_mut(self, *goal_id)?;
        if goal.status != GoalStatus::Running
            || goal.active_run != Some(*run_id)
            || goal.reserved_active_ms.is_some()
            || goal.plan_contract.is_some()
            || !goal.awaiting_plan_contract
            || !valid_contract_call
            || !contract.budget.is_within(&goal.budget)
            || contract
                .budget
                .max_provider_calls
                .is_some_and(|limit| goal.provider_calls > limit)
            || goal.run_ids.len() > usize::from(contract.budget.max_runs)
            || contract
                .budget
                .max_active_ms
                .is_some_and(|limit| goal.active_ms > limit)
            || contract
                .budget
                .max_tokens
                .is_some_and(|limit| goal.token_usage_unknown || goal.known_tokens >= limit)
        {
            return Err(ReplayError::InvalidTransition);
        }
        goal.plan_contract = Some(contract.clone());
        goal.success_criterion_satisfied = satisfied;
        goal.success_state = success_state;
        goal.awaiting_plan_contract = false;
        Ok(())
    }

    pub(super) fn reserve_goal_provider_call(
        &mut self,
        goal_id: &GoalId,
        run_id: &RunId,
        agent_run_id: &AgentRunId,
    ) -> Result<(), ReplayError> {
        let agent_active = current_run(self, *run_id)?
            .agents
            .iter()
            .any(|agent| agent.id == *agent_run_id && agent.status == AgentStatus::Active);
        let goal = current_goal_mut(self, *goal_id)?;
        let budget = goal.effective_budget();
        if goal.status != GoalStatus::Running
            || goal.active_run != Some(*run_id)
            || !agent_active
            || goal.unobserved_provider_calls.contains(agent_run_id)
            || goal.compaction_call_reserved.is_some()
            || budget
                .max_provider_calls
                .is_some_and(|limit| goal.provider_calls >= limit)
            || budget.max_tokens.is_some_and(|limit| {
                goal.token_usage_unknown
                    || goal.known_tokens >= limit
                    || !goal.unobserved_provider_calls.is_empty()
            })
        {
            return Err(ReplayError::InvalidTransition);
        }
        goal.provider_calls = goal
            .provider_calls
            .checked_add(1)
            .ok_or(ReplayError::InvalidTransition)?;
        goal.unobserved_provider_calls.push(*agent_run_id);
        Ok(())
    }

    pub(super) fn reserve_goal_compaction_call(
        &mut self,
        goal_id: &GoalId,
        run_id: &RunId,
    ) -> Result<(), ReplayError> {
        let run = self
            .runs
            .last()
            .filter(|run| run.id == *run_id)
            .ok_or(ReplayError::InvalidTransition)?;
        let run_status = run.status;
        let active_run_ready =
            run_status == RunStatus::Active
                && !run.tools.is_empty()
                && run.tools.iter().all(|tool| tool.observation.is_some())
                && run.tools.last().is_some_and(|tool| {
                    tool.observation
                        .as_ref()
                        .is_some_and(|observation| !observation.stops_run())
                })
                && run.primary().is_some_and(|primary| {
                    let Some(request_index) = primary.provider_calls.iter().rposition(|call| {
                        call.disposition == ProviderCallDisposition::ToolRequested
                    }) else {
                        return false;
                    };
                    primary.status == AgentStatus::Active
                        && matches!(
                            primary.provider_calls[request_index].phase,
                            AgentPhase::RootPlan | AgentPhase::RootSynthesis
                        )
                        && primary.provider_calls[request_index + 1..]
                            .iter()
                            .all(|call| {
                                call.phase == AgentPhase::ToolReview
                                    && call.disposition == ProviderCallDisposition::Finished
                            })
                        && model_tool_count(run)
                            == primary
                                .provider_calls
                                .iter()
                                .filter(|call| {
                                    call.disposition == ProviderCallDisposition::ToolRequested
                                })
                                .count()
                });
        let goal = current_goal_mut(self, *goal_id)?;
        let budget = goal.effective_budget();
        if goal.status != GoalStatus::Running
            || !(goal.active_run.is_none() && run_status.is_terminal()
                || goal.active_run == Some(*run_id) && active_run_ready)
            || goal.run_ids.last() != Some(run_id)
            || !goal.unobserved_provider_calls.is_empty()
            || goal.compaction_call_reserved.is_some()
            || budget
                .max_provider_calls
                .is_some_and(|limit| goal.provider_calls >= limit)
            || budget
                .max_tokens
                .is_some_and(|limit| goal.token_usage_unknown || goal.known_tokens >= limit)
        {
            return Err(ReplayError::InvalidTransition);
        }
        goal.provider_calls = goal
            .provider_calls
            .checked_add(1)
            .ok_or(ReplayError::InvalidTransition)?;
        goal.compaction_call_reserved = Some(*run_id);
        Ok(())
    }

    pub(super) fn finish_goal(
        &mut self,
        goal_id: &GoalId,
        status: &GoalStatus,
        active_ms: &u64,
        report: &GoalReport,
        prior: u64,
    ) -> Result<(), ReplayError> {
        // An unaccepted trailing link is not a Run, so the last accepted Run decides the disposition.
        let run_status = self
            .goal(*goal_id)
            .and_then(|goal| {
                goal.run_ids
                    .iter()
                    .rev()
                    .find_map(|run_id| self.runs.iter().find(|run| run.id == *run_id))
            })
            .map(|run| run.status)
            .ok_or(ReplayError::InvalidTransition)?;
        let goal = current_goal_mut(self, *goal_id)?;
        let active_limit = goal.effective_budget().max_active_ms;
        // Settlement fails an unsatisfied planned Goal whose Finished Run admits no next Run; a planless or
        // satisfied Goal completes instead.
        let exhausted_after_finish = goal.autonomous
            && run_status == RunStatus::Finished
            && goal.plan_contract.is_some()
            && !goal.success_criterion_satisfied
            && !goal.admits_next_run();
        let disposition_matches = matches!(
            (*status, run_status),
            (GoalStatus::Completed, RunStatus::Finished) | (GoalStatus::Failed, RunStatus::Failed)
        ) || (*status == GoalStatus::Failed && exhausted_after_finish);
        let terminal = goal.status == GoalStatus::Running
            && goal.active_run.is_none()
            && disposition_matches
            && goal.reserved_active_ms.is_none()
            && goal.compaction_call_reserved.is_none()
            && *active_ms == goal.active_ms
            && (*status != GoalStatus::Completed
                || active_limit.is_none_or(|limit| *active_ms <= limit))
            && !(*status == GoalStatus::Completed
                && goal.plan_contract.is_some()
                && !goal.success_criterion_satisfied);
        if !terminal {
            return self
                .close_exhausted_goal(goal_id, *status, run_status, *active_ms, report, prior);
        }
        let goal = current_goal_mut(self, *goal_id)?;
        goal.active_ms = *active_ms;
        goal.status = *status;
        goal.report = Some(report.clone());
        goal.finished_sequence = Some(prior);
        Ok(())
    }

    /// Closes a paused Goal that admits no next Run, as explicit resume finds it. Settles the same
    /// unfinished facts resume would, so the close is refused whenever a Run is still admissible.
    fn close_exhausted_goal(
        &mut self,
        goal_id: &GoalId,
        status: GoalStatus,
        run_status: RunStatus,
        active_ms: u64,
        report: &GoalReport,
        prior: u64,
    ) -> Result<(), ReplayError> {
        let goal = self.goal(*goal_id).ok_or(ReplayError::InvalidTransition)?;
        if status != GoalStatus::Failed
            || !matches!(goal.status, GoalStatus::Running | GoalStatus::Interrupted)
        {
            return Err(ReplayError::InvalidTransition);
        }
        let mut settled = goal.clone();
        let mut runs = self.runs.clone();
        // A crash leaves the live Run unfinished in the raw journal; sealing it reads as interrupted.
        let run_status = if settled.active_run.is_some() {
            seal_unfinished_goal(&mut settled, &mut runs);
            RunStatus::Interrupted
        } else {
            run_status
        };
        // A paused Goal is one whose last Run was cancelled or interrupted, or whose non-autonomous Run
        // finished without finishing the Goal; an autonomous Finished Run is settled by its own rules.
        let paused = matches!(run_status, RunStatus::Cancelled | RunStatus::Interrupted)
            || (run_status == RunStatus::Finished && !goal.autonomous);
        if !paused {
            return Err(ReplayError::InvalidTransition);
        }
        let has_unaccepted_run = settled
            .run_ids
            .last()
            .is_some_and(|run_id| !self.runs.iter().any(|run| run.id == *run_id));
        settle_unfinished_facts(&mut settled, has_unaccepted_run)?;
        if settled.admits_resume(self) || active_ms != settled.active_ms {
            return Err(ReplayError::InvalidTransition);
        }
        settled.status = GoalStatus::Failed;
        settled.awaiting_plan_contract = false;
        settled.report = Some(report.clone());
        settled.finished_sequence = Some(prior);
        self.runs = runs;
        *current_goal_mut(self, *goal_id)? = settled;
        Ok(())
    }
}

/// Settles the facts an unfinished Goal leaves open when it pauses: an unaccepted trailing link is not a
/// Run, a reserved active-time segment counts as uncertain time, and crashed or orphaned provider calls
/// leave usage unknown.
fn settle_unfinished_facts(
    goal: &mut GoalView,
    has_unaccepted_run: bool,
) -> Result<(), ReplayError> {
    if has_unaccepted_run {
        goal.run_ids.pop();
    }
    match (goal.reserved_active_run, goal.reserved_active_ms) {
        (Some(run_id), Some(reserved_ms)) if goal.run_ids.last() == Some(&run_id) => {
            goal.active_ms = goal
                .active_ms
                .checked_add(reserved_ms)
                .ok_or(ReplayError::InvalidTransition)?;
            goal.uncertain_active_ms = goal
                .uncertain_active_ms
                .checked_add(reserved_ms)
                .ok_or(ReplayError::InvalidTransition)?;
            goal.reserved_active_run = None;
            goal.reserved_active_ms = None;
        }
        (None, None) => {}
        _ => return Err(ReplayError::InvalidTransition),
    }
    if let Some(run_id) = goal.compaction_call_reserved.take() {
        if goal.run_ids.last() != Some(&run_id) {
            return Err(ReplayError::InvalidTransition);
        }
        goal.uncertain_compaction_calls = goal
            .uncertain_compaction_calls
            .checked_add(1)
            .ok_or(ReplayError::InvalidTransition)?;
        goal.token_usage_unknown = true;
    }
    if !goal.unobserved_provider_calls.is_empty() {
        goal.unobserved_provider_calls.clear();
        goal.token_usage_unknown = true;
    }
    Ok(())
}

/// Projects an unfinished Goal as paused: its live Run is interrupted and no later event may retry its calls or effects.
fn seal_unfinished_goal(goal: &mut GoalView, runs: &mut [RunView]) {
    if let Some(run) = goal
        .active_run
        .and_then(|run_id| runs.iter_mut().find(|run| run.id == run_id))
    {
        run.interrupt();
    }
    goal.active_run = None;
    goal.awaiting_plan_contract = false;
    goal.status = GoalStatus::Interrupted;
}

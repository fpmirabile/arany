use super::{
    AgentStatus, GoalView, RunStatus, SessionView, current_goal_mut, current_run,
    goal_tool_observations, model_tool_count,
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
            || self.goals.last().is_some_and(GoalView::is_active)
            || self
                .workspace_identity
                .is_some_and(|identity| identity != *workspace_identity)
        {
            return Err(ReplayError::InvalidTransition);
        }
        self.workspace_identity = Some(*workspace_identity);
        self.goals.push(GoalView {
            id: *goal_id,
            objective: objective.to_owned(),
            workspace_identity: *workspace_identity,
            budget: budget.clone(),
            phase: *phase,
            autonomous: false,
            status: GoalStatus::Running,
            run_ids: Vec::new(),
            active_run: None,
            plan_contract: None,
            success_criterion_satisfied: false,
            awaiting_plan_contract: false,
            active_ms: 0,
            uncertain_active_ms: 0,
            active_time_reservation: None,
            provider_calls: 0,
            unobserved_provider_calls: Vec::new(),
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
        let run_limit = goal.effective_budget().max_runs;
        if goal.status != GoalStatus::Running
            || goal.active_run.is_some()
            || goal.active_time_reservation.is_some()
            || goal.run_ids.len() >= usize::from(run_limit)
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
        let active_run = self
            .goals
            .last()
            .filter(|goal| goal.id == *goal_id && goal.status == GoalStatus::Running)
            .and_then(|goal| goal.active_run);
        if let Some(run_id) = active_run
            && let Some(run) = self.runs.last_mut().filter(|run| run.id == run_id)
        {
            run.interrupt();
        }
        // A crashed predecessor is still Running in the raw journal; this explicit resume seals it.
        if let Some(goal) = self
            .goals
            .last_mut()
            .filter(|goal| goal.id == *goal_id && goal.status == GoalStatus::Running)
        {
            goal.status = GoalStatus::Interrupted;
            goal.active_run = None;
        }
        let recoverable = self
            .goals
            .last()
            .filter(|goal| goal.id == *goal_id)
            .is_some_and(|goal| goal.can_resume(self));
        let has_unaccepted_run = self.goals.last().is_some_and(|goal| {
            goal.run_ids
                .last()
                .is_some_and(|run_id| !self.runs.iter().any(|run| run.id == *run_id))
        });
        let goal = current_goal_mut(self, *goal_id)?;
        if goal.status != GoalStatus::Interrupted || goal.active_run.is_some() || !recoverable {
            return Err(ReplayError::InvalidTransition);
        }
        if has_unaccepted_run {
            goal.run_ids.pop();
        }
        if let Some((run_id, reserved_ms)) = goal.active_time_reservation {
            if goal.run_ids.last() != Some(&run_id) {
                return Err(ReplayError::InvalidTransition);
            }
            goal.active_ms = goal
                .active_ms
                .checked_add(reserved_ms)
                .ok_or(ReplayError::InvalidTransition)?;
            goal.uncertain_active_ms = goal
                .uncertain_active_ms
                .checked_add(reserved_ms)
                .ok_or(ReplayError::InvalidTransition)?;
            goal.active_time_reservation = None;
        }
        // Orphaned provider reservations keep their call charge; their usage is unknown.
        if !goal.unobserved_provider_calls.is_empty() {
            goal.unobserved_provider_calls.clear();
            goal.token_usage_unknown = true;
        }
        let active_limit = goal.effective_budget().max_active_ms;
        if goal.active_ms > active_limit {
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
        let goal = current_goal_mut(self, *goal_id)?;
        let consumed = goal
            .active_ms
            .checked_add(*reserved_ms)
            .ok_or(ReplayError::InvalidTransition)?;
        let active_limit = goal.effective_budget().max_active_ms;
        if goal.status != GoalStatus::Running
            || goal.active_run != Some(*run_id)
            || goal.active_time_reservation.is_some()
            || consumed > active_limit
        {
            return Err(ReplayError::InvalidTransition);
        }
        goal.active_time_reservation = Some((*run_id, *reserved_ms));
        Ok(())
    }

    pub(super) fn settle_goal_active_time(
        &mut self,
        goal_id: &GoalId,
        run_id: &RunId,
        active_ms: &u64,
    ) -> Result<(), ReplayError> {
        let run_status = current_run(self, *run_id)?.status;
        let goal = current_goal_mut(self, *goal_id)?;
        let (reserved_run, reserved) = goal
            .active_time_reservation
            .ok_or(ReplayError::InvalidTransition)?;
        let total = goal
            .active_ms
            .checked_add(*active_ms)
            .ok_or(ReplayError::InvalidTransition)?;
        let active_limit = goal.effective_budget().max_active_ms;
        if goal.status != GoalStatus::Running
            || goal.active_run.is_some()
            || reserved_run != *run_id
            || *active_ms > reserved
            || !run_status.is_terminal()
            || total > active_limit
        {
            return Err(ReplayError::InvalidTransition);
        }
        goal.active_ms += *active_ms;
        goal.active_time_reservation = None;
        Ok(())
    }

    pub(super) fn record_goal_plan_contract(
        &mut self,
        goal_id: &GoalId,
        run_id: &RunId,
        contract: &GoalPlanContract,
        active_ms: &u64,
        reserved_ms: &u64,
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
        // Pre-plan inspections count: the latest verified observation decides the criterion.
        let satisfied = contract.matches_success(goal_tool_observations(self, *goal_id)?);
        let goal = current_goal_mut(self, *goal_id)?;
        let (reserved_run, reserved) = goal
            .active_time_reservation
            .ok_or(ReplayError::InvalidTransition)?;
        let active_total = goal
            .active_ms
            .checked_add(*active_ms)
            .ok_or(ReplayError::InvalidTransition)?;
        let reserved_total = active_total
            .checked_add(*reserved_ms)
            .ok_or(ReplayError::InvalidTransition)?;
        if goal.status != GoalStatus::Running
            || goal.active_run != Some(*run_id)
            || reserved_run != *run_id
            || goal.plan_contract.is_some()
            || !goal.awaiting_plan_contract
            || !valid_contract_call
            || !contract.budget.is_within(&goal.budget)
            || goal.provider_calls > contract.budget.max_provider_calls
            || goal.run_ids.len() > usize::from(contract.budget.max_runs)
            || *active_ms > reserved
            || active_ms.saturating_add(*reserved_ms) > reserved
            || reserved_total > contract.budget.max_active_ms
            || contract
                .budget
                .max_tokens
                .is_some_and(|limit| goal.token_usage_unknown || goal.known_tokens >= limit)
        {
            return Err(ReplayError::InvalidTransition);
        }
        goal.active_ms = active_total;
        goal.active_time_reservation = Some((*run_id, *reserved_ms));
        goal.plan_contract = Some(contract.clone());
        goal.success_criterion_satisfied = satisfied;
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
        let goal_call_limit = goal.effective_budget().max_provider_calls;
        let goal_token_limit = goal.effective_budget().max_tokens;
        if goal.status != GoalStatus::Running
            || goal.active_run != Some(*run_id)
            || !agent_active
            || goal.unobserved_provider_calls.contains(agent_run_id)
            || goal.provider_calls >= goal_call_limit
            || goal_token_limit.is_some_and(|limit| {
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

    pub(super) fn finish_goal(
        &mut self,
        goal_id: &GoalId,
        status: &GoalStatus,
        active_ms: &u64,
        report: &GoalReport,
        prior: u64,
    ) -> Result<(), ReplayError> {
        let run_id = self
            .goals
            .last()
            .filter(|goal| goal.id == *goal_id)
            .and_then(|goal| goal.run_ids.last())
            .copied()
            .ok_or(ReplayError::InvalidTransition)?;
        let run_status = current_run(self, run_id)?.status;
        let goal = current_goal_mut(self, *goal_id)?;
        let active_limit = goal.effective_budget().max_active_ms;
        // A Finished Run's settled active time can exhaust an autonomous Goal before its next Run is admitted.
        let exhausted_after_finish =
            goal.autonomous && run_status == RunStatus::Finished && goal.active_ms == active_limit;
        let disposition_matches = matches!(
            (*status, run_status),
            (GoalStatus::Completed, RunStatus::Finished)
                | (GoalStatus::Failed, RunStatus::Failed)
                | (GoalStatus::Cancelled, RunStatus::Cancelled)
        ) || (*status == GoalStatus::Failed && exhausted_after_finish);
        if goal.status != GoalStatus::Running
            || goal.active_run.is_some()
            || !disposition_matches
            || goal.active_time_reservation.is_some()
            || *active_ms != goal.active_ms
            || *active_ms > active_limit
            || *status == GoalStatus::Completed
                && goal.plan_contract.is_some()
                && !goal.success_criterion_satisfied
        {
            return Err(ReplayError::InvalidTransition);
        }
        goal.active_ms = *active_ms;
        goal.status = *status;
        goal.report = Some(report.clone());
        goal.finished_sequence = Some(prior);
        Ok(())
    }
}

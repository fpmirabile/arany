use super::{
    AgentStatus, GoalView, RunStatus, RunView, SessionView, ToolView, active_run, current_goal_mut,
    current_run, is_integration_tool, pending_tool_requests,
};
use crate::provider::AgentPhase;
use crate::session::events::{
    AgentRole, AgentRunId, CompactionStatus, Event, GoalBudget, GoalId, GoalPhase,
    GoalPlanContract, GoalReport, GoalStatus, ProviderCallDisposition, ReplayError, RunId,
};
use crate::session::{
    MAX_CHILD_STEERING_LABEL_BYTES, MAX_STEERING_LABEL_BYTES, MAX_STEERING_NOTE_BYTES,
    WorkspaceIdentity,
};

impl SessionView {
    // The event's fields plus the replay prior; a struct would only forward them.
    #[expect(
        clippy::too_many_arguments,
        reason = "Goal start carries independent admitted inputs"
    )]
    pub(super) fn start_goal(
        &mut self,
        goal_id: &GoalId,
        objective: &str,
        include_paths: &Option<Vec<String>>,
        workspace_identity: WorkspaceIdentity,
        budget: &GoalBudget,
        resource_settings: &crate::tools::ToolResourceSettings,
        phase: &GoalPhase,
        autonomous: bool,
        continues: Option<GoalId>,
        prior: u64,
    ) -> Result<(), ReplayError> {
        if let Some(source_id) = continues {
            let source = self.continuable_goal();
            if source.is_none_or(|source| {
                source.id != source_id
                    || source.objective != objective
                    || source.include_paths != *include_paths
                    || source.phase != *phase
                    || source.autonomous != autonomous
            }) {
                return Err(ReplayError::InvalidTransition);
            }
        }
        if self.goals.iter().any(|goal| goal.id == *goal_id)
            || self
                .workspace_identity
                .is_some_and(|identity| identity != workspace_identity)
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
        self.workspace_identity = Some(workspace_identity);
        self.goals.push(GoalView {
            id: *goal_id,
            objective: objective.to_owned(),
            include_paths: include_paths.clone(),
            workspace_identity,
            budget: *budget,
            resource_settings: *resource_settings,
            phase: *phase,
            autonomous,
            status: GoalStatus::Running,
            run_ids: Vec::new(),
            active_run: None,
            plan_contract: None,
            success_state: crate::session::GoalSuccessState::NoContract,
            success_criterion_satisfied: false,
            awaiting_plan_contract: false,
            active_ms: 0,
            uncertain_active_ms: 0,
            active_deadline: None,
            active_ceiling_ms: 0,
            provider_calls: 0,
            unobserved_provider_calls: Vec::new(),
            compaction_call_reserved: None,
            uncertain_compaction_calls: 0,
            known_tokens: 0,
            token_usage_unknown: false,
            report: None,
            started_sequence: prior,
            finished_sequence: None,
            known_cached_tokens: 0,
            continues,
            budget_closed: false,
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
            || goal.active_deadline.is_some()
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
        let valid_contract_call = run.status == RunStatus::Active
            && primary.role == AgentRole::Primary
            && primary
                .provider_calls
                .last()
                .is_some_and(|call| call.disposition == ProviderCallDisposition::PlanRecorded)
            && run.tools.iter().all(ToolView::settled_without_stop)
            && pending_tool_requests(run) == Some(0);
        // Pre-plan inspections count: folding every Run's observations decides the criterion.
        let mut success_state = contract.initial_success_state();
        let invalidated_after = self
            .rewinds
            .operations
            .iter()
            .filter(|r| {
                r.outcome() == crate::session::rewind::RewindOutcome::Restored
                    && crate::session::rewind::invalidates(contract, &r.files)
            })
            .filter_map(|r| r.finished_sequence)
            .max()
            .unwrap_or(0);
        for observation in self
            .runs
            .iter()
            .filter(|run| {
                self.goal(*goal_id)
                    .is_some_and(|goal| goal.run_ids.contains(&run.id))
            })
            .flat_map(|run| run.tools.iter())
            .filter(|tool| {
                tool.finished_sequence
                    .is_some_and(|s| s > invalidated_after)
            })
            .filter_map(|tool| tool.observation.as_ref())
        {
            contract.observe_success(&mut success_state, observation);
        }
        let satisfied = contract.success_state_satisfied(&success_state);
        let goal = current_goal_mut(self, *goal_id)?;
        if goal.status != GoalStatus::Running
            || goal.active_run != Some(*run_id)
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
        let run = current_run(self, *run_id)?;
        if run
            .primary()
            .is_some_and(|agent| agent.id == *agent_run_id && agent.refusal_limit_reached())
        {
            return Err(ReplayError::InvalidTransition);
        }
        if let Some(agent) = run.agents.iter().find(|agent| agent.id == *agent_run_id)
            && let Some(name) = &agent.definition
        {
            let registry = super::definition_registry(run)?;
            let definition = registry.get(name).ok_or(ReplayError::InvalidTransition)?;
            let own_tool_pending = agent
                .tools
                .last()
                .is_some_and(|tool| tool.observation.is_none());
            if !own_tool_pending
                && agent
                    .provider_calls
                    .iter()
                    .filter(|call| call.phase == AgentPhase::ChildWork)
                    .count()
                    >= usize::from(definition.max_turns)
            {
                return Err(ReplayError::InvalidTransition);
            }
        }
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

    pub(super) fn record_goal_steering_note(
        &mut self,
        goal_id: &GoalId,
        run_id: &RunId,
        text: &str,
    ) -> Result<(), ReplayError> {
        let run = active_run(self, *run_id)?;
        let primary_id = run
            .primary()
            .filter(|agent| agent.status == AgentStatus::Active)
            .map(|agent| agent.id)
            .ok_or(ReplayError::InvalidTransition)?;
        let stored_bytes = run.steering_bytes();
        // A pending Tool observation means this reservation is a Tool review, not a planning or synthesis call.
        let tool_pending = run
            .tools
            .last()
            .is_some_and(|tool| tool.observation.is_none());
        if tool_pending
            || stored_bytes + text.len() + MAX_STEERING_LABEL_BYTES > MAX_STEERING_NOTE_BYTES
        {
            return Err(ReplayError::InvalidTransition);
        }
        let goal = current_goal_mut(self, *goal_id)?;
        if goal.status != GoalStatus::Running
            || goal.active_run != Some(*run_id)
            || !goal.unobserved_provider_calls.contains(&primary_id)
        {
            return Err(ReplayError::InvalidTransition);
        }
        active_run(self, *run_id)?
            .steering_notes
            .push(text.to_owned());
        Ok(())
    }

    pub(super) fn record_agent_steering_note(
        &mut self,
        goal_id: &GoalId,
        run_id: &RunId,
        agent_run_id: &AgentRunId,
        text: &str,
    ) -> Result<(), ReplayError> {
        let run = active_run(self, *run_id)?;
        let agent = run
            .agents
            .iter()
            .find(|agent| agent.id == *agent_run_id)
            .ok_or(ReplayError::InvalidTransition)?;
        if agent.role != AgentRole::Child
            || agent.status != AgentStatus::Active
            || run.workers.stop.is_some()
            || run.hook_stop_reason().is_some()
            || agent
                .tools
                .last()
                .is_some_and(|tool| tool.observation.is_none())
            || run.steering_bytes() + text.len() + MAX_CHILD_STEERING_LABEL_BYTES
                > MAX_STEERING_NOTE_BYTES
        {
            return Err(ReplayError::InvalidTransition);
        }
        let goal = current_goal_mut(self, *goal_id)?;
        if goal.status != GoalStatus::Running
            || goal.active_run != Some(*run_id)
            || !goal.unobserved_provider_calls.contains(agent_run_id)
        {
            return Err(ReplayError::InvalidTransition);
        }
        active_run(self, *run_id)?
            .agents
            .iter_mut()
            .find(|agent| agent.id == *agent_run_id)
            .ok_or(ReplayError::InvalidTransition)?
            .steering_notes
            .push(text.to_owned());
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
                && !run.model_tools().is_empty()
                && run
                    .model_tools()
                    .iter()
                    .all(|tool| tool.observation.is_some())
                && run
                    .model_tools()
                    .last()
                    .is_some_and(ToolView::settled_without_stop)
                && run.hooks_settled()
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
                        && pending_tool_requests(run) == Some(0)
                });
        let primary_id = run.primary().ok_or(ReplayError::InvalidTransition)?.id;
        let goal = current_goal_mut(self, *goal_id)?;
        let budget = goal.effective_budget();
        if goal.status != GoalStatus::Running
            || !(goal.active_run.is_none() && run_status.is_terminal()
                || goal.active_run == Some(*run_id) && active_run_ready)
            || goal.run_ids.last() != Some(run_id)
            || goal.unobserved_provider_calls.contains(&primary_id)
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
        goal.compaction_call_reserved = Some(*run_id);
        Ok(())
    }

    pub(super) fn finish_goal(
        &mut self,
        goal_id: &GoalId,
        status: &GoalStatus,
        active_ms: &u64,
        report: &GoalReport,
        exhausted: bool,
        prior: u64,
    ) -> Result<(), ReplayError> {
        // The report's cache-read total must be exactly what the Goal accumulated from its Provider calls.
        if self
            .goal(*goal_id)
            .is_none_or(|goal| report.cached_input_tokens != goal.known_cached_tokens)
        {
            return Err(ReplayError::InvalidTransition);
        }
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
            && goal.active_deadline.is_none()
            && goal.compaction_call_reserved.is_none()
            && *active_ms == goal.active_ms
            && (*status != GoalStatus::Completed
                || active_limit.is_none_or(|limit| *active_ms <= limit))
            && !(*status == GoalStatus::Completed
                && goal.plan_contract.is_some()
                && !goal.success_criterion_satisfied);
        if !terminal {
            if !exhausted {
                return Err(ReplayError::InvalidTransition);
            }
            self.close_exhausted_goal(goal_id, *status, run_status, *active_ms, report, prior)?;
            current_goal_mut(self, *goal_id)?.budget_closed = true;
            return Ok(());
        }
        let goal = current_goal_mut(self, *goal_id)?;
        goal.active_ms = *active_ms;
        if exhausted && (*status != GoalStatus::Failed || !goal.budget_spent())
            || (exhausted_after_finish && *status == GoalStatus::Failed && !exhausted)
        {
            return Err(ReplayError::InvalidTransition);
        }
        goal.budget_closed = exhausted;
        goal.status = *status;
        goal.report = Some(report.clone());
        goal.finished_sequence = Some(prior);
        Ok(())
    }

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
        // A crash with open between-Run compaction can pause an autonomous Goal even after a Finished Run.
        let paused = matches!(
            run_status,
            RunStatus::Cancelled | RunStatus::Interrupted | RunStatus::StoppedByHook
        ) || (run_status == RunStatus::Finished
            && (!goal.autonomous || goal.compaction_call_reserved.is_some()));
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

fn settle_unfinished_facts(
    goal: &mut GoalView,
    has_unaccepted_run: bool,
) -> Result<(), ReplayError> {
    if has_unaccepted_run {
        goal.run_ids.pop();
    }
    if goal
        .active_deadline
        .is_some_and(|(run_id, _)| goal.run_ids.last() != Some(&run_id))
    {
        return Err(ReplayError::InvalidTransition);
    }
    if goal.active_deadline.is_some() || goal.active_ceiling_ms > 0 {
        let charge_ms = goal.unsettled_active_charge_ms();
        goal.active_ms = goal
            .active_ms
            .checked_add(charge_ms)
            .ok_or(ReplayError::InvalidTransition)?;
        goal.uncertain_active_ms = goal
            .uncertain_active_ms
            .checked_add(charge_ms)
            .ok_or(ReplayError::InvalidTransition)?;
        goal.active_deadline = None;
        goal.active_ceiling_ms = 0;
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

pub(super) fn record_active_ceiling(goal: &mut GoalView, ceiling_ms: u64) {
    goal.active_ceiling_ms = ceiling_ms;
}

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

pub(super) fn apply(
    current: &mut SessionView,
    event: &Event,
    prior: u64,
) -> Result<(), ReplayError> {
    match event {
        Event::GoalStarted {
            goal_id,
            objective,
            include_paths,
            workspace_identity,
            budget,
            resource_settings,
            phase,
            autonomous,
            continues,
        } => {
            current.start_goal(
                goal_id,
                objective,
                include_paths,
                *workspace_identity,
                budget,
                resource_settings,
                phase,
                *autonomous,
                *continues,
                prior,
            )?;
        }
        Event::GoalRunLinked {
            goal_id,
            run_id,
            phase,
        } => {
            current.link_goal_run(goal_id, run_id, phase)?;
        }
        Event::GoalResumed { goal_id } => {
            current.resume_goal(goal_id)?;
        }
        Event::GoalPlanContractRecorded {
            goal_id,
            run_id,
            contract,
        } => {
            current.record_goal_plan_contract(goal_id, run_id, contract)?;
        }
        Event::GoalProviderCallReserved {
            goal_id,
            run_id,
            agent_run_id,
            active_ceiling_ms,
        } => {
            if let Some(run) = current.runs.last().filter(|run| run.id == *run_id)
                && run.primary().is_some_and(|primary| {
                    primary.provider_calls.is_empty() && primary.id == *agent_run_id
                })
            {
                super::definition_registry(run)?;
                if run.skill.is_some()
                    && !run.invoked_skill_read().is_some_and(|tool| {
                        tool.observation.as_ref().is_some_and(|observation| {
                            observation.disposition == crate::tools::ToolDisposition::Succeeded
                                && observation.attestation.is_some()
                        })
                    })
                {
                    return Err(ReplayError::InvalidTransition);
                }
            }
            current.reserve_goal_provider_call(goal_id, run_id, agent_run_id)?;
            record_active_ceiling(current_goal_mut(current, *goal_id)?, *active_ceiling_ms);
        }
        Event::GoalCompactionCallReserved {
            goal_id,
            run_id,
            active_ceiling_ms,
        } => {
            current.reserve_goal_compaction_call(goal_id, run_id)?;
            record_active_ceiling(current_goal_mut(current, *goal_id)?, *active_ceiling_ms);
        }
        Event::ToolResultsCleared {
            run_id,
            covered_tool_count,
            cleared_bytes,
        } => {
            let run = current
                .runs
                .last()
                .filter(|run| run.id == *run_id)
                .ok_or(ReplayError::InvalidTransition)?;
            let goal = current
                .goals
                .iter()
                .find(|goal| goal.active_run == Some(*run_id))
                .ok_or(ReplayError::InvalidTransition)?;
            let count = *covered_tool_count as usize;
            let start = run
                .tool_results_cleared_count
                .max(run.tool_compaction_covered_count) as usize;
            let start = start.max(run.initial_tool_count());
            if run.status != RunStatus::Active
                || run
                    .primary()
                    .is_none_or(|primary| primary.status != AgentStatus::Active)
                || goal.status != GoalStatus::Running
                || run
                    .primary()
                    .is_none_or(|primary| goal.unobserved_provider_calls.contains(&primary.id))
                || goal.compaction_call_reserved.is_some()
                || !run.hooks_settled()
                || pending_tool_requests(run) != Some(0)
                || run.tools.iter().any(|tool| tool.observation.is_none())
                || count <= start
                || count > run.tools.len()
                || run
                    .live_tool_records()
                    .filter(|(index, _)| *index >= count)
                    .count()
                    < 3
                || !ends_whole_step(run, count)
            {
                return Err(ReplayError::InvalidTransition);
            }
            let mut before = 0_usize;
            let mut after = 0_usize;
            for (_, tool) in run
                .live_tool_records()
                .filter(|(index, _)| (start..count).contains(index))
            {
                let observation = tool
                    .observation
                    .as_ref()
                    .ok_or(ReplayError::InvalidTransition)?;
                let mut feedback = tool.hook_feedback();
                for proposal in run.hook_rerun_proposals(Some(tool.intent.digest())) {
                    if feedback.iter().map(String::len).sum::<usize>() + proposal.len()
                        <= crate::tools::permissions::MAX_HOOK_FEEDBACK_BYTES
                    {
                        feedback.push(proposal);
                    }
                }
                before += crate::tools::types::model_result(observation, &feedback)
                    .to_string()
                    .len();
                after += crate::tools::types::cleared_model_result(observation)
                    .to_string()
                    .len();
            }
            let savings = before.saturating_sub(after);
            if savings != *cleared_bytes as usize {
                return Err(ReplayError::InvalidTransition);
            }
            active_run(current, *run_id)?.tool_results_cleared_count = *covered_tool_count;
        }
        Event::GoalContextCompacted {
            goal_id,
            run_id,
            covered_tool_count,
            record,
        } => {
            let goal = current
                .goal(*goal_id)
                .ok_or(ReplayError::InvalidTransition)?;
            let run = current
                .runs
                .last()
                .filter(|run| run.id == *run_id)
                .ok_or(ReplayError::InvalidTransition)?;
            let covered_count =
                usize::try_from(*covered_tool_count).map_err(|_| ReplayError::InvalidTransition)?;
            let covered_tool = covered_count
                .checked_sub(1)
                .and_then(|index| run.tools.get(index))
                .ok_or(ReplayError::InvalidTransition)?;
            if goal.status != GoalStatus::Running
                || goal.active_run != Some(*run_id)
                || goal.run_ids.last() != Some(run_id)
                || goal.compaction_call_reserved != Some(*run_id)
                || run
                    .primary()
                    .is_none_or(|primary| goal.unobserved_provider_calls.contains(&primary.id))
                || !run.compaction_matches(record)
                || record.covered_run_id != *run_id
                || run.status != RunStatus::Active
                || run.tools.iter().any(|tool| tool.observation.is_none())
                || covered_count <= run.initial_tool_count()
                || covered_count > run.tools.len()
                || !ends_whole_step(run, covered_count)
                || *covered_tool_count <= run.tool_compaction_covered_count
                || covered_tool.finished_sequence != Some(record.covered_sequence)
            {
                return Err(ReplayError::InvalidTransition);
            }
            let tokens = u64::from(record.input_tokens.unwrap_or_default())
                .checked_add(u64::from(record.output_tokens.unwrap_or_default()))
                .and_then(|tokens| goal.known_tokens.checked_add(tokens))
                .ok_or(ReplayError::InvalidTransition)?;
            let cached = goal
                .known_cached_tokens
                .checked_add(u64::from(
                    record.cache_read_input_tokens.unwrap_or_default(),
                ))
                .ok_or(ReplayError::InvalidTransition)?;
            let run = active_run(current, *run_id)?;
            if let CompactionStatus::Succeeded { summary, .. } = &record.status {
                run.tool_compaction_summary = Some(summary.clone());
                run.tool_compaction_covered_count = *covered_tool_count;
            }
            let goal = current_goal_mut(current, *goal_id)?;
            goal.known_tokens = tokens;
            goal.known_cached_tokens = cached;
            goal.token_usage_unknown |=
                record.input_tokens.is_none() || record.output_tokens.is_none();
            goal.compaction_call_reserved = None;
        }
        Event::GoalFinished {
            goal_id,
            status,
            active_ms,
            report,
            exhausted,
        } => {
            current.finish_goal(goal_id, status, active_ms, report, *exhausted, prior)?;
        }
        Event::AgentSteeringNoteRecorded {
            goal_id,
            run_id,
            agent_run_id,
            text,
        } => {
            current.record_agent_steering_note(goal_id, run_id, agent_run_id, text)?;
        }
        Event::GoalSteeringNoteRecorded {
            goal_id,
            run_id,
            text,
        } => {
            current.record_goal_steering_note(goal_id, run_id, text)?;
        }
        _ => return Err(ReplayError::InvalidTransition),
    }
    Ok(())
}

// A compaction prefix must end on a whole step to preserve Tool-request correlation.
fn ends_whole_step(run: &RunView, covered_count: usize) -> bool {
    let Some(primary) = run.primary() else {
        return false;
    };
    if covered_count > run.tools.len()
        || covered_count <= run.initial_tool_count()
        || run
            .tools
            .get(covered_count)
            .is_some_and(is_integration_tool)
        || run.tools.get(covered_count - 1).is_some_and(|tool| {
            tool.narrowed_call().is_some()
                || matches!(
                    tool.intent.call,
                    crate::tools::ToolCall::PrepareWorker { .. }
                )
        })
    {
        return false;
    }
    let covered = run.tools[run.initial_tool_count()..covered_count]
        .iter()
        .filter(|tool| {
            !is_integration_tool(tool)
                && tool.narrowed_call().is_none()
                && !matches!(
                    tool.intent.call,
                    crate::tools::ToolCall::PrepareWorker { .. }
                )
        })
        .count();
    let step_ends = primary
        .provider_calls
        .iter()
        .filter(|call| call.disposition == ProviderCallDisposition::ToolRequested)
        .scan(0, |total, call| {
            *total += usize::from(call.requested_tools);
            Some(*total)
        });
    step_ends.into_iter().any(|end| end == covered)
}

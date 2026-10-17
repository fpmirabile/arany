use super::{
    AgentStatus, GoalView, RunStatus, RunView, SessionView, active_run, current_goal_mut,
    current_run, is_integration_tool, pending_tool_requests,
};
use crate::provider::AgentPhase;
use crate::session::events::{
    AgentRole, AgentRunId, CompactionStatus, Event, GoalBudget, GoalId, GoalPhase,
    GoalPlanContract, GoalReport, GoalStatus, ProviderCallDisposition, ReplayError, RunId,
};
use crate::session::{MAX_STEERING_LABEL_BYTES, MAX_STEERING_NOTE_BYTES};

impl SessionView {
    // The event's fields plus the replay prior; a struct would only forward them.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn start_goal(
        &mut self,
        goal_id: &GoalId,
        objective: &str,
        workspace_identity: &(u64, u64),
        budget: &GoalBudget,
        resource_settings: &crate::tools::ToolResourceSettings,
        phase: &GoalPhase,
        autonomous: bool,
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
        let valid_previous_tools = run.tools.iter().all(|tool| {
            tool.intent.call.is_goal_plan_prelude()
                && tool
                    .observation
                    .as_ref()
                    .is_some_and(|observation| !observation.stops_run())
        });
        let valid_contract_call = run.status == RunStatus::Active
            && primary.role == AgentRole::Primary
            && primary.provider_calls.last().is_some_and(|call| {
                call.disposition == ProviderCallDisposition::ToolRequested
                    && matches!(call.phase, AgentPhase::RootPlan | AgentPhase::RootSynthesis)
            })
            && valid_previous_tools
            // The contract precedes the step's first started request, so every request of the step is pending.
            && pending_tool_requests(run)
                == Some(primary.provider_calls.last().map_or(0, |call| usize::from(call.requested_tools)));
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

    /// Accepts a steering note only while the Run's primary call holds an open provider reservation.
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
        let stored_bytes: usize = run
            .steering_notes
            .iter()
            .map(|note| note.len() + MAX_STEERING_LABEL_BYTES)
            .sum();
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
                        && pending_tool_requests(run) == Some(0)
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
            && goal.active_deadline.is_none()
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
        // finished without finishing the Goal. A Finished autonomous Run is paused only when a crash left
        // its between-Run compaction call open.
        let paused = matches!(run_status, RunStatus::Cancelled | RunStatus::Interrupted)
            || (run_status == RunStatus::Finished
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

/// Settles the facts an unfinished Goal leaves open when it pauses: an unaccepted trailing link is not a
/// Run, unsettled active time is charged as uncertain time, and crashed or orphaned provider calls
/// leave usage unknown.
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

/// The latest started action's ceiling covers the Run's settled time and that action.
pub(super) fn record_active_ceiling(goal: &mut GoalView, ceiling_ms: u64) {
    goal.active_ceiling_ms = ceiling_ms;
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

pub(super) fn apply(
    current: &mut SessionView,
    event: &Event,
    prior: u64,
) -> Result<(), ReplayError> {
    match event {
        Event::GoalStarted {
            goal_id,
            objective,
            workspace_identity,
            budget,
            resource_settings,
            phase,
            autonomous,
        } => {
            current.start_goal(
                goal_id,
                objective,
                workspace_identity,
                budget,
                resource_settings,
                phase,
                *autonomous,
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
                || !goal.unobserved_provider_calls.is_empty()
                || !run.compaction_matches(record)
                || record.covered_run_id != *run_id
                || run.status != RunStatus::Active
                || run.tools.iter().any(|tool| tool.observation.is_none())
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
        } => {
            current.finish_goal(goal_id, status, active_ms, report, prior)?;
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

/// A covered prefix may not end inside a step: its model Tool count must be a cumulative step total.
fn ends_whole_step(run: &RunView, covered_count: usize) -> bool {
    let Some(primary) = run.primary() else {
        return false;
    };
    let covered = run.tools[..covered_count]
        .iter()
        .filter(|tool| !is_integration_tool(tool))
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

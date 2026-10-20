use super::{
    AgentStatus, RunStatus, RunView, SessionView, active_run, active_run_goal, current_goal_mut,
    current_run, is_delegated_synthesis, is_tool_continuation,
};
use crate::provider::AgentPhase;
use crate::session::events::{
    Event, GoalStatus, ProviderCallDisposition, ProviderCallRecord, ReplayError, RunDisposition,
    WorkspaceIdentity,
};

pub(super) fn apply(
    current: &mut SessionView,
    event: &Event,
    prior: u64,
) -> Result<(), ReplayError> {
    match event {
        Event::MessageAccepted {
            run_id,
            text,
            images,
            skill,
        } => {
            if current.runs.iter().any(|run| run.id == *run_id) {
                return Err(ReplayError::InvalidTransition);
            }
            let goal = active_run_goal(current, *run_id).ok_or(ReplayError::InvalidTransition)?;
            if goal.status != GoalStatus::Running
                || goal.run_ids.first() == Some(run_id) && goal.objective != *text
            {
                return Err(ReplayError::InvalidTransition);
            }
            if let Some(previous) = current.runs.last_mut() {
                previous.interrupt();
            }
            current.runs.push(RunView {
                skill: skill.clone(),
                supervisor: Default::default(),
                workers: Default::default(),
                id: *run_id,
                objective: text.clone(),
                images: images.clone(),
                config: None,
                agents: Vec::new(),
                assistant_message: None,
                status: RunStatus::Pending,
                accepted_sequence: prior,
                finished_sequence: None,
                failure: None,
                tools: Vec::new(),
                tool_compaction_summary: None,
                tool_compaction_covered_count: 0,
                definitions: Default::default(),
                display: Vec::new(),
                steering_notes: Vec::new(),
                lifecycle_hooks: Vec::new(),
                lifecycle_hook_sequence: None,
                provider_sequences: Default::default(),
                provider_fallbacks: Vec::new(),
                provider_retries: Vec::new(),
            });
        }
        Event::RunStarted {
            run_id,
            config,
            active_deadline_ms,
        } => {
            let workspace_identity = WorkspaceIdentity {
                device: config.workspace_device,
                inode: config.workspace_inode,
            };
            if current
                .workspace_identity
                .is_some_and(|identity| identity != workspace_identity)
            {
                return Err(ReplayError::InvalidTransition);
            }
            let goal = active_run_goal(current, *run_id).ok_or(ReplayError::InvalidTransition)?;
            let deadline_fits = match goal.effective_budget().max_active_ms {
                Some(limit) => active_deadline_ms
                    .is_some_and(|deadline| deadline <= limit.saturating_sub(goal.active_ms)),
                None => active_deadline_ms.is_none(),
            };
            if goal.status != GoalStatus::Running
                || goal.workspace_identity != workspace_identity
                || goal.active_deadline.is_some()
                || !deadline_fits
            {
                return Err(ReplayError::InvalidTransition);
            }
            let previous = current
                .runs
                .iter()
                .rev()
                .filter(|run| run.id != *run_id && goal.run_ids.contains(&run.id))
                .find_map(|run| run.config.as_ref())
                .and_then(|config| config.roster.as_ref());
            if previous.is_some_and(|roster| {
                config
                    .roster
                    .as_ref()
                    .map(|selected| selected.selection_digest)
                    != Some(roster.selection_digest)
            }) {
                return Err(ReplayError::InvalidTransition);
            }
            let goal_id = goal.id;
            let run = current_run(current, *run_id)?;
            if run.status != RunStatus::Pending || run.config.is_some() {
                return Err(ReplayError::InvalidTransition);
            }
            run.config = Some(config.clone());
            run.status = RunStatus::Active;
            current.workspace_identity = Some(workspace_identity);
            let goal = current_goal_mut(current, goal_id)?;
            goal.active_deadline = Some((*run_id, *active_deadline_ms));
            goal.active_ceiling_ms = 0;
        }
        Event::AssistantDisplayRecorded {
            run_id,
            agent_run_id,
            chunk,
        } => {
            use crate::session::events::{
                DisplayCategory, MAX_DISPLAY_CHUNKS, MAX_DISPLAY_COMMENTARY_ITEMS,
                MAX_DISPLAY_TEXT_BYTES,
            };
            let run = active_run(current, *run_id)?;
            let primary = run.primary().ok_or(ReplayError::InvalidTransition)?;
            let candidate = ProviderCallRecord {
                delegation: None,
                phase: chunk.phase,
                disposition: ProviderCallDisposition::Finished,
                requested_tools: 0,
                response_id: None,
                input_tokens: None,
                output_tokens: None,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                wire_provenance: None,
                refusal: None,
                failure_reason: None,
            };
            let valid_phase = primary.provider_calls.is_empty()
                && chunk.phase == AgentPhase::RootPlan
                || is_delegated_synthesis(run, primary, &candidate)
                || is_tool_continuation(run, primary, &candidate);
            if primary.id != *agent_run_id
                || primary.status != AgentStatus::Active
                || usize::try_from(chunk.call_index).ok() != Some(primary.provider_calls.len())
                || !valid_phase
                || run.display.len() >= MAX_DISPLAY_CHUNKS
            {
                return Err(ReplayError::InvalidTransition);
            }
            let offset: usize = run
                .display
                .iter()
                .filter(|prior| {
                    prior.call_index == chunk.call_index
                        && prior.category == chunk.category
                        && prior.item == chunk.item
                })
                .map(|prior| prior.text.len())
                .sum();
            let total: usize = run
                .display
                .iter()
                .filter(|prior| prior.category == chunk.category)
                .map(|prior| prior.text.len())
                .sum();
            let mut items = std::collections::HashSet::new();
            for prior in &run.display {
                if prior.category == DisplayCategory::Commentary {
                    items.insert((prior.call_index, prior.item));
                }
            }
            if chunk.category == DisplayCategory::Commentary {
                items.insert((chunk.call_index, chunk.item));
            }
            if chunk.category == DisplayCategory::Limit
                && run
                    .display
                    .iter()
                    .any(|prior| prior.category == DisplayCategory::Limit)
                || chunk.offset as usize != offset
                || total + chunk.text.len() > MAX_DISPLAY_TEXT_BYTES
                || items.len() > MAX_DISPLAY_COMMENTARY_ITEMS
            {
                return Err(ReplayError::InvalidTransition);
            }
            run.display.push(chunk.clone());
        }
        Event::MessageCommitted { run_id, text } => {
            let run = active_run(current, *run_id)?;
            if run.assistant_message.is_some()
                || run.primary().is_none_or(|primary| {
                    primary.status != AgentStatus::Finished
                        || primary.result.as_deref() != Some(text)
                })
            {
                return Err(ReplayError::InvalidTransition);
            }
            run.assistant_message = Some(text.clone());
        }
        Event::RunFinished {
            failure,
            run_id,
            disposition,
            active_ms,
        } => {
            let run = active_run(current, *run_id)?;
            if (*disposition == RunDisposition::Failed) != failure.is_some()
                || *failure == Some(crate::session::RunFailure::RefusalLimit)
                    && !run
                        .primary()
                        .is_some_and(|agent| agent.refusal_limit_reached())
                || run.agents.is_empty()
                || run
                    .agents
                    .iter()
                    .any(|agent| agent.status == AgentStatus::Active)
            {
                return Err(ReplayError::InvalidTransition);
            }
            run.status = match disposition {
                RunDisposition::Finished
                    if run.assistant_message.is_some()
                        && run.agents.iter().all(|agent| {
                            agent.role == crate::session::AgentRole::Primary
                                && agent.status == AgentStatus::Finished
                                || agent.role == crate::session::AgentRole::Child
                                    && super::worker::child_terminal(agent)
                        }) =>
                {
                    RunStatus::Finished
                }
                RunDisposition::StoppedByHook
                    if run.assistant_message.is_none()
                        && run.hooks_settled()
                        && run.hook_stop_reason().is_some()
                        && run
                            .primary()
                            .is_some_and(|agent| agent.status == AgentStatus::StoppedByHook) =>
                {
                    RunStatus::StoppedByHook
                }
                RunDisposition::Failed
                    if run.assistant_message.is_none() && run.hook_stop_reason().is_none() =>
                {
                    RunStatus::Failed
                }
                RunDisposition::Cancelled if run.assistant_message.is_none() => {
                    RunStatus::Cancelled
                }
                _ => return Err(ReplayError::InvalidTransition),
            };
            run.finished_sequence = Some(prior);
            run.failure = *failure;
            if let Some(goal) = current
                .goals
                .iter_mut()
                .find(|goal| goal.active_run == Some(*run_id))
            {
                if goal.active_deadline.map(|(deadline_run, _)| deadline_run) != Some(*run_id) {
                    return Err(ReplayError::InvalidTransition);
                }
                goal.active_ms = goal
                    .active_ms
                    .checked_add(*active_ms)
                    .ok_or(ReplayError::InvalidTransition)?;
                goal.active_deadline = None;
                goal.active_ceiling_ms = 0;
                goal.active_run = None;
                goal.awaiting_plan_contract = false;
            }
        }
        _ => return Err(ReplayError::InvalidTransition),
    }
    Ok(())
}

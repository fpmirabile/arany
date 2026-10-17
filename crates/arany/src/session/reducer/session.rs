use super::{CompactionView, SessionView};
use crate::session::events::{Event, GoalStatus, ReplayError};

pub(super) fn apply(
    current: &mut SessionView,
    event: &Event,
    prior: u64,
) -> Result<(), ReplayError> {
    match event {
        Event::SessionRenamed { title } => {
            current.title = title.clone();
            current.title_is_explicit = true;
        }
        Event::SessionDefaultChanged { defaults } => {
            current.defaults = defaults.clone();
        }
        Event::ContextCompacted { record, active_ms } => {
            let boundary = if let Some(run) = current.runs.last() {
                run.finished_sequence.map(|sequence| (run.id, sequence))
            } else {
                current
                    .lineage
                    .map(|lineage| (lineage.source_run_id, current.created_sequence))
            };
            if boundary != Some((record.covered_run_id, record.covered_sequence)) {
                return Err(ReplayError::InvalidSnapshot);
            }
            if current
                .runs
                .last()
                .is_some_and(|run| !run.compaction_matches(record))
            {
                return Err(ReplayError::InvalidTransition);
            }
            let goal = current.goals.iter_mut().find(|goal| {
                goal.status == GoalStatus::Running
                    && goal.run_ids.last() == Some(&record.covered_run_id)
            });
            if goal.is_some() != active_ms.is_some() {
                return Err(ReplayError::InvalidTransition);
            }
            if let (Some(goal), Some(active_ms)) = (goal, active_ms) {
                if goal.compaction_call_reserved != Some(record.covered_run_id) {
                    return Err(ReplayError::InvalidTransition);
                }
                goal.active_ms = goal
                    .active_ms
                    .checked_add(*active_ms)
                    .ok_or(ReplayError::InvalidTransition)?;
                goal.active_ceiling_ms = 0;
                let tokens = u64::from(record.input_tokens.unwrap_or_default())
                    .checked_add(u64::from(record.output_tokens.unwrap_or_default()))
                    .and_then(|tokens| goal.known_tokens.checked_add(tokens))
                    .ok_or(ReplayError::InvalidTransition)?;
                goal.known_cached_tokens = goal
                    .known_cached_tokens
                    .checked_add(u64::from(
                        record.cache_read_input_tokens.unwrap_or_default(),
                    ))
                    .ok_or(ReplayError::InvalidTransition)?;
                goal.known_tokens = tokens;
                goal.token_usage_unknown |=
                    record.input_tokens.is_none() || record.output_tokens.is_none();
                goal.compaction_call_reserved = None;
            }
            current.compactions.push(CompactionView {
                event_sequence: prior,
                record: record.clone(),
            });
        }
        _ => return Err(ReplayError::InvalidTransition),
    }
    Ok(())
}

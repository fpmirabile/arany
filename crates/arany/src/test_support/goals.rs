use crate::{AgentPhase, Event, Finish, ProviderOutcome, SessionView, StateRoot, Store};
use std::path::Path;

pub fn finish_with(text: &str) -> ProviderOutcome {
    ProviderOutcome::Finish(Finish {
        summary: text.into(),
        result: text.into(),
    })
}

pub fn facts_workspace(root: &Path, text: &str) -> std::path::PathBuf {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
    std::fs::write(workspace.join("notes/facts.txt"), text).expect("facts file");
    workspace
}

pub async fn replayed(
    state_path: &Path,
    session_id: crate::SessionId,
) -> (Vec<crate::EventEnvelope>, SessionView) {
    let store = Store::open_read_only(StateRoot::open_existing(state_path).expect("state"))
        .expect("read-only Store");
    let events = store.load_session(session_id).await.expect("Events");
    let view = SessionView::replay(session_id, &events)
        .expect("strict replay")
        .expect("Session");
    store.close().await.expect("Store shutdown");
    (events, view)
}

/// Splits a journal at each action's first admission and asserts that the action's bound covers the
/// Events up to the next admission, or up to the end of the journal (the terminal Events).
pub fn assert_action_bounds(events: &[crate::EventEnvelope], previews: bool, steering: bool) {
    use crate::Action;
    let primaries: std::collections::HashSet<_> = events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            Event::AgentSpawned {
                agent_run_id,
                role: crate::AgentRole::Primary,
                ..
            } => Some(*agent_run_id),
            _ => None,
        })
        .collect();
    let in_flight = |at: usize| -> u32 {
        let mut open: std::collections::HashMap<crate::AgentRunId, i32> = Default::default();
        for envelope in &events[..at] {
            match &envelope.event {
                Event::GoalProviderCallReserved { agent_run_id, .. }
                    if !primaries.contains(agent_run_id) =>
                {
                    *open.entry(*agent_run_id).or_default() += 1;
                }
                Event::ProviderCallRecorded { agent_run_id, .. } => {
                    if let Some(count) = open.get_mut(agent_run_id) {
                        *count -= 1;
                    }
                }
                _ => {}
            }
        }
        open.values().filter(|count| **count > 0).count() as u32
    };
    let primary_call = |index: usize| {
        matches!(
            &events[index].event,
            Event::GoalProviderCallReserved { agent_run_id, .. } if primaries.contains(agent_run_id)
        )
    };
    // (index, action, explicit end)
    let mut points: Vec<(usize, Action, Option<usize>)> = Vec::new();
    let mut markers: Vec<usize> = Vec::new();
    for (index, envelope) in events.iter().enumerate() {
        let previous = index.checked_sub(1).map(|previous| &events[previous].event);
        match &envelope.event {
            Event::GoalStarted { .. }
            | Event::GoalResumed { .. }
            | Event::GoalRunLinked { .. }
            | Event::SessionStarted { .. } => markers.push(index),
            Event::GoalProviderCallReserved { agent_run_id, .. }
                if primaries.contains(agent_run_id) =>
            {
                let phase = events[index..].iter().find_map(|later| match &later.event {
                    Event::ProviderCallRecorded {
                        agent_run_id: recorded,
                        record,
                        ..
                    } if recorded == agent_run_id => Some(record.phase),
                    _ => None,
                });
                let action = if phase == Some(AgentPhase::ToolReview) {
                    Action::Review
                } else {
                    Action::ProviderCall { previews, steering }
                };
                points.push((index, action, None));
            }
            Event::GoalProviderCallReserved { .. } => points.push((
                index,
                Action::Child {
                    in_flight: in_flight(index),
                },
                None,
            )),
            Event::AgentSpawned { grant: Some(_), .. }
                if !matches!(previous, Some(Event::AgentSpawned { .. })) =>
            {
                let spawned: Vec<_> = events[index..]
                    .iter()
                    .map_while(|later| match &later.event {
                        Event::AgentSpawned { grant, .. } => Some(*grant),
                        _ => None,
                    })
                    .collect();
                // The delegation covers its children through the synthesis call or the Run's end.
                let end = (index + 1..events.len())
                    .find(|later| {
                        primary_call(*later)
                            || matches!(
                                events[*later].event,
                                Event::GoalRunLinked { .. } | Event::GoalResumed { .. }
                            )
                    })
                    .unwrap_or(events.len());
                points.push((
                    index,
                    Action::Delegation {
                        children: spawned.len(),
                        research: spawned.contains(&Some(crate::ChildGrant::ReadOnly)),
                    },
                    Some(end),
                ));
            }
            Event::ProviderFallbackUsed { fallback, .. } => {
                let action = match fallback.role {
                    crate::ProviderRole::Primary => Action::ProviderCall { previews, steering },
                    crate::ProviderRole::Child => Action::Child {
                        in_flight: in_flight(index),
                    },
                    crate::ProviderRole::Review => Action::Review,
                    crate::ProviderRole::Compaction => Action::Compaction,
                };
                points.push((index, action, None));
            }
            Event::GoalCompactionCallReserved { .. } => {
                let between_runs = events[index..]
                    .iter()
                    .find_map(|later| match &later.event {
                        Event::ContextCompacted { .. } => Some(true),
                        Event::GoalContextCompacted { .. } => Some(false),
                        _ => None,
                    })
                    .unwrap_or(false);
                let action = if between_runs {
                    Action::BoundaryCompaction { goal: true }
                } else {
                    Action::Compaction
                };
                points.push((index, action, None));
            }
            Event::ContextCompacted { .. }
                if !matches!(previous, Some(Event::GoalCompactionCallReserved { .. })) =>
            {
                points.push((index, Action::BoundaryCompaction { goal: false }, None));
            }
            Event::GoalPlanContractRecorded { .. } => points.push((index, Action::Tool, None)),
            Event::ToolStarted { agent_run_id, .. } => {
                let action = if primaries.contains(agent_run_id) {
                    Action::Tool
                } else {
                    Action::Child {
                        in_flight: in_flight(index),
                    }
                };
                points.push((index, action, None));
            }
            _ => {}
        }
    }
    assert!(!points.is_empty(), "the journal admits at least one action");
    let boundaries: Vec<usize> = points
        .iter()
        .map(|(index, ..)| *index)
        .chain(markers)
        .collect();
    for (index, action, end) in &points {
        let end = end.unwrap_or_else(|| {
            boundaries
                .iter()
                .copied()
                .filter(|boundary| boundary > index)
                .min()
                .unwrap_or(events.len())
        });
        let observed = end - index;
        let providers = events[..=*index]
            .iter()
            .rev()
            .find_map(|event| match &event.event {
                Event::RunStarted { config, .. } => Some(config),
                _ => None,
            })
            .and_then(|config| config.providers.as_ref());
        let bound = providers.map_or_else(
            || action.max_events(),
            |providers| action.max_events_for(providers),
        );
        assert!(
            bound >= observed,
            "{action:?} at {index} bounds {bound} Events but appends {observed}",
        );
    }
}

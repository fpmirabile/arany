use crate::{AgentPhase, Answer, Event, ProviderOutcome, SessionView, StateRoot, Store};
use std::path::Path;

pub fn finish_with(text: &str) -> ProviderOutcome {
    ProviderOutcome::Answer(Answer { text: text.into() })
}

pub fn long_turn_provider() -> super::scenario::ScriptedProvider {
    super::scenario::ScriptedProvider::new(
        (0..64)
            .map(|_| {
                super::scenario::tool_outcome(crate::ToolCall::Read {
                    path: "notes/facts.txt".into(),
                    offset: None,
                    limit: None,
                })
            })
            .chain([finish_with("Long turn completed")])
            .collect(),
    )
    .with_profile("openai", "gpt-5.4")
    .with_known_input_tokens()
    .with_active_time_per_call(std::time::Duration::from_secs(29))
}

pub fn assert_long_turn(events: &[crate::EventEnvelope], view: &SessionView) {
    let goal = view.goals.last().expect("Goal");
    assert_eq!(goal.status, crate::GoalStatus::Completed);
    assert_eq!(goal.budget.max_provider_calls, None);
    assert_eq!(goal.budget.max_tokens, None);
    assert_eq!(goal.budget.max_active_ms, None);
    assert_eq!(goal.budget.max_runs, 16);
    assert_eq!(goal.provider_calls, 65);
    assert!(goal.active_ms >= 65 * 29_000);
    assert_eq!(goal.uncertain_active_ms, 0);
    assert!(!goal.token_usage_unknown);
    assert_eq!(goal.known_tokens, 65 * 8);
    assert_eq!(view.runs.last().expect("Run").tools.len(), 64);
    assert!(matches!(
        events.last().expect("receipt").event,
        Event::GoalFinished {
            exhausted: false,
            ..
        }
    ));
    assert_action_bounds(events, false, false);
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
    let mut call_phases = vec![None; events.len()];
    let mut next_phase = std::collections::HashMap::new();
    for (index, envelope) in events.iter().enumerate().rev() {
        match &envelope.event {
            Event::ProviderCallRecorded {
                agent_run_id,
                record,
                ..
            } => {
                next_phase.insert(*agent_run_id, record.phase);
            }
            Event::GoalProviderCallReserved { agent_run_id, .. } => {
                call_phases[index] = next_phase.get(agent_run_id).copied();
            }
            _ => {}
        }
    }
    let in_flight = |at: usize| -> u32 {
        let mut open: std::collections::HashMap<crate::AgentRunId, i32> = Default::default();
        for (index, envelope) in events[..at].iter().enumerate() {
            match &envelope.event {
                Event::GoalProviderCallReserved { agent_run_id, .. }
                    if !primaries.contains(agent_run_id)
                        && call_phases[index] != Some(AgentPhase::ToolReview) =>
                {
                    *open.entry(*agent_run_id).or_default() += 1;
                }
                Event::ProviderCallRecorded {
                    agent_run_id,
                    record,
                    ..
                } if record.phase != AgentPhase::ToolReview => {
                    if let Some(count) = open.get_mut(agent_run_id) {
                        *count -= 1;
                    }
                }
                _ => {}
            }
        }
        open.values().filter(|count| **count > 0).count() as u32
    };
    let researchers: std::collections::HashSet<_> = events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            Event::AgentSpawned {
                agent_run_id,
                grant: Some(crate::ChildGrant::ReadOnly { .. }),
                ..
            } => Some(*agent_run_id),
            _ => None,
        })
        .collect();
    let workers: std::collections::HashMap<_, _> = events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            Event::AgentSpawned {
                agent_run_id,
                assignment: Some(_),
                ..
            } => Some((*agent_run_id, 0)),
            _ => None,
        })
        .collect();
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
            Event::GoalProviderCallReserved { agent_run_id, .. } => {
                let action = if call_phases[index] == Some(AgentPhase::ToolReview) {
                    Action::Review
                } else if primaries.contains(agent_run_id) {
                    Action::ProviderCall { previews, steering }
                } else if workers.contains_key(agent_run_id) {
                    Action::Worker
                } else {
                    Action::Child {
                        in_flight: in_flight(index),
                        research: researchers.contains(agent_run_id),
                    }
                };
                points.push((index, action, None));
            }
            Event::AgentSpawned {
                assignment: Some(_),
                ..
            } => points.push((index, Action::Worker, None)),
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
                        research: spawned
                            .iter()
                            .any(|grant| matches!(grant, Some(crate::ChildGrant::ReadOnly { .. }))),
                    },
                    Some(end),
                ));
            }
            // A switch or retry re-admits its attempt's action headroom before it commits.
            Event::ProviderFallbackUsed {
                agent_run_id,
                fallback: crate::ProviderFallback { role, .. },
                ..
            }
            | Event::ProviderRetryScheduled {
                agent_run_id,
                retry: crate::ProviderRetry { role, .. },
                ..
            } => {
                let action = match role {
                    crate::ProviderRole::Primary => Action::ProviderCall { previews, steering },
                    crate::ProviderRole::Child => Action::Child {
                        in_flight: in_flight(index),
                        research: researchers.contains(agent_run_id),
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
            Event::WorkerStopObserved { .. } => {
                points.push((index, Action::Worker, None));
            }
            Event::WorkerVerificationRecorded { .. }
                if !matches!(previous, Some(Event::WorkerVerificationRecorded { .. })) =>
            {
                points.push((index, Action::Worker, None));
            }
            Event::ToolStarted { agent_run_id, .. } => {
                let action = if primaries.contains(agent_run_id) {
                    Action::Tool
                } else {
                    Action::Child {
                        in_flight: in_flight(index),
                        research: researchers.contains(agent_run_id)
                            || workers.contains_key(agent_run_id),
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
        // Admission reserves two Events per matched hook on top of the Tool's own.
        let hook_events = events[*index..end]
            .iter()
            .filter(|event| {
                matches!(
                    event.event,
                    Event::HookStarted { .. } | Event::HookFinished { .. }
                )
            })
            .count();
        let observed = end - index - hook_events;
        let config = events[..=*index]
            .iter()
            .rev()
            .find_map(|event| match &event.event {
                Event::RunStarted { config, .. } => Some(config),
                _ => None,
            });
        let providers = config.map(|config| {
            let mut routes = config
                .providers
                .clone()
                .unwrap_or_else(|| crate::Providers::all(vec![config.identity().unwrap()]));
            if let Some(roster) = &config.roster
                && let Some(chain) = roster
                    .child_profiles
                    .iter()
                    .filter_map(|name| roster.chain(name))
                    .max_by_key(|chain| chain.len())
            {
                routes.child = chain.to_vec();
            }
            routes
        });
        let verdicts = config
            .filter(|config| config.supervisor.is_some())
            .map_or(0, |_| action.verdict_slots());
        let bound = verdicts
            + providers.map_or_else(
                || action.max_events(),
                |providers| action.max_events_for(&providers),
            );
        // A primary step also reserves the Events that draining each in-flight editor appends.
        let bound = bound
            + if matches!(action, Action::Tool | Action::ProviderCall { .. }) {
                let held = events[..*index]
                    .iter()
                    .map(|event| match event.event {
                        Event::AgentSpawned {
                            assignment: Some(_),
                            ..
                        } => 1_i64,
                        Event::WorkerLeaseReleased { .. } => -1,
                        _ => 0,
                    })
                    .sum::<i64>();
                // Each editor still holding its lease drains with four Events and one shadow verdict.
                usize::try_from(held).unwrap_or(0) * (4 + usize::from(verdicts > 0))
            } else {
                0
            };
        assert!(
            bound >= observed,
            "{action:?} at {index} bounds {bound} Events but appends {observed}",
        );
    }
    let admissions: Vec<_> = super::admissions::recorded(events[0].session_id)
        .into_iter()
        .map(|(count, slots, _, _)| (count, slots))
        .collect();
    assert!(!admissions.is_empty(), "the Store recorded no admissions");
    let first = admissions.iter().map(|(count, _)| *count).min();
    for (position, envelope) in events.iter().enumerate() {
        let admitted_elsewhere = matches!(
            envelope.event,
            Event::GoalStarted { .. }
                | Event::GoalResumed { .. }
                | Event::GoalRunLinked { .. }
                | Event::SessionStarted { .. }
                | Event::MessageAccepted { .. }
                | Event::RunStarted { .. }
                | Event::AgentSpawned {
                    role: crate::AgentRole::Primary,
                    ..
                }
        );
        if admitted_elsewhere || first.is_none_or(|first| position < first) {
            continue;
        }
        assert!(
            admissions
                .iter()
                .any(|(count, slots)| *count <= position && position < count + slots),
            "{:?} at {position} is past every earlier Store admission",
            envelope.event.kind(),
        );
    }
}

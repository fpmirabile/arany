use arany::{Event, EventEnvelope, SessionView};
use arany_cli::presentation::{Output, render_session};

fn assert_history(stdout: &[u8], events: &[EventEnvelope]) {
    let view = SessionView::replay(events[0].session_id, events)
        .expect("strict replay")
        .expect("closed Session");
    assert_eq!(
        stdout,
        render_session(&view, events, Output::Jsonl).as_bytes(),
        "exact stdout matches closed history"
    );
}

fn align_goal_id(event: &mut Event, reference: &Event) {
    match (event, reference) {
        (
            Event::GoalStarted { goal_id, .. }
            | Event::GoalRunLinked { goal_id, .. }
            | Event::GoalProviderCallReserved { goal_id, .. }
            | Event::GoalFinished { goal_id, .. },
            Event::GoalStarted { goal_id: goal, .. }
            | Event::GoalRunLinked { goal_id: goal, .. }
            | Event::GoalProviderCallReserved { goal_id: goal, .. }
            | Event::GoalFinished { goal_id: goal, .. },
        ) => *goal_id = *goal,
        _ => {}
    }
}

fn align_ids(event: &mut EventEnvelope, reference: &EventEnvelope) {
    event.session_id = reference.session_id;
    event.run_id = reference.run_id;
    event.agent_run_id = reference.agent_run_id;
    align_goal_id(&mut event.event, &reference.event);
    match &mut event.event {
        Event::GoalRunLinked { run_id, .. }
        | Event::GoalProviderCallReserved { run_id, .. }
        | Event::MessageAccepted { run_id, .. }
        | Event::RunStarted { run_id, .. }
        | Event::MessageCommitted { run_id, .. }
        | Event::RunFinished { run_id, .. }
        | Event::AgentSpawned { run_id, .. }
        | Event::ProviderCallRecorded { run_id, .. }
        | Event::AgentFinished { run_id, .. } => {
            *run_id = reference.run_id.expect("Run-scoped Event");
        }
        _ => {}
    }
    match &mut event.event {
        Event::GoalProviderCallReserved { agent_run_id, .. }
        | Event::AgentSpawned { agent_run_id, .. }
        | Event::ProviderCallRecorded { agent_run_id, .. }
        | Event::AgentFinished { agent_run_id, .. } => {
            *agent_run_id = reference.agent_run_id.expect("agent-scoped Event");
        }
        _ => {}
    }
}

fn align_clocks(event: &mut Event, reference: &Event) {
    match (event, reference) {
        (
            Event::GoalProviderCallReserved { active_ceiling_ms, .. },
            Event::GoalProviderCallReserved {
                active_ceiling_ms: ceiling,
                ..
            },
        ) => *active_ceiling_ms = *ceiling,
        (
            Event::RunFinished { active_ms, .. } | Event::GoalFinished { active_ms, .. },
            Event::RunFinished { active_ms: elapsed, .. }
            | Event::GoalFinished { active_ms: elapsed, .. },
        ) => *active_ms = *elapsed,
        _ => {}
    }
}

pub(super) fn assert_equivalent(
    stdout: &[u8],
    events: &[EventEnvelope],
    failed_stdout: &[u8],
    failed_events: &[EventEnvelope],
) {
    assert_history(stdout, events);
    assert_history(failed_stdout, failed_events);
    assert_eq!(events.len(), failed_events.len(), "same committed transitions");
    let mut normalized = failed_events.to_vec();
    for (event, reference) in normalized.iter_mut().zip(events) {
        assert_eq!(event.event.kind(), reference.event.kind(), "same Event kind");
        event.created_at_ms = reference.created_at_ms;
        align_ids(event, reference);
        align_clocks(&mut event.event, &reference.event);
    }
    assert_eq!(normalized, events, "only UUIDs and measured clocks differ");
    let id = events[0].session_id;
    assert_eq!(
        SessionView::replay(id, &normalized).expect("failed Collector normalized replay"),
        SessionView::replay(id, events).expect("healthy Collector replay"),
        "same reduced view"
    );
}

use super::*;
use crate::worker::{WorkerCause, WorkerDisposition, WorkerStopReason};

pub(super) const GATE_CAPACITY: usize = 1;
const ACTIVE_BUDGET: Duration = Duration::from_secs(120);
const DEADLINE_MARGIN: Duration = Duration::from_secs(1);

pub(super) fn is_timed(case: &str) -> bool {
    matches!(case, "active_time" | "delegation_deadline")
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).expect("duration fits u64 milliseconds")
}

pub(super) fn budget(case: &str) -> GoalBudget {
    GoalBudget {
        max_active_ms: (case == "active_time").then_some(millis(ACTIVE_BUDGET)),
        ..Default::default()
    }
}

pub(super) async fn exhaust(case: &str) {
    let allowance = match case {
        "active_time" => ACTIVE_BUDGET,
        "delegation_deadline" => crate::engine::MAX_DELEGATION_DEADLINE,
        _ => panic!("unexpected clock gate"),
    };
    tokio::time::pause();
    tokio::time::advance(allowance.saturating_add(DEADLINE_MARGIN)).await;
    tokio::time::resume();
}

pub(super) async fn capacity_case(guard: &Path) -> TeamObservation {
    let baseline = run_case_inner(guard, "success", None).await;
    let finished = usize::try_from(
        baseline.view.runs[0].agents[1].tools[0]
            .finished_sequence
            .unwrap(),
    )
    .expect("journal sequence fits usize");
    let admissions = super::super::admissions::recorded(baseline.session);
    let (count, slots, _, _) = *admissions
        .iter()
        .find(|(count, slots, _, _)| *count >= finished && *slots >= Action::Worker.max_events())
        .unwrap();
    let padding = crate::store::MAX_SESSION_EVENTS - count - slots + 1;
    assert!(admissions.iter().filter(|(prior, _, _, _)| *prior < count)
        .all(|(prior, slots, _, _)| padding + prior + slots <= crate::store::MAX_SESSION_EVENTS));
    run_case_inner(guard, "capacity", Some(padding + 1)).await
}

pub(super) async fn pad_session(
    engine: Engine,
    state: &Path,
    guard: &Path,
    session: SessionId,
    through: usize,
) -> Engine {
    engine.close().await.unwrap();
    let mut connection = rusqlite::Connection::open(state.join("events.sqlite3")).unwrap();
    let transaction = connection.transaction().unwrap();
    let mut insert = transaction.prepare("INSERT INTO events (sequence,session_id,kind,event_version,payload,created_at_ms) VALUES (?1,?2,'SessionRenamed',?3,'{\"title\":\"padding\"}',0)").unwrap();
    for sequence in 2..=through {
        insert
            .execute(rusqlite::params![
                i64::try_from(sequence).unwrap(),
                session.to_string(),
                crate::session::payload_version("SessionRenamed").unwrap()
            ])
            .unwrap();
    }
    drop(insert);
    transaction.commit().unwrap();
    drop(connection);
    super::super::engine::open(
        StateRoot::open_existing(state).unwrap(),
        EngineConfig {
            guard_executable: Some(guard.into()),
            ..Default::default()
        },
    )
    .unwrap()
}

pub(super) fn assert_stopped(
    case: &str,
    events: &[EventEnvelope],
    view: &SessionView,
    provider: &LiveProvider,
) {
    if !is_timed(case) && case != "capacity" {
        return;
    }
    let run = &view.runs[0];
    let child = &run.agents[1];
    let worker = child.assignment.as_ref().unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    assert!(
        run.agents
            .iter()
            .all(|agent| agent.status != AgentStatus::Active)
    );
    let outcome = &worker.outcome.as_ref().unwrap().1;
    assert_eq!(outcome.disposition, WorkerDisposition::Failed);
    let release = worker.released.unwrap();
    assert!(release > worker.outcome.as_ref().unwrap().0);
    assert_eq!(child.tools.len(), 1);
    let tool = &child.tools[0];
    assert_eq!(
        tool.observation.as_ref().unwrap().disposition,
        ToolDisposition::Succeeded
    );
    assert!(tool.checkpoint.is_some());
    let calls = provider.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|(phase, _)| *phase == AgentPhase::ChildWork)
            .count(),
        if case == "capacity" { 1 } else { 2 }
    );
    assert!(
        calls
            .iter()
            .all(|(phase, _)| *phase != AgentPhase::RootSynthesis)
    );
    if case == "active_time" {
        assert_eq!(outcome.cause, WorkerCause::Budget);
        assert!(events.iter().any(|event| matches!(
            &event.event,
            Event::WorkerStopObserved { record, .. }
                if record.reason == WorkerStopReason::Budget
                    && event.sequence < worker.outcome.as_ref().unwrap().0
        )));
        assert_active_time(view);
    } else {
        assert_eq!(run.failure, Some(RunFailure::ToolStopped));
        assert!(!view.goals[0].budget_closed);
        assert!(view.goals[0].budget_refusal.is_none());
        if case == "delegation_deadline" {
            assert!(view.goals[0].active_ms >= millis(crate::engine::MAX_DELEGATION_DEADLINE));
            assert!(events.iter().any(|event| matches!(
                &event.event,
                Event::GoalFinished {
                    exhausted: false,
                    budget_refusal: None,
                    ..
                }
            )));
        } else {
            assert_eq!(outcome.cause, WorkerCause::Budget);
        }
    }
    assert_no_dispatch_after_stop(case, events, child.id);
}

fn assert_active_time(view: &SessionView) {
    let goal = &view.goals[0];
    assert_eq!(
        view.run_stop(&view.runs[0]),
        Some(RunStop::GoalExhausted(GoalLimit::ActiveTime))
    );
    assert_eq!(goal.status, GoalStatus::Failed);
    assert!(goal.report.is_some());
    assert!(goal.budget_closed);
    assert!(goal.active_ms >= millis(ACTIVE_BUDGET));
    assert_eq!(
        goal.budget_refusal.as_ref().unwrap().dimension,
        GoalLimit::ActiveTime
    );
}

fn assert_no_dispatch_after_stop(case: &str, events: &[EventEnvelope], child: AgentRunId) {
    let stops = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(&event.event, Event::WorkerStopObserved { record, .. }
            if record.reason == WorkerStopReason::Budget)
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let stopped = if case == "delegation_deadline" {
        assert!(stops.is_empty());
        events.iter().rposition(|event| matches!(&event.event,
            Event::ProviderCallRecorded { agent_run_id, record, .. }
                if *agent_run_id == child && record.phase == AgentPhase::ChildWork
                    && matches!(record.disposition, ProviderCallDisposition::TimedOut | ProviderCallDisposition::Cancelled)
        )).unwrap()
    } else {
        assert_eq!(stops.len(), 1);
        stops[0]
    };
    assert!(events[stopped + 1..].iter().all(|event| !matches!(
        event.event,
        Event::GoalProviderCallReserved { .. }
            | Event::ToolProposed { .. }
            | Event::ToolStarted { .. }
            | Event::AgentSpawned { .. }
    )));
}

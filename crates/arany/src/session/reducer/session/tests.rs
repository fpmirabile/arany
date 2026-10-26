use super::super::test_support::*;
use super::super::*;
use crate::session::events::CollaborationPolicy;

#[test]
fn a_forks_boundary_compaction_is_refused_while_its_own_run_is_open() {
    let (fork, source, source_run, run, agent) = (
        SessionId::new(),
        SessionId::new(),
        RunId::new(),
        RunId::new(),
        AgentRunId::new(),
    );
    let mut events = vec![Event::SessionForked {
        title: "Fork".into(),
        source_session_id: source,
        source_run_id: source_run,
        source_sequence: 1,
        prefix_digest: [0; 32],
    }];
    let goal = GoalId::new();
    events.push(goal_started(goal, GoalBudget::default()));
    events.extend(start_run(
        goal,
        run,
        agent,
        config(CollaborationPolicy::Single, false),
    ));
    let open = journal(fork, events.clone());
    assert!(SessionView::replay_open(fork, &open).unwrap().is_some());
    let compacted = Event::ContextCompacted {
        record: failed_compaction_record(source_run, 1),
        active_ms: None,
    };
    let boundary_only = journal(fork, vec![events[0].clone(), compacted.clone()]);
    assert!(
        SessionView::replay_open(fork, &boundary_only)
            .unwrap()
            .is_some()
    );
    events.push(compacted);
    assert!(matches!(
        SessionView::replay_open(fork, &journal(fork, events)),
        Err(ReplayError::InvalidSnapshot)
    ));
}

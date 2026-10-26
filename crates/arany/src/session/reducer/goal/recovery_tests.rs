use super::super::test_support::*;
use super::super::*;
use crate::provider::AgentPhase;
use crate::session::events::{CollaborationPolicy, GoalSuccessCriterion};
use crate::tools::ToolDisposition;

#[test]
fn pending_plan_recovery_accepts_closed_inspection_and_refuses_open_or_uncertain_tools() {
    let closed = pending_plan_view(Some(ToolDisposition::Succeeded));
    assert!(closed.goals[0].can_resume(&closed));

    let mut started_only = closed.clone();
    started_only.runs.clear();
    started_only.goals[0].run_ids.clear();
    started_only.goals[0].awaiting_plan_contract = false;
    assert!(started_only.goals[0].can_resume(&started_only));

    let open = pending_plan_view(None);
    assert!(!open.goals[0].can_resume(&open));

    let uncertain = pending_plan_view(Some(ToolDisposition::Uncertain));
    assert!(!uncertain.goals[0].can_resume(&uncertain));

    // Resume keeps refusing a Tool that stopped the Run, such as a cancelled one.
    let cancelled = pending_plan_view(Some(ToolDisposition::Cancelled));
    assert!(!cancelled.goals[0].can_resume(&cancelled));
}

#[test]
fn crash_after_steering_note_replays_interrupted_and_resume_starts_without_notes() {
    let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let session = SessionId::new();
    let mut events = vec![goal_started(goal_id, GoalBudget::default())];
    events.extend(start_run(
        goal_id,
        run_id,
        agent_run_id,
        config(CollaborationPolicy::Single, false),
    ));
    events.push(Event::GoalProviderCallReserved {
        goal_id,
        run_id,
        agent_run_id,
        active_ceiling_ms: 0,
    });
    events.push(Event::GoalSteeringNoteRecorded {
        goal_id,
        run_id,
        text: "steer".into(),
    });
    // The process dies before the reserved call's outcome commits.
    let crashed = SessionView::replay(session, &session_with(session, events.clone()))
        .expect("crashed journal replays")
        .expect("Session exists");
    assert_eq!(crashed.runs[0].status, RunStatus::Interrupted);
    assert_eq!(crashed.goals[0].status, GoalStatus::Interrupted);
    assert_eq!(crashed.runs[0].steering_notes, vec!["steer"]);

    let resumed_run = RunId::new();
    let resumed_agent = AgentRunId::new();
    events.push(Event::GoalResumed { goal_id });
    events.extend(start_run(
        goal_id,
        resumed_run,
        resumed_agent,
        config(CollaborationPolicy::Single, false),
    ));
    let resumed = SessionView::replay_open(session, &session_with(session, events))
        .expect("resume replays")
        .expect("Session exists");
    assert_eq!(resumed.runs.len(), 2);
    assert_eq!(resumed.runs[0].steering_notes, vec!["steer"]);
    assert!(resumed.runs[1].steering_notes.is_empty());
}

#[test]
fn a_new_goal_is_accepted_after_a_paused_goal_and_owns_its_run() {
    let (first, second) = (GoalId::new(), GoalId::new());
    let (first_run, first_agent) = (RunId::new(), AgentRunId::new());
    let (second_run, second_agent) = (RunId::new(), AgentRunId::new());
    let mut events = vec![goal_started(first, GoalBudget::default())];
    events.extend(start_run(
        first,
        first_run,
        first_agent,
        config(CollaborationPolicy::Single, false),
    ));
    events.extend(cancel_run(first_run, first_agent));
    events.push(goal_started(second, GoalBudget::default()));
    events.extend(start_run(
        second,
        second_run,
        second_agent,
        config(CollaborationPolicy::Single, false),
    ));
    events.extend(complete_run_and_goal(second, second_run, second_agent));
    let session = SessionId::new();
    let journal = session_with(session, events);

    let open = SessionView::replay_open(session, &journal)
        .expect("a new Goal pauses its paused predecessor")
        .expect("Session exists");
    assert_eq!(open.goals[0].status, GoalStatus::Interrupted);
    assert_eq!(open.goals[1].status, GoalStatus::Completed);
    assert_eq!(open.goals[1].run_ids, vec![second_run]);
    assert_eq!(open.goals[0].run_ids, vec![first_run]);
    let closed = SessionView::replay(session, &journal)
        .expect("closed replay agrees")
        .expect("Session exists");
    assert_eq!(closed.interrupted_goal_id(), Some(first));
}

#[test]
fn a_crashed_predecessor_is_sealed_by_a_new_goal() {
    let (first, second) = (GoalId::new(), GoalId::new());
    let (first_run, first_agent) = (RunId::new(), AgentRunId::new());
    let (second_run, second_agent) = (RunId::new(), AgentRunId::new());
    let mut events = vec![goal_started(first, GoalBudget::default())];
    events.extend(start_run(
        first,
        first_run,
        first_agent,
        config(CollaborationPolicy::Single, false),
    ));
    // No terminal Run or Goal event: the process died mid-Run.
    events.push(goal_started(second, GoalBudget::default()));
    events.extend(start_run(
        second,
        second_run,
        second_agent,
        config(CollaborationPolicy::Single, false),
    ));
    let session = SessionId::new();
    let journal = session_with(session, events);

    let open = SessionView::replay_open(session, &journal)
        .expect("a crashed predecessor does not block a new Goal")
        .expect("Session exists");
    assert_eq!(open.goals[0].status, GoalStatus::Interrupted);
    assert_eq!(open.runs[0].status, RunStatus::Interrupted);
    assert_eq!(open.goals[1].status, GoalStatus::Running);
}

#[test]
fn resume_continues_the_most_recent_paused_goal_after_a_newer_goal_finished() {
    let (first, second) = (GoalId::new(), GoalId::new());
    let (first_run, first_agent) = (RunId::new(), AgentRunId::new());
    let (second_run, second_agent) = (RunId::new(), AgentRunId::new());
    let (resumed_run, resumed_agent) = (RunId::new(), AgentRunId::new());
    let mut events = vec![goal_started(first, GoalBudget::default())];
    events.extend(start_run(
        first,
        first_run,
        first_agent,
        config(CollaborationPolicy::Single, false),
    ));
    events.extend(cancel_run(first_run, first_agent));
    events.push(goal_started(second, GoalBudget::default()));
    events.extend(start_run(
        second,
        second_run,
        second_agent,
        config(CollaborationPolicy::Single, false),
    ));
    events.extend(complete_run_and_goal(second, second_run, second_agent));
    events.push(Event::GoalResumed { goal_id: first });
    events.extend(start_run(
        first,
        resumed_run,
        resumed_agent,
        config(CollaborationPolicy::Single, false),
    ));
    events.extend(complete_run_and_goal(first, resumed_run, resumed_agent));
    let session = SessionId::new();
    let journal = session_with(session, events);

    let view = SessionView::replay(session, &journal)
        .expect("resumed Goal replays")
        .expect("Session exists");
    assert_eq!(view.goals[0].status, GoalStatus::Completed);
    assert_eq!(view.goals[0].run_ids, vec![first_run, resumed_run]);
    assert_eq!(view.goals[1].status, GoalStatus::Completed);
    assert_eq!(view.interrupted_goal_id(), None);
}

#[test]
fn an_older_paused_goal_is_not_resumable_once_a_newer_goal_is_paused() {
    let (first, second) = (GoalId::new(), GoalId::new());
    let (first_run, first_agent) = (RunId::new(), AgentRunId::new());
    let (second_run, second_agent) = (RunId::new(), AgentRunId::new());
    let mut paused = vec![goal_started(first, GoalBudget::default())];
    paused.extend(start_run(
        first,
        first_run,
        first_agent,
        config(CollaborationPolicy::Single, false),
    ));
    paused.extend(cancel_run(first_run, first_agent));
    paused.push(goal_started(second, GoalBudget::default()));
    paused.extend(start_run(
        second,
        second_run,
        second_agent,
        config(CollaborationPolicy::Single, false),
    ));
    paused.extend(cancel_run(second_run, second_agent));
    let session = SessionId::new();
    let closed_session = session_with(session, paused.clone());
    let view = SessionView::replay(session, &closed_session)
        .expect("both paused Goals replay")
        .expect("Session exists");
    assert_eq!(view.interrupted_goal_id(), Some(second));

    let mut older = paused.clone();
    older.push(Event::GoalResumed { goal_id: first });
    assert!(
        SessionView::replay_open(session, &session_with(session, older)).is_err(),
        "the older paused Goal is history, not resumable"
    );
    let mut newer = paused;
    newer.push(Event::GoalResumed { goal_id: second });
    assert!(SessionView::replay_open(session, &session_with(session, newer)).is_ok());
}

#[test]
fn resumed_crash_left_compaction_call_settles_as_uncertain() {
    let session_id = SessionId::new();
    let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let mut events = vec![goal_started(goal_id, GoalBudget::default())];
    events.extend(start_run(
        goal_id,
        run_id,
        agent_run_id,
        config(CollaborationPolicy::Single, false),
    ));
    events.extend(cancel_run(run_id, agent_run_id));
    // The process died after the compaction call was reserved, before its outcome was journaled.
    events.push(Event::GoalCompactionCallReserved {
        goal_id,
        run_id,
        active_ceiling_ms: 0,
    });
    events.push(Event::GoalResumed { goal_id });

    let view = SessionView::replay_open(session_id, &session_with(session_id, events))
        .expect("resumed Goal replays")
        .expect("Session exists");
    let goal = view.goal(goal_id).expect("Goal");
    assert_eq!(goal.status, GoalStatus::Running);
    assert_eq!(goal.compaction_call_reserved, None);
    assert_eq!(goal.uncertain_compaction_calls, 1);
    assert!(view.usage.pending.contains(&(session_id, run_id, None)));
    assert!(goal.token_usage_unknown);
}

#[test]
fn resumed_orphaned_provider_reservation_settles_as_uncertain() {
    let session_id = SessionId::new();
    let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let mut events = vec![
        Event::SessionStarted {
            title: "Goal".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
        },
        goal_started(goal_id, GoalBudget::default()),
    ];
    events.extend(start_run(
        goal_id,
        run_id,
        agent_run_id,
        config(CollaborationPolicy::Single, false),
    ));
    events.push(Event::GoalProviderCallReserved {
        goal_id,
        run_id,
        agent_run_id,
        active_ceiling_ms: 0,
    });
    events.push(Event::GoalResumed { goal_id });

    let view = SessionView::replay_open(session_id, &journal(session_id, events))
        .expect("resumed Goal replays")
        .expect("Session exists");
    let goal = view.goals.last().expect("Goal");
    assert!(goal.unobserved_provider_calls.is_empty());
    assert!(goal.token_usage_unknown);
}

#[test]
fn crash_between_tool_start_and_finish_replays_interrupted_with_the_tool_ceiling_charged() {
    let session_id = SessionId::new();
    let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let mut events = vec![
        Event::SessionStarted {
            title: "Goal".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
        },
        goal_started(goal_id, GoalBudget::default()),
    ];
    events.extend(start_run(
        goal_id,
        run_id,
        agent_run_id,
        config(CollaborationPolicy::Single, true),
    ));
    let intent = read_intent(run_id, agent_run_id, "src/lib.rs");
    events.extend([
        Event::GoalProviderCallReserved {
            goal_id,
            run_id,
            agent_run_id,
            active_ceiling_ms: 605_000,
        },
        Event::ProviderCallRecorded {
            run_id,
            agent_run_id,
            record: record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::ToolRequested,
                Some(0),
            ),
        },
        Event::ToolStarted {
            verdict: None,
            run_id,
            agent_run_id,
            intent: intent.into(),
            active_ceiling_ms: 46_000,
        },
    ]);
    let view = SessionView::replay(session_id, &journal(session_id, events.clone()))
        .expect("crash-left prefix")
        .expect("Session");
    let goal = view.goal(goal_id).expect("Goal");
    assert_eq!(view.runs[0].status, RunStatus::Interrupted);
    assert!(view.runs[0].tools[0].observation.is_none());
    assert_eq!(goal.status, GoalStatus::Interrupted);
    assert_eq!(
        goal.resume_usage(&view).active_ms,
        46_000,
        "only the open Tool's ceiling is charged, not the settled call's reservation"
    );
    assert!(
        !goal.can_resume(&view),
        "an unobserved effect stops continuation"
    );

    let mut resumed = events;
    resumed.push(Event::GoalResumed { goal_id });
    assert!(SessionView::replay_open(session_id, &journal(session_id, resumed)).is_err());
}

#[test]
fn crash_mid_run_charges_only_the_open_action_and_resume_continues() {
    let session_id = SessionId::new();
    let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let mut events = vec![
        Event::SessionStarted {
            title: "Goal".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
        },
        goal_started(goal_id, GoalBudget::default()),
    ];
    events.extend(start_run(
        goal_id,
        run_id,
        agent_run_id,
        config(CollaborationPolicy::Single, false),
    ));
    events.push(Event::GoalProviderCallReserved {
        goal_id,
        run_id,
        agent_run_id,
        active_ceiling_ms: 7_000,
    });
    let mut resumed = events.clone();
    resumed.push(Event::GoalResumed { goal_id });
    resumed.extend(start_run(
        goal_id,
        RunId::new(),
        AgentRunId::new(),
        config(CollaborationPolicy::Single, false),
    ));
    let view = SessionView::replay_open(session_id, &journal(session_id, resumed))
        .expect("resume continues with time left")
        .expect("Session");
    let goal = view.goal(goal_id).expect("Goal");
    assert_eq!((goal.active_ms, goal.uncertain_active_ms), (7_000, 7_000));
    assert_eq!(goal.active_ceiling_ms, 0, "charged once");

    let view = SessionView::replay(session_id, &journal(session_id, events))
        .expect("crash-left Run")
        .expect("Session");
    let mut capped = view.goals[0].clone();
    capped.active_deadline = Some((run_id, Some(3_000)));
    assert_eq!(
        capped.unsettled_active_charge_ms(),
        3_000,
        "the Run deadline caps it"
    );
    capped.active_deadline = Some((run_id, Some(30_000)));
    capped.plan_contract = Some(GoalPlanContract {
        mission: GOAL_OBJECTIVE.into(),
        resource_class: crate::tools::ToolResourceClass::Default,
        command_timeout_ms: None,
        paths: Vec::new(),
        commands: Vec::new(),
        skills: Vec::new(),
        mcp_tools: Vec::new(),
        success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
            path: "src/lib.rs".into(),
            content: "ok".into(),
        },
        budget: GoalBudget {
            max_active_ms: Some(5_000),
            ..GoalBudget::default()
        },
    });
    assert_eq!(
        capped.unsettled_active_charge_ms(),
        5_000,
        "plan narrowing caps it"
    );
}

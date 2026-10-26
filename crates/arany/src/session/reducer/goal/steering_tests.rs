use super::super::test_support::*;
use super::super::*;
use crate::provider::AgentPhase;
use crate::session::events::{ChildGrant, CollaborationPolicy};
use crate::tools::ToolCall;

#[test]
fn steering_note_is_accepted_only_inside_an_open_primary_reservation() {
    let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let session = SessionId::new();
    let started = || {
        let mut events = vec![goal_started(goal_id, GoalBudget::default())];
        events.extend(start_run(
            goal_id,
            run_id,
            agent_run_id,
            config(CollaborationPolicy::Single, false),
        ));
        events
    };
    let open = || {
        let mut events = started();
        events.push(Event::GoalProviderCallReserved {
            goal_id,
            run_id,
            agent_run_id,
            active_ceiling_ms: 0,
        });
        events
    };
    let note = |goal_id: GoalId, text: &str| Event::GoalSteeringNoteRecorded {
        goal_id,
        run_id,
        text: text.into(),
    };
    let accepts = |events: Vec<Event>| {
        SessionView::replay_open(session, &session_with(session, events)).is_ok()
    };

    let mut accepted = open();
    accepted.push(note(goal_id, "use the smaller fixture"));
    let view = SessionView::replay_open(session, &session_with(session, accepted.clone()))
        .expect("a note inside the open reservation replays")
        .expect("Session exists");
    assert_eq!(view.runs[0].steering_notes, vec!["use the smaller fixture"]);

    // Every rejected case first proves its prefix is accepted, so the rejection is not vacuous.
    let prefix = started();
    assert!(accepts(prefix.clone()));
    let mut before = prefix;
    before.push(note(goal_id, "before the reservation"));
    assert!(!accepts(before));

    let recorded_prefix = {
        let mut events = open();
        events.push(Event::ProviderCallRecorded {
            run_id,
            agent_run_id,
            record: record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::Finished,
                None,
            ),
        });
        events
    };
    assert!(accepts(recorded_prefix.clone()));
    let mut recorded = recorded_prefix;
    recorded.push(note(goal_id, "after the call settled"));
    assert!(!accepts(recorded));

    let child = AgentRunId::new();
    let child_prefix = {
        let mut events = vec![goal_started(goal_id, GoalBudget::default())];
        events.extend(start_run(
            goal_id,
            run_id,
            agent_run_id,
            config(
                CollaborationPolicy::Auto {
                    max_active_children: 1,
                },
                false,
            ),
        ));
        events.push(Event::GoalProviderCallReserved {
            goal_id,
            run_id,
            agent_run_id,
            active_ceiling_ms: 0,
        });
        events.push(Event::ProviderCallRecorded {
            run_id,
            agent_run_id,
            record: record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::Delegated,
                None,
            ),
        });
        events.push(Event::AgentSpawned {
            profile: None,
            assignment: None,
            definition: None,
            run_id,
            agent_run_id: child,
            role: AgentRole::Child,
            ordinal: 1,
            objective: Some("inspect the parser".into()),
            grant: Some(ChildGrant::NoTools),
        });
        events.push(Event::GoalProviderCallReserved {
            goal_id,
            run_id,
            agent_run_id: child,
            active_ceiling_ms: 0,
        });
        events
    };
    assert!(accepts(child_prefix.clone()));
    let mut child_open = child_prefix;
    child_open.push(note(goal_id, "for the child"));
    assert!(!accepts(child_open));

    let mut full = open();
    full.extend((1..=4).map(|n| note(goal_id, &n.to_string())));
    assert!(accepts(full.clone()));
    full.push(note(
        goal_id,
        &"f".repeat(crate::session::MAX_STEERING_NOTE_BYTES),
    ));
    assert!(!accepts(full), "a Run's notes stay within the byte cap");

    // Each note charges its text plus its label, so exactly the per-Run byte cap is accepted.
    let label = crate::session::MAX_STEERING_LABEL_BYTES;
    let mut exact = open();
    exact.push(note(goal_id, &"a".repeat(4 * 1024 - label)));
    exact.push(note(goal_id, &"b".repeat(4 * 1024 - label)));
    assert!(accepts(exact.clone()));
    let mut oversized = open();
    oversized.push(note(goal_id, &"a".repeat(4 * 1024 - label)));
    oversized.push(note(goal_id, &"b".repeat(4 * 1024 - label + 1)));
    assert!(!accepts(oversized));

    let mut finished_prefix = open();
    finished_prefix.push(Event::ProviderCallRecorded {
        run_id,
        agent_run_id,
        record: record(
            AgentPhase::RootPlan,
            ProviderCallDisposition::Finished,
            None,
        ),
    });
    finished_prefix.push(Event::AgentFinished {
        worker: None,
        run_id,
        agent_run_id,
        disposition: AgentDisposition::Finished,
        summary: Some("done".into()),
        result: Some("done".into()),
    });
    assert!(accepts(finished_prefix.clone()));
    let mut between = finished_prefix;
    between.push(note(goal_id, "between AgentFinished and RunFinished"));
    assert!(!accepts(between));

    let mut ended_prefix = open();
    ended_prefix.extend(cancel_run(run_id, agent_run_id));
    assert!(accepts(ended_prefix.clone()));
    let mut finished = ended_prefix;
    finished.push(note(goal_id, "after the Run ended"));
    assert!(!accepts(finished));

    let other_prefix = open();
    assert!(accepts(other_prefix.clone()));
    let mut other = other_prefix;
    other.push(note(GoalId::new(), "another Goal"));
    assert!(!accepts(other));

    // A Tool review reserves under the primary id; its pending Tool observation must reject notes.
    let mut review = SessionView::replay_open(session, &session_with(session, open()))
        .expect("open reservation replays")
        .expect("Session exists");
    let intent = crate::tools::EffectIntent {
        host: None,
        host_command: None,
        target: crate::session::worker::EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id,
        agent_run_id,
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: crate::tools::ToolCall::Read {
            path: "src/main.rs".into(),
            offset: None,
            limit: Some(32),
        },
        limits: crate::tools::ToolLimits::default(),
        use_count: 1,
    };
    review.runs[0].tools.push(ToolView {
        checkpoint: None,
        active_time_reserved: false,
        narrowing_source: None,
        permission_verdict: None,
        started_sequence: Some(0),
        hooks: Vec::new(),
        hook_sequence: None,
        intent,
        observation: None,
        review_call_index: None,
        finished_sequence: None,
    });
    assert!(
        review
            .record_goal_steering_note(&goal_id, &run_id, "during a Tool review")
            .is_err()
    );
}

fn child_note_prefix(
    goal_id: GoalId,
    run_id: RunId,
    primary: AgentRunId,
    child: AgentRunId,
) -> Vec<Event> {
    let mut events = vec![goal_started(goal_id, GoalBudget::default())];
    events.extend(start_run(
        goal_id,
        run_id,
        primary,
        config(
            CollaborationPolicy::Auto {
                max_active_children: 2,
            },
            false,
        ),
    ));
    events.extend([
        Event::GoalProviderCallReserved {
            goal_id,
            run_id,
            agent_run_id: primary,
            active_ceiling_ms: 0,
        },
        Event::ProviderCallRecorded {
            run_id,
            agent_run_id: primary,
            record: record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::Delegated,
                None,
            ),
        },
        Event::AgentSpawned {
            profile: None,
            assignment: None,
            definition: None,
            run_id,
            agent_run_id: child,
            role: AgentRole::Child,
            ordinal: 1,
            objective: Some("Inspect facts".into()),
            grant: Some(ChildGrant::NoTools),
        },
    ]);
    events
}

#[test]
fn child_steering_requires_its_open_work_reservation() {
    let (session, goal, run, primary, child) = (
        SessionId::new(),
        GoalId::new(),
        RunId::new(),
        AgentRunId::new(),
        AgentRunId::new(),
    );
    let base = child_note_prefix(goal, run, primary, child);
    let reservation = Event::GoalProviderCallReserved {
        goal_id: goal,
        run_id: run,
        agent_run_id: child,
        active_ceiling_ms: 0,
    };
    let note = Event::AgentSteeringNoteRecorded {
        goal_id: goal,
        run_id: run,
        agent_run_id: child,
        text: "literal /grant all".into(),
    };
    let mut open = base.clone();
    open.push(reservation);
    let accepts = |events: Vec<Event>| {
        SessionView::replay_open(session, &session_with(session, events)).is_ok()
    };
    assert!(accepts(open.clone()));
    let mut valid = open.clone();
    valid.push(note.clone());
    let view = SessionView::replay_open(session, &session_with(session, valid))
        .unwrap()
        .unwrap();
    assert!(view.runs[0].steering_notes.is_empty());
    assert_eq!(
        view.runs[0].agents[1].steering_notes,
        ["literal /grant all"]
    );
    assert_eq!(view.runs[0].agents[1].grant, Some(ChildGrant::NoTools));
    let mut settled = open.clone();
    settled.push(Event::ProviderCallRecorded {
        run_id: run,
        agent_run_id: child,
        record: record(
            AgentPhase::ChildWork,
            ProviderCallDisposition::Finished,
            None,
        ),
    });
    let mut finished = settled.clone();
    finished.push(Event::AgentFinished {
        worker: None,
        run_id: run,
        agent_run_id: child,
        disposition: AgentDisposition::Finished,
        summary: Some("done".into()),
        result: Some("done".into()),
    });
    for prefix in [base, settled, finished] {
        assert!(accepts(prefix.clone()), "valid rejection prefix");
        let mut rejected = prefix;
        rejected.push(note.clone());
        assert!(!accepts(rejected));
    }
    for invalid in [
        Event::AgentSteeringNoteRecorded {
            goal_id: GoalId::new(),
            run_id: run,
            agent_run_id: child,
            text: "x".into(),
        },
        Event::AgentSteeringNoteRecorded {
            goal_id: goal,
            run_id: RunId::new(),
            agent_run_id: child,
            text: "x".into(),
        },
        Event::AgentSteeringNoteRecorded {
            goal_id: goal,
            run_id: run,
            agent_run_id: primary,
            text: "x".into(),
        },
        Event::AgentSteeringNoteRecorded {
            goal_id: goal,
            run_id: run,
            agent_run_id: AgentRunId::new(),
            text: "x".into(),
        },
    ] {
        assert!(accepts(open.clone()));
        let mut rejected = open.clone();
        rejected.push(invalid);
        assert!(!accepts(rejected));
    }
    let mut capped = open.clone();
    capped.push(Event::AgentSteeringNoteRecorded {
        goal_id: goal,
        run_id: run,
        agent_run_id: child,
        text: "x".repeat(
            crate::session::MAX_STEERING_NOTE_BYTES
                - crate::session::MAX_CHILD_STEERING_LABEL_BYTES,
        ),
    });
    assert!(accepts(capped.clone()));
    capped.push(note);
    assert!(!accepts(capped));
    let mut prefix = SessionView::replay_open(session, &session_with(session, open))
        .unwrap()
        .unwrap();
    let pending = pending_plan_view(None).runs[0].tools[0].clone();
    prefix.runs[0].tools.push(pending.clone());
    assert!(
        prefix
            .record_agent_steering_note(&goal, &run, &child, "unrelated primary review")
            .is_ok()
    );
    prefix.runs[0].agents[1].tools.push(pending);
    assert!(
        prefix
            .record_agent_steering_note(&goal, &run, &child, "own review")
            .is_err()
    );
    prefix.runs[0].agents[1].tools.pop();
    prefix.runs[0].tools.pop();
    assert!(
        prefix
            .clone()
            .record_agent_steering_note(&goal, &run, &child, "control")
            .is_ok()
    );
    type Mutate = fn(&mut SessionView, RunId, AgentRunId);
    let rejections: [(&str, Mutate); 6] = [
        ("paused goal", |view, _, _| {
            view.goals[0].status = GoalStatus::Interrupted;
        }),
        ("finished goal", |view, _, _| {
            view.goals[0].status = GoalStatus::Completed;
        }),
        ("non-latest run", |view, _, _| {
            let mut later = view.runs[0].clone();
            later.id = RunId::new();
            view.runs.push(later);
        }),
        ("workers stop", |view, _, _| {
            view.runs[0].workers.stop = Some((1, crate::session::worker::WorkerStopReason::Paused));
        }),
        ("hook stop", |view, run, child| {
            let intent = child_intent(
                run,
                child,
                ToolCall::Command {
                    program: "sh".into(),
                    args: Vec::new(),
                    cwd: String::new(),
                },
            );
            view.runs[0].lifecycle_hooks.push(HookView {
                sequence: 99,
                invocation: crate::tools::permissions::HookInvocation {
                    event: crate::tools::permissions::HookEvent::SubagentStop,
                    group: 0,
                    handler: 0,
                    subject_digest: [0; 32],
                    intent: intent.clone(),
                    input_digest: [0; 32],
                    active_ceiling_ms: None,
                },
                outcome: Some(crate::tools::permissions::HookOutcome {
                    observation: observation_for(intent, b""),
                    result: crate::tools::permissions::HookResult::Stopped { reason: None },
                    boundary_active_ms: None,
                }),
                idle: false,
            });
        }),
        ("finished child", |view, _, _| {
            view.runs[0].agents[1].status = AgentStatus::Finished;
        }),
    ];
    for (name, mutate) in rejections {
        let mut view = prefix.clone();
        mutate(&mut view, run, child);
        assert!(
            view.record_agent_steering_note(&goal, &run, &child, "x")
                .is_err(),
            "{name}"
        );
    }
}

#[tokio::test]
async fn child_steering_replays_as_history_without_redelivery() {
    let (session, goal, run, primary, child) = (
        SessionId::new(),
        GoalId::new(),
        RunId::new(),
        AgentRunId::new(),
        AgentRunId::new(),
    );
    let mut prefix = child_note_prefix(goal, run, primary, child);
    prefix.extend([
        Event::GoalProviderCallReserved {
            goal_id: goal,
            run_id: run,
            agent_run_id: child,
            active_ceiling_ms: 0,
        },
        Event::AgentSteeringNoteRecorded {
            goal_id: goal,
            run_id: run,
            agent_run_id: child,
            text: "keep the fixture".into(),
        },
    ]);
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let store = crate::Store::open(crate::StateRoot::admit(&state).unwrap()).unwrap();
    for envelope in session_with(session, prefix) {
        store.append(session, envelope.event).await.unwrap();
    }
    store.close().await.unwrap();
    let store =
        crate::Store::open_read_only(crate::StateRoot::open_existing(&state).unwrap()).unwrap();
    let mut events = store.load_session(session).await.unwrap();
    store.close().await.unwrap();
    let crashed = SessionView::replay(session, &events).unwrap().unwrap();
    assert_eq!(crashed.runs[0].status, RunStatus::Interrupted);
    assert_eq!(crashed.runs[0].agents[1].status, AgentStatus::Interrupted);
    assert_eq!(
        crashed.runs[0].agents[1].steering_notes,
        ["keep the fixture"]
    );
    assert!(
        crashed.runs[0]
            .history_user_text()
            .contains("[child 1 / 1] keep the fixture")
    );
    let mut next = vec![Event::GoalResumed { goal_id: goal }];
    next.extend(start_run(
        goal,
        RunId::new(),
        AgentRunId::new(),
        config(CollaborationPolicy::Single, false),
    ));
    let first = events.last().unwrap().sequence + 1;
    for (sequence, event) in (first..).zip(next) {
        let (run_id, agent_run_id) = event.scope();
        events.push(EventEnvelope {
            session_id: session,
            sequence,
            run_id,
            agent_run_id,
            event,
            created_at_ms: 0,
        });
    }
    let resumed = SessionView::replay_open(session, &events).unwrap().unwrap();
    assert!(resumed.runs[1].steering_notes.is_empty());
    assert!(
        resumed.runs[1]
            .agents
            .iter()
            .all(|actor| actor.steering_notes.is_empty())
    );
}

#[test]
fn steering_notes_charge_labels_and_stay_within_the_run_byte_bound() {
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
    let note = |text: &str| Event::GoalSteeringNoteRecorded {
        goal_id,
        run_id,
        text: text.into(),
    };
    // One byte of text plus its label is the densest packing the bound allows.
    let most =
        crate::session::MAX_STEERING_NOTE_BYTES / (1 + crate::session::MAX_STEERING_LABEL_BYTES);
    events.extend((0..most).map(|_| note("n")));
    let view = SessionView::replay_open(session, &session_with(session, events.clone()))
        .expect("the maximum tiny notes replay")
        .expect("Session exists");
    let run = &view.runs[0];
    assert_eq!(run.steering_notes.len(), most);
    let text = run.history_user_text();
    assert!(text.len() - run.objective.len() <= crate::session::MAX_STEERING_CONTEXT_BYTES);
    events.push(note("n"));
    assert!(
        SessionView::replay_open(session, &session_with(session, events)).is_err(),
        "the next tiny note exceeds the Run's bound with its label"
    );
}

#[test]
fn steering_notes_have_no_count_cap_but_keep_the_run_byte_bound() {
    let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let session = SessionId::new();
    let open = || {
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
        events
    };
    let note = |len: usize| Event::GoalSteeringNoteRecorded {
        goal_id,
        run_id,
        text: "n".repeat(len),
    };
    let mut events = open();
    events.extend((0..9).map(|_| note(800)));
    let view = SessionView::replay_open(session, &session_with(session, events.clone()))
        .expect("nine notes within 8 KiB replay")
        .expect("Session exists");
    assert_eq!(view.runs[0].steering_notes.len(), 9);
    // Nine notes of 800 bytes charge 9 x (800 + 8) = 7272; a 912-byte tenth charges 920 and
    // reaches exactly 8192, while 913 bytes would charge 921 and reach 8193.
    events.push(note(912));
    assert!(
        SessionView::replay_open(session, &session_with(session, events.clone())).is_ok(),
        "the Run's notes together reach exactly 8 KiB with labels"
    );
    events.push(note(913));
    assert!(
        SessionView::replay_open(session, &session_with(session, events)).is_err(),
        "one more byte exceeds the Run's bound"
    );
}

#[test]
fn history_user_text_labels_notes_within_the_steering_reserve() {
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
    for _ in 0..4 {
        events.push(Event::GoalSteeringNoteRecorded {
            goal_id,
            run_id,
            text: "n".repeat(2000),
        });
    }
    let view = SessionView::replay_open(session, &session_with(session, events))
        .expect("notes replay")
        .expect("Session exists");
    let run = &view.runs[0];
    let text = run.history_user_text();
    assert!(text.starts_with(&run.objective));
    assert!(text.contains("\n[4] nnnn"));
    // The reserve covers every note byte plus its labels and header, so compaction and history agree on size.
    assert!(text.len() - run.objective.len() <= crate::session::MAX_STEERING_CONTEXT_BYTES);
    let mut bare = run.clone();
    bare.steering_notes.clear();
    assert_eq!(bare.history_user_text(), run.objective);
    let mut mixed = bare;
    let primary = mixed.agents[0].clone();
    for ordinal in 1..=8 {
        let mut child = primary.clone();
        child.role = AgentRole::Child;
        child.ordinal = ordinal;
        child.id = AgentRunId::new();
        mixed.agents.push(child);
    }
    let mut next = 0;
    while mixed.steering_bytes() + 1 + crate::session::MAX_CHILD_STEERING_LABEL_BYTES
        <= crate::session::MAX_STEERING_NOTE_BYTES
    {
        if next % 9 == 0 {
            mixed.steering_notes.push("n".into());
        } else {
            mixed.agents[next % 9].steering_notes.push("n".into());
        }
        next += 1;
    }
    assert!(
        mixed.history_user_text().len() - mixed.objective.len()
            <= crate::session::MAX_STEERING_CONTEXT_BYTES
    );
    assert!(mixed.history_user_text().contains("[child 8 /"));
}

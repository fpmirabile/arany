use super::goals::*;
use super::team::{assert_paused_child_stopped, child_tool_starts};
use super::*;
use arany::test_support::goals::*;

pub(super) fn steering_note_at(steering: &RunSteering, text: &str) {
    steering
        .offer_note(text.into())
        .expect("steering note offered while its Run is open");
}

#[tokio::test]
async fn steering_note_reaches_the_next_primary_call_and_is_journaled_once() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let offer = steering.clone();
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read()),
        finish_with("noted"),
    ])
    .with_before_call(0, move || {
        steering_note_at(&offer, "use the smaller fixture")
    });
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Read the facts");
    input.include_paths.clear();
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert_eq!(calls.len(), 2);
    assert!(call_steering_notes(&calls[0]).is_empty());
    assert_eq!(call_steering_notes(&calls[1]), ["use the smaller fixture"]);
    assert_eq!(steering.delivered_notes(), 1);
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert_action_bounds(&events, false, true);
    assert_eq!(view.runs[0].steering_notes, ["use the smaller fixture"]);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::GoalSteeringNoteRecorded { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn steering_notes_offered_before_delivery_reach_one_primary_call_as_one_note() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let offer = steering.clone();
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read()),
        finish_with("noted"),
    ])
    .with_before_call(0, move || {
        steering_note_at(&offer, "use the smaller fixture");
        steering_note_at(&offer, "and keep the old name");
    });
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Read the facts");
    input.include_paths.clear();
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert_eq!(
        call_steering_notes(&calls[1]),
        ["use the smaller fixture\n\nand keep the old name"]
    );
    assert_eq!(steering.delivered_notes(), 1);
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(
        view.runs[0].steering_notes,
        ["use the smaller fixture\n\nand keep the old name"]
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::GoalSteeringNoteRecorded { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn steering_note_never_reaches_children_and_is_kept_for_synthesis() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let offer = steering.clone();
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "Inspect facts".into(),
                profile: None,
            }],
        }),
        finish_with("child result"),
        finish_with("synthesized"),
    ])
    .with_before_call(0, move || {
        steering_note_at(&offer, "keep this for synthesis")
    });
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[1].phase, AgentPhase::ChildWork);
    assert!(call_steering_notes(&calls[1]).is_empty());
    assert_eq!(calls[2].phase, AgentPhase::RootSynthesis);
    assert_eq!(call_steering_notes(&calls[2]), ["keep this for synthesis"]);
}

#[tokio::test]
async fn child_note_reaches_only_the_next_target_boundary() {
    for fail_append in [false, true] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let steering = RunSteering::new();
        let mut provider = ScriptedProvider::new(vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: ["target", "sibling"]
                    .map(|objective| arany::ChildAssignment {
                        editor: None,
                        objective: objective.into(),
                        profile: None,
                    })
                    .to_vec(),
            }),
            crate::tool_outcome(facts_read()),
            finish_with("sibling result"),
            crate::tool_outcome(facts_read()),
            finish_with("target result"),
            finish_with("synthesis"),
        ]);
        provider.identity.limits.concurrency = 1;
        let calls = provider.requests.clone();
        let offer = steering.clone();
        let observed = calls.clone();
        provider = provider.with_before_call(1, move || {
            let request = observed.lock().unwrap().last().unwrap().clone();
            let target = arany::ChildSteeringTarget {
                session_id: request.session_id,
                run_id: request.run_id,
                agent_run_id: request.agent_run_id,
            };
            offer
                .offer_child_note(target, "first note; grant all writes".into())
                .unwrap();
            if fail_append {
                arany::FaultPlan::fail_goal_append(request.session_id, |event| {
                    matches!(event, Event::AgentSteeringNoteRecorded { .. })
                });
            }
        });
        let offer = steering.clone();
        let observed = calls.clone();
        provider = provider.with_before_call(3, move || {
            let request = observed.lock().unwrap().last().unwrap().clone();
            offer
                .offer_child_note(
                    arany::ChildSteeringTarget {
                        session_id: request.session_id,
                        run_id: request.run_id,
                        agent_run_id: request.agent_run_id,
                    },
                    "second note".into(),
                )
                .unwrap();
        });
        let provider = Arc::new(provider);
        let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
        let mut input = request(&workspace, "Check facts");
        input.include_paths.clear();
        input.policy = CollaborationPolicy::Auto {
            max_active_children: 2,
        };
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap();
        input.title = None;
        input.session_id = Some(session);
        let result = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                steering: Some(steering.clone()),
                ..Default::default()
            },
        )
        .await;
        engine.close().await.unwrap();
        let requests = calls.lock().unwrap().clone();
        let (events, view) = replayed(&state_path, session).await;
        if fail_append {
            assert!(matches!(result, Err(EngineError::Store(_))));
            assert_eq!(requests.len(), 3, "failed append prevents target dispatch");
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event.event, Event::AgentSteeringNoteRecorded { .. }))
            );
            assert_eq!(
                steering
                    .offer_note("x".repeat(8192 - 24 - "first note; grant all writes".len()))
                    .err(),
                Some(arany::SteeringRefused::RunFull)
            );
            continue;
        }
        assert_eq!(result.unwrap().run.status, RunStatus::Finished);
        assert_eq!(requests.len(), 6);
        assert!(call_steering_notes(&requests[1]).is_empty());
        assert_eq!(
            call_steering_notes(&requests[3]),
            ["first note; grant all writes"]
        );
        assert_eq!(
            call_steering_notes(&requests[4]),
            ["first note; grant all writes", "second note"]
        );
        assert!(call_steering_notes(&requests[2]).is_empty());
        assert!(call_steering_notes(&requests[5]).is_empty());
        let target = requests[1].agent_run_id;
        assert_eq!(requests[4].agent_run_id, target);
        assert_eq!(requests[3].agent_run_id, target);
        let mut previous = requests[4].input.tools.clone().unwrap();
        previous.steps.pop();
        assert_eq!(
            previous,
            requests[3].input.tools.clone().unwrap(),
            "earlier context stays append-only"
        );
        assert_eq!(
            requests[1].input.tools.as_ref().unwrap().catalog(),
            requests[4].input.tools.as_ref().unwrap().catalog()
        );
        let notes: Vec<_> = events
            .iter()
            .filter(|event| matches!(event.event, Event::AgentSteeringNoteRecorded { .. }))
            .collect();
        assert_eq!(notes.len(), 2);
        for note in notes {
            assert_eq!(note.agent_run_id, Some(target));
            let next = events.iter().find(|event| event.sequence > note.sequence && matches!(event.event, Event::ProviderCallRecorded { agent_run_id, .. } if agent_run_id == target)).unwrap();
            assert!(note.sequence < next.sequence);
        }
        assert!(view.runs[0].steering_notes.is_empty());
        assert!(view.runs[0].agents[2].steering_notes.is_empty());
        assert!(matches!(
            view.runs[0].agents[1].grant,
            Some(arany::ChildGrant::ReadOnly { .. })
        ));
        assert_action_bounds(&events, false, true);
    }
}

#[tokio::test]
async fn child_note_is_not_taken_when_the_run_stops_after_the_call_reservation() {
    for pause in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let steering = RunSteering::new();
        let cancel = RunCancellation::new();
        let mut provider = ScriptedProvider::new(vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: ["first", "second"]
                    .map(|objective| arany::ChildAssignment {
                        editor: None,
                        objective: objective.into(),
                        profile: None,
                    })
                    .to_vec(),
            }),
            finish_with("first result"),
            finish_with("second result"),
            finish_with("synthesis"),
        ]);
        provider.identity.limits.concurrency = 1;
        let calls = provider.requests.clone();
        let (publish, published) = tokio::sync::oneshot::channel();
        let publish = Arc::new(std::sync::Mutex::new(Some(publish)));
        let observed = calls.clone();
        let provider = Arc::new(provider.with_before_call(1, move || {
            let request = observed.lock().unwrap().last().unwrap().clone();
            let gate =
                arany::FaultPlan::gate_goal_append_acknowledgment(request.session_id, |event| {
                    matches!(event, Event::GoalProviderCallReserved { .. })
                });
            if let Some(publish) = publish.lock().unwrap().take() {
                let _ = publish.send((request.agent_run_id, gate));
            }
        }));
        let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
        let mut input = request(&workspace, "Check facts");
        input.include_paths.clear();
        input.policy = CollaborationPolicy::Auto {
            max_active_children: 2,
        };
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap();
        input.title = None;
        input.session_id = Some(session);
        let arany::RunHandle {
            mut events, result, ..
        } = engine
            .submit(
                arany::AdmittedProviders::single(provider.clone()),
                input,
                RunControls {
                    steering: Some(steering.clone()),
                    cancellation: cancel.clone(),
                    ..Default::default()
                },
            )
            .unwrap();
        let driver = {
            let (steering, cancel) = (steering.clone(), cancel.clone());
            tokio::spawn(async move {
                let (first, (entered, release)) = published.await.expect("first child dispatched");
                let mut children = Vec::new();
                while children.len() < 2 {
                    if let Some(arany::Tapped::Committed(envelope)) = events.next().await
                        && let Event::AgentSpawned {
                            role: arany::AgentRole::Child,
                            run_id,
                            agent_run_id,
                            ..
                        } = envelope.event
                    {
                        children.push((run_id, agent_run_id));
                    }
                }
                entered
                    .await
                    .expect("second child reservation reached its acknowledgment");
                let (run_id, agent_run_id) = children
                    .into_iter()
                    .find(|(_, id)| *id != first)
                    .expect("second child spawned");
                steering
                    .offer_child_note(
                        arany::ChildSteeringTarget {
                            session_id: session,
                            run_id,
                            agent_run_id,
                        },
                        "too late".into(),
                    )
                    .expect("the child is registered and not paused");
                if pause {
                    assert!(steering.pause());
                } else {
                    cancel.cancel();
                }
                release.send(()).expect("release the reservation");
            })
        };
        let result = tokio::time::timeout(Duration::from_secs(30), result)
            .await
            .expect("Run settles");
        driver.await.unwrap();
        assert!(result.is_ok(), "pause={pause}");
        engine.close().await.unwrap();
        let (events, _) = replayed(&state_path, session).await;
        assert_eq!(calls.lock().unwrap().len(), 2, "pause={pause}: no dispatch");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event, Event::AgentSteeringNoteRecorded { .. })),
            "pause={pause}"
        );
        let reserved = events
            .iter()
            .filter(|event| matches!(event.event, Event::GoalProviderCallReserved { .. }))
            .count();
        let recorded = events
            .iter()
            .filter(|event| matches!(event.event, Event::ProviderCallRecorded { .. }))
            .count();
        assert_eq!(reserved, 3, "pause={pause}: the undispatched call is spent");
        assert_eq!(
            recorded, reserved,
            "pause={pause}: every reservation settles"
        );
        assert!(events.iter().any(|event| matches!(
            &event.event,
            Event::ProviderCallRecorded { record, .. }
                if record.disposition == arany::ProviderCallDisposition::Cancelled
        )));
    }
}

#[tokio::test]
async fn child_note_is_retained_when_no_boundary_remains() {
    for stop in ["finish", "cancel", "pause"] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let steering = RunSteering::new();
        let cancel = RunCancellation::new();
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: vec![arany::ChildAssignment {
                    editor: None,
                    objective: "target".into(),
                    profile: None,
                }],
            }),
            finish_with("target result"),
            finish_with("synthesis"),
        ]);
        let calls = provider.requests.clone();
        let observed = calls.clone();
        let offer = steering.clone();
        let cancellation = cancel.clone();
        let offered = Arc::new(std::sync::Mutex::new(None));
        let target_copy = offered.clone();
        let provider = Arc::new(provider.with_before_call(1, move || {
            let request = observed.lock().unwrap().last().unwrap().clone();
            let target = arany::ChildSteeringTarget {
                session_id: request.session_id,
                run_id: request.run_id,
                agent_run_id: request.agent_run_id,
            };
            offer.offer_child_note(target, "too late".into()).unwrap();
            *target_copy.lock().unwrap() = Some(target);
            if stop == "cancel" {
                cancellation.cancel();
            } else if stop == "pause" {
                offer.pause();
            }
        }));
        let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
        let mut input = request(&workspace, "Check facts");
        input.include_paths.clear();
        input.policy = CollaborationPolicy::Auto {
            max_active_children: 1,
        };
        let result = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                steering: Some(steering.clone()),
                cancellation: cancel,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        engine.close().await.unwrap();
        assert_eq!(
            calls.lock().unwrap().len(),
            if stop == "finish" { 3 } else { 2 },
            "{stop}"
        );
        let (events, view) = replayed(&state_path, result.session_id).await;
        assert_eq!(view.runs[0].agents.len(), 2);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event, Event::AgentSteeringNoteRecorded { .. }))
        );
        let target = offered.lock().unwrap().unwrap();
        assert_eq!(
            steering.offer_child_note(target, "again".into()),
            Err(arany::SteeringRefused::Unavailable)
        );
        assert_eq!(
            steering.withdraw_child_note(target).as_deref(),
            Some("too late")
        );
    }
}

#[tokio::test]
async fn undelivered_steering_note_stays_with_the_caller_when_the_run_finishes() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let offer = steering.clone();
    let provider = ScriptedProvider::new(vec![finish_with("done")])
        .with_before_call(0, move || steering_note_at(&offer, "too late"));
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Answer quickly");
    input.include_paths.clear();
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    assert_eq!(steering.delivered_notes(), 0);
    assert_eq!(
        steering.withdraw_note().as_deref(),
        Some("too late"),
        "the caller still holds the note"
    );
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert!(view.runs[0].steering_notes.is_empty());
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, Event::GoalSteeringNoteRecorded { .. }))
    );
}

#[tokio::test]
async fn steering_note_is_history_for_a_later_run_and_never_redelivered() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let offer = steering.clone();
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read()),
        finish_with("first answer"),
        finish_with("second answer"),
    ])
    .with_before_call(0, move || {
        steering_note_at(&offer, "use the smaller fixture")
    });
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut first = request(&workspace, "Read the facts");
    first.include_paths.clear();
    let first_outcome = run_with(
        &engine,
        &provider,
        first,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("first Run");

    let mut second = request(&workspace, "Second question");
    second.session_id = Some(first_outcome.session_id);
    second.title = None;
    second.include_paths.clear();
    let second_outcome = run_with(
        &engine,
        &provider,
        second,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("second Run");
    assert_eq!(second_outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls[2].input.history[0].user,
        "Read the facts\n\nSteering notes the user sent during this Run:\n[1] use the smaller fixture"
    );
    assert!(call_steering_notes(&calls[2]).is_empty());
    assert_eq!(steering.delivered_notes(), 1);
    let (events, _) = replayed(&state_path, second_outcome.session_id).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::GoalSteeringNoteRecorded { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn pause_waits_for_the_in_flight_call_and_pauses_before_its_tool() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let pause = steering.clone();
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read()),
        finish_with("never reached"),
    ])
    .with_before_call(0, move || assert!(pause.pause(), "first pause"));
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Read the facts");
    input.include_paths.clear();
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("paused Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");

    assert_eq!(requests.lock().expect("requests").len(), 1);
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, Event::ToolStarted { .. })),
        "the paused Run starts no requested Tool"
    );
    let dispositions: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::ProviderCallRecorded { record, .. } => Some(record.disposition),
            _ => None,
        })
        .collect();
    assert_eq!(
        dispositions,
        [ProviderCallDisposition::ToolRequested],
        "the in-flight call completes as the observed Tool request"
    );
    assert_eq!(
        view.goal(outcome.goal_id).expect("Goal").status,
        arany::GoalStatus::Interrupted
    );
}

#[tokio::test]
async fn pause_keeps_a_finish_returned_by_the_in_flight_call() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let pause = steering.clone();
    let provider = ScriptedProvider::new(vec![finish_with("finished before pause")])
        .with_before_call(0, move || assert!(pause.pause(), "first pause"));
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Answer quickly");
    input.include_paths.clear();
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");
}

#[tokio::test]
async fn pause_between_autonomous_runs_starts_no_next_run() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let pause = steering.clone();
    // The first Run's Answer is unsatisfied against the planned digest, so the Goal would continue.
    let provider = ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        finish_with("never started"),
    ])
    .with_before_call(2, move || assert!(pause.pause(), "first pause"));
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let outcome = run_with(
        &engine,
        &provider,
        autonomous_input(&workspace),
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("first pass returns without a next Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    assert_eq!(requests.lock().expect("requests").len(), 3);
    let (_, view) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(view.runs.len(), 1);
    // Like Ctrl+C between Runs, the pause leaves the Goal open for explicit resume.
    assert_eq!(
        view.goal(outcome.goal_id).expect("Goal").status,
        arany::GoalStatus::Interrupted
    );
}

#[tokio::test]
async fn pause_at_the_delegate_boundary_spawns_no_child() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let pause = steering.clone();
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "Inspect facts".into(),
                profile: None,
            }],
        }),
        finish_with("never reached"),
    ])
    .with_before_call(0, move || assert!(pause.pause(), "first pause"));
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("paused Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");

    assert_eq!(requests.lock().expect("requests").len(), 1);
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert!(
        !events.iter().any(|event| matches!(
            event.event,
            Event::AgentSpawned {
                role: arany::AgentRole::Child,
                ..
            }
        )),
        "a paused Delegate spawns no child"
    );
    assert_eq!(
        view.goal(outcome.goal_id).expect("Goal").status,
        arany::GoalStatus::Interrupted
    );
}

#[tokio::test]
async fn pause_before_synthesis_starts_no_synthesis_call() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let pause = steering.clone();
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "Inspect facts".into(),
                profile: None,
            }],
        }),
        finish_with("child result"),
        finish_with("never synthesized"),
    ])
    .with_before_call(1, move || assert!(pause.pause(), "first pause"));
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("paused Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert_eq!(
        calls.len(),
        2,
        "the in-flight child call completes, synthesis does not start"
    );
    assert_eq!(calls[1].phase, AgentPhase::ChildWork);
    let (_, view) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(
        view.goal(outcome.goal_id).expect("Goal").status,
        arany::GoalStatus::Interrupted
    );
}

#[tokio::test]
async fn steering_note_waits_for_the_next_autonomous_run_primary_call() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let offer = steering.clone();
    // The first Run's Answer is unsatisfied against the planned digest, so the Goal starts a second Run.
    // The note is offered while the first Run's Answer is in flight: it has no primary call left in
    // that Run, so it waits for the second Run's first primary call.
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget.max_runs = 2;
    let provider = ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(plan),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        finish_with("second pass"),
    ])
    .with_before_call(2, move || {
        steering_note_at(&offer, "carry this into the next Run")
    });
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let budget = arany::GoalBudget {
        max_runs: 2,
        ..arany::GoalBudget::default()
    };
    let mut input = request(&workspace, AUTONOMOUS_OBJECTIVE);
    input.include_paths.clear();
    input.goal = arany::GoalRequest::autonomous(budget, arany::GoalPhase::Implementation);
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("terminal Run");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert_eq!(calls.len(), 4);
    assert!(call_steering_notes(&calls[2]).is_empty());
    assert_eq!(
        call_steering_notes(&calls[3]),
        ["carry this into the next Run"]
    );
    assert_eq!(steering.delivered_notes(), 1);
    let (_, view) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(view.runs.len(), 2);
    assert!(view.runs[0].steering_notes.is_empty());
    assert_eq!(
        view.runs[1].steering_notes,
        ["carry this into the next Run"]
    );
    assert_eq!(view.runs[1].id, outcome.run.id);
}

#[tokio::test]
async fn pause_during_research_finishes_the_child_call_and_dispatches_no_child_tool() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let pause = steering.clone();
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "Inspect facts".into(),
                profile: None,
            }],
        }),
        crate::tool_outcome(facts_read()),
        finish_with("never reached"),
    ])
    .with_before_call(1, move || assert!(pause.pause(), "first pause"));
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("paused Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");
    assert_eq!(requests.lock().expect("requests").len(), 2);
    assert_paused_child_stopped(&state_path, outcome.session_id, outcome.goal_id).await;
}

#[tokio::test]
async fn pause_before_queued_children_start_cancels_them_without_failing_the_run() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let pause = steering.clone();
    let mut provider = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![
                arany::ChildAssignment {
                    editor: None,
                    objective: "Inspect facts".into(),
                    profile: None,
                },
                arany::ChildAssignment {
                    editor: None,
                    objective: "Inspect facts again".into(),
                    profile: None,
                },
            ],
        }),
        crate::tool_outcome(facts_read()),
        finish_with("never reached"),
    ])
    .with_before_call(1, move || assert!(pause.pause(), "first pause"));
    // One call at a time: the second child stays queued when the pause lands.
    provider.identity.limits.concurrency = 1;
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 2,
    };
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("paused Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");
    assert_eq!(requests.lock().expect("requests").len(), 2);

    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let children: Vec<_> = view.runs[0]
        .agents
        .iter()
        .skip(1)
        .map(|agent| agent.id)
        .collect();
    assert_eq!(children.len(), 2);
    for child in children {
        assert_eq!(child_tool_starts(&events, child), 0);
        assert!(events.iter().any(|event| matches!(
            &event.event,
            Event::AgentFinished { agent_run_id, disposition, .. }
                if *agent_run_id == child && *disposition == arany::AgentDisposition::Cancelled
        )));
    }
    assert_eq!(
        view.goal(outcome.goal_id).expect("Goal").status,
        arany::GoalStatus::Interrupted
    );
}

#[tokio::test]
async fn step_facts_and_notes_are_written_before_render() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let offer = steering.clone();
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read()),
        crate::tool_outcome(facts_read()),
        finish_with("noted"),
    ])
    .with_before_call(1, move || steering_note_at(&offer, "mid-run note"));
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Read twice");
    input.include_paths.clear();
    run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("Run");
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert!(call_facts(&calls[0]).is_some());
    for call in &calls[1..] {
        let steps = &call.input.tools.as_ref().expect("Tool context").steps;
        let last = steps.last().expect("a step");
        assert!(
            last.facts.is_some(),
            "facts exist before the request is built"
        );
        assert!(
            steps[..steps.len() - 1]
                .iter()
                .all(|step| step.facts.is_some()),
            "earlier steps keep the facts they were rendered with"
        );
    }
    let provider_calls: Vec<_> = calls
        .iter()
        .map(|call| call_facts(call).unwrap().provider_calls)
        .collect();
    assert!(provider_calls.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(call_steering_notes(&calls[1]), Vec::<String>::new());
    assert_eq!(call_steering_notes(&calls[2]), ["mid-run note"]);
    let late = &calls[2].input.tools.as_ref().expect("Tool context").steps;
    assert_eq!(
        late[1].steering_notes,
        ["mid-run note"],
        "the note rides the newest step"
    );
}

#[tokio::test]
async fn approving_review_cut_short_by_a_pause_is_not_recorded_as_an_ai_approval() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    for (name, bytes) in [
        (
            "tools.json",
            &br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#[..],
        ),
        (
            "permissions.json",
            &br#"{"permissions":{"defaultMode":"auto"}}"#[..],
        ),
    ] {
        let path = state_path.join(name);
        std::fs::write(&path, bytes).expect("settings");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
    }
    let steering = RunSteering::new();
    let pause = steering.clone();
    let provider = Arc::new(
        ScriptedProvider::new(vec![
            arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
            crate::tool_outcome(facts_read()),
            crate::tool_outcome(arany::ToolCall::Write {
                path: "notes/facts.txt".into(),
                expected_digest: None,
                content: "v2".into(),
            }),
            ProviderOutcome::Review(arany::ReviewVerdict::Approve),
            finish_with("never reached"),
        ])
        .with_before_call(3, move || assert!(pause.pause(), "first pause")),
    );
    let outcome = run_with(
        &engine,
        &provider,
        request(&workspace, "Write after review"),
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering),
            ..Default::default()
        },
    )
    .await
    .expect("paused Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/facts.txt")).expect("facts"),
        "v1"
    );
    let (events, _) = replayed(&state_path, outcome.session_id).await;
    let verdicts: Vec<_> = events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            Event::ToolStarted {
                verdict: Some(verdict),
                ..
            } => Some(verdict),
            _ => None,
        })
        .collect();
    let write = verdicts.last().expect("Write verdict");
    assert!(!write.approved);
    assert_eq!(format!("{:?}", write.source), "UnavailableApproval");
}

#[tokio::test]
async fn a_pause_in_an_in_run_compaction_backoff_starts_no_further_compaction_call() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = paged_read_workspace(temp.path());
    let state_path = temp.path().join("state");
    let mut failing = ScriptedProvider::new(vec![]).with_profile("openai", "compact-primary");
    failing.compact_responses = Mutex::new(vec![Err(ProviderError::RemoteHttp(503))].into());
    let failing = Arc::new(failing);
    let primary =
        Arc::new(ScriptedProvider::new(paged_read_outcomes()).with_profile("openai", "primary"));
    let mut bindings = AdmittedProviders::single(primary.clone());
    bindings.roles.compaction = vec![failing.clone()];
    let engine = facts_engine(
        StateRoot::admit(&state_path).expect("private state"),
        &state_path,
    );
    let session = engine
        .create_session(workspace.clone(), Some("Pause".into()))
        .await
        .expect("session");
    let (entered, release) = arany::FaultPlan::gate_retry_backoff(session);
    let mut input = request(&workspace, "Inspect the bounded file in pages");
    input.include_paths.clear();
    input.session_id = Some(session);
    input.title = None;
    input.goal.budget = Some(arany::GoalBudget {
        max_provider_calls: None,
        max_tokens: None,
        max_active_ms: Some(30 * 60 * 1000),
        max_runs: 1,
    });
    let steering = RunSteering::new();
    let run = super::engine_support::run_routed(
        &engine,
        bindings,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    );
    let outcome = {
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => panic!("Run ended before the held compaction backoff: {}", result.is_ok()),
            entered = entered => entered.expect("backoff entered"),
        }
        assert!(steering.pause());
        run.await.expect("paused Run")
    };
    // The gate stays held: only the pause can end the wait.
    drop(release);
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");
    let (events, view) = replayed(&state_path, session).await;
    let run = &view.runs[0];
    assert!(run.provider_fallbacks.is_empty());
    assert_eq!(
        run.provider_retries
            .iter()
            .map(|(_, retry)| retry.role)
            .collect::<Vec<_>>(),
        [arany::ProviderRole::Compaction]
    );
    assert_eq!(failing.compact_requests.lock().expect("requests").len(), 1);
    let record = events
        .iter()
        .find_map(|event| match &event.event {
            Event::GoalContextCompacted { record, .. } => Some(record),
            _ => None,
        })
        .expect("compaction record");
    assert!(matches!(
        record.status,
        arany::CompactionStatus::Failed {
            reason: arany::CompactionFailure::Cancelled
        }
    ));
}

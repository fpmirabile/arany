use super::engine_support::run_routed;
use super::*;
use arany::test_support::routing::*;
use arany::{AgentRole, FaultPlan};
use tokio::sync::oneshot;

const WATCHDOG: Duration = Duration::from_secs(20);

type Started = (String, oneshot::Sender<()>);

fn gated(
    model: &str,
    concurrency: u8,
    started: &mpsc::UnboundedSender<Started>,
) -> Arc<GatedProvider> {
    let mut identity = gated_identity(concurrency);
    identity.profile = "scripted".into();
    identity.model = model.into();
    Arc::new(GatedProvider {
        started: started.clone(),
        requests: Arc::default(),
        fail_child: None,
        identity,
    })
}

fn delegate(children: &[(&str, Option<&str>)]) -> ProviderOutcome {
    ProviderOutcome::Delegate(Delegate {
        agent: None,
        children: children
            .iter()
            .map(|(objective, profile)| arany::ChildAssignment {
                editor: None,
                objective: (*objective).into(),
                profile: profile.map(Into::into),
            })
            .collect(),
    })
}

fn unavailable(model: &str, failures: usize) -> Arc<ScriptedProvider> {
    let mut provider = ScriptedProvider::new(vec![]).with_profile("scripted", model);
    provider.identity.limits.concurrency = 1;
    provider.responses = Mutex::new(
        (0..failures)
            .map(|_| Err(ProviderError::RemoteHttp(503)))
            .collect(),
    );
    route(provider)
}

fn answering(model: &str, answers: &[&str]) -> Arc<ScriptedProvider> {
    let mut provider = ScriptedProvider::new(answers.iter().map(|answer| finish(answer)).collect())
        .with_profile("scripted", model);
    provider.identity.limits.concurrency = 1;
    route(provider)
}

fn team(workspace: &Path, session: Option<SessionId>, max_active_children: u8) -> RunRequest {
    let mut request = input(workspace);
    request.session_id = session;
    if session.is_some() {
        request.title = None;
    }
    request.policy = CollaborationPolicy::Team {
        max_active_children,
    };
    request
}

fn count(provider: &Arc<ScriptedProvider>) -> usize {
    provider.requests.lock().unwrap().len()
}

fn events_of(events: &[arany::EventEnvelope], wanted: impl Fn(&Event) -> bool) -> usize {
    events.iter().filter(|event| wanted(&event.event)).count()
}

fn spawned_children(events: &[arany::EventEnvelope]) -> usize {
    events_of(events, |event| {
        matches!(
            event,
            Event::AgentSpawned {
                role: AgentRole::Child,
                ..
            }
        )
    })
}

#[tokio::test]
async fn exhausted_assigned_chain_fails_its_child_and_starts_no_later_sibling() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let primary = route(
        ScriptedProvider::new(vec![
            delegate(&[("A", Some("spare")), ("B", None)]),
            finish("must not synthesize"),
        ])
        .with_profile("scripted", "primary"),
    );
    let first = unavailable("spare-first", 1);
    let last = unavailable("spare-last", 1);
    let default = answering("default", &["must not run"]);
    let mut bindings = routes(&primary);
    bindings.roles.child = vec![default.clone()];
    pin_roster(&mut bindings, Some(vec![first.clone(), last.clone()]));
    let engine = open(&state);
    let result = run_routed(
        &engine,
        bindings,
        team(&workspace, None, 2),
        RunControls::default(),
    )
    .await
    .unwrap();
    assert_eq!(result.run.status, RunStatus::Failed);
    engine.close().await.unwrap();
    let (events, view) = reopen(&state, result.session_id).await;
    let run = &view.runs[0];
    assert_eq!(
        run.agents
            .iter()
            .map(|agent| agent.status)
            .collect::<Vec<_>>(),
        [
            AgentStatus::Failed,
            AgentStatus::Failed,
            AgentStatus::Cancelled
        ]
    );
    assert_eq!(run.provider_fallbacks.len(), 1);
    assert_eq!(run.provider_fallbacks[0].1.from, first.identity);
    assert_eq!(run.provider_fallbacks[0].1.to, last.identity);
    assert_eq!([count(&first), count(&last)], [1, 1]);
    assert_eq!(count(&default), 0);
    assert_eq!(count(&primary), 1);
    assert_eq!(view.goals[0].provider_calls, 3);
    assert_eq!(
        events_of(&events, |event| matches!(event, Event::ToolStarted { .. })),
        0
    );
}

#[tokio::test]
async fn call_and_token_budgets_refuse_a_selected_profile_switch() {
    for tokens in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let primary = route(
            ScriptedProvider::new(vec![delegate(&[("A", Some("spare"))]), finish("done")])
                .with_profile("scripted", "primary")
                .with_known_input_tokens(),
        );
        let first = unavailable("spare-first", 1);
        let second = unavailable("spare-second", 1);
        let last = answering("spare-last", &["never reached"]);
        let mut bindings = routes(&primary);
        pin_roster(
            &mut bindings,
            Some(vec![first.clone(), second.clone(), last.clone()]),
        );
        let mut request = team(&workspace, None, 1);
        let budget = request.goal.budget.as_mut().unwrap();
        if tokens {
            budget.max_tokens = Some(1000);
        } else {
            budget.max_provider_calls = Some(3);
        }
        let engine = open(&state);
        let result = run_routed(&engine, bindings, request, RunControls::default())
            .await
            .unwrap();
        assert_eq!(result.run.status, RunStatus::Failed, "tokens {tokens}");
        engine.close().await.unwrap();
        let (_, view) = reopen(&state, result.session_id).await;
        let switches = usize::from(!tokens);
        assert_eq!(view.runs[0].provider_fallbacks.len(), switches);
        assert_eq!(
            [&first, &second, &last].map(count),
            [1, switches, 0],
            "tokens {tokens}"
        );
        assert_eq!(view.goals[0].provider_calls, 2 + switches as u32);
        assert!(view.goals[0].token_usage_unknown);
        assert_eq!(view.runs[0].agents[1].status, AgentStatus::Failed);
    }
}

#[derive(Clone, Copy, Debug)]
enum Interruption {
    SpawnRefused,
    SpawnAckLost,
    SpawnAcknowledgedThenCancelled,
    SwitchAckLost,
    SwitchAcknowledgedThenCancelled,
}

#[tokio::test]
async fn interruptions_around_spawn_and_switch_acknowledgements_start_no_further_call() {
    for stop in [
        Interruption::SpawnRefused,
        Interruption::SpawnAckLost,
        Interruption::SpawnAcknowledgedThenCancelled,
        Interruption::SwitchAckLost,
        Interruption::SwitchAcknowledgedThenCancelled,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let primary = route(
            ScriptedProvider::new(vec![
                delegate(&[("A", Some("spare")), ("B", None)]),
                finish("done"),
            ])
            .with_profile("scripted", "primary"),
        );
        let first = unavailable("spare-first", 1);
        let last = answering("spare-last", &["spare answer"]);
        let default = answering("default", &["default answer"]);
        let mut bindings = routes(&primary);
        bindings.roles.child = vec![default.clone()];
        pin_roster(&mut bindings, Some(vec![first.clone(), last.clone()]));
        let engine = open(&state);
        let session = engine
            .create_session(workspace.clone(), Some("Roster".into()))
            .await
            .unwrap();
        let cancellation = RunCancellation::new();
        let controls = RunControls {
            cancellation: cancellation.clone(),
            ..RunControls::default()
        };
        let run = run_routed(
            &engine,
            bindings,
            team(&workspace, Some(session), 2),
            controls,
        );
        let outcome = if let Interruption::SpawnRefused = stop {
            FaultPlan::fail_goal_append(session, |event| {
                matches!(
                    event,
                    Event::AgentSpawned {
                        role: AgentRole::Child,
                        ..
                    }
                )
            });
            tokio::time::timeout(WATCHDOG, run).await.unwrap()
        } else {
            let (entered, release) = FaultPlan::gate_goal_append_acknowledgment(
                session,
                match stop {
                    Interruption::SpawnAckLost | Interruption::SpawnAcknowledgedThenCancelled => {
                        |event| {
                            matches!(
                                event,
                                Event::AgentSpawned {
                                    role: AgentRole::Child,
                                    ..
                                }
                            )
                        }
                    }
                    _ => |event| matches!(event, Event::ProviderFallbackUsed { .. }),
                },
            );
            tokio::pin!(run);
            tokio::time::timeout(WATCHDOG, async {
                tokio::select! {
                    result = &mut run => panic!("Run ended before the held acknowledgment: {}", result.is_ok()),
                    entered = entered => entered.unwrap(),
                }
            })
            .await
            .unwrap();
            match stop {
                Interruption::SpawnAckLost | Interruption::SwitchAckLost => drop(release),
                _ => {
                    assert!(cancellation.cancel());
                    release.send(()).unwrap();
                }
            }
            tokio::time::timeout(WATCHDOG, run).await.unwrap()
        };
        let _ = engine.close().await;
        let calls = [count(&first), count(&last), count(&default)];
        let (events, view) = reopen(&state, session).await;
        let run = &view.runs[0];
        match stop {
            Interruption::SpawnRefused | Interruption::SpawnAckLost => {
                assert!(outcome.is_err(), "{stop:?}");
                assert_eq!(calls, [0, 0, 0], "{stop:?}");
                assert_eq!(
                    spawned_children(&events),
                    usize::from(matches!(stop, Interruption::SpawnAckLost)),
                    "{stop:?}"
                );
                assert_eq!(count(&primary), 1);
            }
            Interruption::SpawnAcknowledgedThenCancelled => {
                assert_eq!(outcome.unwrap().run.status, RunStatus::Cancelled);
                assert_eq!(calls, [0, 0, 0]);
                assert_eq!(spawned_children(&events), 1);
                assert_eq!(count(&primary), 1);
            }
            Interruption::SwitchAckLost => {
                assert!(outcome.is_err());
                assert_eq!(
                    calls,
                    [1, 0, 0],
                    "no call after the lost switch acknowledgment"
                );
                assert_eq!(count(&primary), 1);
                assert_eq!(run.provider_fallbacks.len(), 1);
            }
            Interruption::SwitchAcknowledgedThenCancelled => {
                assert_eq!(outcome.unwrap().run.status, RunStatus::Cancelled);
                assert_eq!(calls, [1, 0, 0], "no call after a cancelled switch");
                assert_eq!(run.provider_fallbacks.len(), 1);
                assert_eq!(count(&primary), 1);
            }
        }
        assert!(run.status != RunStatus::Active, "{stop:?}");
        let cold = arany::resume_session(
            StateRoot::open_existing(&state).unwrap(),
            workspace.clone(),
            session,
        )
        .await
        .unwrap();
        assert_eq!(cold.runs, view.runs, "{stop:?}");
    }
}

#[tokio::test]
async fn slow_heterogeneous_routes_obey_the_lowest_admitted_ceiling() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let (started, mut starts) = mpsc::unbounded_channel();
    let primary = route(
        ScriptedProvider::new(vec![
            delegate(&[("A", None), ("B", Some("spare")), ("C", None)]),
            finish("done"),
        ])
        .with_profile("scripted", "primary"),
    );
    let wide = gated("wide", 3, &started);
    let narrow = gated("narrow", 1, &started);
    let mut bindings = routes(&primary);
    bindings.roles.child = vec![wide.clone()];
    pin_roster(&mut bindings, Some(vec![narrow.clone()]));
    let engine = open(&state);
    let run = run_routed(
        &engine,
        bindings,
        team(&workspace, None, 3),
        RunControls::default(),
    );
    let release = async {
        let mut order = Vec::new();
        for _ in 0..3 {
            let (objective, release) = starts.recv().await.unwrap();
            order.push(objective);
            release.send(()).unwrap();
        }
        order
    };
    let (result, order) = tokio::time::timeout(WATCHDOG, async { tokio::join!(run, release) })
        .await
        .unwrap();
    let result = result.unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);
    assert_eq!(order, ["A", "B", "C"]);
    assert_eq!(wide.requests.lock().unwrap().len(), 2);
    assert_eq!(narrow.requests.lock().unwrap().len(), 1);
    engine.close().await.unwrap();
    let (events, view) = reopen(&state, result.session_id).await;
    let children: Vec<_> = view.runs[0].agents.iter().skip(1).map(|a| a.id).collect();
    let position = |wanted: &dyn Fn(&Event) -> bool| events.iter().position(|e| wanted(&e.event));
    for pair in children.windows(2) {
        let recorded = position(&|event| {
            matches!(event, Event::ProviderCallRecorded { agent_run_id, .. } if *agent_run_id == pair[0])
        })
        .unwrap();
        let reserved = position(&|event| {
            matches!(event, Event::GoalProviderCallReserved { agent_run_id, .. } if *agent_run_id == pair[1])
        })
        .unwrap();
        assert!(
            recorded < reserved,
            "a child call started before the previous one was recorded"
        );
    }
    assert_eq!(
        view.runs[0]
            .agents
            .iter()
            .map(|agent| agent.profile.as_deref())
            .collect::<Vec<_>>(),
        [Some("primary"), Some("child"), Some("spare"), Some("child")]
    );
    assert!(view.runs[0].provider_fallbacks.is_empty());
}

fn researcher_bindings(
    primary: &Arc<ScriptedProvider>,
    spare: Vec<Arc<dyn Provider>>,
) -> arany::AdmittedProviders {
    let mut bindings = routes(primary);
    pin_roster(&mut bindings, Some(spare));
    bindings
}

fn read_facts() -> ProviderOutcome {
    tool_outcome(
        arany::ToolCall::Read {
            path: "notes/facts.txt".into(),
            offset: 0,
            limit: 4096,
        },
        None,
    )
}

#[tokio::test]
async fn explicit_resume_continues_the_read_only_researcher_only_with_the_same_preset() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = arany::test_support::goals::facts_workspace(temp.path(), "facts");
    let state = temp.path().join("state");
    let goal_request = || {
        let mut request = team(&workspace, None, 1);
        request.goal = arany::GoalRequest::new(
            arany::GoalBudget::default(),
            arany::GoalPhase::Implementation,
        );
        request.goal.finish_after_run = false;
        request
    };
    let planner = || {
        route(
            ScriptedProvider::new(vec![delegate(&[("A", Some("spare"))]), finish("done")])
                .with_profile("scripted", "primary"),
        )
    };

    let (started, mut starts) = mpsc::unbounded_channel();
    let primary = planner();
    let first = unavailable("spare-first", 1);
    let held = gated("spare-last", 3, &started);
    let bindings = researcher_bindings(&primary, vec![first.clone(), held.clone()]);
    let engine = super::goals::facts_engine(StateRoot::admit(&state).unwrap(), &state);
    let cancellation = RunCancellation::new();
    let run = run_routed(
        &engine,
        bindings,
        goal_request(),
        RunControls {
            cancellation: cancellation.clone(),
            ..configured()
        },
    );
    let interrupt = async {
        let (objective, _release) = starts.recv().await.unwrap();
        assert_eq!(objective, "A");
        assert!(cancellation.cancel());
    };
    let (cancelled, ()) = tokio::time::timeout(WATCHDOG, async { tokio::join!(run, interrupt) })
        .await
        .unwrap();
    let cancelled = cancelled.unwrap();
    assert_eq!(cancelled.run.status, RunStatus::Cancelled);
    engine.close().await.unwrap();
    let session = cancelled.session_id;
    let goal = cancelled.goal_id;
    let (_, interrupted) = reopen(&state, session).await;
    assert_eq!(interrupted.runs[0].provider_fallbacks.len(), 1);
    assert_eq!(
        interrupted.goal(goal).unwrap().status,
        arany::GoalStatus::Interrupted
    );

    let resume_request = || {
        let mut request = team(&workspace, Some(session), 1);
        request.goal = arany::GoalRequest::resume(goal, true);
        request
    };
    let engine = super::goals::facts_engine(StateRoot::open_existing(&state).unwrap(), &state);
    let primary = planner();
    let first = route(
        ScriptedProvider::new(vec![read_facts(), finish("researched")])
            .with_profile("scripted", "spare-first"),
    );
    let last = answering("spare-last", &["never"]);
    let matching = researcher_bindings(&primary, vec![first.clone(), last.clone()]);
    let edited = answering("spare-edited", &[]);
    let mut unlisted = routes(&primary);
    pin_roster(&mut unlisted, None);
    for (name, changed) in [
        (
            "edited route",
            researcher_bindings(&primary, vec![edited.clone(), last.clone()]),
        ),
        ("dropped candidate", unlisted),
    ] {
        assert!(
            matches!(
                run_routed(&engine, changed, resume_request(), configured()).await,
                Err(EngineError::RosterSelectionMismatch)
            ),
            "{name}"
        );
        assert_eq!(
            [&primary, &first, &last, &edited].map(count),
            [0, 0, 0, 0],
            "{name}"
        );
    }
    let resumed = run_routed(&engine, matching, resume_request(), configured())
        .await
        .unwrap();
    assert_eq!(resumed.run.status, RunStatus::Finished);
    engine.close().await.unwrap();
    assert_eq!([count(&first), count(&last)], [2, 0]);
    assert!(resumed.run.provider_fallbacks.is_empty());
    let (_, view) = reopen(&state, session).await;
    assert_eq!(view.runs.len(), 2);
    let child = &view.runs[1].agents[1];
    assert_eq!(child.profile.as_deref(), Some("spare"));
    assert_ne!(child.id, view.runs[0].agents[1].id);
    assert_eq!(child.grant, Some(arany::ChildGrant::ReadOnly));
    assert_eq!(child.status, AgentStatus::Finished);
    assert_eq!(child.tools.len(), 1);
}

#[tokio::test]
async fn preset_changes_after_a_run_cannot_move_its_pinned_compaction() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let compaction = |model: &str| {
        route(
            ScriptedProvider::new(vec![])
                .with_profile("scripted", model)
                .with_compactions(vec![Ok(CompactionResponse {
                    summary: format!("summary from {model}"),
                    response_id: None,
                    usage: arany::Usage::default(),
                    wire_provenance: None,
                })]),
        )
    };
    let primary = answering("primary", &["done"]);
    let pinned = compaction("compact-pinned");
    let mut bindings = routes(&primary);
    bindings.roles.compaction = vec![pinned.clone()];
    pin_roster(&mut bindings, None);
    let engine = open(&state);
    let result = run_routed(
        &engine,
        bindings.clone(),
        input(&workspace),
        RunControls::default(),
    )
    .await
    .unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);

    let edited = compaction("compact-edited");
    let mut mutated = routes(&primary);
    mutated.roles.compaction = vec![edited.clone()];
    pin_roster(&mut mutated, None);
    assert!(matches!(
        engine
            .compact_session(&mutated, result.session_id, workspace.clone())
            .await,
        Err(EngineError::InvalidRequest)
    ));
    assert_eq!(edited.compact_requests.lock().unwrap().len(), 0);
    assert_eq!(pinned.compact_requests.lock().unwrap().len(), 0);

    let record = engine
        .compact_session(&bindings, result.session_id, workspace)
        .await
        .unwrap();
    assert_eq!(record.model, "compact-pinned");
    assert_eq!(pinned.compact_requests.lock().unwrap().len(), 1);
    assert_eq!(edited.compact_requests.lock().unwrap().len(), 0);
    engine.close().await.unwrap();
    let (_, view) = reopen(&state, result.session_id).await;
    assert_eq!(view.compactions.len(), 1);
}

#[tokio::test]
async fn every_committed_prefix_of_a_roster_team_run_replays_without_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let primary = route(
        ScriptedProvider::new(vec![
            delegate(&[("A", Some("spare")), ("B", None)]),
            finish("done"),
        ])
        .with_profile("scripted", "primary"),
    );
    let first = unavailable("spare-first", 1);
    let last = answering("spare-last", &["spare answer"]);
    let default = answering("default", &["default answer"]);
    let mut bindings = routes(&primary);
    bindings.roles.child = vec![default.clone()];
    pin_roster(&mut bindings, Some(vec![first.clone(), last.clone()]));
    let engine = open(&state);
    let result = run_routed(
        &engine,
        bindings,
        team(&workspace, None, 2),
        RunControls::default(),
    )
    .await
    .unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);
    engine.close().await.unwrap();
    let (events, _) = reopen(&state, result.session_id).await;
    let calls = [&primary, &first, &last, &default].map(count);
    assert_eq!(calls, [2, 1, 1, 1]);
    for end in 1..=events.len() {
        let view = SessionView::replay(result.session_id, &events[..end])
            .unwrap_or_else(|error| panic!("prefix {end} must replay: {error:?}"))
            .unwrap();
        if end < events.len() {
            assert!(
                view.runs
                    .last()
                    .is_none_or(|run| run.status != RunStatus::Active)
            );
        }
    }
}

use super::engine_support::run_routed;
use super::*;
use arany::test_support::routing::*;
use arany::{ProviderRole, Providers};

#[tokio::test]
async fn role_selection_pins_primary_child_and_compaction_and_rejects_inconsistent_provenance() {
    for named in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let primary = route(
            ScriptedProvider::new(vec![
                ProviderOutcome::Delegate(Delegate {
                    agent: None,
                    children: vec![arany::ChildAssignment {
                        objective: "Inspect".into(),
                        profile: None,
                    }],
                }),
                finish("done"),
            ])
            .with_profile("scripted", "primary-model"),
        );
        let child = route(
            ScriptedProvider::new(vec![finish("child report")])
                .with_profile("scripted", "child-model"),
        );
        let compact = route(
            ScriptedProvider::new(vec![])
                .with_profile("scripted", "compact-model")
                .with_compactions(vec![Ok(CompactionResponse {
                    summary: "summary".into(),
                    response_id: None,
                    usage: arany::Usage::default(),
                    wire_provenance: None,
                })]),
        );
        let mut bindings = routes(&primary);
        bindings.roles.child = vec![child.clone()];
        bindings.roles.compaction = vec![compact.clone()];
        let spare = route(ScriptedProvider::new(vec![]).with_profile("scripted", "unused-model"));
        if named {
            pin_roster(&mut bindings, Some(vec![spare.clone()]));
        }
        let engine = open(&state);
        let mut request = input(&workspace);
        request.policy = CollaborationPolicy::Auto {
            max_active_children: 1,
        };
        let result = run_routed(&engine, bindings.clone(), request, RunControls::default())
            .await
            .unwrap();
        assert_eq!(result.run.status, RunStatus::Finished);
        let record = engine
            .compact_session(&bindings, result.session_id, workspace)
            .await
            .unwrap();
        assert_eq!(record.model, "compact-model");
        engine.close().await.unwrap();
        let (events, view) = reopen(&state, result.session_id).await;
        assert_eq!(
            view.runs[0]
                .config
                .as_ref()
                .unwrap()
                .providers
                .as_ref()
                .unwrap()
                .child[0],
            child.identity
        );
        assert_eq!(
            primary
                .requests
                .lock()
                .unwrap()
                .iter()
                .map(|request| (request.phase, request.model.as_str()))
                .collect::<Vec<_>>(),
            [
                (AgentPhase::RootPlan, "primary-model"),
                (AgentPhase::RootSynthesis, "primary-model")
            ]
        );
        assert_eq!(child.requests.lock().unwrap()[0].model, "child-model");
        assert_eq!(
            compact.compact_requests.lock().unwrap()[0].model,
            "compact-model"
        );
        assert!(spare.requests.lock().unwrap().is_empty());
        assert_eq!(
            view.runs[0].config.as_ref().unwrap().roster.is_some(),
            named
        );
        for change in 0..if named { 10 } else { 3 } {
            let mut invalid = events.clone();
            let config = invalid
                .iter_mut()
                .find_map(|event| match &mut event.event {
                    Event::RunStarted { config, .. } => Some(config),
                    _ => None,
                })
                .unwrap();
            let bindings = config.providers.as_mut().unwrap();
            match change {
                0 => bindings.primary[0].model = "different-primary".into(),
                1 => bindings.child[0].profile = "custom:unverified".into(),
                2 => bindings.review.clear(),
                change => {
                    let roster = config.roster.as_mut().unwrap();
                    match change {
                        3 => roster.selection_digest[0] ^= 1,
                        4 => roster.providers.primary = "child".into(),
                        5 => roster.child_profiles.push("missing".into()),
                        6 => roster.profiles.swap(0, 1),
                        7 => roster.profiles[0].routes[0].model = "forged".into(),
                        8 => roster.profiles[0].routes.clear(),
                        _ => roster.profiles[0].name = "../escape".into(),
                    }
                }
            }
            assert!(SessionView::replay(result.session_id, &invalid).is_err());
        }
    }
}

#[tokio::test]
async fn fallback_chain_is_ordered_and_call_and_unknown_token_budgets_stop_it() {
    for budget in [None, Some(1), Some(2)] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let mut primary = ScriptedProvider::new(vec![]).with_profile("scripted", "first");
        primary.responses = Mutex::new(vec![Err(ProviderError::RemoteHttp(503))].into());
        let primary = route(primary);
        let mut second = ScriptedProvider::new(vec![]).with_profile("scripted", "second");
        second.responses = Mutex::new(vec![Err(ProviderError::RemoteHttp(503))].into());
        let second = route(second);
        let third =
            route(ScriptedProvider::new(vec![finish("third")]).with_profile("scripted", "third"));
        let mut bindings = routes(&primary);
        bindings.roles.primary.extend([
            second.clone() as Arc<dyn Provider>,
            third.clone() as Arc<dyn Provider>,
        ]);
        let engine = open(&state);
        let mut request = input(&workspace);
        if budget == Some(1) {
            request.goal.budget.as_mut().unwrap().max_provider_calls = Some(1);
        }
        if budget == Some(2) {
            request.goal.budget.as_mut().unwrap().max_tokens = Some(100);
        }
        let result = run_routed(&engine, bindings, request, RunControls::default())
            .await
            .unwrap();
        engine.close().await.unwrap();
        let (_, view) = reopen(&state, result.session_id).await;
        let allowed = budget.is_none();
        assert_eq!(
            result.run.status,
            if allowed {
                RunStatus::Finished
            } else {
                RunStatus::Failed
            }
        );
        assert_eq!(
            view.runs[0].provider_fallbacks.len(),
            if allowed { 2 } else { 0 }
        );
        assert_eq!(view.goals[0].provider_calls, if allowed { 3 } else { 1 });
        assert_eq!(second.requests.lock().unwrap().len(), usize::from(allowed));
        assert_eq!(third.requests.lock().unwrap().len(), usize::from(allowed));
    }
}

#[tokio::test]
async fn child_and_compaction_fallbacks_keep_role_provenance() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let primary = route(
        ScriptedProvider::new(vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: vec![
                    arany::ChildAssignment {
                        objective: "Inspect".into(),
                        profile: None,
                    },
                    arany::ChildAssignment {
                        objective: "Compare".into(),
                        profile: None,
                    },
                ],
            }),
            finish("done"),
        ])
        .with_profile("scripted", "primary"),
    );
    let mut child = ScriptedProvider::new(vec![]).with_profile("scripted", "child");
    child.responses = Mutex::new(
        vec![
            Err(ProviderError::RemoteHttp(503)),
            Err(ProviderError::RemoteHttp(503)),
        ]
        .into(),
    );
    let child = route(child);
    let fallback = route(
        ScriptedProvider::new(vec![finish("child fallback"), finish("child fallback")])
            .with_profile("scripted", "child-fallback"),
    );
    let compaction = route(
        ScriptedProvider::new(vec![])
            .with_profile("scripted", "compact")
            .with_compactions(vec![Err(ProviderError::RemoteHttp(503))]),
    );
    let compaction_fallback = route(
        ScriptedProvider::new(vec![])
            .with_profile("scripted", "compact-fallback")
            .with_compactions(vec![Ok(CompactionResponse {
                summary: "compacted".into(),
                response_id: None,
                usage: arany::Usage::default(),
                wire_provenance: None,
            })]),
    );
    let mut bindings = routes(&primary);
    bindings.roles.child = vec![child.clone(), fallback.clone()];
    bindings.roles.compaction = vec![compaction.clone(), compaction_fallback.clone()];
    let engine = open(&state);
    let mut request = input(&workspace);
    request.policy = CollaborationPolicy::Auto {
        max_active_children: 2,
    };
    let result = run_routed(&engine, bindings.clone(), request, RunControls::default())
        .await
        .unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);
    let record = engine
        .compact_session(&bindings, result.session_id, workspace)
        .await
        .unwrap();
    assert_eq!(record.model, "compact-fallback");
    engine.close().await.unwrap();
    let (_, view) = reopen(&state, result.session_id).await;
    assert_eq!(
        view.runs[0]
            .provider_fallbacks
            .iter()
            .map(|(_, fallback)| fallback.role)
            .collect::<Vec<_>>(),
        [
            ProviderRole::Child,
            ProviderRole::Child,
            ProviderRole::Compaction
        ]
    );
    assert_eq!(fallback.requests.lock().unwrap()[0].model, "child-fallback");
    assert_eq!(
        compaction_fallback.compact_requests.lock().unwrap()[0].model,
        "compact-fallback"
    );
}

fn goal_request(workspace: &Path) -> RunRequest {
    let mut request = input(workspace);
    request.goal = arany::GoalRequest::new(
        arany::GoalBudget {
            max_provider_calls: Some(4),
            ..arany::GoalBudget::default()
        },
        arany::GoalPhase::Implementation,
    );
    request.goal.finish_after_run = false;
    request
}

fn summary_response() -> Result<CompactionResponse, ProviderError> {
    Ok(CompactionResponse {
        summary: "compacted".into(),
        response_id: None,
        usage: arany::Usage::default(),
        wire_provenance: None,
    })
}

#[tokio::test]
async fn exhausted_compaction_routes_settle_the_reservation_for_the_next_compaction() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let primary =
        route(ScriptedProvider::new(vec![finish("done")]).with_profile("scripted", "primary"));
    let compaction = route(
        ScriptedProvider::new(vec![])
            .with_profile("scripted", "compact")
            .with_compactions(vec![
                Err(ProviderError::RemoteHttp(503)),
                summary_response(),
            ]),
    );
    let compaction_fallback = route(
        ScriptedProvider::new(vec![])
            .with_profile("scripted", "compact-fallback")
            .with_compactions(vec![
                Err(ProviderError::RemoteHttp(503)),
                summary_response(),
            ]),
    );
    let mut bindings = routes(&primary);
    bindings.roles.compaction = vec![compaction.clone(), compaction_fallback.clone()];
    let engine = open(&state);
    let result = run_routed(
        &engine,
        bindings.clone(),
        goal_request(&workspace),
        RunControls::default(),
    )
    .await
    .unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);
    let failed = engine
        .compact_session(&bindings, result.session_id, workspace.clone())
        .await
        .unwrap();
    assert!(matches!(failed.status, CompactionStatus::Failed { .. }));
    let next = engine
        .compact_session(&bindings, result.session_id, workspace)
        .await;
    assert!(
        next.is_ok(),
        "a settled reservation must admit the next compaction"
    );
    engine.close().await.unwrap();
    let (events, view) = reopen(&state, result.session_id).await;
    assert_eq!(view.goals[0].compaction_call_reserved, None);
    let kinds = events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            Event::GoalCompactionCallReserved { .. } => Some("reserved"),
            Event::ProviderFallbackUsed { fallback, .. }
                if fallback.role == ProviderRole::Compaction =>
            {
                Some("fallback")
            }
            Event::ContextCompacted { record, .. } => Some(match record.status {
                CompactionStatus::Failed { .. } => "failed",
                CompactionStatus::Succeeded { .. } => "saved",
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        ["reserved", "fallback", "failed", "reserved", "saved"]
    );
}

#[tokio::test]
async fn refused_fallback_admission_settles_the_known_compaction_failure() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let primary =
        route(ScriptedProvider::new(vec![finish("done")]).with_profile("scripted", "primary"));
    let compaction = route(
        ScriptedProvider::new(vec![])
            .with_profile("scripted", "compact")
            .with_compactions(vec![Err(ProviderError::RemoteHttp(503))]),
    );
    let compaction_fallback = route(
        ScriptedProvider::new(vec![])
            .with_profile("scripted", "compact-fallback")
            .with_compactions(vec![summary_response()]),
    );
    let mut bindings = routes(&primary);
    bindings.roles.compaction = vec![compaction.clone(), compaction_fallback.clone()];
    let engine = open(&state);
    let result = run_routed(
        &engine,
        bindings.clone(),
        goal_request(&workspace),
        RunControls::default(),
    )
    .await
    .unwrap();
    engine.close().await.unwrap();
    // Fill the Session so only the boundary compaction's own admission fits; the fallback's larger
    // admission is then refused by the Store.
    let slots = arany::Action::BoundaryCompaction { goal: true }.max_events_for(&bindings);
    let mut connection = rusqlite::Connection::open(state.join("events.sqlite3")).unwrap();
    let transaction = connection.transaction().unwrap();
    let last: i64 = transaction
        .query_row("SELECT COALESCE(MAX(sequence), 0) FROM events", [], |row| {
            row.get(0)
        })
        .unwrap();
    for sequence in last + 1..=10_000 - i64::try_from(slots).unwrap() {
        transaction
            .execute(
                "INSERT INTO events (sequence,session_id,kind,event_version,payload,created_at_ms) VALUES (?1,?2,'SessionRenamed',6,'{\"title\":\"x\"}',0)",
                rusqlite::params![sequence, result.session_id.to_string()],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    let engine = open(&state);
    let failed = engine
        .compact_session(&bindings, result.session_id, workspace)
        .await
        .unwrap();
    assert!(matches!(
        failed.status,
        CompactionStatus::Failed {
            reason: CompactionFailure::ProviderUnavailable
        }
    ));
    engine.close().await.unwrap();
    // Padding rows are not Actions, so `reopen`'s action-bound check does not apply here.
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    let events = store.load_session(result.session_id).await.unwrap();
    store.close().await.unwrap();
    let view = SessionView::replay(result.session_id, &events)
        .unwrap()
        .unwrap();
    assert_eq!(view.goals[0].compaction_call_reserved, None);
    assert!(
        compaction_fallback
            .compact_requests
            .lock()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn tool_enabled_fallback_between_models_of_one_chatgpt_account_is_admitted() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = arany::test_support::goals::facts_workspace(temp.path(), "v1");
    let state = temp.path().join("state");
    let account = Uuid::now_v7();
    let mut first = ScriptedProvider::new(vec![])
        .with_profile("chatgpt", "model-a")
        .with_chatgpt_account(account, [1; 32]);
    first.responses = Mutex::new(vec![Err(ProviderError::RemoteHttp(503))].into());
    let first = route(first);
    let second = route(
        ScriptedProvider::new(vec![finish("fallback answer")])
            .with_profile("chatgpt", "model-b")
            .with_chatgpt_account(account, [2; 32]),
    );
    let mut bindings = routes(&first);
    bindings.roles.primary = vec![first.clone(), second.clone()];
    let engine = super::goals::facts_engine(StateRoot::admit(&state).unwrap(), &state);
    let result = run_routed(&engine, bindings, input(&workspace), configured())
        .await
        .unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);
    engine.close().await.unwrap();
    let (_, view) = reopen(&state, result.session_id).await;
    assert_eq!(view.runs[0].provider_fallbacks.len(), 1);
}

#[tokio::test]
async fn fallback_after_a_tool_observation_never_redispatches_the_tool() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = arany::test_support::goals::facts_workspace(temp.path(), "facts");
    let state = temp.path().join("state");
    let read = arany::ToolCall::Read {
        path: "notes/facts.txt".into(),
        offset: 0,
        limit: 4096,
    };
    let mut primary = ScriptedProvider::new(vec![]).with_profile("scripted", "primary");
    primary.responses = Mutex::new(
        vec![
            Ok(tool_outcome(read, None)),
            Err(ProviderError::RemoteHttp(503)),
        ]
        .into(),
    );
    let primary = route(primary);
    let fallback =
        route(ScriptedProvider::new(vec![finish("done")]).with_profile("scripted", "fallback"));
    let engine = super::goals::facts_engine(StateRoot::admit(&state).unwrap(), &state);
    let mut bindings = routes(&primary);
    bindings.roles.primary.push(fallback.clone());
    let result = run_routed(&engine, bindings, input(&workspace), configured())
        .await
        .unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);
    engine.close().await.unwrap();
    let (events, view) = reopen(&state, result.session_id).await;
    assert_eq!(view.runs[0].tools.len(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::ToolStarted { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::ToolFinished { .. }))
            .count(),
        1
    );
    let requests = fallback.requests.lock().unwrap();
    let step = &requests[0].input.tools.as_ref().unwrap().steps[0];
    assert_eq!(step.observations.len(), 1);
    assert_eq!(step.requests.len(), 1);
}

#[tokio::test]
async fn tool_enabled_chains_refuse_different_adapters_and_accounts_before_dispatch() {
    for kind in 0..3 {
        for role in [ProviderRole::Primary, ProviderRole::Child] {
            let temp = tempfile::tempdir().unwrap();
            let workspace = arany::test_support::goals::facts_workspace(temp.path(), "facts");
            let primary = route(ScriptedProvider::new(vec![]).with_profile("openai", "model-a"));
            let alternate = route(match kind {
                0 => ScriptedProvider::new(vec![]).with_profile("anthropic", "model-b"),
                1 => ScriptedProvider::new(vec![])
                    .with_profile("openai", "model-b")
                    .with_saved_api_account(Uuid::now_v7()),
                _ => ScriptedProvider::new(vec![]).with_chatgpt_account(Uuid::now_v7(), [4; 32]),
            });
            let mut bindings = routes(&primary);
            bindings.roles.get_mut(role).push(alternate.clone());
            let state = temp.path().join("state");
            let engine = super::goals::facts_engine(StateRoot::admit(&state).unwrap(), &state);
            assert!(matches!(
                run_routed(&engine, bindings, input(&workspace), configured()).await,
                Err(EngineError::IncompatibleProviderChain)
            ));
            assert!(primary.requests.lock().unwrap().is_empty());
            assert!(alternate.requests.lock().unwrap().is_empty());
            engine.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn roster_disagreeing_with_role_chains_is_refused_before_any_call() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let first =
        route(ScriptedProvider::new(vec![finish("done")]).with_profile("scripted", "first"));
    let other = route(ScriptedProvider::new(vec![]).with_profile("scripted", "other"));
    let mut bindings = routes(&first);
    pin_roster(&mut bindings, None);
    bindings.roles.child = vec![other];
    let engine = open(&temp.path().join("state"));
    assert!(matches!(
        run_routed(&engine, bindings, input(&workspace), RunControls::default()).await,
        Err(EngineError::InvalidRequest)
    ));
    assert!(first.requests.lock().unwrap().is_empty());
    engine.close().await.unwrap();
}

#[tokio::test]
async fn crash_after_fallback_charges_open_ceiling_and_resume_starts_at_first_route() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let mut first = ScriptedProvider::new(vec![]).with_profile("scripted", "first");
    first.responses = Mutex::new(vec![Err(ProviderError::RemoteHttp(503))].into());
    let first = route(first);
    let next = route(ScriptedProvider::new(vec![finish("done")]).with_profile("scripted", "next"));
    let mut bindings = routes(&first);
    bindings.roles.primary.push(next.clone());
    pin_roster(&mut bindings, None);
    let engine = open(&state);
    let mut initial = input(&workspace);
    initial.goal = arany::GoalRequest::new(
        arany::GoalBudget {
            max_runs: 2,
            ..Default::default()
        },
        arany::GoalPhase::Implementation,
    );
    let outcome = run_routed(&engine, bindings, initial, RunControls::default())
        .await
        .unwrap();
    engine.close().await.unwrap();
    let (mut events, _) = reopen(&state, outcome.session_id).await;
    let switched = events
        .iter()
        .position(|event| matches!(event.event, Event::ProviderFallbackUsed { .. }))
        .unwrap();
    events.truncate(switched + 1);
    let ceiling = events
        .iter()
        .find_map(|event| match event.event {
            Event::GoalProviderCallReserved {
                active_ceiling_ms, ..
            } => Some(active_ceiling_ms),
            _ => None,
        })
        .unwrap();
    let crashed = SessionView::replay(outcome.session_id, &events)
        .unwrap()
        .unwrap();
    assert_eq!(crashed.goals[0].provider_calls, 2);
    assert_eq!(crashed.goals[0].active_ceiling_ms, ceiling);
    assert_eq!(crashed.goals[0].unobserved_provider_calls.len(), 1);
    assert!(crashed.goals[0].token_usage_unknown);
    assert!(
        crashed.runs[0]
            .agents
            .iter()
            .find(|agent| agent.role == arany::AgentRole::Primary)
            .unwrap()
            .provider_calls
            .is_empty()
    );
    assert_eq!(first.requests.lock().unwrap().len(), 1);
    assert_eq!(next.requests.lock().unwrap().len(), 1);

    let recovered_state = temp.path().join("recovered");
    let store = Store::open(StateRoot::admit(&recovered_state).unwrap()).unwrap();
    for event in &events {
        store
            .append(outcome.session_id, event.event.clone())
            .await
            .unwrap();
    }
    store.close().await.unwrap();
    let first =
        route(ScriptedProvider::new(vec![finish("recovered")]).with_profile("scripted", "first"));
    let next = route(ScriptedProvider::new(vec![]).with_profile("scripted", "next"));
    let mut bindings = routes(&first);
    bindings.roles.primary.push(next.clone());
    pin_roster(&mut bindings, None);
    let engine = engine_support::open(
        StateRoot::open_existing(&recovered_state).unwrap(),
        EngineConfig::default(),
    )
    .unwrap();
    let mut resume = input(&workspace);
    resume.session_id = Some(outcome.session_id);
    resume.title = None;
    resume.goal = arany::GoalRequest::resume(outcome.goal_id, true);
    for omitted in [true, false] {
        let mut changed = bindings.clone();
        if omitted {
            changed.roster = None;
        } else {
            let snapshot = &mut changed.roster.as_mut().unwrap().snapshot;
            snapshot.preset = "changed".into();
            snapshot.selection_digest = snapshot.digest().unwrap();
        }
        let mut attempted_resume = input(&workspace);
        attempted_resume.session_id = Some(outcome.session_id);
        attempted_resume.title = None;
        attempted_resume.goal = arany::GoalRequest::resume(outcome.goal_id, true);
        assert!(matches!(
            run_routed(&engine, changed, attempted_resume, RunControls::default()).await,
            Err(EngineError::RosterSelectionMismatch)
        ));
        assert!(first.requests.lock().unwrap().is_empty());
    }
    let recovered = run_routed(&engine, bindings, resume, RunControls::default())
        .await
        .unwrap();
    assert_eq!(recovered.run.status, RunStatus::Finished);
    assert_eq!(first.requests.lock().unwrap().len(), 1);
    assert!(next.requests.lock().unwrap().is_empty());
    assert!(recovered.run.provider_fallbacks.is_empty());
    engine.close().await.unwrap();
    let (events, view) = reopen(&recovered_state, outcome.session_id).await;
    for omitted in [false, true] {
        let mut forged = events.clone();
        let config = forged
            .iter_mut()
            .rev()
            .find_map(|event| match &mut event.event {
                Event::RunStarted { config, .. } => Some(config),
                _ => None,
            })
            .unwrap();
        if omitted {
            config.roster = None;
        } else {
            let snapshot = config.roster.as_mut().unwrap();
            snapshot.preset = "forged".into();
            snapshot.selection_digest = snapshot.digest().unwrap();
        }
        assert!(SessionView::replay(outcome.session_id, &forged).is_err());
    }
    assert_eq!(view.goals[0].provider_calls, 3);
    assert_eq!(view.goals[0].uncertain_active_ms, ceiling);
}

#[tokio::test]
async fn fallback_only_on_pre_output_call_failure_and_never_after_partial_output() {
    for (error, partial, fallback_expected) in [
        (ProviderError::RemoteHttp(503), false, true),
        (ProviderError::Unavailable, false, false),
        (ProviderError::RemoteHttp(400), false, false),
        (ProviderError::RemoteHttp(401), false, false),
        (ProviderError::RemoteHttp(413), false, false),
        (ProviderError::RemoteHttp(529), false, true),
        (ProviderError::RemoteHttp(429), false, false),
        (ProviderError::PartialTransport, false, false),
        (ProviderError::InvalidStream, false, false),
        (ProviderError::InvalidOutcome, false, false),
        (ProviderError::Rejected, false, false),
        (ProviderError::RemoteHttp(503), true, false),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let mut primary = ScriptedProvider::new(vec![]).with_profile("scripted", "primary-model");
        primary.responses = Mutex::new(vec![Err(error)].into());
        if partial {
            primary.preview = Some(arany::PreviewText {
                answer: "partial".into(),
                ..Default::default()
            });
        }
        let primary = route(primary);
        let fallback = route(
            ScriptedProvider::new(vec![finish("fallback answer")])
                .with_profile("scripted", "fallback-model"),
        );
        let bindings = arany::AdmittedProviders::from_roles(Providers::all(vec![
            primary.clone() as Arc<dyn Provider>,
            fallback.clone() as Arc<dyn Provider>,
        ]));
        let engine = open(&state);
        let result = run_routed(
            &engine,
            bindings,
            input(&workspace),
            RunControls {
                progress: Some(RunHints::with_previews()),
                ..RunControls::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            result.run.status,
            if fallback_expected {
                RunStatus::Finished
            } else {
                RunStatus::Failed
            }
        );
        engine.close().await.unwrap();
        let (events, view) = reopen(&state, result.session_id).await;
        let switches = &view.runs[0].provider_fallbacks;
        assert_eq!(switches.len(), usize::from(fallback_expected));
        assert_eq!(
            fallback.requests.lock().unwrap().len(),
            usize::from(fallback_expected)
        );
        assert_eq!(
            view.goals[0].provider_calls,
            if fallback_expected { 2 } else { 1 }
        );
        if fallback_expected {
            assert_eq!(switches[0].1.role, ProviderRole::Primary);
            assert_eq!(switches[0].1.from, primary.identity);
            assert_eq!(switches[0].1.to, fallback.identity);
            assert_eq!(fallback.requests.lock().unwrap()[0].model, "fallback-model");
            assert!(view.goals[0].token_usage_unknown);
            assert!(
                view.goals[0]
                    .report
                    .as_ref()
                    .unwrap()
                    .unresolved
                    .iter()
                    .any(|item| item.contains("failed call may have consumed usage"))
            );

            for change in 0..3 {
                let mut invalid = events.clone();
                let switch = invalid
                    .iter_mut()
                    .find_map(|event| match &mut event.event {
                        Event::ProviderFallbackUsed { fallback, .. } => Some(fallback),
                        _ => None,
                    })
                    .unwrap();
                match change {
                    0 => switch.to.model = "unadmitted".into(),
                    1 => switch.role = ProviderRole::Child,
                    _ => switch.call_index += 1,
                }
                assert!(SessionView::replay(result.session_id, &invalid).is_err());
            }
        }
    }
}

#[tokio::test]
async fn assignment_profiles_keep_shared_first_routes_distinct_and_finish_in_assignment_order() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let primary = route(
        ScriptedProvider::new(vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: vec![
                    arany::ChildAssignment {
                        objective: "A".into(),
                        profile: None,
                    },
                    arany::ChildAssignment {
                        objective: "B".into(),
                        profile: Some("spare".into()),
                    },
                    arany::ChildAssignment {
                        objective: "C".into(),
                        profile: Some("spare".into()),
                    },
                ],
            }),
            finish("done"),
        ])
        .with_profile("scripted", "primary"),
    );
    let mut first = ScriptedProvider::new(vec![]).with_profile("scripted", "shared-first");
    first.responses = Mutex::new(
        vec![
            Err(ProviderError::RemoteHttp(503)),
            Err(ProviderError::RemoteHttp(503)),
            Err(ProviderError::RemoteHttp(503)),
        ]
        .into(),
    );
    let first = route(first);
    let (started, mut starts) = mpsc::unbounded_channel();
    let fallback = |model: &str, cap| {
        let mut identity = gated_identity(3);
        identity.model = model.into();
        identity.limits.max_output_tokens = cap;
        Arc::new(GatedProvider {
            started: started.clone(),
            requests: Default::default(),
            fail_child: None,
            identity,
        })
    };
    let default = fallback("default-fallback", 8);
    let spare = fallback("spare-fallback", 11);
    let mut bindings = routes(&primary);
    bindings.roles.child = vec![first.clone(), default.clone()];
    pin_roster(&mut bindings, Some(vec![first.clone(), spare.clone()]));
    let engine = open(&state);
    let mut request = input(&workspace);
    request.policy = CollaborationPolicy::Team {
        max_active_children: 3,
    };
    let mut events = engine.subscribe();
    let run = run_routed(&engine, bindings, request, RunControls::default());
    let release = async {
        let mut gates = HashMap::new();
        let mut children = HashMap::new();
        for _ in 0..3 {
            let (objective, release) = starts.recv().await.unwrap();
            assert!(gates.insert(objective, release).is_none());
        }
        for objective in ["C", "B", "A"] {
            gates.remove(objective).unwrap().send(()).unwrap();
            loop {
                let Some(arany::Tapped::Committed(event)) = events.next().await else {
                    panic!("committed child completion");
                };
                match &event.event {
                    Event::AgentSpawned {
                        agent_run_id,
                        objective: Some(objective),
                        ..
                    } => {
                        children.insert(*agent_run_id, objective.clone());
                    }
                    Event::AgentFinished { agent_run_id, .. }
                        if children.get(agent_run_id).map(String::as_str) == Some(objective) =>
                    {
                        break;
                    }
                    _ => {}
                }
            }
        }
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(run, release)
    })
    .await
    .unwrap();
    let result = result.unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);
    engine.close().await.unwrap();
    let (events, view) = reopen(&state, result.session_id).await;
    let run = &view.runs[0];
    assert_eq!(
        run.agents
            .iter()
            .map(|agent| agent.profile.as_deref())
            .collect::<Vec<_>>(),
        [Some("primary"), Some("child"), Some("spare"), Some("spare")]
    );
    let finished: Vec<_> = events
        .iter()
        .filter_map(|event| match event.event {
            Event::AgentFinished { agent_run_id, .. } => run
                .agents
                .iter()
                .find(|agent| agent.id == agent_run_id)?
                .objective
                .as_deref(),
            _ => None,
        })
        .collect();
    assert_eq!(finished, ["C", "B", "A"]);
    let primary_calls = primary.requests.lock().unwrap();
    assert_eq!(primary_calls.len(), 2);
    assert!(
        primary_calls
            .iter()
            .all(|call| call.input.assignment_profiles == ["child", "spare"])
    );
    assert_eq!(
        primary_calls[1]
            .input
            .child_results
            .iter()
            .map(|child| child.objective.as_str())
            .collect::<Vec<_>>(),
        ["A", "B", "C"]
    );
    let initial = first.requests.lock().unwrap();
    assert_eq!(initial.len(), 3);
    assert!(initial.iter().all(|call| call.model == "shared-first"
        && call.phase == AgentPhase::ChildWork
        && call.input.assignment_profiles.is_empty()));
    for (provider, count, cap, objectives) in
        [(&default, 1, 8, vec!["A"]), (&spare, 2, 11, vec!["B", "C"])]
    {
        let calls = provider.requests.lock().unwrap();
        assert_eq!(calls.len(), count);
        assert!(
            calls
                .iter()
                .all(|call| call.max_output_tokens == cap
                    && call.input.assignment_profiles.is_empty())
        );
        let mut actual: Vec<_> = calls
            .iter()
            .map(|call| call.input.objective.as_str())
            .collect();
        actual.sort();
        assert_eq!(actual, objectives);
    }
    assert_eq!(run.provider_fallbacks.len(), 3);
    for change in 0..7 {
        let mut invalid = events.clone();
        let child_id = run.agents[1].id;
        match change {
            0..=3 => {
                let profile = invalid
                    .iter_mut()
                    .find_map(|event| match &mut event.event {
                        Event::AgentSpawned {
                            agent_run_id,
                            profile,
                            ..
                        } if *agent_run_id
                            == if change == 3 {
                                run.agents[0].id
                            } else {
                                child_id
                            } =>
                        {
                            Some(profile)
                        }
                        _ => None,
                    })
                    .unwrap();
                *profile = match change {
                    0 => None,
                    1 => Some("unknown".into()),
                    2 => Some("review".into()),
                    _ => Some("child".into()),
                };
            }
            4 => {
                let switch = invalid
                    .iter_mut()
                    .find_map(|event| match &mut event.event {
                        Event::ProviderFallbackUsed {
                            agent_run_id,
                            fallback,
                            ..
                        } if *agent_run_id == child_id => Some(fallback),
                        _ => None,
                    })
                    .unwrap();
                switch.to = spare.identity.clone();
            }
            5 => {
                let call = invalid
                    .iter_mut()
                    .find_map(|event| match &mut event.event {
                        Event::ProviderCallRecorded {
                            agent_run_id,
                            record,
                            ..
                        } if *agent_run_id == child_id => Some(record),
                        _ => None,
                    })
                    .unwrap();
                call.output_tokens = Some(9);
            }
            _ => {
                let spawn = invalid.iter().position(|event| matches!(event.event, Event::AgentSpawned { agent_run_id, .. } if agent_run_id == child_id)).unwrap();
                let reserve = invalid.iter().position(|event| matches!(event.event, Event::GoalProviderCallReserved { agent_run_id, .. } if agent_run_id == child_id)).unwrap();
                invalid.swap(spawn, reserve);
                for (index, event) in invalid.iter_mut().enumerate() {
                    event.sequence = u64::try_from(index + 1).unwrap();
                }
            }
        }
        assert!(
            SessionView::replay(result.session_id, &invalid).is_err(),
            "tampering {change}"
        );
    }
}

#[tokio::test]
async fn invalid_last_assignment_rejects_the_whole_plan_without_spawns_or_repair() {
    for named in [false, true] {
        for bad in [
            arany::ChildAssignment {
                objective: "Inspect".into(),
                profile: Some("unknown".into()),
            },
            arany::ChildAssignment {
                objective: "Inspect".into(),
                profile: Some("review".into()),
            },
            arany::ChildAssignment {
                objective: "Inspect".into(),
                profile: Some("../escape".into()),
            },
            arany::ChildAssignment {
                objective: " ".into(),
                profile: None,
            },
            arany::ChildAssignment {
                objective: "x".repeat(2049),
                profile: None,
            },
        ] {
            let temp = tempfile::tempdir().unwrap();
            let workspace = temp.path().join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let state = temp.path().join("state");
            let primary = route(ScriptedProvider::new(vec![ProviderOutcome::Delegate(
                Delegate {
                    agent: None,
                    children: vec![
                        arany::ChildAssignment {
                            objective: "Valid first".into(),
                            profile: None,
                        },
                        bad,
                    ],
                },
            )]));
            let child = route(ScriptedProvider::new(vec![finish("forbidden dispatch")]));
            let mut bindings = routes(&primary);
            bindings.roles.child = vec![child.clone()];
            if named {
                pin_roster(&mut bindings, None);
            }
            let engine = open(&state);
            let mut request = input(&workspace);
            request.policy = CollaborationPolicy::Team {
                max_active_children: 2,
            };
            let result = run_routed(&engine, bindings, request, RunControls::default())
                .await
                .unwrap();
            assert_eq!(result.run.status, RunStatus::Failed);
            engine.close().await.unwrap();
            let (events, view) = reopen(&state, result.session_id).await;
            assert_eq!(view.runs[0].agents.len(), 1);
            assert_eq!(primary.requests.lock().unwrap().len(), 1);
            assert!(child.requests.lock().unwrap().is_empty());
            assert!(!events.iter().any(|event| matches!(
                event.event,
                Event::AgentSpawned {
                    role: arany::AgentRole::Child,
                    ..
                }
            )));
            assert_eq!(
                view.runs[0].agents[0].provider_calls[0].disposition,
                ProviderCallDisposition::InvalidResponse
            );
        }
    }
}

struct PreviewGate {
    identity: arany::ProviderIdentity,
    failing: bool,
}

impl PreviewGate {
    fn route(model: &str, failing: bool) -> Arc<dyn Provider> {
        Arc::new(Self {
            identity: arany::ProviderIdentity::new("gate", model, arany::Provenance::native()),
            failing,
        })
    }

    fn done() -> Result<arany::ProviderResponse, ProviderError> {
        Ok(arany::ProviderResponse {
            turn: arany::ProviderTurn::scripted(),
            outcome: finish("child done"),
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: Some(8),
                ..Default::default()
            },
            wire_provenance: None,
        })
    }
}

#[async_trait::async_trait]
impl Provider for PreviewGate {
    fn identity(&self) -> &arany::ProviderIdentity {
        &self.identity
    }

    async fn invoke(
        &self,
        _: arany::ProviderRequest,
    ) -> Result<arany::ProviderResponse, ProviderError> {
        if self.failing {
            return Err(ProviderError::RemoteHttp(503));
        }
        Self::done()
    }

    async fn invoke_with_preview(
        &self,
        _: arany::ProviderRequest,
        preview: arany::ResponsePreview,
    ) -> Result<arany::ProviderResponse, ProviderError> {
        assert!(
            self.failing,
            "a single-route chain must not stream previews"
        );
        preview.publish(arany::PreviewText {
            answer: "partial".into(),
            ..Default::default()
        })?;
        Err(ProviderError::RemoteHttp(503))
    }
}

#[tokio::test]
async fn child_preview_follows_the_assigned_chain_not_the_default_child_chain() {
    for assigned_routes in [1, 2] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let primary = route(
            ScriptedProvider::new(vec![
                ProviderOutcome::Delegate(Delegate {
                    agent: None,
                    children: vec![arany::ChildAssignment {
                        objective: "Inspect".into(),
                        profile: Some("spare".into()),
                    }],
                }),
                finish("done"),
            ])
            .with_profile("scripted", "primary"),
        );
        let fallback = route(
            ScriptedProvider::new(vec![finish("fallback")]).with_profile("scripted", "fallback"),
        );
        let assigned = if assigned_routes == 1 {
            vec![PreviewGate::route("single", false)]
        } else {
            vec![PreviewGate::route("first", true), fallback.clone()]
        };
        let mut bindings = routes(&primary);
        pin_roster(&mut bindings, Some(assigned));
        let engine = open(&temp.path().join("state"));
        let mut request = input(&workspace);
        request.policy = CollaborationPolicy::Team {
            max_active_children: 1,
        };
        let result = run_routed(&engine, bindings, request, RunControls::default())
            .await
            .unwrap();
        if assigned_routes == 1 {
            assert_eq!(result.run.status, RunStatus::Finished);
        } else {
            assert_eq!(result.run.status, RunStatus::Failed);
            assert!(result.run.provider_fallbacks.is_empty());
            assert!(fallback.requests.lock().unwrap().is_empty());
        }
        engine.close().await.unwrap();
    }
}

#[tokio::test]
async fn assigned_fallback_stays_with_its_researcher_while_the_sibling_starts_at_zero() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = arany::test_support::goals::facts_workspace(temp.path(), "facts");
    let state = temp.path().join("state");
    let mut primary = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![
                arany::ChildAssignment {
                    objective: "A".into(),
                    profile: None,
                },
                arany::ChildAssignment {
                    objective: "B".into(),
                    profile: Some("spare".into()),
                },
            ],
        }),
        finish("done"),
    ])
    .with_profile("scripted", "primary");
    primary.identity.limits.concurrency = 1;
    let primary = route(primary);
    let mut first = ScriptedProvider::new(vec![]).with_profile("scripted", "shared-first");
    first.responses = Mutex::new(
        vec![
            Err(ProviderError::RemoteHttp(503)),
            Err(ProviderError::RemoteHttp(503)),
        ]
        .into(),
    );
    let first = route(first);
    let fallback = |model| {
        route(
            ScriptedProvider::new(vec![
                tool_outcome(
                    arany::ToolCall::Read {
                        path: "notes/facts.txt".into(),
                        offset: 0,
                        limit: 4096,
                    },
                    None,
                ),
                finish("read facts"),
            ])
            .with_profile("scripted", model),
        )
    };
    let default = fallback("default-fallback");
    let spare = fallback("spare-fallback");
    let mut bindings = routes(&primary);
    bindings.roles.child = vec![first.clone(), default.clone()];
    pin_roster(&mut bindings, Some(vec![first.clone(), spare.clone()]));
    let engine = super::goals::facts_engine(StateRoot::admit(&state).unwrap(), &state);
    let mut request = input(&workspace);
    request.policy = CollaborationPolicy::Team {
        max_active_children: 2,
    };
    let result = run_routed(&engine, bindings, request, configured())
        .await
        .unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);
    engine.close().await.unwrap();
    let (_, view) = reopen(&state, result.session_id).await;
    assert_eq!(first.requests.lock().unwrap().len(), 2);
    for provider in [&default, &spare] {
        let calls = provider.requests.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].agent_run_id, calls[1].agent_run_id);
        assert!(
            calls
                .iter()
                .all(|call| call.model == provider.identity.model
                    && call.collaboration == CollaborationPolicy::Single
                    && call.input.assignment_profiles.is_empty()
                    && call.input.goal_context_facts.is_none())
        );
        assert_eq!(
            calls[1].input.tools.as_ref().unwrap().observations().len(),
            1
        );
    }
    assert_eq!(view.runs[0].provider_fallbacks.len(), 2);
    for child in view.runs[0].agents.iter().skip(1) {
        assert_eq!(child.grant, Some(arany::ChildGrant::ReadOnly));
        assert_eq!(child.tools.len(), 1);
        assert_eq!(
            child.tools[0].observation.as_ref().unwrap().disposition,
            arany::ToolDisposition::Succeeded
        );
    }
}

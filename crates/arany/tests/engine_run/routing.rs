use super::engine_support::run_routed;
use super::*;
use arany::test_support::routing::*;
use arany::{MAX_PROVIDER_RETRIES, ProviderRole, Providers};

// One failed attempt per call plus its three same-route retries.
fn unavailable_attempts(calls: usize) -> Mutex<VecDeque<Result<ProviderOutcome, ProviderError>>> {
    Mutex::new(
        (0..calls * (usize::from(MAX_PROVIDER_RETRIES) + 1))
            .map(|_| Err(ProviderError::RemoteHttp(503)))
            .collect(),
    )
}

#[tokio::test]
async fn role_selection_pins_primary_child_and_compaction_and_rejects_inconsistent_provenance() {
    for named in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let primary = route(
            ScriptedProvider::new(vec![delegate(&[("Inspect", None)]), finish("done")])
                .with_profile("openai", "primary-model"),
        );
        let child = route(
            ScriptedProvider::new(vec![finish("child report")])
                .with_profile("openai", "child-model"),
        );
        let compact = route(
            ScriptedProvider::new(vec![])
                .with_profile("openai", "compact-model")
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
        let spare = route(ScriptedProvider::new(vec![]).with_profile("openai", "unused-model"));
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
        let mut models: Vec<_> = view
            .usage
            .groups
            .iter()
            .map(|group| group.model.as_str())
            .collect();
        models.sort_unstable();
        assert_eq!(models, ["child-model", "compact-model", "primary-model"]);
        assert!(view.usage.pending.is_empty());
        assert_eq!(
            view.usage
                .groups
                .iter()
                .find(|group| group.model == "compact-model")
                .unwrap()
                .input
                .unreported,
            1
        );

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
        let pinned = view.runs[0]
            .config
            .as_ref()
            .unwrap()
            .providers
            .as_ref()
            .unwrap();
        assert_eq!(pinned.primary[0], primary.identity);
        assert_eq!(
            pinned.review, pinned.primary,
            "review inherits the whole admitted primary chain"
        );
        assert_eq!(pinned.compaction[0], compact.identity);
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

#[tokio::test(start_paused = true)]
async fn fallback_chain_is_ordered_and_call_and_unknown_token_budgets_stop_it() {
    for budget in [None, Some(1), Some(2)] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let mut primary = ScriptedProvider::new(vec![]).with_profile("openai", "first");
        primary.responses = unavailable_attempts(1);
        let primary = route(primary);
        let mut second = ScriptedProvider::new(vec![]).with_profile("openai", "second");
        second.responses = Mutex::new(vec![Err(ProviderError::RemoteHttp(503))].into());
        let second = route(second);
        let third =
            route(ScriptedProvider::new(vec![finish("third")]).with_profile("openai", "third"));
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
        // Retries come first and count once per semantic call, so the second route switches at once.
        assert_eq!(
            view.runs[0].provider_retries.len(),
            if allowed { 3 } else { 0 }
        );
        assert_eq!(view.goals[0].provider_calls, if allowed { 6 } else { 1 });
        assert_eq!(
            primary.requests.lock().unwrap().len(),
            if allowed { 4 } else { 1 }
        );
        assert_eq!(second.requests.lock().unwrap().len(), usize::from(allowed));
        assert_eq!(third.requests.lock().unwrap().len(), usize::from(allowed));
    }
}

#[tokio::test(start_paused = true)]
async fn child_and_compaction_fallbacks_keep_role_provenance() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let primary = route(
        ScriptedProvider::new(vec![
            delegate(&[("Inspect", None), ("Compare", None)]),
            finish("done"),
        ])
        .with_profile("openai", "primary"),
    );
    let mut child = ScriptedProvider::new(vec![]).with_profile("openai", "child");
    child.responses = unavailable_attempts(2);
    let child = route(child);
    let fallback = route(
        ScriptedProvider::new(vec![finish("child fallback"), finish("child fallback")])
            .with_profile("openai", "child-fallback"),
    );
    let compaction = route(
        ScriptedProvider::new(vec![])
            .with_profile("openai", "compact")
            .with_compactions(
                (0..=MAX_PROVIDER_RETRIES)
                    .map(|_| Err(ProviderError::RemoteHttp(503)))
                    .collect(),
            ),
    );
    let compaction_fallback = route(
        ScriptedProvider::new(vec![])
            .with_profile("openai", "compact-fallback")
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
    // Each child and the between-Run compaction retry their own call before switching.
    let roles: Vec<_> = view.runs[0]
        .provider_retries
        .iter()
        .map(|(_, retry)| retry.role)
        .collect();
    assert_eq!(roles.len(), 9);
    assert_eq!(
        roles
            .iter()
            .filter(|role| **role == ProviderRole::Compaction)
            .count(),
        3
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
            // Room for each compaction's three same-route retries and its switch.
            max_provider_calls: Some(12),
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

#[tokio::test(start_paused = true)]
async fn exhausted_compaction_routes_settle_the_reservation_for_the_next_compaction() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let primary =
        route(ScriptedProvider::new(vec![finish("done")]).with_profile("openai", "primary"));
    let compaction = route(
        ScriptedProvider::new(vec![])
            .with_profile("openai", "compact")
            .with_compactions(
                (0..=MAX_PROVIDER_RETRIES)
                    .map(|_| Err(ProviderError::RemoteHttp(503)))
                    .chain([summary_response()])
                    .collect(),
            ),
    );
    let compaction_fallback = route(
        ScriptedProvider::new(vec![])
            .with_profile("openai", "compact-fallback")
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
        route(ScriptedProvider::new(vec![finish("done")]).with_profile("openai", "primary"));
    let compaction = route(
        ScriptedProvider::new(vec![])
            .with_profile("openai", "compact")
            .with_compactions(vec![Err(ProviderError::RemoteHttp(503))]),
    );
    let compaction_fallback = route(
        ScriptedProvider::new(vec![])
            .with_profile("openai", "compact-fallback")
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
                "INSERT INTO events (sequence,session_id,kind,event_version,payload,created_at_ms) VALUES (?1,?2,'SessionRenamed',?3,'{\"title\":\"x\"}',0)",
                rusqlite::params![sequence, result.session_id.to_string(), arany::test_support::payload_version("SessionRenamed")],
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
    // The clock pauses for the backoff only; no Tool runs while it is paused.
    let mut first = ScriptedProvider::new(vec![])
        .with_profile("chatgpt", "model-a")
        .with_chatgpt_account(account, [1; 32])
        .with_before_call(0, tokio::time::pause);
    first.responses = unavailable_attempts(1);
    first
        .rate_limits
        .get_mut()
        .unwrap()
        .extend((0..=MAX_PROVIDER_RETRIES).map(|index| Some(plan_sample(1000 + i64::from(index)))));
    let first = route(first);
    let second = route(
        ScriptedProvider::new(vec![finish("fallback answer")])
            .with_profile("chatgpt", "model-b")
            .with_chatgpt_account(account, [2; 32])
            .with_before_call(0, tokio::time::resume),
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
    assert_eq!(
        view.runs[0].provider_fallbacks[0].1.rate_limits,
        Some(plan_sample(1003))
    );
    assert!(
        view.runs[0].agents[0].provider_calls[0]
            .rate_limits
            .is_none()
    );
}

#[tokio::test]
async fn fallback_after_a_tool_observation_never_redispatches_the_tool() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = arany::test_support::goals::facts_workspace(temp.path(), "facts");
    let state = temp.path().join("state");
    let read = arany::ToolCall::Read {
        path: "notes/facts.txt".into(),
        offset: None,
        limit: None,
    };
    // The clock pauses after the Tool result, for the backoff only.
    let mut primary = ScriptedProvider::new(vec![])
        .with_profile("openai", "primary")
        .with_before_call(1, tokio::time::pause);
    primary.responses = unavailable_attempts(1);
    primary
        .responses
        .get_mut()
        .unwrap()
        .push_front(Ok(tool_outcome(read)));
    let primary = route(primary);
    let fallback = route(
        ScriptedProvider::new(vec![finish("done")])
            .with_profile("openai", "fallback")
            .with_before_call(0, tokio::time::resume),
    );
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
async fn custom_routes_receive_the_same_workspace_tools_as_native_routes() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = arany::test_support::goals::facts_workspace(temp.path(), "facts");
    let state = temp.path().join("state");
    let read = arany::ToolCall::Read {
        path: "notes/facts.txt".into(),
        offset: None,
        limit: None,
    };
    let mut primary = ScriptedProvider::new(vec![tool_outcome(read), finish("done")])
        .with_profile("openai", "model-a");
    primary.identity.profile = "custom:messages".into();
    primary.identity.provenance = arany::Provenance::Custom(arany::CustomProfileProvenance {
        endpoint: "https://example.com/anthropic/v1/messages".into(),
        evidence_digest: [7; 32],
        capability_evidence_version: 2,
    });
    primary.identity.limits.concurrency = 1;
    let primary = route(primary);
    let engine = super::goals::facts_engine(StateRoot::admit(&state).unwrap(), &state);
    let result = run_routed(&engine, routes(&primary), input(&workspace), configured())
        .await
        .unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);
    engine.close().await.unwrap();
    let (events, view) = reopen(&state, result.session_id).await;
    assert!(
        view.runs[0]
            .config
            .as_ref()
            .unwrap()
            .custom_profile_provenance
            .is_some()
    );
    assert_eq!(view.runs[0].tools.len(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::ToolFinished { .. }))
            .count(),
        1
    );
    let requests = primary.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|request| request.input.role == arany::PromptRole::Primary)
    );
    assert_eq!(
        requests[1].input.tools.as_ref().unwrap().steps[0]
            .observations
            .len(),
        1
    );
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
async fn invalid_custom_names_refuse_primary_fallback_and_roster_admission() {
    let oversized = "a".repeat(arany::MAX_CUSTOM_PROFILE_NAME_BYTES + 1);
    for name in ["", "Upper", "bad/name", oversized.as_str()] {
        for placement in 0..3 {
            let temp = tempfile::tempdir().unwrap();
            let workspace = temp.path().join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let primary = route(ScriptedProvider::new(vec![finish("unexpected call")]));
            let mut invalid = ScriptedProvider::new(vec![finish("unexpected call")])
                .with_profile(&arany::ProviderKind::custom_profile(name), "model-a");
            invalid.identity.provenance =
                arany::Provenance::Custom(arany::CustomProfileProvenance {
                    endpoint: "https://example.com/responses".into(),
                    evidence_digest: [7; 32],
                    capability_evidence_version: 2,
                });
            let invalid = route(invalid);
            let mut bindings = routes(&primary);
            match placement {
                0 => bindings = routes(&invalid),
                1 => bindings.roles.primary.push(invalid.clone()),
                _ => {
                    pin_roster(&mut bindings, Some(vec![primary.clone()]));
                    let roster = bindings.roster.as_mut().unwrap();
                    roster.profiles.last_mut().unwrap().1 = vec![invalid.clone()];
                    roster.snapshot.profiles.last_mut().unwrap().routes =
                        vec![invalid.identity.clone()];
                    roster.snapshot.selection_digest = roster.snapshot.digest().unwrap();
                }
            }
            let engine = open(&temp.path().join("state"));
            assert!(
                matches!(
                    engine.submit(bindings, input(&workspace), RunControls::default()),
                    Err(EngineError::InvalidRequest)
                ),
                "name {name:?}, placement {placement}"
            );
            assert!(primary.requests.lock().unwrap().is_empty());
            assert!(invalid.requests.lock().unwrap().is_empty());
            engine.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn roster_disagreeing_with_role_chains_is_refused_before_any_call() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let first = route(ScriptedProvider::new(vec![finish("done")]).with_profile("openai", "first"));
    let other = route(ScriptedProvider::new(vec![]).with_profile("openai", "other"));
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

#[tokio::test(start_paused = true)]
async fn crash_after_fallback_charges_open_ceiling_and_resume_starts_at_first_route() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let mut first = ScriptedProvider::new(vec![]).with_profile("openai", "first");
    first.responses = unavailable_attempts(1);
    let first = route(first);
    let next = route(ScriptedProvider::new(vec![finish("done")]).with_profile("openai", "next"));
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
    // The reservation, three retries and the switch.
    assert_eq!(crashed.goals[0].provider_calls, 5);
    assert_eq!(crashed.runs[0].provider_retries.len(), 3);
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
    assert_eq!(first.requests.lock().unwrap().len(), 4);
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
        route(ScriptedProvider::new(vec![finish("recovered")]).with_profile("openai", "first"));
    let next = route(ScriptedProvider::new(vec![]).with_profile("openai", "next"));
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
    assert_eq!(view.goals[0].provider_calls, 6);
    assert_eq!(view.goals[0].uncertain_active_ms, ceiling);
}

#[tokio::test(start_paused = true)]
async fn fallback_only_on_pre_output_call_failure_and_never_after_partial_output() {
    // (failure, visible partial output, same-route retries, fallback)
    type Case = (fn() -> ProviderError, bool, u8, bool);
    let cases: [Case; 14] = [
        (|| ProviderError::RemoteHttp(503), false, 3, true),
        (|| ProviderError::Unavailable, false, 0, false),
        (|| ProviderError::Transport, false, 3, false),
        (|| ProviderError::StreamEnded, false, 3, false),
        (|| ProviderError::RemoteHttp(400), false, 0, false),
        (|| ProviderError::RemoteHttp(401), false, 0, false),
        (|| ProviderError::RemoteHttp(413), false, 0, false),
        (|| ProviderError::RemoteHttp(529), false, 3, true),
        (|| ProviderError::RemoteHttp(429), false, 3, false),
        (|| ProviderError::PartialTransport, false, 3, false),
        (|| ProviderError::InvalidStream, false, 0, false),
        (|| ProviderError::InvalidOutcome, false, 0, false),
        (|| ProviderError::Rejected, false, 0, false),
        (|| ProviderError::RemoteHttp(503), true, 3, false),
    ];
    for (error, partial, retries, fallback_expected) in cases {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let mut primary = ScriptedProvider::new(vec![]).with_profile("openai", "primary-model");
        primary.responses = Mutex::new((0..=retries).map(|_| Err(error())).collect());
        if partial {
            primary.preview = Some(arany::PreviewText {
                answer: "partial".into(),
                ..Default::default()
            });
        }
        let primary = route(primary);
        let fallback = route(
            ScriptedProvider::new(vec![finish("fallback answer")])
                .with_profile("openai", "fallback-model"),
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
                hints: Some(RunHints::with_previews()),
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
        // A provisional preview is cleared before each retry, so visible output blocks only the switch.
        assert_eq!(
            view.runs[0]
                .provider_retries
                .iter()
                .map(|(_, retry)| retry.attempt)
                .collect::<Vec<_>>(),
            (1..=retries).collect::<Vec<_>>()
        );
        assert_eq!(
            primary.requests.lock().unwrap().len(),
            usize::from(retries) + 1
        );
        assert_eq!(
            view.goals[0].provider_calls,
            1 + u32::from(retries) + u32::from(fallback_expected)
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
            delegate(&[("A", None), ("B", Some("spare")), ("C", Some("spare"))]),
            finish("done"),
        ])
        .with_profile("openai", "primary"),
    );
    let mut first = ScriptedProvider::new(vec![]).with_profile("openai", "shared-first");
    first.responses = unavailable_attempts(3);
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
    let session = engine
        .create_session(workspace.clone(), Some("Assignments".into()))
        .await
        .unwrap();
    // The journaled retry delays stay; only the waits end at once.
    arany::FaultPlan::skip_retry_backoff(session);
    let mut request = input(&workspace);
    request.session_id = Some(session);
    request.title = None;
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
    assert_eq!(initial.len(), 12);
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
                editor: None,
                objective: "Inspect".into(),
                profile: Some("unknown".into()),
            },
            arany::ChildAssignment {
                editor: None,
                objective: "Inspect".into(),
                profile: Some("review".into()),
            },
            arany::ChildAssignment {
                editor: None,
                objective: "Inspect".into(),
                profile: Some("../escape".into()),
            },
            arany::ChildAssignment {
                editor: None,
                objective: " ".into(),
                profile: None,
            },
            arany::ChildAssignment {
                editor: None,
                objective: "x".repeat(2049),
                profile: None,
            },
        ] {
            let temp = tempfile::tempdir().unwrap();
            let workspace = temp.path().join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let state = temp.path().join("state");
            let primary = route(ScriptedProvider::new(
                (0..3)
                    .map(|_| {
                        ProviderOutcome::Delegate(Delegate {
                            agent: None,
                            children: vec![
                                arany::ChildAssignment {
                                    editor: None,
                                    objective: "Valid first".into(),
                                    profile: None,
                                },
                                bad.clone(),
                            ],
                        })
                    })
                    .collect(),
            ));
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
            assert_eq!(primary.requests.lock().unwrap().len(), 3);
            assert!(child.requests.lock().unwrap().is_empty());
            assert!(!events.iter().any(|event| matches!(
                event.event,
                Event::AgentSpawned {
                    role: arany::AgentRole::Child,
                    ..
                }
            )));
            assert_eq!(view.runs[0].agents[0].provider_calls.len(), 3);
            assert!(view.runs[0].agents[0].provider_calls.iter().all(|call| {
                call.disposition == ProviderCallDisposition::Rejected
                    && call.failure_reason == Some(arany::ProviderFailureReason::ProposalRefused)
                    && call.refusal.as_ref().is_some_and(|refusal| {
                        refusal.cause == arany::ProposalRefusalCause::DelegateInvalid
                    })
            }));
            assert_eq!(view.runs[0].failure, Some(arany::RunFailure::RefusalLimit));
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
            identity: arany::ProviderIdentity::new("openai", model, arany::Provenance::native()),
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

#[tokio::test(start_paused = true)]
async fn child_preview_follows_the_assigned_chain_not_the_default_child_chain() {
    for assigned_routes in [1, 2] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let primary = route(
            ScriptedProvider::new(vec![
                delegate(&[("Inspect", Some("spare"))]),
                finish("done"),
            ])
            .with_profile("openai", "primary"),
        );
        let fallback = route(
            ScriptedProvider::new(vec![finish("fallback")]).with_profile("openai", "fallback"),
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
            // Private visible output never blocks a same-route retry, only the switch after it.
            assert_eq!(
                result.run.provider_retries.len(),
                usize::from(MAX_PROVIDER_RETRIES)
            );
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
        delegate(&[("A", None), ("B", Some("spare"))]),
        finish("done"),
    ])
    .with_profile("openai", "primary");
    primary.identity.limits.concurrency = 1;
    let primary = route(primary);
    // Each researcher's first call and its three same-route retries on the shared first route.
    let mut first = ScriptedProvider::new(vec![]).with_profile("openai", "shared-first");
    first.responses = unavailable_attempts(2);
    let first = route(first);
    let fallback = |model| {
        route(
            ScriptedProvider::new(vec![
                tool_outcome(arany::ToolCall::Read {
                    path: "notes/facts.txt".into(),
                    offset: None,
                    limit: None,
                }),
                finish("read facts"),
            ])
            .with_profile("openai", model),
        )
    };
    let default = fallback("default-fallback");
    let spare = fallback("spare-fallback");
    let mut bindings = routes(&primary);
    bindings.roles.child = vec![first.clone(), default.clone()];
    pin_roster(&mut bindings, Some(vec![first.clone(), spare.clone()]));
    let engine = super::goals::facts_engine(StateRoot::admit(&state).unwrap(), &state);
    let session = engine
        .create_session(workspace.clone(), Some("Assigned".into()))
        .await
        .unwrap();
    // The journaled retry delays stay; only the waits end at once.
    arany::FaultPlan::skip_retry_backoff(session);
    let mut request = input(&workspace);
    request.session_id = Some(session);
    request.title = None;
    request.policy = CollaborationPolicy::Team {
        max_active_children: 2,
    };
    let result = run_routed(&engine, bindings, request, configured())
        .await
        .unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);
    engine.close().await.unwrap();
    let (_, view) = reopen(&state, result.session_id).await;
    // Exactly three retries per researcher's call before its own switch: nothing retries unbounded.
    assert_eq!(first.requests.lock().unwrap().len(), 8);
    assert_eq!(view.runs[0].provider_retries.len(), 6);
    for child in view.runs[0].agents.iter().skip(1) {
        assert_eq!(
            view.runs[0]
                .provider_retries
                .iter()
                .filter(|(actor, _)| *actor == child.id)
                .map(|(_, retry)| retry.attempt)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }
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
        assert_eq!(
            child.grant,
            Some(arany::ChildGrant::ReadOnly {
                unguarded: arany::UnguardedGrant::Denied
            })
        );
        assert_eq!(child.tools.len(), 1);
        assert_eq!(
            child.tools[0].observation.as_ref().unwrap().disposition,
            arany::ToolDisposition::Succeeded
        );
    }
}

#[tokio::test(start_paused = true)]
async fn same_route_retries_are_journaled_charged_and_bounded_by_deadline_and_budget() {
    use arany::{ProviderRetry, RetryCause, RunFailure};
    let hostile = Duration::from_secs(u64::from(u32::MAX));
    // (case, failed attempts, finite token budget, committed retry causes and delays in ms, success)
    type Case = (
        &'static str,
        Vec<ProviderError>,
        bool,
        Vec<(RetryCause, u32)>,
        bool,
    );
    let cases: [Case; 8] = [
        (
            "429 honoring Retry-After, then success",
            vec![ProviderError::RateLimited(Some(Duration::from_secs(5)))],
            false,
            vec![(RetryCause::RateLimited, 5000)],
            true,
        ),
        (
            "custom Messages 429 without Retry-After",
            vec![ProviderError::RateLimited(None)],
            false,
            vec![(RetryCause::RateLimited, 2000)],
            true,
        ),
        (
            "custom Messages 429 exhausts three retries without Retry-After",
            (0..=MAX_PROVIDER_RETRIES)
                .map(|_| ProviderError::RateLimited(None))
                .collect(),
            false,
            vec![
                (RetryCause::RateLimited, 2000),
                (RetryCause::RateLimited, 4000),
                (RetryCause::RateLimited, 8000),
            ],
            false,
        ),
        (
            "custom Messages quota exhaustion is terminal",
            vec![ProviderError::QuotaExceeded],
            false,
            vec![],
            false,
        ),
        (
            "custom Messages 401 is terminal",
            vec![ProviderError::RemoteHttp(401)],
            false,
            vec![],
            false,
        ),
        (
            "hostile Retry-After past the action deadline",
            vec![ProviderError::RateLimited(Some(hostile))],
            false,
            vec![],
            false,
        ),
        (
            "three stream cuts, then failure",
            (0..=MAX_PROVIDER_RETRIES)
                .map(|_| ProviderError::PartialTransport)
                .collect(),
            false,
            vec![
                (RetryCause::StreamCut, 2000),
                (RetryCause::StreamCut, 4000),
                (RetryCause::StreamCut, 8000),
            ],
            false,
        ),
        (
            "finite token budget with unknown usage",
            vec![ProviderError::RemoteHttp(503)],
            true,
            vec![],
            false,
        ),
    ];
    for (case, failures, tokens, retries, success) in cases {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let mut primary =
            ScriptedProvider::new(vec![finish("done")]).with_profile("openai", "primary");
        if case.starts_with("custom Messages") {
            primary.identity.profile = "custom:messages".into();
            primary.identity.provenance =
                arany::Provenance::Custom(arany::CustomProfileProvenance {
                    endpoint: "https://example.com/anthropic/v1/messages".into(),
                    evidence_digest: [7; 32],
                    capability_evidence_version: 2,
                });
            primary.identity.limits.concurrency = 1;
            let journal = state.join("events.sqlite3");
            primary = primary.with_before_call(1, move || {
                let connection = rusqlite::Connection::open_with_flags(
                    &journal,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
                .unwrap();
                let retries: i64 = connection
                    .query_row(
                        "SELECT COUNT(*) FROM events WHERE kind='ProviderRetryScheduled'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(
                    retries, 1,
                    "retry must be committed before its next dispatch"
                );
            });
        }
        let responses = primary.responses.get_mut().unwrap();
        for failure in failures.into_iter().rev() {
            responses.push_front(Err(failure));
        }
        let primary = route(primary);
        let mut request = input(&workspace);
        if tokens {
            request.goal.budget.as_mut().unwrap().max_tokens = Some(1000);
        }
        let engine = open(&state);
        let started = tokio::time::Instant::now();
        let result = run_routed(&engine, routes(&primary), request, RunControls::default())
            .await
            .unwrap();
        let waited = started.elapsed();
        engine.close().await.unwrap();
        let (events, view) = reopen(&state, result.session_id).await;
        let run = &view.runs[0];
        let primary_id = run.agents[0].id;
        if case.starts_with("custom Messages") {
            assert_eq!(
                run.config.as_ref().unwrap().provider.profile(),
                "custom:messages"
            );
            assert_eq!(
                run.config
                    .as_ref()
                    .unwrap()
                    .custom_profile_provenance
                    .as_ref()
                    .unwrap()
                    .endpoint,
                "https://example.com/anthropic/v1/messages"
            );
        }

        assert_eq!(
            run.provider_retries,
            retries
                .iter()
                .zip(1..)
                .map(|(&(cause, delay_ms), attempt)| (
                    primary_id,
                    ProviderRetry {
                        rate_limits: None,
                        role: ProviderRole::Primary,
                        call_index: 0,
                        attempt,
                        cause,
                        delay_ms,
                    }
                ))
                .collect::<Vec<_>>(),
            "{case}"
        );
        // Every retry waits inside the action deadline; a hostile Retry-After is never waited out.
        let delays: u64 = retries.iter().map(|&(_, delay)| u64::from(delay)).sum();
        assert!(
            waited >= Duration::from_millis(delays) && waited < Duration::from_secs(60),
            "{case}: {waited:?}"
        );
        // One charged call per dispatched attempt; replay never re-sends one.
        let dispatched = retries.len() + 1;
        assert_eq!(primary.requests.lock().unwrap().len(), dispatched, "{case}");
        assert_eq!(view.goals[0].provider_calls as usize, dispatched, "{case}");
        assert_eq!(run.agents[0].provider_calls.len(), 1, "{case}");
        let usage = &view.usage.groups[0];
        assert_eq!(usage.input.unreported as usize, retries.len() + 1);
        assert_eq!(
            usage.output.unreported as usize,
            retries.len() + usize::from(!success)
        );
        assert!(view.usage.pending.is_empty());

        if success {
            assert_eq!(result.run.status, RunStatus::Finished, "{case}");
            assert!(view.goals[0].token_usage_unknown);
            let at = |wanted: fn(&Event) -> bool| {
                events
                    .iter()
                    .position(|event| wanted(&event.event))
                    .unwrap()
            };
            let reserved = at(|event| matches!(event, Event::GoalProviderCallReserved { .. }));
            let retried = at(|event| matches!(event, Event::ProviderRetryScheduled { .. }));
            let recorded = at(|event| matches!(event, Event::ProviderCallRecorded { .. }));
            assert!(
                reserved < retried && retried < recorded,
                "fact before re-dispatch"
            );
            let notice = run.provider_retries[0].1.notice();
            assert!(notice.starts_with("Reconnecting\u{2026} 1/3 (primary, rate limited"));
            assert!(
                view.goals[0]
                    .report
                    .as_ref()
                    .unwrap()
                    .unresolved
                    .contains(&notice)
            );
        } else {
            assert_eq!(result.run.status, RunStatus::Failed, "{case}");
            assert_eq!(run.failure, Some(RunFailure::ProviderCall), "{case}");
        }
    }
}

#[tokio::test]
async fn lost_retry_acknowledgement_and_cancellation_in_the_backoff_start_no_further_call() {
    for lost in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let mut primary = ScriptedProvider::new(vec![finish("never")])
            .with_chatgpt_account(Uuid::now_v7(), [1; 32]);
        primary
            .responses
            .get_mut()
            .unwrap()
            .push_front(Err(ProviderError::RemoteHttp(503)));
        primary
            .rate_limits
            .get_mut()
            .unwrap()
            .push_back(Some(plan_sample(1000)));
        let primary = route(primary);
        let engine = open(&state);
        let session = engine
            .create_session(workspace.clone(), Some("Retry".into()))
            .await
            .unwrap();
        let cancellation = RunCancellation::new();
        let mut request = input(&workspace);
        request.session_id = Some(session);
        request.title = None;
        let run = run_routed(
            &engine,
            routes(&primary),
            request,
            RunControls {
                cancellation: cancellation.clone(),
                ..RunControls::default()
            },
        );
        let (entered, release) =
            arany::FaultPlan::gate_goal_append_acknowledgment(session, |event| {
                matches!(event, Event::ProviderRetryScheduled { .. })
            });
        let outcome = {
            tokio::pin!(run);
            tokio::select! {
                result = &mut run => panic!("Run ended before the held retry acknowledgment: {}", result.is_ok()),
                entered = entered => entered.unwrap(),
            }
            if lost {
                drop(release);
            } else {
                assert!(cancellation.cancel());
                release.send(()).unwrap();
            }
            run.await
        };
        let _ = engine.close().await;
        assert_eq!(primary.requests.lock().unwrap().len(), 1, "lost {lost}");
        let (events, view) = reopen(&state, session).await;
        let run = &view.runs[0];
        assert_eq!(run.provider_retries.len(), 1, "lost {lost}");
        assert_eq!(
            run.provider_retries[0].1.rate_limits,
            Some(plan_sample(1000))
        );
        assert!(
            run.agents[0]
                .provider_calls
                .iter()
                .all(|call| call.rate_limits.is_none()),
            "retry drain cannot duplicate a sample"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalProviderCallReserved { .. }))
                .count(),
            1,
            "no reservation after the retry fact"
        );
        assert_eq!(view.goals[0].provider_calls, 2);
        assert!(view.goals[0].token_usage_unknown);
        if lost {
            // A crash right after the fact: the prefix keeps the open reservation and charges it.
            assert!(outcome.is_err());
            assert_eq!(view.goals[0].unobserved_provider_calls, [run.agents[0].id]);
            assert!(run.agents[0].provider_calls.is_empty());
        } else {
            assert_eq!(outcome.unwrap().run.status, RunStatus::Cancelled);
            assert_eq!(
                run.agents[0].provider_calls[0].disposition,
                ProviderCallDisposition::Cancelled
            );
        }
        let cold = arany::resume_session(
            StateRoot::open_existing(&state).unwrap(),
            workspace.clone(),
            session,
        )
        .await
        .unwrap();
        assert_eq!(cold.runs, view.runs, "lost {lost}");
        assert_eq!(
            primary.requests.lock().unwrap().len(),
            1,
            "replay never re-sends"
        );
    }
}

#[tokio::test]
async fn a_pause_in_the_retry_backoff_starts_no_further_reservation_or_dispatch() {
    for child in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let mut failing =
            ScriptedProvider::new(vec![finish("never")]).with_profile("openai", "failing");
        failing
            .responses
            .get_mut()
            .unwrap()
            .push_front(Err(ProviderError::RemoteHttp(503)));
        let failing = route(failing);
        let primary = if child {
            route(
                ScriptedProvider::new(vec![
                    delegate(&[("Inspect", None)]),
                    finish("never synthesized"),
                ])
                .with_profile("openai", "primary"),
            )
        } else {
            failing.clone()
        };
        let mut bindings = routes(&primary);
        let mut request = input(&workspace);
        if child {
            bindings.roles.child = vec![failing.clone()];
            request.policy = CollaborationPolicy::Team {
                max_active_children: 1,
            };
        }
        let engine = open(&state);
        let session = engine
            .create_session(workspace.clone(), Some("Pause".into()))
            .await
            .unwrap();
        request.session_id = Some(session);
        request.title = None;
        let (entered, release) = arany::FaultPlan::gate_retry_backoff(session);
        let steering = RunSteering::new();
        let run = run_routed(
            &engine,
            bindings,
            request,
            RunControls {
                steering: Some(steering.clone()),
                ..RunControls::default()
            },
        );
        let outcome = {
            tokio::pin!(run);
            tokio::select! {
                result = &mut run => panic!("Run ended before its held backoff: {}", result.is_ok()),
                entered = entered => entered.unwrap(),
            }
            assert!(steering.pause());
            run.await.unwrap()
        };
        // The gate stays held: only the pause can end the wait.
        drop(release);
        engine.close().await.unwrap();
        assert_eq!(outcome.run.status, RunStatus::Cancelled, "child {child}");
        let (events, view) = reopen(&state, session).await;
        let run = &view.runs[0];
        let actor = run.agents.last().unwrap();
        assert_eq!(run.provider_retries.len(), 1, "child {child}");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event.event,
                    Event::GoalProviderCallReserved { agent_run_id, .. } if agent_run_id == actor.id
                ))
                .count(),
            1,
            "no reservation after the paused backoff"
        );
        assert_eq!(
            actor.provider_calls.last().unwrap().disposition,
            ProviderCallDisposition::Cancelled
        );
        assert_eq!(
            failing.requests.lock().unwrap().len(),
            1,
            "no dispatch after the paused backoff"
        );
        if child {
            assert_eq!(primary.requests.lock().unwrap().len(), 1, "no synthesis");
        }
    }
}

#[tokio::test]
async fn a_single_route_boundary_compaction_retries_a_transient_failure_then_saves() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let primary =
        route(ScriptedProvider::new(vec![finish("done")]).with_profile("openai", "primary"));
    let compaction = route(
        ScriptedProvider::new(vec![])
            .with_profile("openai", "compact")
            .with_compactions(vec![
                Err(ProviderError::RemoteHttp(503)),
                summary_response(),
            ]),
    );
    let mut bindings = routes(&primary);
    bindings.roles.compaction = vec![compaction.clone()];
    let engine = open(&state);
    let result = run_routed(
        &engine,
        bindings.clone(),
        input(&workspace),
        RunControls::default(),
    )
    .await
    .unwrap();
    // The journaled retry delay stays; only the wait ends at once.
    arany::FaultPlan::skip_retry_backoff(result.session_id);
    let record = engine
        .compact_session(&bindings, result.session_id, workspace)
        .await
        .unwrap();
    assert!(matches!(record.status, CompactionStatus::Succeeded { .. }));
    engine.close().await.unwrap();
    let (_, view) = reopen(&state, result.session_id).await;
    let run = &view.runs[0];
    assert!(run.provider_fallbacks.is_empty());
    assert_eq!(
        run.provider_retries
            .iter()
            .map(|(_, retry)| (retry.role, retry.attempt, retry.cause))
            .collect::<Vec<_>>(),
        [(ProviderRole::Compaction, 1, arany::RetryCause::ServerError)]
    );
    assert_eq!(compaction.compact_requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn api_compaction_observations_reopen_only_after_committed_settlement() {
    for case in ["success", "error", "append failure"] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let response = if case == "error" {
            Err(ProviderError::RemoteHttp(401))
        } else {
            Ok(CompactionResponse {
                summary: "summary".into(),
                response_id: None,
                usage: arany::Usage::default(),
                wire_provenance: None,
            })
        };
        let mut provider = ScriptedProvider::new(vec![finish("done")])
            .with_profile("openai", "model-a")
            .with_saved_api_account(Uuid::now_v7())
            .with_compactions(vec![response]);
        provider
            .compact_rate_limits
            .get_mut()
            .unwrap()
            .push_back(Some(api_sample(1000)));
        let provider = route(provider);
        let engine = open(&state);
        let run = run_to_end(&engine, &provider, input(&workspace))
            .await
            .unwrap();
        if case == "append failure" {
            arany::FaultPlan::fail_goal_append(run.session_id, |event| {
                matches!(event, Event::ContextCompacted { .. })
            });
        }
        let outcome = engine
            .compact_session(&routes(&provider), run.session_id, workspace)
            .await;
        let _ = engine.close().await;
        let (events, view) = reopen(&state, run.session_id).await;
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        assert_eq!(provider.compact_requests.lock().unwrap().len(), 1);
        if case == "append failure" {
            assert!(outcome.is_err());
            assert!(view.compactions.is_empty());
            assert!(view.usage.api_limits.is_empty());
        } else {
            let record = outcome.unwrap();
            assert_eq!(record.rate_limits, Some(api_sample(1000)));
            assert_eq!(
                matches!(record.status, CompactionStatus::Succeeded { .. }),
                case == "success"
            );
            assert!(events.iter().any(|event| matches!(
                &event.event,
                Event::ContextCompacted { record: committed, .. } if committed == &record
            )));
            let group = &view.usage.api_limits[0];
            assert_eq!(group.run_id, run.run.id);
            assert_eq!(group.role, ProviderRole::Compaction);
            assert_eq!(group.identity.profile, provider.identity().profile);
            assert_eq!(group.identity.model, provider.identity().model);
            assert_eq!(group.identity.provenance, provider.identity().provenance);
            assert_eq!(group.rows, api_sample(1000).api.unwrap().rows);
        }
    }
}

#[tokio::test]
async fn plan_observations_settle_each_attempt_and_replay_without_dispatch_or_backfill() {
    for cancelled in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let cancellation = RunCancellation::new();
        let mut primary = ScriptedProvider::new(vec![finish("done")])
            .with_chatgpt_account(Uuid::now_v7(), [1; 32]);
        primary
            .rate_limits
            .get_mut()
            .unwrap()
            .extend([Some(plan_sample(1000)), Some(plan_sample(900))]);
        if cancelled {
            primary.cancel_at_completion = Some((AgentPhase::RootPlan, cancellation.clone()));
        } else {
            primary
                .responses
                .get_mut()
                .unwrap()
                .push_front(Err(ProviderError::RateLimited(None)));
            primary
                .compact_responses
                .get_mut()
                .unwrap()
                .push_back(Err(ProviderError::InvalidOutcome));
            primary
                .compact_rate_limits
                .get_mut()
                .unwrap()
                .push_back(Some(plan_sample(1100)));
        }
        let primary = route(primary);
        let engine = open(&state);
        let session = engine
            .create_session(workspace.clone(), Some("Plan observations".into()))
            .await
            .unwrap();
        arany::FaultPlan::skip_retry_backoff(session);
        let mut request = input(&workspace);
        request.session_id = Some(session);
        request.title = None;
        let result = run_routed(
            &engine,
            routes(&primary),
            request,
            RunControls {
                cancellation,
                ..RunControls::default()
            },
        )
        .await
        .unwrap();
        if !cancelled {
            let record = engine
                .compact_session(&routes(&primary), session, workspace.clone())
                .await
                .unwrap();
            assert_eq!(record.rate_limits, Some(plan_sample(1100)));
        }
        let fork = engine
            .fork_session(
                session,
                workspace.clone(),
                Some("Inherited observations".into()),
            )
            .await
            .unwrap();
        engine.close().await.unwrap();
        let (events, view) = reopen(&state, session).await;
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        let fork_view = store.load_view(fork).await.unwrap().unwrap();
        let inherited = store.load_session(session).await.unwrap();
        let boundary_sequence = fork_view.lineage.unwrap().source_sequence;
        let boundary_index = events
            .iter()
            .position(|event| event.sequence == boundary_sequence)
            .unwrap();
        assert!(matches!(
            events[boundary_index].event,
            Event::RunFinished { .. }
        ));
        let boundary_view = SessionView::replay(session, &events[..=boundary_index])
            .unwrap()
            .unwrap();
        assert_eq!(fork_view.usage, boundary_view.usage);
        for original in &events {
            let copy = inherited
                .iter()
                .find(|e| e.session_id == original.session_id && e.sequence == original.sequence)
                .unwrap();
            assert_eq!(
                copy.event.payload().unwrap(),
                original.event.payload().unwrap()
            );
        }
        store.close().await.unwrap();
        let calls = &view.runs[0].agents[0].provider_calls;
        assert_eq!(
            calls[0].rate_limits,
            Some(plan_sample(if cancelled { 1000 } else { 900 }))
        );
        if !cancelled {
            assert_eq!(
                view.runs[0].provider_retries[0].1.rate_limits,
                Some(plan_sample(1000))
            );
        }
        if !cancelled {
            let before_compaction = events
                .iter()
                .position(|event| matches!(event.event, Event::ContextCompacted { .. }))
                .unwrap();
            let prefix = &events[..before_compaction];
            let ordered = SessionView::replay(session, prefix).unwrap().unwrap();
            assert_eq!(
                ordered.usage.plan_limits[0]
                    .primary
                    .as_ref()
                    .unwrap()
                    .observed_at,
                1000,
                "a later settlement with an older receipt cannot overwrite the newer receipt"
            );
            let mut tied = prefix.to_vec();
            let record = tied
                .iter_mut()
                .find_map(|event| match &mut event.event {
                    Event::ProviderCallRecorded { record, .. } => Some(record),
                    _ => None,
                })
                .unwrap();
            let window = record
                .rate_limits
                .as_mut()
                .unwrap()
                .primary
                .as_mut()
                .unwrap();
            window.observed_at = 1000;
            window.used_percent = Some(arany::UsedPercent::try_from(50.0).unwrap());
            assert_eq!(
                SessionView::replay(session, &tied)
                    .unwrap()
                    .unwrap()
                    .usage
                    .plan_limits[0]
                    .primary
                    .as_ref()
                    .unwrap()
                    .used_percent
                    .map(f64::from),
                Some(50.0),
                "committed sequence breaks a receipt-time tie"
            );
        }
        let group = &view.usage.plan_limits[0];
        assert_eq!(
            group.primary.as_ref().unwrap().observed_at,
            if cancelled { 1000 } else { 1100 }
        );
        assert_eq!(
            SessionView::replay(session, &events)
                .unwrap()
                .unwrap()
                .usage,
            view.usage
        );
        assert_eq!(
            primary.requests.lock().unwrap().len(),
            if cancelled { 1 } else { 2 }
        );
        assert_eq!(result.run.status == RunStatus::Cancelled, cancelled);
        let mut forged = events.clone();
        let record = forged
            .iter_mut()
            .find_map(|e| match &mut e.event {
                Event::ProviderCallRecorded { record, .. } => Some(record),
                _ => None,
            })
            .unwrap();
        record
            .rate_limits
            .as_mut()
            .unwrap()
            .primary
            .as_mut()
            .unwrap()
            .window_minutes = Some(0);
        assert!(SessionView::replay(session, &forged).is_err());
    }
}

#[tokio::test]
async fn api_observations_reopen_per_attempt_without_backfill_or_redispatch() {
    for case in [
        "success",
        "failed",
        "unavailable",
        "cancelled",
        "retry",
        "omitted",
        "invalid",
        "fallback",
        "fallback after retries",
        "append failure",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let cancellation = RunCancellation::new();
        let mut primary =
            ScriptedProvider::new(vec![finish("done")]).with_profile("openai", "model-a");
        let account = Uuid::now_v7();
        primary.identity.provenance = arany::Provenance::Native {
            saved_api_account_id: Some(account),
        };
        primary
            .rate_limits
            .get_mut()
            .unwrap()
            .extend([Some(api_sample(1000)), Some(api_sample(900))]);
        if case == "omitted" {
            *primary.rate_limits.get_mut().unwrap().back_mut().unwrap() = None;
        } else if case == "invalid" {
            let sample = primary
                .rate_limits
                .get_mut()
                .unwrap()
                .back_mut()
                .unwrap()
                .as_mut()
                .unwrap();
            let row = &mut sample.api.as_mut().unwrap().rows[0];
            row.limit = None;
            row.remaining = None;
            row.reset_at = None;
            row.unavailable = Some(arany::LimitUnavailable::Invalid);
        }
        if case == "cancelled" {
            primary.cancel_at_completion = Some((AgentPhase::RootPlan, cancellation.clone()));
        } else if case == "failed" {
            primary
                .responses
                .get_mut()
                .unwrap()
                .push_front(Err(ProviderError::InvalidOutcome));
        } else if matches!(case, "retry" | "omitted" | "invalid" | "append failure") {
            primary
                .responses
                .get_mut()
                .unwrap()
                .push_front(Err(ProviderError::RateLimited(None)));
        } else if case == "fallback" {
            // A pre-admission 5xx switches without retrying when Retry-After exceeds the cap.
            let retry_after = Some(Duration::MAX);
            primary
                .responses
                .get_mut()
                .unwrap()
                .push_front(Err(ProviderError::HttpServerUnavailable(retry_after)));
        } else if case == "unavailable" {
            primary
                .responses
                .get_mut()
                .unwrap()
                .push_front(Err(ProviderError::Unavailable));
        } else if case == "fallback after retries" {
            primary.responses = unavailable_attempts(1);
            primary
                .rate_limits
                .get_mut()
                .unwrap()
                .resize(usize::from(MAX_PROVIDER_RETRIES) + 1, Some(api_sample(900)));
        }
        let primary = route(primary);
        let replacement = route(
            ScriptedProvider::new(vec![finish("replacement")]).with_profile("openai", "model-b"),
        );
        let mut bindings = routes(&primary);
        let fallback = matches!(case, "fallback" | "fallback after retries");
        if fallback || case == "unavailable" {
            bindings.roles.primary.push(replacement.clone());
        }
        let engine = open(&state);
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap();
        arany::FaultPlan::skip_retry_backoff(session);
        if case == "append failure" {
            arany::FaultPlan::fail_goal_append(session, |event| {
                matches!(event, Event::ProviderRetryScheduled { .. })
            });
        }
        let mut request = input(&workspace);
        request.session_id = Some(session);
        request.title = None;
        let outcome = run_routed(
            &engine,
            bindings,
            request,
            RunControls {
                cancellation,
                ..RunControls::default()
            },
        )
        .await;
        let _ = engine.close().await;
        let (events, view) = reopen(&state, session).await;
        assert_eq!(
            primary.requests.lock().unwrap().len(),
            match case {
                "retry" | "omitted" | "invalid" => 2,
                "fallback after retries" => usize::from(MAX_PROVIDER_RETRIES) + 1,
                _ => 1,
            },
            "case {case}"
        );
        if case == "append failure" {
            assert!(outcome.is_err());
            assert!(view.usage.api_limits.is_empty());
            assert!(!view.usage.pending.is_empty());
            continue;
        }
        let result = outcome.unwrap();
        assert_eq!(
            result.run.status,
            match case {
                "failed" | "unavailable" => RunStatus::Failed,
                "cancelled" => RunStatus::Cancelled,
                _ => RunStatus::Finished,
            },
            "case {case}"
        );
        assert_eq!(
            view.runs[0].provider_fallbacks.len(),
            usize::from(fallback),
            "case {case}"
        );
        if case == "fallback" {
            assert_eq!(
                view.runs[0].provider_fallbacks[0].1.rate_limits,
                Some(api_sample(1000))
            );
            assert!(
                view.runs[0].agents[0].provider_calls[0]
                    .rate_limits
                    .is_none()
            );
        }
        let group = &view.usage.api_limits[0];
        assert_eq!(group.rows[0].observed_at, 1000);
        assert_eq!(
            group.identity.provenance,
            arany::Provenance::Native {
                saved_api_account_id: Some(account)
            }
        );
        assert_eq!(group.role, ProviderRole::Primary);
        assert_eq!(group.latest_omitted.is_some(), case == "omitted");
        assert_eq!(group.latest_unavailable.is_some(), case == "invalid");
        assert_eq!(
            replacement.requests.lock().unwrap().len(),
            usize::from(fallback)
        );
        assert!(view.usage.plan_limits.is_empty());
        assert_eq!(
            SessionView::replay(session, &events)
                .unwrap()
                .unwrap()
                .usage,
            view.usage
        );
        for plan in [false, true] {
            let mut forged = events.clone();
            let record = forged
                .iter_mut()
                .find_map(|event| match &mut event.event {
                    Event::ProviderCallRecorded { record, .. } => Some(record),
                    _ => None,
                })
                .unwrap();
            if record.rate_limits.is_none() {
                record.rate_limits = Some(api_sample(1000));
            }
            if plan {
                record.rate_limits = Some(plan_sample(1000));
            } else {
                record
                    .rate_limits
                    .as_mut()
                    .unwrap()
                    .api
                    .as_mut()
                    .unwrap()
                    .dialect = arany::ApiLimitDialect::Anthropic;
            }
            assert!(SessionView::replay(session, &forged).is_err());
        }
    }
}

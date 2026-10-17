use super::*;
use arany::{AdmittedProviders, ProviderRole, Providers};

fn finish(text: &str) -> ProviderOutcome {
    ProviderOutcome::Finish(Finish {
        summary: text.into(),
        result: text.into(),
    })
}

async fn reopen(state: &Path, session: SessionId) -> (Vec<arany::EventEnvelope>, SessionView) {
    let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
    let events = store.load_session(session).await.unwrap();
    let view = SessionView::replay(session, &events).unwrap().unwrap();
    store.close().await.unwrap();
    (events, view)
}

fn input(workspace: &Path) -> RunRequest {
    let mut input = request(workspace, "Complete the task");
    input.include_paths.clear();
    input
}

fn route(provider: ScriptedProvider) -> Arc<ScriptedProvider> {
    Arc::new(provider)
}

fn routes(primary: &Arc<ScriptedProvider>) -> AdmittedProviders {
    Providers::all(vec![Arc::clone(primary) as Arc<dyn Provider>])
}

#[tokio::test]
async fn role_selection_pins_primary_child_and_compaction_and_rejects_inconsistent_provenance() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = temp.path().join("state");
    let primary = route(
        ScriptedProvider::new(vec![
            ProviderOutcome::Delegate(Delegate {
                children: vec!["Inspect".into()],
            }),
            finish("done"),
        ])
        .with_profile("scripted", "primary-model"),
    );
    let child = route(
        ScriptedProvider::new(vec![finish("child report")]).with_profile("scripted", "child-model"),
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
    bindings.child = vec![child.clone()];
    bindings.compaction = vec![compact.clone()];
    let mut engine = Engine::open(StateRoot::admit(&state).unwrap(), primary.clone()).unwrap();
    engine.set_providers(bindings).unwrap();
    let mut request = input(&workspace);
    request.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    let result = engine.run(request).await.unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);
    let record = engine
        .compact_session(result.session_id, workspace)
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
    for change in 0..3 {
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
            _ => bindings.review.clear(),
        }
        assert!(SessionView::replay(result.session_id, &invalid).is_err());
    }
}

#[tokio::test]
async fn fallback_only_on_pre_output_call_failure_and_never_after_partial_output() {
    for (error, partial, fallback_expected) in [
        (ProviderError::Unavailable, false, true),
        (ProviderError::CallRejected, false, true),
        (ProviderError::RemoteHttp(429), false, true),
        (ProviderError::PartialTransport, false, false),
        (ProviderError::InvalidStream, false, false),
        (ProviderError::InvalidOutcome, false, false),
        (ProviderError::Rejected, false, false),
        (ProviderError::Unavailable, true, false),
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
        let bindings = Providers::all(vec![
            primary.clone() as Arc<dyn Provider>,
            fallback.clone() as Arc<dyn Provider>,
        ]);
        let mut engine = Engine::open(StateRoot::admit(&state).unwrap(), primary.clone()).unwrap();
        engine.set_providers(bindings).unwrap();
        let result = engine
            .run_with_progress(
                input(&workspace),
                RunCancellation::new(),
                RunProgress::with_previews(),
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
            assert!(
                arany::render_run_feedback(&view.runs[0])
                    .unwrap()
                    .contains("fallback-model")
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
async fn fallback_chain_is_ordered_and_call_and_unknown_token_budgets_stop_it() {
    for budget in [None, Some(1), Some(2)] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let mut primary = ScriptedProvider::new(vec![]).with_profile("scripted", "first");
        primary.responses = Mutex::new(vec![Err(ProviderError::Unavailable)].into());
        let primary = route(primary);
        let mut second = ScriptedProvider::new(vec![]).with_profile("scripted", "second");
        second.responses = Mutex::new(vec![Err(ProviderError::Unavailable)].into());
        let second = route(second);
        let third =
            route(ScriptedProvider::new(vec![finish("third")]).with_profile("scripted", "third"));
        let mut bindings = routes(&primary);
        bindings.primary.extend([
            second.clone() as Arc<dyn Provider>,
            third.clone() as Arc<dyn Provider>,
        ]);
        let mut engine = Engine::open(StateRoot::admit(&state).unwrap(), primary.clone()).unwrap();
        engine.set_providers(bindings).unwrap();
        let mut request = input(&workspace);
        if budget == Some(1) {
            request.goal.budget.as_mut().unwrap().max_provider_calls = Some(1);
        }
        if budget == Some(2) {
            request.goal.budget.as_mut().unwrap().max_tokens = Some(100);
        }
        let result = engine.run(request).await.unwrap();
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
                children: vec!["Inspect".into(), "Compare".into()],
            }),
            finish("done"),
        ])
        .with_profile("scripted", "primary"),
    );
    let mut child = ScriptedProvider::new(vec![]).with_profile("scripted", "child");
    child.responses = Mutex::new(
        vec![
            Err(ProviderError::Unavailable),
            Err(ProviderError::Unavailable),
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
            .with_compactions(vec![Err(ProviderError::Unavailable)]),
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
    bindings.child = vec![child.clone(), fallback.clone()];
    bindings.compaction = vec![compaction.clone(), compaction_fallback.clone()];
    let mut engine = Engine::open(StateRoot::admit(&state).unwrap(), primary.clone()).unwrap();
    engine.set_providers(bindings).unwrap();
    let mut request = input(&workspace);
    request.policy = CollaborationPolicy::Auto {
        max_active_children: 2,
    };
    let result = engine.run(request).await.unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);
    let record = engine
        .compact_session(result.session_id, workspace)
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

#[tokio::test]
async fn fallback_after_a_tool_observation_never_redispatches_the_tool() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = super::goals::facts_workspace(temp.path(), "facts");
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
            Err(ProviderError::Unavailable),
        ]
        .into(),
    );
    let primary = route(primary);
    let fallback =
        route(ScriptedProvider::new(vec![finish("done")]).with_profile("scripted", "fallback"));
    let mut engine =
        super::goals::facts_engine(StateRoot::admit(&state).unwrap(), &state, primary.clone());
    let mut bindings = routes(&primary);
    bindings.primary.push(fallback.clone());
    engine.set_providers(bindings).unwrap();
    let result = engine.run(input(&workspace)).await.unwrap();
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
async fn review_role_fallback_does_not_dispatch_the_proposed_tool_twice() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let workspace = super::goals::facts_workspace(temp.path(), "facts");
    let state = temp.path().join("state");
    let write = arany::ToolCall::Write {
        path: "notes/result.txt".into(),
        expected_digest: None,
        content: "done".into(),
    };
    let plan = arany::GoalPlanContract {
        mission: "Write the result".into(),
        paths: vec!["notes".into()],
        commands: vec![],
        skills: vec![],
        mcp_tools: vec![],
        resource_class: arany::ToolResourceClass::Default,
        command_timeout_ms: None,
        budget: arany::GoalBudget::default(),
        success_criterion: arany::GoalSuccessCriterion::WorkspaceFileContent {
            path: "notes/result.txt".into(),
            content: "done".into(),
        },
    };
    let primary = route(
        ScriptedProvider::new(vec![
            tool_outcome(write, Some(Box::new(plan))),
            finish("done"),
        ])
        .with_profile("scripted", "primary"),
    );
    let mut review = ScriptedProvider::new(vec![]).with_profile("scripted", "review");
    review.responses = Mutex::new(vec![Err(ProviderError::Unavailable)].into());
    let review = route(review);
    let fallback = route(
        ScriptedProvider::new(vec![finish("approve")]).with_profile("scripted", "review-fallback"),
    );
    let mut engine = Engine::open(StateRoot::admit(&state).unwrap(), primary.clone()).unwrap();
    let config = state.join("tools.json");
    std::fs::write(&config, br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    engine
        .enable_tools(env!("CARGO_BIN_EXE_arany").into())
        .unwrap();
    let (approvals, _inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Auto);
    engine.set_tool_approvals(approvals);
    let mut bindings = routes(&primary);
    bindings.review = vec![review.clone(), fallback.clone()];
    engine.set_providers(bindings).unwrap();
    let result = engine.run(input(&workspace)).await.unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);
    engine.close().await.unwrap();
    let (events, view) = reopen(&state, result.session_id).await;
    assert_eq!(
        view.runs[0].provider_fallbacks[0].1.role,
        ProviderRole::Review
    );
    assert_eq!(review.requests.lock().unwrap()[0].model, "review");
    assert_eq!(
        fallback.requests.lock().unwrap()[0].model,
        "review-fallback"
    );
    assert!(
        primary
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.phase != AgentPhase::ToolReview)
    );
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
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/result.txt")).unwrap(),
        "done"
    );
}

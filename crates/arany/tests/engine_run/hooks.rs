use super::*;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn stopped_hook_reopens_and_requires_explicit_goal_resume_without_model_reason() {
    for pre_compact in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
        let source = temp.path().join("source");
        let root = StateRoot::admit(&source).unwrap();
        for (name, bytes) in [
        ("tools.json", br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#.as_slice()),
        ("permissions.json", b"{}".as_slice()),
    ] {
        std::fs::write(source.join(name), bytes).unwrap();
        std::fs::set_permissions(source.join(name), std::fs::Permissions::from_mode(0o600)).unwrap();
    }
        let engine = engine_support::open(
            root,
            EngineConfig {
                guard_executable: Some(std::env::current_exe().unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            arany::test_support::goals::finish_with("candidate"),
        ]));
        let mut input = request(&workspace, "Inspect the task");
        input.include_paths.clear();
        input.goal = arany::GoalRequest::autonomous(
            arany::GoalBudget {
                max_runs: 3,
                ..Default::default()
            },
            arany::GoalPhase::Implementation,
        );
        let result = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        engine.close().await.unwrap();
        let store = Store::open(StateRoot::admit(&source).unwrap()).unwrap();
        let events = store.load_session(result.session_id).await.unwrap();
        store.close().await.unwrap();
        let actor = result.run.agents[0].id;
        let config = result.run.config.as_ref().unwrap();
        let intent = arany::EffectIntent {
            host: None,
            host_command: None,
            target: arany::worker::EffectTarget::Workspace,
            id: Uuid::now_v7(),
            run_id: result.run.id,
            agent_run_id: actor,
            policy_digest: config.tool_policy.as_ref().unwrap().config_digest,
            enforcement_digest: [2; 32],
            workspace_device: config.workspace_device,
            workspace_inode: config.workspace_inode,
            call: arany::ToolCall::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "exit 0".into()],
                cwd: String::new(),
                resource_class: None,
                timeout_ms: None,
            },
            limits: arany::ToolLimits::default(),
            use_count: 1,
        };
        let hook = Event::HookStarted {
            run_id: result.run.id,
            agent_run_id: actor,
            hook: arany::permissions::HookInvocation {
                event: if pre_compact {
                    arany::permissions::HookEvent::PreCompact
                } else {
                    arany::permissions::HookEvent::Stop
                },
                group: 0,
                handler: 0,
                subject_digest: if pre_compact {
                    Sha256::digest(serde_json::to_vec(&("auto", 1_usize)).unwrap()).into()
                } else {
                    Sha256::digest(serde_json::to_vec(&(actor, 0_usize)).unwrap()).into()
                },
                intent: intent.clone(),
                input_digest: [3; 32],
                active_ceiling_ms: Some(1),
            },
        };
        let reason = "private hook stop reason";
        let outcome = Event::HookFinished {
        run_id: result.run.id, agent_run_id: actor,
        outcome: arany::permissions::HookOutcome {
            boundary_active_ms: None,
            result: arany::permissions::HookResult::Stopped { reason: Some(reason.into()) },
            observation: arany::ToolObservation {
                spill: None,
                attestation: Some(arany::Attestation::Guard(arany::GuardReceipt {
                    contract_version: 1, intent_digest: Sha256::digest(serde_json::to_vec(&intent).unwrap()).into(), enforcement_digest: intent.enforcement_digest, limits: intent.limits.clone(),
                })),
                intent, disposition: arany::ToolDisposition::Succeeded,
                output: serde_json::json!({"selection":{"elapsed_ms":null,"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code":0,"stdout":serde_json::json!({"continue":false,"stopReason":reason}).to_string(),"stderr":"","stdout_total":{"bytes":serde_json::json!({"continue":false,"stopReason":reason}).to_string().len(),"lines":1},"stderr_total":{"bytes":0,"lines":0},"changeset":{"files":[]}}).to_string(),
                approval_cause: None,
                guard_stage: None,
                cache_update: arany::CacheUpdate::NotApplicable, enforcement_cause: None, limit_cause: None,
            },
        },
    };
        // Seed recorded evidence; native execution belongs to guard_runs.
        let state = temp.path().join("stopped");
        let store = Store::open(StateRoot::admit(&state).unwrap()).unwrap();
        for envelope in events {
            let mut event = envelope.event;
            match &mut event {
                Event::AgentFinished {
                    disposition,
                    summary,
                    result: answer,
                    ..
                } => {
                    store.append(result.session_id, hook.clone()).await.unwrap();
                    store
                        .append(result.session_id, outcome.clone())
                        .await
                        .unwrap();
                    *disposition = arany::AgentDisposition::StoppedByHook;
                    *summary = None;
                    *answer = None;
                }
                Event::RunFinished { disposition, .. } => {
                    *disposition = arany::RunDisposition::StoppedByHook
                }
                Event::MessageCommitted { .. } | Event::GoalFinished { .. } => continue,
                _ => {}
            }
            store.append(result.session_id, event).await.unwrap();
        }
        store.close().await.unwrap();
        let store = Store::open(StateRoot::admit(&state).unwrap()).unwrap();
        let view = store.load_view(result.session_id).await.unwrap().unwrap();
        assert_eq!(view.runs[0].status, RunStatus::StoppedByHook);
        assert_eq!(view.runs[0].hook_stop_reason(), Some(Some(reason)));
        assert!(view.runs[0].assistant_message.is_none());
        assert_eq!(view.goals[0].status, arany::GoalStatus::Interrupted);
        store.close().await.unwrap();
        let engine =
            engine_support::open(StateRoot::admit(&state).unwrap(), EngineConfig::default())
                .unwrap();
        let forked = engine
            .fork_session(result.session_id, workspace.clone(), None)
            .await
            .unwrap();
        assert_ne!(forked, result.session_id);
        let provider = Arc::new(ScriptedProvider::new(vec![
            arany::test_support::goals::finish_with("resumed"),
        ]));
        let mut input = request(&workspace, "Continue after the hook stop");
        input.session_id = Some(result.session_id);
        input.title = None;
        input.include_paths.clear();
        input.goal = arany::GoalRequest::resume(result.goal_id, true);
        run_to_end(&engine, &provider, input).await.unwrap();
        {
            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(!format!("{:?}", requests[0]).contains(reason));
        }
        engine.close().await.unwrap();
        let database = state.join("events.sqlite3");
        let connection = rusqlite::Connection::open(&database).unwrap();
        let version: i64 = connection
            .query_row(
                "SELECT event_version FROM events WHERE kind='HookFinished'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, outcome.version());
        assert_eq!(
            connection
                .execute(
                    "UPDATE events SET event_version=8 WHERE kind='HookFinished'",
                    []
                )
                .unwrap(),
            1
        );
        drop(connection);
        let before = std::fs::read(&database).unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        assert!(store.load_view(result.session_id).await.is_err());
        store.close().await.unwrap();
        assert_eq!(std::fs::read(&database).unwrap(), before);
    }
}

#[tokio::test]
async fn researcher_tool_hooks_fail_closed_before_dispatch_if_the_program_is_unpinned() {
    for event in ["PreToolUse", "PostToolUse"] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
        let state = temp.path().join("state");
        let root = StateRoot::admit(&state).unwrap();
        for (name, bytes) in [
            ("tools.json", br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#.to_vec()),
            ("permissions.json", serde_json::to_vec(&serde_json::json!({"hooks":{event:[{"matcher":"Read","hooks":[{"type":"command","command":"unpinned","args":[]}]}]}})).unwrap()),
        ] {
            std::fs::write(state.join(name), bytes).unwrap();
            std::fs::set_permissions(state.join(name), std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let engine = engine_support::open(
            root,
            EngineConfig {
                guard_executable: Some(std::env::current_exe().unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: vec![arany::ChildAssignment {
                    objective: "Inspect notes".into(),
                    profile: None,
                    editor: None,
                }],
            }),
            tool_outcome(arany::ToolCall::Read {
                path: "notes/facts.txt".into(),
                offset: None,
                limit: Some(32),
            }),
            arany::test_support::goals::finish_with("child report"),
            arany::test_support::goals::finish_with("answer"),
        ]));
        let mut input = request(&workspace, "Inspect notes");
        input.include_paths.clear();
        input.policy = CollaborationPolicy::Auto {
            max_active_children: 1,
        };
        let result = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(result.run.status, RunStatus::Finished);
        assert!(provider.responses.lock().unwrap().is_empty());
        engine.close().await.unwrap();
        let (events, view) = arany::test_support::goals::replayed(&state, result.session_id).await;
        let tool = &view.runs[0].agents[1].tools[0];
        assert_eq!(
            tool.observation.as_ref().unwrap().output,
            "hook_denied: no operation was dispatched"
        );
        assert!(tool.observation.as_ref().unwrap().attestation.is_none());
        assert!(tool.hooks.is_empty());
        assert!(
            events
                .iter()
                .all(|event| !matches!(event.event, Event::HookStarted { .. }))
        );
    }
}

#[tokio::test]
async fn child_unguarded_inheritance_is_explicit_narrowing_and_pinned_on_replay() {
    use arany::UnguardedGrant::{Denied, HumanOnly};
    let temp = tempfile::tempdir().unwrap();
    let workspace = arany::test_support::goals::facts_workspace(temp.path(), "fact");
    let state = temp.path().join("state");
    let root = StateRoot::admit(&state).unwrap();
    let shell = std::fs::canonicalize("/bin/sh").unwrap();
    let sha256: String = Sha256::digest(std::fs::read(&shell).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut config = serde_json::json!({"version":1,"workspace_paths":["notes"],"write":false,
        "commands":[{"name":"sh","executable":shell,"sha256":sha256,"interpreter":true,"inputs":[]}],
        "skills":[],"mcp":[]});
    std::fs::write(
        state.join("tools.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(
        state.join("tools.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let engine = engine_support::open(
        root,
        EngineConfig {
            guard_executable: Some(std::env::current_exe().unwrap()),
            ..Default::default()
        },
    )
    .unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "Inspect facts".into(),
                profile: None,
            }],
        }),
        arany::test_support::goals::finish_with("fact"),
        arany::test_support::goals::finish_with("answer"),
    ]));
    config["child_unguarded"] = serde_json::json!(HumanOnly);
    std::fs::write(
        state.join("tools.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        engine.submit(
            AdmittedProviders::single(provider.clone()),
            request(&workspace, "Inspect facts"),
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        ),
        Err(EngineError::Tool(arany::ToolError::Configuration))
    ));
    assert!(provider.requests.lock().unwrap().is_empty());
    config["child_unguarded"] = serde_json::json!(Denied);
    std::fs::write(
        state.join("tools.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let mut input = request(&workspace, "Inspect facts");
    input.policy = arany::CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.run.status, RunStatus::Finished);
    let child = outcome.run.agents[1].id;
    assert_eq!(
        outcome.run.agents[1].grant,
        Some(arany::ChildGrant::ReadOnly { unguarded: Denied })
    );
    {
        let calls = provider.requests.lock().unwrap();
        let catalog: serde_json::Value =
            serde_json::from_str(calls[1].input.tools.as_ref().unwrap().catalog()).unwrap();
        assert_eq!(catalog["commands"], serde_json::json!([]));
    }
    engine.close().await.unwrap();
    let (events, _) = arany::test_support::goals::replayed(&state, outcome.session_id).await;
    for requested in [Denied, HumanOnly] {
        let mut changed = events.clone();
        for event in &mut changed {
            if let Event::AgentSpawned {
                agent_run_id,
                grant,
                ..
            } = &mut event.event
                && *agent_run_id == child
            {
                *grant = Some(arany::ChildGrant::ReadOnly {
                    unguarded: requested,
                });
            }
        }
        assert_eq!(
            SessionView::replay(outcome.session_id, &changed).is_ok(),
            requested == Denied
        );
    }
    let mut forged = events;
    for event in &mut forged {
        if let Event::RunStarted { config, .. } = &mut event.event {
            let policy = config.tool_policy.as_mut().unwrap();
            policy.unguarded = Denied;
            policy.child_unguarded = HumanOnly;
        }
    }
    assert!(SessionView::replay(outcome.session_id, &forged).is_err());
}

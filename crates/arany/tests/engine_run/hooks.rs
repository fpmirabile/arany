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
            },
            limits: arany::ToolLimits::default(),
            expires_at_ms: 1,
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
                attestation: Some(arany::Attestation::Guard(arany::GuardReceipt {
                    contract_version: 1, intent_digest: Sha256::digest(serde_json::to_vec(&intent).unwrap()).into(), enforcement_digest: intent.enforcement_digest, limits: intent.limits.clone(),
                })),
                intent, disposition: arany::ToolDisposition::Succeeded,
                output: serde_json::json!({"exit_code":0,"stdout":serde_json::json!({"continue":false,"stopReason":reason}).to_string(),"stderr":"","changeset":{"files":[]}}).to_string(),
                cache_update: arany::CacheUpdate::NotApplicable, enforcement_cause: None,
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
            tool_outcome(
                arany::ToolCall::Read {
                    path: "notes/facts.txt".into(),
                    offset: 0,
                    limit: 32,
                },
                None,
            ),
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
            "Hook denied this Tool: hook program is not pinned"
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

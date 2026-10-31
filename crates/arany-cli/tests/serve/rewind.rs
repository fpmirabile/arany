use super::*;

#[test]
#[cfg_attr(not(arany_ci), ignore)]
fn rewind_serve_guarded_corpus_restores_refuses_drift_keeps_overwrites_and_blocks_uncertain() {
    use arany::test_support::goals::finish_with;
    use arany::test_support::scenario::tool_outcome;
    use sha2::{Digest, Sha256};
    let _gate = crate::test_process::NATIVE_GATE.lock().unwrap();
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(Box::pin(async {
    arany::test_support::isolate_account_root();
    for case in [
        "success",
        "late_code",
        "late_both",
        "drift",
        "kept",
        "overwrite",
        "mixed",
        "saturated",
        "saturated_blocked",
        "uncertain",
        "bounds",
    ] {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/a"), "original").unwrap();
        let state = root.path().join("state");
        let binary = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("arany");
        let engine = Engine::open(
            StateRoot::admit(&state).unwrap(),
            EngineConfig {
                guard_executable: Some(binary),
                ..Default::default()
            },
        )
        .unwrap();
        private_json(
            &state.join("tools.json"),
            json!({"workspace_paths":["src"],"write":true,"commands":[],"skills":[],"mcp":[]}),
        );
        let digest = Sha256::digest(b"original")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let plan = arany::GoalPlanContract {
            mission: "Change files".into(),
            resource_class: arany::ToolResourceClass::Default,
            command_timeout_ms: None,
            paths: vec!["src/a".into(), "src/b".into()],
            commands: Vec::new(),
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            success_criterion: arany::GoalSuccessCriterion::WorkspaceFileContent {
                path: "src/a".into(),
                content: "changed".into(),
            },
            budget: arany::GoalBudget::default(),
        };
        let provider = Arc::new(
            ScriptedProvider::new(vec![
                arany::test_support::scenario::plan_outcome(plan),
                tool_outcome(
                    arany::ToolCall::Write {
                        path: "src/a".into(),
                        expected_digest: Some(digest),
                        content: "changed".into(),
                    }),
                tool_outcome(
                    arany::ToolCall::Write {
                        path: "src/b".into(),
                        expected_digest: None,
                        content: "new".into(),
                    }),
                finish_with("Changed"),
            ])
            .with_profile("openai", "gpt-5.4"),
        );
        let mut submission = arany::test_support::scenario::request(&workspace, "Change files");
        submission.include_paths.clear();
        let source = arany::test_support::engine::run_with(
            &engine,
            &provider,
            submission,
            arany::RunControls {
                tools: arany::ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        engine.close().await.unwrap();
        let thread = source.session_id;
        let run = source.run.id;
        if case == "uncertain" {
            let store = Store::open(StateRoot::open_existing(&state).unwrap()).unwrap();
            let events = store.load_session(thread).await.unwrap();
            store.close().await.unwrap();
            let cut = root.path().join("uncertain-state");
            let store = Store::open(StateRoot::admit(&cut).unwrap()).unwrap();
            for event in events {
                let stop = matches!(event.event, Event::ToolStarted { .. });
                store.append(thread, event.event).await.unwrap();
                if stop {
                    break;
                }
            }
            store.close().await.unwrap();
            std::fs::copy(state.join("tools.json"), cut.join("tools.json")).unwrap();
            let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
            let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
            let client = async {
                input.send(initialize()).await.unwrap();
                next(&mut output).await;
                input.send(request(6,"thread/resume",json!({"threadId":thread,"cwd":workspace,"provider":"openai","model":"gpt-5.4","tools":true}))).await.unwrap();
                next(&mut output).await;
                input
                    .send(request(
                        10,
                        "thread/rewind/preview",
                        json!({"threadId":thread,"turnId":run}),
                    ))
                    .await
                    .unwrap();
                let preview = next(&mut output).await["result"].clone();
                assert_eq!(preview["restore"], json!(["conversation"]));
                assert!(
                    preview["blockers"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("uncertainPublication"))
                );
                input.send(request(11,"thread/rewind",json!({"threadId":thread,"previewToken":preview["previewToken"],"restore":"code"}))).await.unwrap();
                assert_eq!(
                    next(&mut output).await["error"]["data"]["reason"],
                    "rewindBlocked"
                );
                input.send(ClientMessage::Eof).await.unwrap();
            };
            let native = Engine::open(
                StateRoot::open_existing(&cut).unwrap(),
                EngineConfig {
                    guard_executable: Some(
                        std::env::current_exe()
                            .unwrap()
                            .parent()
                            .unwrap()
                            .parent()
                            .unwrap()
                            .join("arany"),
                    ),
                    ..Default::default()
                },
            )
            .unwrap();
            let (result, ()) = tokio::join!(
                server::run_with_threads(
                    native,
                    cut,
                    inbound,
                    outbound,
                    threads::Threads::with_provider(scripted())
                ),
                client
            );
            result.unwrap();
            continue;
        }
        if matches!(case, "drift" | "kept" | "overwrite" | "mixed" | "saturated_blocked") {
            std::fs::write(workspace.join("src/a"), "external").unwrap();
        }
        let (before, _) = closed_view(&state, thread).await;
        let native = Engine::open(
            StateRoot::open_existing(&state).unwrap(),
            EngineConfig {
                guard_executable: Some(
                    std::env::current_exe()
                        .unwrap()
                        .parent()
                        .unwrap()
                        .parent()
                        .unwrap()
                        .join("arany"),
                ),
                ..Default::default()
            },
        )
        .unwrap();
        let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
        let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
        let outbound_probe = outbound.clone();
        let client = async {
            input.send(initialize()).await.unwrap();
            next(&mut output).await;
            input.send(request(6,"thread/resume",json!({"threadId":thread,"cwd":workspace,"provider":"openai","model":"gpt-5.4","tools":true}))).await.unwrap();
            next(&mut output).await;
            input
                .send(request(
                    10,
                    "thread/rewind/preview",
                    json!({"threadId":thread,"turnId":run}),
                ))
                .await
                .unwrap();
            let preview = next(&mut output).await["result"].clone();
            assert_eq!(preview["files"].as_array().unwrap().len(), 2, "{case}");
            let restore = if case == "late_both" { "both" } else { "code" };
            let mut params = json!({"threadId":thread,"previewToken":preview["previewToken"],"restore":restore});
            let clock = matches!(case, "late_code" | "late_both")
                .then(|| arany::test_support::test_clock(arany::now_ms()));
            if let Some(clock) = &clock {
                clock.advance(61_000);
            }
            if case == "kept" {
                params["kept"] = json!(["src/a"]);
            }
            if case == "bounds" {
                params["kept"] = json!((0..65).map(|n| format!("src/{n:02}")).collect::<Vec<_>>());
            }
            if case == "mixed" {
                params["kept"] = json!(["src/b"]);
            }
            if case == "overwrite" || case == "mixed" {
                params["overwrite"] = json!(["src/a"]);
                input
                    .send(request(11, "thread/rewind", params.clone()))
                    .await
                    .unwrap();
                assert_eq!(
                    next(&mut output).await["error"]["data"]["reason"],
                    "overwriteConfirmationRequired"
                );
                assert_eq!(std::fs::read(workspace.join("src/a")).unwrap(), b"external");
                params["confirmOverwrite"] = json!(true);
            }
            if case.starts_with("saturated") {
                let probe = outbound_probe;
                for _ in 0..OUTBOUND_CAPACITY {
                    probe
                        .try_send(server::response(RequestId::Integer(0), json!({})))
                        .unwrap();
                }
                drop(probe);
                input.send(request(11, "thread/rewind", params)).await.unwrap();
                input.send(request(12, "thread/list", json!({}))).await.unwrap();
                if case == "saturated" {
                    input.send(ClientMessage::Eof).await.unwrap();
                }
                let (mut reply, mut busy) = (None, false);
                while reply.is_none() || !busy {
                    let message = output.recv().await.expect("every request is answered");
                    let value = serde_json::to_value(message).unwrap();
                    match value["id"].as_i64() {
                        Some(11) => reply = Some(value),
                        Some(12) => busy = true,
                        _ => {}
                    }
                }
                let reply = reply.expect("rewind answered");
                if case == "saturated_blocked" {
                    assert_eq!(reply["error"]["data"]["reason"], "rewindBlocked", "{reply}");
                    input.send(ClientMessage::Eof).await.unwrap();
                } else {
                    assert_ne!(reply["result"]["rewind"]["outcome"], "uncertain", "{reply}");
                }
                return;
            }
            input
                .send(request(11, "thread/rewind", params))
                .await
                .unwrap();
            let result = next(&mut output).await;
            if matches!(case, "drift" | "bounds") {
                assert_eq!(
                    result["error"]["data"]["reason"],
                    if case == "drift" {
                        "rewindBlocked"
                    } else {
                        "invalidParams"
                    }
                );
            } else {
                assert_eq!(
                    result["result"]["rewind"]["outcome"], "restored",
                    "{case}: {result}"
                );
                if case == "late_both" {
                    assert_eq!(result["result"]["conversation"]["threadId"], Value::Null);
                    assert_eq!(result["result"]["conversation"]["prompt"], "Change files");
                }
            }
            input.send(ClientMessage::Eof).await.unwrap();
        };
        let (result, ()) = tokio::join!(
            server::run_with_threads(
                native,
                state.clone(),
                inbound,
                outbound,
                threads::Threads::with_provider(scripted())
            ),
            client
        );
        result.unwrap();
        let (after, view) = closed_view(&state, thread).await;
        if case == "saturated_blocked" {
            assert_eq!(after, before);
        } else if case == "saturated" {
            if let Some(operation) = view.rewinds.operations.last() {
                assert_ne!(operation.outcome(), arany::rewind::RewindOutcome::Uncertain);
            }
        } else if matches!(case, "drift" | "bounds") {
            assert_eq!(after, before);
            assert!(workspace.join("src/b").exists());
        } else {
            assert_eq!(
                std::fs::read(workspace.join("src/a")).unwrap(),
                if case == "kept" {
                    b"external".as_slice()
                } else {
                    b"original".as_slice()
                }
            );
            assert_eq!(workspace.join("src/b").exists(), case == "mixed");
            assert_eq!(
                view.rewinds.operations.last().unwrap().outcome(),
                arany::rewind::RewindOutcome::Restored
            );
            if matches!(case, "late_code" | "late_both") {
                assert_eq!(after.len(), before.len() + 2, "{case}");
            }
        }
    }
    }));
}

#[tokio::test]
async fn rewind_preview_tokens_bind_thread_head_survive_time_and_fork_once() {
    let _deadline = deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let provider = Arc::new(
        ScriptedProvider::new(vec![
            arany::test_support::goals::finish_with("First"),
            arany::test_support::goals::finish_with("Second"),
        ])
        .with_profile("openai", "gpt-5.4"),
    );
    let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let client = async {
        input.send(initialize()).await.unwrap();
        next(&mut output).await;
        input.send(thread_start(&workspace)).await.unwrap();
        let started = next(&mut output).await;
        let thread = started["result"]["thread"]["id"].clone();
        next(&mut output).await;
        let mut runs = Vec::new();
        for prompt in ["First prompt", "Second prompt"] {
            let mut content = vec![json!({"type":"text","text":prompt})];
            if prompt == "First prompt" {
                content.push(json!({"type":"image","mediaType":"image/png","data":arany::test_support::test_image().base64()}));
            }
            input
                .send(request(
                    3,
                    "turn/start",
                    json!({"threadId":thread,"input":content}),
                ))
                .await
                .unwrap();
            runs.push(next(&mut output).await["result"]["turn"]["id"].clone());
            while next(&mut output).await["method"] != "turn/completed" {}
        }
        for (params, reason) in [
            (
                json!({"threadId":arany::SessionId::new(),"turnId":runs[0]}),
                "threadNotLoaded",
            ),
            (
                json!({"threadId":thread,"turnId":arany::RunId::new()}),
                "invalidParams",
            ),
        ] {
            input
                .send(request(10, "thread/rewind/preview", params))
                .await
                .unwrap();
            assert_eq!(next(&mut output).await["error"]["data"]["reason"], reason);
        }
        input
            .send(request(
                10,
                "thread/rewind/preview",
                json!({"threadId":thread,"turnId":runs[1]}),
            ))
            .await
            .unwrap();
        let preview = next(&mut output).await["result"].clone();
        assert!(preview.get("expiresAtMs").is_none());
        assert_eq!(preview["restore"], json!(["conversation"]));
        assert_eq!(preview["blockers"], json!(["noWriteGrant"]));
        let source: arany::SessionId = serde_json::from_value(thread.clone()).unwrap();
        let (before, _) = closed_view(&state, source).await;
        let clock = arany::test_support::test_clock(arany::now_ms());
        input.send(request(12,"thread/start",json!({"cwd":workspace,"provider":"openai","model":"gpt-5.4","collaboration":"single"}))).await.unwrap();
        let foreign = next(&mut output).await["result"]["thread"]["id"].clone();
        next(&mut output).await;
        input.send(request(11,"thread/rewind",json!({"threadId":foreign,"previewToken":preview["previewToken"],"restore":"conversation"}))).await.unwrap();
        assert_eq!(
            next(&mut output).await["error"]["data"]["reason"],
            "previewChanged"
        );
        input
            .send(request(
                11,
                "thread/rewind",
                json!({"threadId":thread,"previewToken":"wrong","restore":"conversation"}),
            ))
            .await
            .unwrap();
        assert_eq!(
            next(&mut output).await["error"]["data"]["reason"],
            "previewChanged"
        );
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(61)).await;
        clock.advance(61_000);
        tokio::time::resume();
        input.send(request(11,"thread/rewind",json!({"threadId":thread,"previewToken":preview["previewToken"],"restore":"conversation"}))).await.unwrap();
        let restored = next(&mut output).await["result"].clone();
        assert_eq!(restored["rewind"]["outcome"], "restored");
        assert_eq!(restored["conversation"]["imagesOmitted"], false);
        assert_eq!(next(&mut output).await["method"], "thread/started");
        let (after, _) = closed_view(&state, source).await;
        assert_eq!(after, before);
        let fork: arany::SessionId =
            serde_json::from_value(restored["thread"]["id"].clone()).unwrap();
        let (_, fork_view) = closed_view(&state, fork).await;
        assert_eq!(fork_view.runs.len(), 1);
        assert_eq!(fork_view.runs[0].objective, "First prompt");
        input
            .send(request(
                10,
                "thread/rewind/preview",
                json!({"threadId":thread,"turnId":runs[1]}),
            ))
            .await
            .unwrap();
        let token = next(&mut output).await["result"]["previewToken"].clone();
        let store = Store::open(StateRoot::open_existing(&state).unwrap()).unwrap();
        store
            .append(
                source,
                Event::SessionRenamed {
                    title: "Changed head".into(),
                },
            )
            .await
            .unwrap();
        store.close().await.unwrap();
        input
            .send(request(
                11,
                "thread/rewind",
                json!({"threadId":thread,"previewToken":token,"restore":"conversation"}),
            ))
            .await
            .unwrap();
        assert_eq!(
            next(&mut output).await["error"]["data"]["reason"],
            "previewChanged"
        );
        input
            .send(request(
                10,
                "thread/rewind/preview",
                json!({"threadId":thread,"turnId":runs[1]}),
            ))
            .await
            .unwrap();
        let token = next(&mut output).await["result"]["previewToken"].clone();
        let (before, _) = closed_view(&state, source).await;
        for field in ["kept", "overwrite"] {
            let mut params =
                json!({"threadId":thread,"previewToken":token,"restore":"conversation"});
            params[field] = json!(["x".repeat(513)]);
            input
                .send(request(11, "thread/rewind", params))
                .await
                .unwrap();
            assert_eq!(
                next(&mut output).await["error"]["data"]["reason"],
                "invalidParams"
            );
        }
        input
            .send(request(
                11,
                "thread/rewind",
                json!({"threadId":thread,"previewToken":token,"restore":"conversation"}),
            ))
            .await
            .unwrap();
        let restored = next(&mut output).await["result"].clone();
        assert_eq!(restored["rewind"]["outcome"], "restored");
        assert_eq!(restored["conversation"]["prompt"], "Second prompt");
        let fork: arany::SessionId =
            serde_json::from_value(restored["thread"]["id"].clone()).unwrap();
        assert_ne!(fork, source);
        assert_eq!(next(&mut output).await["method"], "thread/started");
        let (after, _) = closed_view(&state, source).await;
        assert_eq!(before, after);
        let (_, view) = closed_view(&state, fork).await;
        assert_eq!(view.runs.len(), 1);
        assert_eq!(view.runs[0].objective, "First prompt");
        assert!(view.goals.is_empty());
        input
            .send(request(
                11,
                "thread/rewind",
                json!({"threadId":thread,"previewToken":token,"restore":"conversation"}),
            ))
            .await
            .unwrap();
        assert_eq!(
            next(&mut output).await["error"]["data"]["reason"],
            "previewChanged"
        );
        input
            .send(request(
                10,
                "thread/rewind/preview",
                json!({"threadId":thread,"turnId":runs[0]}),
            ))
            .await
            .unwrap();
        let token = next(&mut output).await["result"]["previewToken"].clone();
        input
            .send(request(
                11,
                "thread/rewind",
                json!({"threadId":thread,"previewToken":token,"restore":"conversation"}),
            ))
            .await
            .unwrap();
        let first = next(&mut output).await["result"].clone();
        assert!(first.get("thread").is_none());
        assert_eq!(first["conversation"]["threadId"], Value::Null);
        assert_eq!(first["conversation"]["prompt"], "First prompt");
        assert_eq!(first["conversation"]["imagesOmitted"], true);
        assert_eq!(
            first["notices"],
            json!(["Images were not copied; attach them again"])
        );
        input.send(ClientMessage::Eof).await.unwrap();
    };
    let (result, ()) = tokio::join!(
        server::run_with_threads(
            engine(&state),
            state.clone(),
            inbound,
            outbound,
            threads::Threads::with_provider(provider.clone())
        ),
        client
    );
    result.unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
}

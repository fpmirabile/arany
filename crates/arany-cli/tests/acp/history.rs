use super::*;

#[tokio::test]
async fn setup_edits_block_loaded_session_prompts_without_events_or_provider_calls() {
    let _deadline = crate::cli::serve::tests::deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let provider = Arc::new(ScriptedProvider::new(Vec::new()).with_profile("openai", "gpt-5.4"));
    let (mut writer, mut reader, task) = start(&state, provider.clone());
    initialize(&mut writer, &mut reader).await;
    let session = session(&mut writer, &mut reader, &workspace).await;
    let store = Store::open(StateRoot::open_existing(&state).unwrap()).unwrap();
    for uncertain in [false, true] {
        arany::test_support::record_setup_edit_state(&store, session, uncertain).await;
        let events = store.load_session(session).await.unwrap();
        for (method, params) in [
            (
                "session/prompt",
                json!({"sessionId":session,"prompt":[{"type":"text","text":"Must not disclose"}]}),
            ),
            (
                "session/load",
                json!({"sessionId":session,"cwd":workspace,"mcpServers":[]}),
            ),
            (
                "session/resume",
                json!({"sessionId":session,"cwd":workspace}),
            ),
        ] {
            send(&mut writer, json!(3), method, params).await;
            let response = next(&mut reader).await;
            assert_eq!(response["id"], 3);
            assert_eq!(response["error"]["code"], -32603);
            assert_eq!(response["error"]["data"]["reason"], "providerSetupBlocked");
        }
        assert_eq!(store.load_session(session).await.unwrap(), events);
    }
    assert_eq!(
        arany::list_sessions(StateRoot::open_existing(&state).unwrap(), workspace)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(provider.requests.lock().unwrap().is_empty());
    assert!(provider.compact_requests.lock().unwrap().is_empty());
    store.close().await.unwrap();
    writer.shutdown().await.unwrap();
    assert_eq!(reader.read(&mut [0; 1]).await.unwrap(), 0);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn load_replays_committed_history_after_restart_and_prompt_continues_the_session() {
    let _deadline = crate::cli::serve::tests::deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let foreign = root.path().join("foreign");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&foreign).unwrap();
    let state = root.path().join("state");
    let provider = scripted();
    let (mut writer, mut reader, task) = start(&state, provider.clone());
    initialize(&mut writer, &mut reader).await;
    let session = session(&mut writer, &mut reader, &workspace).await;
    send(
        &mut writer,
        json!(3),
        "session/prompt",
        json!({"sessionId":session,"prompt":[{"type":"text","text":"Hi"}]}),
    )
    .await;
    assert_eq!(
        next(&mut reader).await["params"]["update"]["content"]["text"],
        "Hello"
    );
    assert_eq!(next(&mut reader).await["result"]["stopReason"], "end_turn");
    writer.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    let before = store.load_session(session).await.unwrap();
    let view = store.load_view(session).await.unwrap().unwrap();
    store.close().await.unwrap();

    let continuation = scripted();
    let (mut writer, mut reader, task) = start(&state, continuation.clone());
    initialize(&mut writer, &mut reader).await;
    send(
        &mut writer,
        json!(3),
        "authenticate",
        json!({"methodId":"arany-setup"}),
    )
    .await;
    assert_eq!(next(&mut reader).await["result"], json!({}));
    for method in ["session/load", "session/resume"] {
        for (id, cwd, reason) in [
            (SessionId::new(), workspace.clone(), "resource_not_found"),
            (session, foreign.clone(), "workspaceMismatch"),
            (session, root.path().to_path_buf(), "invalidParams"),
            (session, PathBuf::from("relative"), "invalidParams"),
        ] {
            send(
                &mut writer,
                json!(4),
                method,
                json!({"sessionId":id,"cwd":cwd,"mcpServers":[]}),
            )
            .await;
            let refused = next(&mut reader).await;
            assert_eq!(refused["error"]["data"]["reason"], reason);
            assert_eq!(
                refused["error"]["code"],
                if reason == "resource_not_found" {
                    -32002
                } else if reason == "workspaceMismatch" {
                    -32603
                } else {
                    -32602
                }
            );
        }
        send(&mut writer, json!(4), method, json!({"sessionId":session,"cwd":workspace,"mcpServers":[],"additionalDirectories":[foreign]})).await;
        assert_eq!(next(&mut reader).await["error"]["code"], -32602);
    }
    send(
        &mut writer,
        json!(5),
        "session/prompt",
        json!({"sessionId":session,"prompt":[{"type":"text","text":"Not loaded"}]}),
    )
    .await;
    assert_eq!(next(&mut reader).await["error"]["code"], -32002);
    send(
        &mut writer,
        json!(6),
        "session/load",
        json!({"sessionId":session,"cwd":workspace,"mcpServers":[],"_meta":{"trust":true}}),
    )
    .await;
    let user = next(&mut reader).await;
    assert_eq!(user["params"]["sessionId"], json!(session));
    assert_eq!(
        user["params"]["update"]["sessionUpdate"],
        "user_message_chunk"
    );
    assert_eq!(
        user["params"]["update"]["content"]["text"],
        view.runs[0].objective
    );
    assert_eq!(
        user["params"]["update"]["_meta"]["arany.turnStatus"],
        "completed"
    );
    assert_eq!(
        next(&mut reader).await,
        wire::answer(session, view.runs[0].assistant_message.as_ref().unwrap())
    );
    let loaded = next(&mut reader).await;
    assert_eq!(loaded["id"], 6);
    validate_session_result(&loaded, "session/load");
    assert!(continuation.requests.lock().unwrap().is_empty());
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    assert_eq!(store.load_session(session).await.unwrap(), before);
    store.close().await.unwrap();
    send(
        &mut writer,
        json!(8),
        "session/prompt",
        json!({"sessionId":session,"prompt":[{"type":"text","text":"Continue"}]}),
    )
    .await;
    assert_eq!(
        next(&mut reader).await["params"]["update"]["content"]["text"],
        "Hello"
    );
    assert_eq!(next(&mut reader).await["result"]["stopReason"], "end_turn");
    send(
        &mut writer,
        json!(9),
        "session/resume",
        json!({"sessionId":session,"cwd":workspace}),
    )
    .await;
    let resumed = next(&mut reader).await;
    assert_eq!(resumed["id"], 9);
    validate_session_result(&resumed, "session/resume");
    writer.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(continuation.requests.lock().unwrap().len(), 1);
    assert!(continuation.responses.lock().unwrap().is_empty());
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    let view = store.load_view(session).await.unwrap().unwrap();
    store.close().await.unwrap();
    assert_eq!(view.runs.len(), 2);
    assert_eq!(view.runs[1].objective, "Continue");
    assert_ne!(view.runs[0].id, view.runs[1].id);
    assert!(view.runs.iter().all(|r| r.status == RunStatus::Finished));
}

#[tokio::test]
async fn session_admission_preserves_cli_child_restrictions() {
    let _deadline = crate::cli::serve::tests::deadline();
    for (collaboration, limit, policy) in [
        (
            Collaboration::Single,
            None,
            arany::CollaborationPolicy::Single,
        ),
        (
            Collaboration::Auto,
            Some(0),
            arany::CollaborationPolicy::Auto {
                max_active_children: 0,
            },
        ),
    ] {
        for method in ["session/new", "session/load", "session/resume"] {
            let root = tempfile::tempdir().unwrap();
            let workspace = root.path().join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let state = root.path().join("state");
            let mut selection = config(&state);
            selection.collaboration = collaboration.clone();
            selection.max_active_children = limit;
            let provider = Arc::new(
                ScriptedProvider::new(
                    (0..3)
                        .map(|_| {
                            arany::ProviderOutcome::Delegate(arany::Delegate {
                                children: vec![arany::ChildAssignment {
                                    editor: None,
                                    objective: "Inspect".into(),
                                    profile: None,
                                }],
                                agent: None,
                            })
                        })
                        .collect(),
                )
                .with_profile("openai", "gpt-5.4"),
            );
            let (mut writer, mut reader, mut task) = start_with_config(
                selection,
                provider.clone(),
                std::env::current_exe().unwrap(),
            );
            initialize(&mut writer, &mut reader).await;
            let session = session(&mut writer, &mut reader, &workspace).await;
            if method == "session/resume" {
                writer.shutdown().await.unwrap();
                task.await.unwrap().unwrap();
                let mut selection = config(&state);
                selection.collaboration = collaboration.clone();
                selection.max_active_children = limit;
                (writer, reader, task) = start_with_config(
                    selection,
                    provider.clone(),
                    std::env::current_exe().unwrap(),
                );
                initialize(&mut writer, &mut reader).await;
            }
            if method != "session/new" {
                send(
                    &mut writer,
                    json!(3),
                    method,
                    json!({"sessionId":session,"cwd":workspace,"mcpServers":[]}),
                )
                .await;
                validate_session_result(&next(&mut reader).await, method);
            }
            send(
                &mut writer,
                json!(4),
                "session/prompt",
                json!({"sessionId":session,"prompt":[{"type":"text","text":"Delegate inspection"}]}),
            )
            .await;
            let response = loop {
                let message = next(&mut reader).await;
                if message["id"] == 4 {
                    break message;
                }
            };
            writer.shutdown().await.unwrap();
            task.await.unwrap().unwrap();
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let events = store.load_session(session).await.unwrap();
            let view = store.load_view(session).await.unwrap().unwrap();
            store.close().await.unwrap();
            assert!(
                !events.iter().any(|event| matches!(
                    event.event,
                    Event::AgentSpawned {
                        role: arany::AgentRole::Child,
                        ..
                    }
                )),
                "{method} under {policy:?} spawned a child"
            );
            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), 3, "{method} under {policy:?}");
            assert_eq!(requests[0].collaboration, policy, "{method}");
            assert_eq!(
                response["error"]["data"]["reason"], "turnFailed",
                "{method}"
            );
            assert_eq!(view.runs.len(), 1, "{method}");
            assert_eq!(view.runs[0].status, RunStatus::Failed, "{method}");
        }
    }
}

#[tokio::test]
async fn oversized_load_and_resume_refuse_without_history_or_a_loaded_session() {
    let _deadline = crate::cli::serve::tests::deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let engine = arany::test_support::engine::open(
        StateRoot::admit(&state).unwrap(),
        EngineConfig::default(),
    )
    .unwrap();
    let session = engine
        .create_session(workspace.clone(), None)
        .await
        .unwrap();
    let source = Arc::new(
        ScriptedProvider::new(vec![
            arany::ProviderOutcome::Answer(arany::Answer {
                text: "Hello".into()
            });
            3
        ])
        .with_profile("openai", "gpt-5.4"),
    );
    for _ in 0..3 {
        let mut request =
            arany::test_support::scenario::request(&workspace, &"\u{1}".repeat(64 * 1024));
        request.session_id = Some(session);
        request.title = None;
        request.include_paths.clear();
        arany::test_support::engine::run_with(
            &engine,
            &source,
            request,
            arany::RunControls::default(),
        )
        .await
        .unwrap();
    }
    engine.close().await.unwrap();
    let idle = scripted();
    let (mut writer, mut reader, task) = start(&state, idle.clone());
    initialize(&mut writer, &mut reader).await;
    send(
        &mut writer,
        json!(3),
        "authenticate",
        json!({"methodId":"arany-setup"}),
    )
    .await;
    next(&mut reader).await;
    for method in ["session/load", "session/resume"] {
        send(
            &mut writer,
            json!(4),
            method,
            json!({"sessionId":session,"cwd":workspace,"mcpServers":[]}),
        )
        .await;
        let refused = next(&mut reader).await;
        assert_eq!(refused["error"]["code"], -32603);
        assert_eq!(refused["error"]["data"]["reason"], "contextTooLarge");
        send(
            &mut writer,
            json!(5),
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":"Again"}]}),
        )
        .await;
        assert_eq!(next(&mut reader).await["error"]["code"], -32002);
    }
    writer.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
    assert!(idle.requests.lock().unwrap().is_empty());
    assert_eq!(source.requests.lock().unwrap().len(), 3);
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    assert_eq!(
        store.load_view(session).await.unwrap().unwrap().runs.len(),
        3
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn load_replays_tool_results_and_interrupted_intents_without_execution() {
    use std::os::unix::fs::PermissionsExt;
    let _deadline = crate::cli::serve::tests::deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("facts.txt"), "Fact").unwrap();
    let source = root.path().join("source");
    let engine = arany::test_support::engine::open(
        StateRoot::admit(&source).unwrap(),
        EngineConfig {
            guard_executable: Some(std::env::current_exe().unwrap()),
            ..Default::default()
        },
    )
    .unwrap();
    let tools_path = source.join("tools.json");
    std::fs::write(&tools_path, serde_json::to_vec(&json!({"version":1,"workspace_paths":["facts.txt"],"write":false,"commands":[],"skills":[],"mcp":[]})).unwrap()).unwrap();
    std::fs::set_permissions(&tools_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let provider = Arc::new(
        ScriptedProvider::new(vec![
            arany::test_support::scenario::tool_outcome(arany::ToolCall::Read {
                path: "facts.txt".into(),
                offset: None,
                limit: Some(32),
            }),
            arany::ProviderOutcome::Answer(arany::Answer {
                text: "Fact".into(),
            }),
        ])
        .with_profile("openai", "gpt-5.4"),
    );
    let mut request = arany::test_support::scenario::request(&workspace, "Inspect");
    request.include_paths.clear();
    let outcome = arany::test_support::engine::run_with(
        &engine,
        &provider,
        request,
        arany::RunControls {
            tools: arany::ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    engine.close().await.unwrap();
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    assert!(provider.responses.lock().unwrap().is_empty());
    let store = Store::open_read_only(StateRoot::open_existing(&source).unwrap()).unwrap();
    let events = store.load_session(outcome.session_id).await.unwrap();
    let view = store.load_view(outcome.session_id).await.unwrap().unwrap();
    store.close().await.unwrap();
    let intent_id = view.runs[0].tools[0].intent.id.to_string();
    assert_eq!(
        view.runs[0].tools[0]
            .observation
            .as_ref()
            .unwrap()
            .disposition,
        arany::ToolDisposition::Succeeded
    );
    let interrupted = root.path().join("interrupted");
    let store = Store::open(StateRoot::admit(&interrupted).unwrap()).unwrap();
    for envelope in &events {
        store
            .append(outcome.session_id, envelope.event.clone())
            .await
            .unwrap();
        if matches!(envelope.event, Event::ToolStarted { .. }) {
            break;
        }
    }
    store.close().await.unwrap();
    std::fs::remove_file(workspace.join("facts.txt")).unwrap();
    for (state, status) in [(source, "completed"), (interrupted, "interrupted")] {
        let idle = scripted();
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        let before = store.load_session(outcome.session_id).await.unwrap();
        store.close().await.unwrap();
        let (mut writer, mut reader, task) = start(&state, idle.clone());
        initialize(&mut writer, &mut reader).await;
        send(
            &mut writer,
            json!(3),
            "session/load",
            json!({"sessionId":outcome.session_id,"cwd":workspace,"mcpServers":[]}),
        )
        .await;
        let user = next(&mut reader).await;
        assert_eq!(user["params"]["update"]["content"]["text"], "Inspect");
        assert_eq!(
            user["params"]["update"]["_meta"]["arany.turnStatus"],
            status
        );
        let started = next(&mut reader).await;
        assert_eq!(started["params"]["update"]["sessionUpdate"], "tool_call");
        assert_eq!(started["params"]["update"]["toolCallId"], intent_id);
        let finished = next(&mut reader).await;
        assert_eq!(
            finished["params"]["update"]["sessionUpdate"],
            "tool_call_update"
        );
        assert_eq!(finished["params"]["update"]["toolCallId"], intent_id);
        if status == "completed" {
            assert_eq!(finished["params"]["update"]["status"], "completed");
            assert!(
                finished["params"]["update"]["content"][0]["content"]["text"]
                    .as_str()
                    .unwrap()
                    .contains("Fact")
            );
            assert_eq!(
                next(&mut reader).await["params"]["update"]["content"]["text"],
                "Fact"
            );
        } else {
            assert_eq!(finished["params"]["update"]["status"], "failed");
            assert_eq!(
                finished["params"]["update"]["_meta"]["arany.status"],
                "uncertain"
            );
        }
        let loaded = next(&mut reader).await;
        assert_eq!(loaded["id"], 3);
        validate_session_result(&loaded, "session/load");
        writer.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
        assert!(idle.requests.lock().unwrap().is_empty());
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        assert_eq!(
            store.load_session(outcome.session_id).await.unwrap(),
            before
        );
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn load_and_resume_translate_pinned_schema_without_private_state() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    let intent = adapter.intent(arany::ToolCall::Read {
        path: "facts.txt".into(),
        offset: None,
        limit: Some(32),
    });
    let params = json!({"sessionId":adapter.session,"cwd":"/var/workspace","mcpServers":[]});
    let schema = schema();
    for (definition, value) in [
        ("LoadSessionRequest", params.clone()),
        (
            "ResumeSessionRequest",
            json!({"sessionId":adapter.session,"cwd":"/var/workspace"}),
        ),
    ] {
        let part = json!({"$schema":schema["$schema"], "$defs":schema["$defs"], "$ref":format!("#/$defs/{definition}")});
        assert!(jsonschema::validator_for(&part).unwrap().is_valid(&value));
    }
    adapter
        .call(
            json!(8),
            "session/load",
            json!({"sessionId":adapter.session,"cwd":"/var/workspace"}),
        )
        .await;
    assert_eq!(adapter.next().await["error"]["code"], -32602);
    adapter.call(json!(9), "session/load", params.clone()).await;
    let request = adapter.request().await;
    assert_eq!(request.method, "thread/resume");
    let forwarded: ThreadResumeParams = serde_json::from_value(request.params.unwrap()).unwrap();
    assert_eq!(forwarded.cwd, "/var/workspace");
    assert_eq!(forwarded.provider, "openai");
    assert_eq!(forwarded.tools, Some(true));
    let result = json!({"thread":{"id":adapter.session,"title":"History","workspace":"/private/var/workspace","createdAtMs":0},
    "turns":[{"id":adapter.run,"goalId":arany::GoalId::new(),"status":"interrupted","items":[
        {"type":"userMessage","id":format!("{}:user",adapter.run),"text":"Inspect","images":[]},
        serve::items::tool_item(&intent, None, true)
    ]}]});
    adapter
        .events
        .send(Message::Response(Response {
            jsonrpc: Default::default(),
            id: request.id,
            result: result.clone(),
        }))
        .await
        .unwrap();
    let user = adapter.next().await;
    assert_eq!(
        user["params"]["update"]["sessionUpdate"],
        "user_message_chunk"
    );
    assert_eq!(
        user["params"]["update"]["_meta"]["arany.turnStatus"],
        "interrupted"
    );
    let start = adapter.next().await;
    assert_eq!(start["params"]["update"]["sessionUpdate"], "tool_call");
    assert_eq!(start["params"]["update"]["status"], "pending");
    assert_eq!(
        start["params"]["update"]["locations"][0]["path"],
        "/var/workspace/facts.txt"
    );
    let finish = adapter.next().await;
    assert_eq!(
        finish["params"]["update"]["sessionUpdate"],
        "tool_call_update"
    );
    assert_eq!(finish["params"]["update"]["status"], "failed");
    assert_eq!(
        finish["params"]["update"]["_meta"]["arany.status"],
        "uncertain"
    );
    assert!(
        finish["params"]["update"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains(tools::UNKNOWN_ACP_TOOL_OUTCOME)
    );
    let loaded = adapter.next().await;
    assert_eq!(loaded["id"], 9);
    validate_session_result(&loaded, "session/load");
    assert!(
        adapter.input.try_recv().is_err(),
        "loading must not start a turn or request approval"
    );
    adapter
        .call(
            json!(10),
            "session/resume",
            json!({"sessionId":adapter.session,"cwd":"/var/workspace"}),
        )
        .await;
    adapter.respond(result).await;
    let resumed = adapter.next().await;
    assert_eq!(resumed["id"], 10);
    validate_session_result(&resumed, "session/resume");
    assert!(adapter.input.try_recv().is_err());
    for reason in [Reason::MissingSession, Reason::ContextTooLarge] {
        let session = SessionId::new();
        adapter
            .call(
                json!(11),
                "session/load",
                json!({"sessionId":session,"cwd":"/var/workspace","mcpServers":[]}),
            )
            .await;
        let request = adapter.request().await;
        adapter
            .events
            .send(Message::Error(ErrorResponse {
                jsonrpc: Default::default(),
                id: Some(request.id),
                error: ErrorObject {
                    code: -32000,
                    message: "History unavailable".into(),
                    data: Some(ErrorData {
                        reason: reason.clone(),
                    }),
                },
            }))
            .await
            .unwrap();
        let refused = adapter.next().await;
        assert_eq!(refused["id"], 11);
        assert_eq!(
            refused["error"]["code"],
            if reason == Reason::MissingSession {
                -32002
            } else {
                -32603
            }
        );
        assert_eq!(
            refused["error"]["data"]["reason"],
            if reason == Reason::MissingSession {
                "resource_not_found"
            } else {
                "contextTooLarge"
            }
        );
        adapter
            .call(
                json!(12),
                "session/prompt",
                json!({"sessionId":session,"prompt":[{"type":"text","text":"Not loaded"}]}),
            )
            .await;
        assert_eq!(adapter.next().await["error"]["code"], -32002);
        assert!(adapter.input.try_recv().is_err());
    }
    adapter.close().await;
}

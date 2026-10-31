use super::*;

#[tokio::test]
async fn busy_cancel_and_eof_commit_interruption() {
    let _deadline = crate::cli::serve::tests::deadline();
    for cancel in ["session/cancel", "$/cancel_request", "session/close", "eof"] {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = root.path().join("state");
        let (started, mut started_rx) = mpsc::unbounded_channel();
        let dropped = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(WaitingProvider {
            started,
            dropped: dropped.clone(),
            requests: Arc::default(),
            identity: scripted().identity().clone(),
        });
        let (mut writer, mut reader, task) = start(&state, provider);
        initialize(&mut writer, &mut reader).await;
        let session = session(&mut writer, &mut reader, &workspace).await;
        send(
            &mut writer,
            json!(3),
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":"Wait"}]}),
        )
        .await;
        started_rx.recv().await.unwrap();
        send(
            &mut writer,
            json!(4),
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":"Again"}]}),
        )
        .await;
        assert_eq!(next(&mut reader).await["error"]["data"]["reason"], "busy");
        for method in ["session/load", "session/resume"] {
            send(
                &mut writer,
                json!(5),
                method,
                json!({"sessionId":session,"cwd":workspace,"mcpServers":[]}),
            )
            .await;
            assert_eq!(next(&mut reader).await["error"]["data"]["reason"], "busy");
        }
        let idle = scripted();
        let (mut other_writer, mut other_reader, other_task) = start(&state, idle.clone());
        initialize(&mut other_writer, &mut other_reader).await;
        for method in ["session/load", "session/resume"] {
            send(
                &mut other_writer,
                json!(6),
                method,
                json!({"sessionId":session,"cwd":workspace,"mcpServers":[]}),
            )
            .await;
            assert_eq!(
                next(&mut other_reader).await["error"]["data"]["reason"],
                "sessionBusy"
            );
        }
        other_writer.shutdown().await.unwrap();
        other_task.await.unwrap().unwrap();
        assert!(idle.requests.lock().unwrap().is_empty());
        if cancel == "session/close" {
            send(
                &mut writer,
                json!(8),
                "session/close",
                json!({"sessionId":session}),
            )
            .await;
            assert_eq!(next(&mut reader).await["result"]["stopReason"], "cancelled");
            let closed = next(&mut reader).await;
            assert_eq!(closed["id"], 8);
            assert_eq!(closed["result"], json!({}));
            send(
                &mut writer,
                json!(9),
                "session/prompt",
                json!({"sessionId":session,"prompt":[{"type":"text","text":"Again"}]}),
            )
            .await;
            assert_eq!(
                next(&mut reader).await["error"]["data"]["reason"],
                "resource_not_found"
            );
        } else if cancel != "eof" {
            writer.write_all(&encode_line(&json!({"jsonrpc":"2.0","method":cancel,"params":if cancel == "session/cancel" {json!({"sessionId":session})} else {json!({"requestId":3})}})).unwrap()).await.unwrap();
            let completion = next(&mut reader).await;
            assert_eq!(completion["id"], 3);
            assert_eq!(completion["result"]["stopReason"], "cancelled");
        }
        writer.shutdown().await.unwrap();
        if cancel == "eof" {
            assert_eq!(next(&mut reader).await["result"]["stopReason"], "cancelled");
        }
        task.await.unwrap().unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        let view = store.load_view(session).await.unwrap().unwrap();
        store.close().await.unwrap();
        assert_eq!(view.runs.len(), 1);
        assert_eq!(view.runs[0].status, RunStatus::Cancelled);
    }
}

#[tokio::test]
async fn translator_latches_cancel_reconciles_and_remaps_failures_without_private_state() {
    let _deadline = crate::cli::serve::tests::deadline();
    let shared = Arc::new(Mutex::new(translate::State::default()));
    let config = config(std::path::Path::new("/state"));
    let (serve_tx, mut serve_rx) = mpsc::channel(64);
    let (events, event_rx) = mpsc::channel(256);
    let (outbound, mut output) = mpsc::channel(256);
    let (_stopping, stopping) = watch::channel(false);
    let task = tokio::spawn(translate::output(
        event_rx,
        serve_tx.clone(),
        outbound.clone(),
        shared.clone(),
        stopping,
    ));
    let session = arany::SessionId::new();
    let run = arany::RunId::new();
    let goal = arany::GoalId::new();
    let invoke = |id, method, params| {
        wire::decode(
            &encode_line(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .unwrap(),
        )
        .unwrap()
    };
    let reply = |id, result| {
        Message::Response(Response {
            jsonrpc: Default::default(),
            id: RequestId::Integer(id),
            result,
        })
    };
    translate::input(
        invoke(1, "initialize", json!({"protocolVersion":1})),
        &config,
        &shared,
        &serve_tx,
        &outbound,
    )
    .await
    .unwrap();
    assert!(matches!(
        serve_rx.recv().await,
        Some(serve::server::ClientMessage::Message(Message::Request(_)))
    ));
    events.send(reply(1, json!({}))).await.unwrap();
    validate(&output.recv().await.unwrap());
    translate::input(invoke(2, "session/new", json!({"cwd":"/workspace","mcpServers":[{"name":"ignored","command":"never-execute-this","args":[],"env":[]}]})), &config, &shared, &serve_tx, &outbound).await.unwrap();
    let Some(serve::server::ClientMessage::Message(Message::Request(new))) = serve_rx.recv().await
    else {
        panic!("thread start")
    };
    assert_eq!(new.method, "thread/start");
    assert_eq!(new.params.unwrap()["tools"], false);
    events
        .send(reply(
            2,
            json!({"thread":{"id":session,"title":"","workspace":"/workspace","createdAtMs":0}}),
        ))
        .await
        .unwrap();
    validate(&output.recv().await.unwrap());
    translate::input(invoke(99, "session/prompt", json!({"sessionId":arany::SessionId::new(),"prompt":[{"type":"text","text":"unknown Session"}]})), &config, &shared, &serve_tx, &outbound).await.unwrap();
    let missing = output.recv().await.unwrap();
    validate(&missing);
    assert_eq!(missing["id"], 99);
    assert_eq!(missing["error"]["code"], -32002);
    assert_eq!(missing["error"]["data"]["reason"], "resource_not_found");
    assert!(
        serve_rx.try_recv().is_err(),
        "unknown Session never starts a turn"
    );
    translate::input(invoke(3, "session/prompt", json!({"sessionId":session,"prompt":[
        {"type":"text","text":"a"},
        {"type":"resource_link","name":"file","uri":"file:///never-read","mimeType":"text/plain"},
        {"type":"resource_link","name":"web","uri":"http://never-fetch.invalid"},
        {"type":"resource_link","name":"é","uri":"../../never-open","_meta":{"tools":true}},
        {"type":"text","text":"b"}
    ]})), &config, &shared, &serve_tx, &outbound).await.unwrap();
    let Some(serve::server::ClientMessage::Message(Message::Request(start))) =
        serve_rx.recv().await
    else {
        panic!("turn start")
    };
    assert_eq!(
        start.params.unwrap()["input"][0]["text"],
        "a[@file](file:///never-read)[@web](http://never-fetch.invalid)[@é](../../never-open)b"
    );
    let cancel = wire::decode(
        &encode_line(
            &json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}),
        )
        .unwrap(),
    )
    .unwrap();
    translate::input(cancel, &config, &shared, &serve_tx, &outbound)
        .await
        .unwrap();
    assert!(
        serve_rx.try_recv().is_err(),
        "cancellation must wait for the committed turn ID"
    );
    events
        .send(reply(
            3,
            json!({"turn":{"id":run,"goalId":goal,"status":"inProgress","items":[]}}),
        ))
        .await
        .unwrap();
    let Some(serve::server::ClientMessage::Message(Message::Request(interrupt))) =
        serve_rx.recv().await
    else {
        panic!("interrupt")
    };
    assert_eq!(interrupt.method, "turn/interrupt");
    assert_eq!(interrupt.params.unwrap()["turnId"], json!(run));
    events.send(reply(4, json!({}))).await.unwrap();
    events.send(Message::Notification(Notification { jsonrpc:Default::default(), method:"turn/completed".into(), params:Some(json!({"threadId":session,"turn":{"id":run,"goalId":goal,"status":"failed","items":[],"error":{"message":"Run failed"},"goal":{"status":"failed"}}})) })).await.unwrap();
    let cancelled = output.recv().await.unwrap();
    validate(&cancelled);
    assert_eq!(cancelled["result"]["stopReason"], "cancelled");
    translate::input(
        invoke(
            5,
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":"again"}]}),
        ),
        &config,
        &shared,
        &serve_tx,
        &outbound,
    )
    .await
    .unwrap();
    assert!(serve_rx.recv().await.is_some());
    events
        .send(reply(
            5,
            json!({"turn":{"id":run,"goalId":goal,"status":"inProgress","items":[]}}),
        ))
        .await
        .unwrap();
    events.send(Message::Notification(Notification { jsonrpc:Default::default(), method:"turn/completed".into(), params:Some(json!({"threadId":session,"turn":{"id":run,"goalId":goal,"status":"completed","items":[],"goal":{"status":"completed"}}})) })).await.unwrap();
    let Some(serve::server::ClientMessage::Message(Message::Request(recover))) =
        serve_rx.recv().await
    else {
        panic!("reconcile missing committed answer")
    };
    assert_eq!(recover.method, "events/subscribe");
    for method in ["session/load", "session/resume"] {
        translate::input(
            invoke(
                90,
                method,
                json!({"sessionId":session,"cwd":"/workspace","mcpServers":[]}),
            ),
            &config,
            &shared,
            &serve_tx,
            &outbound,
        )
        .await
        .unwrap();
        let busy = output.try_recv().expect("recovery keeps the Session busy");
        validate(&busy);
        assert_eq!(busy["id"], 90);
        assert_eq!(busy["error"]["code"], -32600);
        assert_eq!(busy["error"]["data"]["reason"], "busy");
        assert!(
            serve_rx.try_recv().is_err(),
            "busy Session never reaches serve"
        );
    }
    events.send(reply(6, json!({"events":[{"sequence":20,"session_id":session.to_string(),"run_id":run.to_string(),"agent_run_id":null,"kind":"MessageCommitted","payload":{"run_id":run,"text":"Recovered"},"created_at_ms":0}],"nextSince":null}))).await.unwrap();
    let recovered = output.recv().await.unwrap();
    validate(&recovered);
    assert_eq!(
        recovered["params"]["update"]["content"]["text"],
        "Recovered"
    );
    let finished = output.recv().await.unwrap();
    validate(&finished);
    assert_eq!(finished["result"]["stopReason"], "end_turn");
    translate::input(
        invoke(
            7,
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":"again"}]}),
        ),
        &config,
        &shared,
        &serve_tx,
        &outbound,
    )
    .await
    .unwrap();
    assert!(serve_rx.recv().await.is_some());
    events
        .send(Message::Error(ErrorResponse {
            jsonrpc: Default::default(),
            id: Some(RequestId::Integer(7)),
            error: ErrorObject {
                code: -32000,
                message: "Goal budget exhausted".into(),
                data: Some(ErrorData {
                    reason: Reason::GoalBudgetExhausted,
                }),
            },
        }))
        .await
        .unwrap();
    let failed = output.recv().await.unwrap();
    validate(&failed);
    assert_eq!(failed["error"]["code"], -32603);
    assert_eq!(failed["error"]["data"]["reason"], "goalBudgetExhausted");
    translate::input(
        invoke(
            8,
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":"stalled output"}]}),
        ),
        &config,
        &shared,
        &serve_tx,
        &outbound,
    )
    .await
    .unwrap();
    assert!(serve_rx.recv().await.is_some());
    for _ in 0..256 {
        outbound
            .try_send(wire::response(RequestId::Integer(99), json!({})))
            .unwrap();
    }
    events
        .send(reply(
            8,
            json!({"turn":{"id":run,"goalId":goal,"status":"inProgress","items":[]}}),
        ))
        .await
        .unwrap();
    events.send(Message::Notification(Notification { jsonrpc:Default::default(), method:"item/completed".into(), params:Some(json!({"threadId":session,"turnId":run,"completedAtMs":0,"item":{"type":"agentMessage","id":format!("{run}:answer"),"text":"# Committed","phase":"final_answer"}})) })).await.unwrap();
    drop(events.reserve_many(256).await.unwrap());
    let cancel = wire::decode(
        &encode_line(
            &json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}),
        )
        .unwrap(),
    )
    .unwrap();
    translate::input(cancel, &config, &shared, &serve_tx, &outbound)
        .await
        .unwrap();
    let Some(serve::server::ClientMessage::Message(Message::Request(interrupt))) =
        serve_rx.recv().await
    else {
        panic!("cancel reaches serve despite stalled ACP output")
    };
    assert_eq!(interrupt.method, "turn/interrupt");
    for _ in 0..256 {
        output.recv().await.unwrap();
    }
    let answer = output.recv().await.unwrap();
    validate(&answer);
    assert_eq!(answer["params"]["update"]["content"]["text"], "# Committed");
    events.send(reply(9, json!({}))).await.unwrap();
    events.send(Message::Notification(Notification { jsonrpc:Default::default(), method:"turn/completed".into(), params:Some(json!({"threadId":session,"turn":{"id":run,"goalId":goal,"status":"interrupted","items":[],"goal":{"status":"interrupted"}}})) })).await.unwrap();
    let cancelled = output.recv().await.unwrap();
    validate(&cancelled);
    assert_eq!(cancelled["result"]["stopReason"], "cancelled");
    translate::input(invoke(10, "session/prompt", json!({"sessionId":session,"prompt":[{"type":"text","text":"shutdown during recovery"}]})), &config, &shared, &serve_tx, &outbound).await.unwrap();
    assert!(serve_rx.recv().await.is_some());
    events
        .send(reply(
            10,
            json!({"turn":{"id":run,"goalId":goal,"status":"inProgress","items":[]}}),
        ))
        .await
        .unwrap();
    events.send(Message::Notification(Notification { jsonrpc:Default::default(), method:"turn/completed".into(), params:Some(json!({"threadId":session,"turn":{"id":run,"goalId":goal,"status":"completed","items":[],"goal":{"status":"completed"}}})) })).await.unwrap();
    assert!(serve_rx.recv().await.is_some());
    _stopping.send_replace(true);
    events
        .send(reply(11, json!({"events":[],"nextSince":1000})))
        .await
        .unwrap();
    let incomplete = output.recv().await.unwrap();
    validate(&incomplete);
    assert_eq!(
        incomplete["result"]["_meta"]["arany.incompleteUpdates"],
        true
    );
    assert!(
        serve_rx.try_recv().is_err(),
        "shutdown must not start another replay page"
    );
    translate::input(
        invoke(
            12,
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":"EOF before acceptance"}]}),
        ),
        &config,
        &shared,
        &serve_tx,
        &outbound,
    )
    .await
    .unwrap();
    assert!(serve_rx.recv().await.is_some());
    translate::cancel_active(&shared);
    events
        .send(reply(
            12,
            json!({"turn":{"id":run,"goalId":goal,"status":"inProgress","items":[]}}),
        ))
        .await
        .unwrap();
    events.send(Message::Notification(Notification { jsonrpc:Default::default(), method:"turn/completed".into(), params:Some(json!({"threadId":session,"turn":{"id":run,"goalId":goal,"status":"interrupted","items":[],"goal":{"status":"interrupted"}}})) })).await.unwrap();
    let cancelled = output.recv().await.unwrap();
    validate(&cancelled);
    assert_eq!(cancelled["result"]["stopReason"], "cancelled");
    assert!(
        serve_rx.try_recv().is_err(),
        "EOF already cancels serve; no late interrupt may be sent"
    );
    drop(events);
    task.await.unwrap().unwrap();
    _stopping.send_replace(false);
    for close_before_recovery in [false, true] {
        let (events, event_rx) = mpsc::channel(256);
        let task = tokio::spawn(translate::output(
            event_rx,
            serve_tx.clone(),
            outbound.clone(),
            shared.clone(),
            _stopping.subscribe(),
        ));
        translate::input(
            invoke(13, "session/prompt", json!({"sessionId":session,"prompt":[{"type":"text","text":"EOF before recovery reply"}]})),
            &config, &shared, &serve_tx, &outbound,
        ).await.unwrap();
        let Some(serve::server::ClientMessage::Message(Message::Request(start))) =
            serve_rx.recv().await
        else {
            panic!("turn start")
        };
        events
            .send(Message::Response(Response {
                jsonrpc: Default::default(),
                id: start.id,
                result: json!({"turn":{"id":run,"goalId":goal,"status":"inProgress","items":[]}}),
            }))
            .await
            .unwrap();
        if close_before_recovery {
            serve_rx.close();
        }
        events.send(Message::Notification(Notification { jsonrpc:Default::default(), method:"turn/completed".into(), params:Some(json!({"threadId":session,"turn":{"id":run,"goalId":goal,"status":"completed","items":[],"goal":{"status":"completed"}}})) })).await.unwrap();
        if !close_before_recovery {
            let Some(serve::server::ClientMessage::Message(Message::Request(recover))) =
                serve_rx.recv().await
            else {
                panic!("recovery request")
            };
            assert_eq!(recover.method, "events/subscribe");
        }
        drop(events);
        task.await.unwrap().unwrap();
        let incomplete = output
            .try_recv()
            .expect("EOF must answer a committed completion");
        validate(&incomplete);
        assert_eq!(incomplete["id"], 13);
        assert_eq!(incomplete["result"]["stopReason"], "end_turn");
        assert_eq!(
            incomplete["result"]["_meta"]["arany.incompleteUpdates"],
            true
        );
    }
}

#[tokio::test]
async fn session_lifecycle_closes_full_roster_and_lists_bounded_inert_metadata() {
    let _deadline = crate::cli::serve::tests::deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let foreign = root.path().join("foreign");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&foreign).unwrap();
    let state = root.path().join("state");
    let engine =
        arany::Engine::open(StateRoot::admit(&state).unwrap(), EngineConfig::default()).unwrap();
    let title = "Hostile\u{1b}[31m\u{202e}<img src=x>";
    let saved = engine
        .create_session(workspace.clone(), Some(title.into()))
        .await
        .unwrap();
    let hidden = engine
        .create_session(foreign.clone(), Some("Hidden".into()))
        .await
        .unwrap();
    engine.close().await.unwrap();
    let provider = scripted();
    let (mut writer, mut reader, task) = start(&state, provider.clone());
    initialize(&mut writer, &mut reader).await;
    let mut ids = Vec::new();
    for _ in 0..64 {
        ids.push(session(&mut writer, &mut reader, &workspace).await);
    }
    send(
        &mut writer,
        json!(20),
        "session/new",
        json!({"cwd":workspace,"mcpServers":[]}),
    )
    .await;
    assert_eq!(
        next(&mut reader).await["error"]["data"]["reason"],
        "threadLimit"
    );
    send(
        &mut writer,
        json!(20),
        "session/close",
        json!({"sessionId":ids[0]}),
    )
    .await;
    assert_eq!(next(&mut reader).await["result"], json!({}));
    ids.push(session(&mut writer, &mut reader, &workspace).await);
    send(
        &mut writer,
        json!(20),
        "session/close",
        json!({"sessionId":ids[0]}),
    )
    .await;
    assert_eq!(next(&mut reader).await["error"]["code"], -32002);
    send(
        &mut writer,
        json!(20),
        "session/list",
        json!({"cwd":workspace,"_meta":{"untrusted":"ignored"}}),
    )
    .await;
    let first = next(&mut reader).await;
    assert_eq!(first["result"]["sessions"].as_array().unwrap().len(), 50);
    let cursor = first["result"]["nextCursor"].as_str().unwrap();
    send(
        &mut writer,
        json!(20),
        "session/list",
        json!({"cwd":workspace,"cursor":cursor}),
    )
    .await;
    let second = next(&mut reader).await;
    assert_eq!(second["result"]["sessions"].as_array().unwrap().len(), 16);
    assert!(second["result"]["nextCursor"].is_null());
    let mut listed = std::collections::BTreeSet::new();
    for entry in first["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second["result"]["sessions"].as_array().unwrap())
    {
        assert!(listed.insert(entry["sessionId"].as_str().unwrap().to_owned()));
        assert_eq!(entry["cwd"], json!(workspace));
        assert_eq!(entry.as_object().unwrap().len(), 4);
        if entry["sessionId"] == json!(saved) {
            assert_eq!(entry["title"], crate::presentation::escape_terminal(title));
            assert!(
                !entry["title"]
                    .as_str()
                    .unwrap()
                    .chars()
                    .any(char::is_control)
            );
        }
    }
    assert!(listed.contains(&saved.to_string()));
    assert!(listed.contains(&ids[0].to_string()));
    for response in [&first, &second] {
        let encoded = response.to_string();
        assert!(!encoded.contains(foreign.to_str().unwrap()));
        assert!(!encoded.contains(state.to_str().unwrap()));
        assert!(!encoded.contains(&hidden.to_string()));
    }
    for params in [
        json!({"cwd":workspace,"cursor":"bad"}),
        json!({"cwd":workspace,"cursor":"x".repeat(129)}),
        json!({"cwd":foreign,"cursor":cursor}),
        json!({"cwd":"relative"}),
        json!({"cwd":root.path()}),
    ] {
        send(&mut writer, json!(20), "session/list", params).await;
        assert_eq!(next(&mut reader).await["error"]["code"], -32602);
    }
    for method in ["session/delete", "session/fork", "session/set_model"] {
        send(&mut writer, json!(20), method, json!({"sessionId":saved})).await;
        assert_eq!(next(&mut reader).await["error"]["code"], -32601);
    }
    writer.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
    assert!(provider.requests.lock().unwrap().is_empty());
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    assert_eq!(store.load_session(ids[0]).await.unwrap().len(), 1);
    store.close().await.unwrap();
}

#[tokio::test]
async fn close_cancels_pending_permissions_and_preserves_unknown_effects_without_private_state() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    adapter.prompt(json!([{"type":"text","text":"Edit"}])).await;
    let intent = adapter.intent(arany::ToolCall::Write {
        path: "file".into(),
        content: "new".into(),
        expected_digest: None,
    });
    adapter.item(&intent, None).await;
    adapter.next().await;
    adapter.permission(&intent, 1).await;
    let permission = adapter.next().await;
    adapter
        .call(
            json!(30),
            "session/close",
            json!({"sessionId":adapter.session}),
        )
        .await;
    let interrupt = adapter.request().await;
    assert_eq!(interrupt.method, "turn/interrupt");
    let observation = arany::ToolObservation {
        spill: None,
        intent: intent.clone(),
        disposition: arany::ToolDisposition::Uncertain,
        enforcement_cause: None,
        limit_cause: None,
        output: "Outcome unknown".into(),
        attestation: None,
        approval_cause: None,
        guard_stage: None,
        cache_update: arany::CacheUpdate::NotApplicable,
    };
    adapter.item(&intent, Some(&observation)).await;
    let withdrawal = adapter.next().await;
    assert_eq!(withdrawal["method"], "$/cancel_request");
    assert_eq!(withdrawal["params"]["requestId"], permission["id"]);
    let item = adapter.next().await;
    assert_eq!(
        item["params"]["update"]["_meta"]["arany.status"],
        "uncertain"
    );
    adapter.events.send(Message::Notification(Notification {
        jsonrpc: Default::default(), method: "turn/completed".into(),
        params: Some(json!({"threadId":adapter.session,"turn":{"id":adapter.run,"goalId":arany::GoalId::new(),"status":"failed","items":[],"goal":{"status":"failed"}}})),
    })).await.unwrap();
    adapter.respond(json!({"events":[],"nextSince":null})).await;
    let prompt = adapter.next().await;
    assert_eq!(prompt["id"], "prompt");
    assert_eq!(prompt["result"]["stopReason"], "cancelled");
    assert_eq!(prompt["result"]["_meta"]["arany.uncertainEffects"], 1);
    let close = adapter.request().await;
    assert_eq!(close.method, "thread/unsubscribe");
    assert_eq!(close.params.unwrap(), json!({"threadId":adapter.session}));
    adapter
        .events
        .send(Message::Response(Response {
            jsonrpc: Default::default(),
            id: close.id,
            result: json!({"status":"unsubscribed"}),
        }))
        .await
        .unwrap();
    assert_eq!(adapter.next().await["id"], 30);
    adapter
        .call(
            json!(31),
            "session/prompt",
            json!({"sessionId":adapter.session,"prompt":[{"type":"text","text":"Again"}]}),
        )
        .await;
    assert_eq!(adapter.next().await["error"]["code"], -32002);
    assert!(
        adapter.input.try_recv().is_err(),
        "closing never retries or continues the effect"
    );
    adapter.close().await;
}

#[tokio::test]
async fn close_defers_unload_until_tap_loss_reconciliation_recovers_the_uncertain_effect() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    adapter.prompt(json!([{"type":"text","text":"Edit"}])).await;
    let intent = adapter.intent(arany::ToolCall::Write {
        path: "file".into(),
        content: "new".into(),
        expected_digest: None,
    });
    adapter
        .call(
            json!(30),
            "session/close",
            json!({"sessionId":adapter.session}),
        )
        .await;
    assert_eq!(adapter.request().await.method, "turn/interrupt");
    adapter
        .call(
            json!(31),
            "session/close",
            json!({"sessionId":adapter.session}),
        )
        .await;
    assert_eq!(adapter.next().await["error"]["data"]["reason"], "busy");
    let observation = arany::ToolObservation {
        spill: None,
        intent: intent.clone(),
        disposition: arany::ToolDisposition::Uncertain,
        enforcement_cause: None,
        limit_cause: None,
        output: "Outcome unknown".into(),
        attestation: None,
        approval_cause: None,
        guard_stage: None,
        cache_update: arany::CacheUpdate::NotApplicable,
    };
    adapter
        .events
        .send(Message::Notification(Notification {
            jsonrpc: Default::default(),
            method: "events/resync".into(),
            params: Some(json!({"threadId":adapter.session,"missed":2,"lastSequence":0})),
        }))
        .await
        .unwrap();
    adapter.events.send(Message::Notification(Notification { jsonrpc:Default::default(),method:"turn/completed".into(),params:Some(json!({"threadId":adapter.session,"turn":{"id":adapter.run,"goalId":arany::GoalId::new(),"status":"failed","items":[],"goal":{"status":"failed"}}})) })).await.unwrap();
    let events: Vec<Value> = [("ToolStarted",json!({"intent":intent,"active_ceiling_ms":1,"verdict":null})),("ToolFinished",{let mut p=serde_json::to_value(&observation).unwrap();p["checkpoint"]=Value::Null;p})].into_iter().enumerate().map(|(index,(kind,payload))| json!({
        "sequence":index+1,"session_id":adapter.session,"run_id":adapter.run,"agent_run_id":intent.agent_run_id,"kind":kind,"payload":payload,"created_at_ms":0
    })).collect();
    let recover = adapter.request().await;
    assert_eq!(
        recover.method, "events/subscribe",
        "the thread stays loaded for reconciliation before unload"
    );
    adapter
        .events
        .send(Message::Response(Response {
            jsonrpc: Default::default(),
            id: recover.id,
            result: json!({"events":events,"nextSince":null}),
        }))
        .await
        .unwrap();
    let mut recovered = false;
    let prompt = loop {
        let message = adapter.next().await;
        if message["id"] == "prompt" {
            break message;
        }
        recovered |= message["params"]["update"]["_meta"]["arany.status"] == "uncertain";
    };
    assert!(recovered, "the missed Uncertain observation is recovered");
    assert_eq!(prompt["result"]["stopReason"], "cancelled");
    assert_eq!(prompt["result"]["_meta"]["arany.uncertainEffects"], 1);
    assert!(prompt["result"]["_meta"]["arany.incompleteUpdates"].is_null());
    let close = adapter.request().await;
    assert_eq!(close.method, "thread/unsubscribe");
    for (id, method, params) in [
        (
            json!(40),
            "session/prompt",
            json!({"sessionId":adapter.session,"prompt":[{"type":"text","text":"Race"}]}),
        ),
        (
            json!(41),
            "session/close",
            json!({"sessionId":adapter.session}),
        ),
    ] {
        adapter.call(id, method, params).await;
        assert_eq!(
            adapter.next().await["error"]["data"]["reason"],
            "busy",
            "closing state survives until the unsubscribe response"
        );
    }
    assert!(adapter.input.try_recv().is_err());
    adapter
        .events
        .send(Message::Response(Response {
            jsonrpc: Default::default(),
            id: close.id,
            result: json!({"status":"unsubscribed"}),
        }))
        .await
        .unwrap();
    assert_eq!(adapter.next().await["id"], 30);
    adapter
        .call(
            json!(32),
            "session/prompt",
            json!({"sessionId":adapter.session,"prompt":[{"type":"text","text":"Again"}]}),
        )
        .await;
    assert_eq!(adapter.next().await["error"]["code"], -32002);
    assert!(
        adapter.input.try_recv().is_err(),
        "the Uncertain effect is never retried and the slot is freed"
    );
    adapter.close().await;
}

#[tokio::test(start_paused = true)]
async fn close_deadline_holds_under_output_backpressure_and_ignores_late_completion() {
    let mut adapter = Adapter::new().await;
    adapter
        .prompt(json!([{"type":"text","text":"Stall"}]))
        .await;
    adapter
        .call(
            json!(30),
            "session/close",
            json!({"sessionId":adapter.session}),
        )
        .await;
    assert_eq!(adapter.request().await.method, "turn/interrupt");
    let mut filler = 0;
    while adapter.outbound.try_send(json!({"filler":filler})).is_ok() {
        filler += 1;
    }
    tokio::time::sleep(crate::cli::exec::SHUTDOWN_GRACE + std::time::Duration::from_millis(1))
        .await;
    let close = adapter.request().await;
    assert_eq!(
        close.method, "thread/unsubscribe",
        "unsubscribe is sent while the writer is blocked"
    );
    for _ in 0..filler {
        adapter.output.recv().await.unwrap();
    }
    let prompt = adapter.next().await;
    assert_eq!(prompt["id"], "prompt");
    assert_eq!(prompt["result"]["stopReason"], "cancelled");
    adapter
        .events
        .send(Message::Response(Response {
            jsonrpc: Default::default(),
            id: close.id,
            result: json!({"status":"unsubscribed"}),
        }))
        .await
        .unwrap();
    assert_eq!(adapter.next().await["id"], 30);
    adapter.events.send(Message::Notification(Notification { jsonrpc:Default::default(),method:"turn/completed".into(),params:Some(json!({"threadId":adapter.session,"turn":{"id":adapter.run,"goalId":arany::GoalId::new(),"status":"interrupted","items":[],"goal":{"status":"interrupted"}}})) })).await.unwrap();
    tokio::task::yield_now().await;
    assert!(
        adapter.output.try_recv().is_err(),
        "late completion is dropped"
    );
    assert!(adapter.input.try_recv().is_err());
    adapter.close().await;
}

#[tokio::test(start_paused = true)]
async fn close_with_a_stalled_turn_completes_at_its_deadline_without_retrying() {
    let mut adapter = Adapter::new().await;
    adapter
        .prompt(json!([{"type":"text","text":"Stall"}]))
        .await;
    adapter
        .call(
            json!(30),
            "session/close",
            json!({"sessionId":adapter.session}),
        )
        .await;
    assert_eq!(adapter.request().await.method, "turn/interrupt");
    tokio::time::sleep(crate::cli::exec::SHUTDOWN_GRACE + std::time::Duration::from_millis(1))
        .await;
    let prompt = adapter.next().await;
    assert_eq!(prompt["id"], "prompt");
    assert_eq!(prompt["result"]["stopReason"], "cancelled");
    assert_eq!(prompt["result"]["_meta"]["arany.incompleteUpdates"], true);
    let close = adapter.request().await;
    assert_eq!(close.method, "thread/unsubscribe");
    adapter
        .events
        .send(Message::Response(Response {
            jsonrpc: Default::default(),
            id: close.id,
            result: json!({"status":"unsubscribed"}),
        }))
        .await
        .unwrap();
    assert_eq!(adapter.next().await["id"], 30);
    adapter
        .call(
            json!(31),
            "session/prompt",
            json!({"sessionId":adapter.session,"prompt":[{"type":"text","text":"Again"}]}),
        )
        .await;
    assert_eq!(adapter.next().await["error"]["code"], -32002);
    assert!(adapter.input.try_recv().is_err());
    adapter.close().await;
}

#[tokio::test(start_paused = true)]
async fn completion_before_expiry_queues_unsubscribe_even_with_a_blocked_writer() {
    let mut adapter = Adapter::new().await;
    adapter
        .prompt(json!([{"type":"text","text":"Stall"}]))
        .await;
    adapter
        .call(
            json!(30),
            "session/close",
            json!({"sessionId":adapter.session}),
        )
        .await;
    assert_eq!(adapter.request().await.method, "turn/interrupt");
    let mut filler = 0;
    while adapter.outbound.try_send(json!({"filler":filler})).is_ok() {
        filler += 1;
    }
    adapter.events.send(Message::Notification(Notification { jsonrpc:Default::default(),method:"turn/completed".into(),params:Some(json!({"threadId":adapter.session,"turn":{"id":adapter.run,"goalId":arany::GoalId::new(),"status":"interrupted","items":[],"goal":{"status":"interrupted"}}})) })).await.unwrap();
    adapter.respond(json!({"events":[],"nextSince":null})).await;
    let close = adapter.request().await;
    assert_eq!(close.method, "thread/unsubscribe");
    for _ in 0..filler {
        adapter.output.recv().await.unwrap();
    }
    assert_eq!(adapter.next().await["id"], "prompt");
    adapter
        .events
        .send(Message::Response(Response {
            jsonrpc: Default::default(),
            id: close.id,
            result: json!({"status":"unsubscribed"}),
        }))
        .await
        .unwrap();
    assert_eq!(adapter.next().await["id"], 30);
    adapter.close().await;
}

#[tokio::test(start_paused = true)]
async fn expiry_during_blocked_reconciliation_drops_the_resumed_completion() {
    let mut adapter = Adapter::new().await;
    adapter
        .prompt(json!([{"type":"text","text":"Stall"}]))
        .await;
    adapter
        .call(
            json!(30),
            "session/close",
            json!({"sessionId":adapter.session}),
        )
        .await;
    assert_eq!(adapter.request().await.method, "turn/interrupt");
    adapter.events.send(Message::Notification(Notification { jsonrpc:Default::default(),method:"turn/completed".into(),params:Some(json!({"threadId":adapter.session,"turn":{"id":adapter.run,"goalId":arany::GoalId::new(),"status":"completed","items":[],"goal":{"status":"completed"}}})) })).await.unwrap();
    let recover = adapter.request().await;
    assert_eq!(recover.method, "events/subscribe");
    let mut filler = 0;
    while adapter.outbound.try_send(json!({"filler":filler})).is_ok() {
        filler += 1;
    }
    adapter
        .events
        .send(Message::Response(Response {
            jsonrpc: Default::default(),
            id: recover.id,
            result: json!({"events":[{
                "sequence":1,"session_id":adapter.session,"run_id":adapter.run,"agent_run_id":null,
                "kind":"MessageCommitted","payload":{"text":"Late"},"created_at_ms":0
            }],"nextSince":null}),
        }))
        .await
        .unwrap();
    tokio::time::sleep(crate::cli::exec::SHUTDOWN_GRACE + std::time::Duration::from_millis(1))
        .await;
    let close = adapter.request().await;
    assert_eq!(close.method, "thread/unsubscribe");
    for _ in 0..filler {
        adapter.output.recv().await.unwrap();
    }
    let mut prompts = 0;
    for _ in 0..2 {
        let message = adapter.next().await;
        if message["id"] == "prompt" {
            prompts += 1;
            assert_eq!(message["result"]["stopReason"], "cancelled");
        }
    }
    assert_eq!(prompts, 1);
    adapter
        .events
        .send(Message::Response(Response {
            jsonrpc: Default::default(),
            id: close.id,
            result: json!({"status":"unsubscribed"}),
        }))
        .await
        .unwrap();
    assert_eq!(adapter.next().await["id"], 30);
    tokio::task::yield_now().await;
    assert!(adapter.output.try_recv().is_err());
    adapter.close().await;
}

#[tokio::test(start_paused = true)]
async fn reused_client_prompt_id_is_not_consumed_by_expired_blocked_work() {
    let mut adapter = Adapter::new().await;
    let other = SessionId::new();
    adapter
        .call(
            json!(3),
            "session/new",
            json!({"cwd":"/var/workspace","mcpServers":[]}),
        )
        .await;
    let start = adapter.request().await;
    adapter.events.send(Message::Response(Response { jsonrpc:Default::default(), id:start.id,
        result:json!({"thread":{"id":other,"workspace":"/private/var/workspace","title":"Other","createdAtMs":0}}) })).await.unwrap();
    assert_eq!(adapter.next().await["id"], 3);
    adapter
        .prompt(json!([{"type":"text","text":"Stall"}]))
        .await;
    adapter
        .call(
            json!(30),
            "session/close",
            json!({"sessionId":adapter.session}),
        )
        .await;
    assert_eq!(adapter.request().await.method, "turn/interrupt");
    adapter.events.send(Message::Notification(Notification { jsonrpc:Default::default(),method:"turn/completed".into(),params:Some(json!({"threadId":adapter.session,"turn":{"id":adapter.run,"goalId":arany::GoalId::new(),"status":"completed","items":[],"goal":{"status":"completed"}}})) })).await.unwrap();
    let recover = adapter.request().await;
    let mut filler = 0;
    while adapter.outbound.try_send(json!({"filler":filler})).is_ok() {
        filler += 1;
    }
    adapter
        .events
        .send(Message::Response(Response {
            jsonrpc: Default::default(),
            id: recover.id,
            result: json!({"events":[{
                "sequence":1,"session_id":adapter.session,"run_id":adapter.run,"agent_run_id":null,
                "kind":"MessageCommitted","payload":{"text":"Late"},"created_at_ms":0
            }],"nextSince":null}),
        }))
        .await
        .unwrap();
    tokio::time::sleep(crate::cli::exec::SHUTDOWN_GRACE + std::time::Duration::from_millis(1))
        .await;
    let close = adapter.request().await;
    assert_eq!(close.method, "thread/unsubscribe");
    adapter
        .call(
            json!("prompt"),
            "session/prompt",
            json!({"sessionId":other,"prompt":[{"type":"text","text":"Reuse"}]}),
        )
        .await;
    let turn_start = adapter.request().await;
    assert_eq!(turn_start.method, "turn/start");
    for _ in 0..filler {
        adapter.output.recv().await.unwrap();
    }
    let mut old = 0;
    for _ in 0..2 {
        if adapter.next().await["id"] == "prompt" {
            old += 1;
        }
    }
    assert_eq!(old, 1, "only the expired prompt's own response");
    tokio::task::yield_now().await;
    let run = arany::RunId::new();
    adapter.events.send(Message::Response(Response { jsonrpc:Default::default(), id:turn_start.id,
        result:json!({"turn":{"id":run,"goalId":arany::GoalId::new(),"status":"inProgress","items":[]}}) })).await.unwrap();
    adapter.events.send(Message::Notification(Notification { jsonrpc:Default::default(),method:"turn/completed".into(),params:Some(json!({"threadId":other,"turn":{"id":run,"goalId":arany::GoalId::new(),"status":"interrupted","items":[],"goal":{"status":"interrupted"}}})) })).await.unwrap();
    for _ in 0..2 {
        adapter.respond(json!({"events":[],"nextSince":null})).await;
    }
    let reused = adapter.next().await;
    assert_eq!(
        reused["id"], "prompt",
        "the reused ID still owns the new prompt"
    );
    adapter.close().await;
    drop(close);
}

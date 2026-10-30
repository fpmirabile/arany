use super::*;

#[tokio::test]
async fn setup_edits_block_loaded_thread_prompts_without_events_or_provider_calls() {
    let _deadline = deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let provider = Arc::new(ScriptedProvider::new(Vec::new()).with_profile("openai", "gpt-5.4"));
    let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let server = tokio::spawn(server::run_with_threads(
        engine(&state),
        state.clone(),
        inbound,
        outbound,
        threads::Threads::with_provider(provider.clone()),
    ));
    input.send(initialize()).await.unwrap();
    assert!(next(&mut output).await.get("result").is_some());
    input.send(thread_start(&workspace)).await.unwrap();
    let thread: arany::SessionId =
        serde_json::from_value(next(&mut output).await["result"]["thread"]["id"].clone()).unwrap();
    let started = next(&mut output).await;
    assert_eq!(started["method"], "thread/started");
    assert_eq!(started["params"]["thread"]["id"], thread.to_string());
    let store = Store::open(StateRoot::open_existing(&state).unwrap()).unwrap();
    for uncertain in [false, true] {
        arany::test_support::record_setup_edit_state(&store, thread, uncertain).await;
        let events = store.load_session(thread).await.unwrap();
        assert_setup_blocked_requests(&input, &mut output, thread, &workspace).await;
        assert_eq!(store.load_session(thread).await.unwrap(), events);
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
    input.send(ClientMessage::Eof).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

async fn assert_setup_blocked_requests(
    input: &mpsc::Sender<ClientMessage>,
    output: &mut mpsc::Receiver<Message>,
    thread: arany::SessionId,
    workspace: &std::path::Path,
) {
    for (id, method, params) in [
        (
            3,
            "turn/start",
            json!({"threadId":thread,"input":[{"type":"text","text":"Must not disclose"}]}),
        ),
        (
            4,
            "turn/start",
            json!({"threadId":thread,"input":[{"type":"text","text":"Must not disclose"}],"goal":{"autonomous":true}}),
        ),
        (
            5,
            "goal/resume",
            json!({"threadId":thread,"goalId":uuid::Uuid::now_v7()}),
        ),
        (
            6,
            "thread/resume",
            json!({"threadId":thread,"cwd":workspace,"provider":"openai","model":"gpt-5.4"}),
        ),
    ] {
        input.send(request(id, method, params)).await.unwrap();
        let response = next(output).await;
        assert_eq!(response["id"], id);
        assert_eq!(response["error"]["data"]["reason"], "providerSetupBlocked");
    }
}

#[tokio::test]
async fn turn_start_while_a_turn_is_active_is_busy_and_interrupt_ends_the_turn_interrupted() {
    let _deadline = deadline();
    waiting_turn(TurnStop::Interrupt).await;
}
#[tokio::test]
async fn eof_during_a_turn_cancels_and_closes_the_engine() {
    let _deadline = deadline();
    waiting_turn(TurnStop::Eof).await;
}

#[tokio::test]
async fn reader_failure_during_a_turn_cancels_and_closes_the_engine() {
    let _deadline = deadline();
    waiting_turn(TurnStop::ReaderFailure).await;
}

#[tokio::test]
async fn unsubscribe_during_a_turn_cancels_and_closes_the_engine() {
    let _deadline = deadline();
    waiting_turn(TurnStop::Close).await;
}

#[tokio::test]
async fn full_inbound_queue_does_not_block_signal_or_writer_failure_cancellation() {
    let _deadline = deadline();
    for stop in [
        TurnStop::FullInboundSignal,
        TurnStop::FullInboundWriterFailure,
    ] {
        waiting_turn(stop).await;
    }
}

#[derive(Clone, Copy)]
enum TurnStop {
    Interrupt,
    Close,
    Eof,
    ReaderFailure,
    FullInboundSignal,
    FullInboundWriterFailure,
}

async fn waiting_turn(stop: TurnStop) {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let (started, mut start) = mpsc::unbounded_channel();
    let dropped = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(WaitingProvider {
        started,
        dropped: dropped.clone(),
        requests: Arc::new(Mutex::new(Vec::new())),
        identity: arany::ProviderIdentity::new("openai", "gpt-5.4", arany::Provenance::native()),
    });
    let threads = threads::Threads {
        provider: Some(provider),
        ..Default::default()
    };
    let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let reader_output = outbound.clone();
    let transport_output = outbound.clone();
    let transport_input = input.clone();
    let (full, full_queue) = tokio::sync::oneshot::channel();
    let (signal, signalled) = tokio::sync::oneshot::channel();
    let (fail_writer, writer_failed) = tokio::sync::oneshot::channel();
    let client = async {
        input.send(initialize()).await.unwrap();
        next(&mut output).await;
        input.send(thread_start(&workspace)).await.unwrap();
        let thread = next(&mut output).await["result"]["thread"]["id"].clone();
        next(&mut output).await;
        input
            .send(request(
                3,
                "turn/start",
                json!({"threadId":thread,"input":[{"type":"text","text":"Wait"}]}),
            ))
            .await
            .unwrap();
        let run = next(&mut output).await["result"]["turn"]["id"].clone();
        start.recv().await.unwrap();
        if matches!(
            stop,
            TurnStop::FullInboundSignal | TurnStop::FullInboundWriterFailure
        ) {
            for _ in 0..INBOUND_CAPACITY {
                input
                    .try_send(ClientMessage::Message(Message::Notification(
                        Notification {
                            jsonrpc: Default::default(),
                            method: "initialized".into(),
                            params: None,
                        },
                    )))
                    .unwrap();
            }
            assert_eq!(input.capacity(), 0);
            full.send(()).unwrap();
            if matches!(stop, TurnStop::FullInboundSignal) {
                signal.send(()).unwrap();
            } else {
                fail_writer.send(()).unwrap();
            }
        } else if matches!(stop, TurnStop::ReaderFailure) {
            assert_eq!(
                read_messages(FailingReader, input.clone(), reader_output).await,
                Err("protocol input failed".into())
            );
        } else if matches!(stop, TurnStop::Close) {
            input
                .send(request(5, "thread/unsubscribe", json!({"threadId":thread})))
                .await
                .unwrap();
        } else if matches!(stop, TurnStop::Eof) {
            input.send(ClientMessage::Eof).await.unwrap();
        } else {
            for (method, params) in [
                (
                    "thread/rewind/preview",
                    json!({"threadId":thread,"turnId":run}),
                ),
                (
                    "thread/rewind",
                    json!({"threadId":thread,"previewToken":"unused","restore":"conversation"}),
                ),
            ] {
                input.send(request(40, method, params)).await.unwrap();
                loop {
                    let message = next(&mut output).await;
                    if message["id"] == 40 {
                        assert_eq!(message["error"]["data"]["reason"], "busy");
                        break;
                    }
                }
            }
            input
                .send(request(
                    4,
                    "turn/start",
                    json!({"threadId":thread,"input":[{"type":"text","text":"Again"}]}),
                ))
                .await
                .unwrap();
            loop {
                let message = next(&mut output).await;
                if message["id"] == 4 {
                    assert_eq!(message["error"]["data"]["reason"], "busy");
                    break;
                }
            }
            input
                .send(request(
                    5,
                    "turn/interrupt",
                    json!({"threadId":thread,"turnId":run}),
                ))
                .await
                .unwrap();
            assert_eq!(next(&mut output).await["result"], json!({}));
        }
        loop {
            let message = next(&mut output).await;
            if message["method"] == "turn/completed" {
                assert_eq!(message["params"]["turn"]["status"], "interrupted");
                break;
            }
        }
        if matches!(stop, TurnStop::Close) {
            let closed = next(&mut output).await;
            assert_eq!(closed["id"], 5);
            assert_eq!(closed["result"], json!({"status":"unsubscribed"}));
            input
                .send(request(
                    7,
                    "turn/start",
                    json!({"threadId":thread,"input":[{"type":"text","text":"Again"}]}),
                ))
                .await
                .unwrap();
            assert_eq!(
                next(&mut output).await["error"]["data"]["reason"],
                "threadNotLoaded"
            );
        }
        if matches!(stop, TurnStop::Interrupt | TurnStop::Close) {
            input.send(ClientMessage::Eof).await.unwrap();
        }
        serde_json::from_value(thread).unwrap()
    };
    let serving =
        server::run_with_threads(engine(&state), state.clone(), inbound, outbound, threads);
    let server = async {
        if !matches!(
            stop,
            TurnStop::FullInboundSignal | TurnStop::FullInboundWriterFailure
        ) {
            return serving.await;
        }
        let (shutdown, mut stopping) = watch::channel(false);
        let mut writer_shutdown = shutdown.subscribe();
        tokio::pin!(serving);
        let gated = async {
            tokio::select! { biased;
                _ = full_queue => {
                    // Keep the full queue intact until the transport selects its stop branch.
                    let _ = stopping.wait_for(|stopping| *stopping).await;
                    serving.await
                }
                result = &mut serving => result,
            }
        };
        let reader = tokio::spawn(std::future::pending());
        let writer = tokio::spawn(async move {
            if matches!(stop, TurnStop::FullInboundWriterFailure) {
                writer_failed.await.unwrap();
                Err("protocol output failed".into())
            } else {
                let _ = writer_shutdown.wait_for(|stopping| *stopping).await;
                Ok(())
            }
        });
        serve_transport(
            gated,
            async {
                let _ = signalled.await;
            },
            Transport {
                inbound: transport_input,
                outbound: transport_output,
                reader,
                writer,
                shutdown,
            },
        )
        .await
    };
    let (result, id) = tokio::join!(server, client);
    if matches!(stop, TurnStop::FullInboundWriterFailure) {
        assert_eq!(result, Err("protocol output failed".into()));
    } else {
        result.unwrap();
    }
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    let (events, view) = closed_view(&state, id).await;
    assert_eq!(view.runs[0].status, arany::RunStatus::Cancelled);
    assert!(events.iter().any(|e| matches!(
        e.event,
        Event::RunFinished {
            disposition: arany::RunDisposition::Cancelled,
            ..
        }
    )));
}

#[test]
fn thread_capacity_refuses_new_threads_and_preserves_loaded_threads() {
    let mut threads = threads::Threads::default();
    let provider = scripted();
    let mut first = None;
    for _ in 0..threads::MAX_THREADS {
        threads
            .check_capacity(None)
            .unwrap_or_else(|_| panic!("thread capacity"));
        let id = arany::SessionId::new();
        first.get_or_insert(id);
        threads.map.insert(
            id,
            threads::LoadedThread {
                workspace: "/workspace".into(),
                providers: arany::AdmittedProviders::single(provider.clone()),
                grant: arany::ToolGrant::None,
                approval_mode: arany::ApprovalMode::Request,
                include_paths: Vec::new(),
                policy: arany::CollaborationPolicy::Single,
                goal_budget: arany::GoalBudget::default(),
                resource_settings: arany::ToolResourceSettings::default(),
                context_window: arany::ContextWindow::default(),
            },
        );
    }
    let error = threads.check_capacity(None).err().unwrap().message(None);
    assert_eq!(
        serde_json::to_value(error).unwrap()["error"]["data"]["reason"],
        "threadLimit"
    );
    assert!(threads.check_capacity(first).is_ok());
    assert_eq!(threads.map.len(), threads::MAX_THREADS);
}

#[tokio::test]
async fn failed_accepted_turns_complete_with_error_guidance_instead_of_rpc_errors() {
    for budget_stop in [false, true] {
        let _deadline = deadline();
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = root.path().join("state");
        StateRoot::admit(&state).unwrap().record(arany::RecordKind::UiPreferences).replace(&serde_json::to_vec(&json!({"preferences":{
        "goal_budget":arany::GoalBudget { max_provider_calls:Some(1), ..Default::default() },
    }})).unwrap()).unwrap();
        let provider = Arc::new(
            ScriptedProvider::new(if budget_stop {
                vec![
                    arany::ProviderOutcome::Answer(arany::Answer { text: " ".into() }),
                    arany::ProviderOutcome::Answer(arany::Answer {
                        text: "Forbidden extra call".into(),
                    }),
                ]
            } else {
                Vec::new()
            })
            .with_profile("openai", "gpt-5.4"),
        );
        if !budget_stop {
            provider
                .responses
                .lock()
                .unwrap()
                .push_back(Err(arany::ProviderError::Unavailable));
        }
        let requests = provider.requests.clone();
        let threads = threads::Threads {
            provider: Some(provider),
            ..Default::default()
        };
        let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
        let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
        let client = async {
            input.send(initialize()).await.unwrap();
            next(&mut output).await;
            input.send(thread_start(&workspace)).await.unwrap();
            let thread = next(&mut output).await["result"]["thread"]["id"].clone();
            next(&mut output).await;
            input
                .send(request(
                    3,
                    "turn/start",
                    json!({"threadId":thread,"input":[{"type":"text","text":"Try"}]}),
                ))
                .await
                .unwrap();
            assert_eq!(
                next(&mut output).await["result"]["turn"]["status"],
                "inProgress"
            );
            let completed = loop {
                let message = next(&mut output).await;
                assert!(message.get("error").is_none());
                if message["method"] == "turn/completed" {
                    break message;
                }
            };
            assert_eq!(completed["params"]["turn"]["status"], "failed");
            input.send(ClientMessage::Eof).await.unwrap();
            (serde_json::from_value(thread).unwrap(), completed)
        };
        let (result, (id, completed)) = tokio::join!(
            server::run_with_threads(engine(&state), state.clone(), inbound, outbound, threads),
            client
        );
        result.unwrap();
        let (events, view) = closed_view(&state, id).await;
        assert_eq!(
            completed["params"]["turn"]["error"]["message"],
            crate::presentation::session_run_feedback(&view, &view.runs[0]).unwrap()
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(view.goals[0].budget_closed, budget_stop);
        assert_eq!(view.goals[0].budget_refusal.is_some(), budget_stop);
        assert_eq!(
            completed["params"]["turn"]["goal"]["budgetRefusal"],
            json!(view.goals[0].budget_refusal)
        );
        assert!(events.iter().any(|e| matches!(
            e.event,
            Event::RunFinished {
                disposition: arany::RunDisposition::Failed,
                ..
            }
        )));
        assert!(view.runs[0].assistant_message.is_none());
    }
}

#[tokio::test]
async fn shutdown_grace_bounds_a_client_that_stops_reading() {
    let _deadline = deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let (started, mut start) = mpsc::unbounded_channel();
    let dropped = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(WaitingProvider {
        started,
        dropped: dropped.clone(),
        requests: Arc::new(Mutex::new(Vec::new())),
        identity: arany::ProviderIdentity::new("openai", "gpt-5.4", arany::Provenance::native()),
    });
    let threads = threads::Threads {
        provider: Some(provider),
        ..Default::default()
    };
    let engine = engine(&state);
    let mut events = engine.subscribe();
    let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let stalled = outbound.clone();
    let client = async {
        input.send(initialize()).await.unwrap();
        next(&mut output).await;
        input.send(thread_start(&workspace)).await.unwrap();
        let thread = next(&mut output).await["result"]["thread"]["id"].clone();
        next(&mut output).await;
        input
            .send(request(
                3,
                "turn/start",
                json!({"threadId":thread,"input":[{"type":"text","text":"Wait"}]}),
            ))
            .await
            .unwrap();
        next(&mut output).await;
        for _ in 0..3 {
            next(&mut output).await;
        }
        start.recv().await.unwrap();
        for _ in 0..OUTBOUND_CAPACITY {
            stalled
                .try_send(server::response(RequestId::Integer(99), json!({})))
                .unwrap();
        }
        input.send(ClientMessage::Eof).await.unwrap();
        loop {
            if let Some(arany::Tapped::Committed(event)) = events.next().await
                && matches!(event.event, Event::RunFinished { .. })
            {
                break;
            }
        }
        tokio::time::pause();
        tokio::time::advance(crate::cli::exec::SHUTDOWN_GRACE).await;
        tokio::time::resume();
        serde_json::from_value(thread).unwrap()
    };
    let (result, id) = tokio::join!(
        server::run_with_threads(engine, state.clone(), inbound, outbound, threads),
        client
    );
    assert!(
        result
            .unwrap_err()
            .contains("Run may be interrupted; check Session history")
    );
    let (_, view) = closed_view(&state, id).await;
    assert_eq!(view.runs[0].status, arany::RunStatus::Cancelled);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn shutdown_grace_includes_output_drain_without_restarting_the_timer() {
    let _deadline = deadline();
    let (shutdown, receiver) = watch::channel(false);
    let (outbound, output) = mpsc::channel(OUTBOUND_CAPACITY);
    outbound
        .send(server::response(RequestId::Integer(99), json!({})))
        .await
        .unwrap();
    drop(outbound);
    let (writer, _stalled_client) = tokio::io::duplex(1);
    let started = tokio::time::Instant::now();
    let result = shutdown_grace(receiver, async {
        shutdown.send_replace(true);
        tokio::time::sleep(Duration::from_secs(2)).await;
        write_messages(writer, output).await
    })
    .await;
    assert!(
        result
            .unwrap_err()
            .contains("Run may be interrupted; check Session history")
    );
    assert_eq!(started.elapsed(), crate::cli::exec::SHUTDOWN_GRACE);
}

#[tokio::test]
async fn oversized_resume_does_not_load_the_thread() {
    let _deadline = deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let engine = engine(&state);
    let thread = engine
        .create_session(workspace.clone(), Some("Large history".into()))
        .await
        .unwrap();
    let provider = Arc::new(
        ScriptedProvider::new(vec![
            arany::ProviderOutcome::Answer(arany::Answer {
                text: "Hello".into()
            });
            3
        ])
        .with_profile("openai", "gpt-5.4"),
    );
    for _ in 0..3 {
        let mut input =
            arany::test_support::scenario::request(&workspace, &"\u{1}".repeat(64 * 1024));
        input.session_id = Some(thread);
        input.title = None;
        input.include_paths.clear();
        engine
            .submit(
                arany::AdmittedProviders::single(provider.clone()),
                input,
                arany::RunControls::default(),
            )
            .unwrap()
            .result
            .await
            .unwrap();
    }
    let threads = threads::Threads {
        provider: Some(scripted()),
        ..Default::default()
    };
    let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let client = async {
        input.send(initialize()).await.unwrap();
        next(&mut output).await;
        input
            .send(request(
                6,
                "thread/resume",
                json!({"threadId":thread,"cwd":workspace,"provider":"openai","model":"gpt-5.4"}),
            ))
            .await
            .unwrap();
        let refusal = next(&mut output).await;
        assert_eq!(refusal["error"]["code"], -32000);
        assert_eq!(refusal["error"]["data"]["reason"], "contextTooLarge");
        input
            .send(request(
                3,
                "turn/start",
                json!({"threadId":thread,"input":[{"type":"text","text":"Again"}]}),
            ))
            .await
            .unwrap();
        assert_eq!(
            next(&mut output).await["error"]["data"]["reason"],
            "threadNotLoaded"
        );
        for id in 0..threads::MAX_THREADS {
            input.send(request(100 + id as i64, "thread/start", json!({"cwd":workspace,"provider":"openai","model":"gpt-5.4","collaboration":"single"}))).await.unwrap();
            assert!(next(&mut output).await.get("result").is_some());
            next(&mut output).await;
        }
        input.send(ClientMessage::Eof).await.unwrap();
    };
    let (result, ()) = tokio::join!(
        server::run_with_threads(engine, state.clone(), inbound, outbound, threads),
        client
    );
    result.unwrap();
    let (_, view) = closed_view(&state, thread).await;
    assert_eq!(view.runs.len(), 3);
    assert!(
        view.runs
            .iter()
            .all(|run| run.status == arany::RunStatus::Finished)
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 3);
    assert!(provider.responses.lock().unwrap().is_empty());
}

#[tokio::test]
async fn thread_settings_update_accepts_only_loaded_threads_and_closed_modes() {
    let _deadline = deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let server = server::run_with_threads(
        engine(&state),
        state.clone(),
        inbound,
        outbound,
        threads::Threads::with_provider(scripted()),
    );
    let client = async {
        input.send(initialize()).await.unwrap();
        assert!(
            next(&mut output).await["result"]["capabilities"]
                .as_array()
                .unwrap()
                .contains(&json!("thread_settings_update_v1"))
        );
        input.send(thread_start(&workspace)).await.unwrap();
        let thread = next(&mut output).await["result"]["thread"]["id"].clone();
        next(&mut output).await;
        for mode in ["default", "acceptEdits", "auto"] {
            input
                .send(request(
                    9,
                    "thread/settings/update",
                    json!({"threadId":thread,"permissionMode":mode}),
                ))
                .await
                .unwrap();
            let result = next(&mut output).await;
            assert_eq!(result["result"], json!({"permissionMode":mode}));
            let schema = protocol_schema();
            let part = json!({"$defs":schema["$defs"],"$ref":schema["methods"]["thread/settings/update"]["result"]["$ref"]});
            assert!(
                jsonschema::validator_for(&part)
                    .unwrap()
                    .is_valid(&result["result"])
            );
        }
        for mode in [
            json!("plan"),
            json!("dontAsk"),
            json!("bypassPermissions"),
            json!("x".repeat(65536)),
            json!({"auto":null}),
            json!(42),
            json!(null),
            json!("unknown"),
        ] {
            input
                .send(request(
                    9,
                    "thread/settings/update",
                    json!({"threadId":thread,"permissionMode":mode}),
                ))
                .await
                .unwrap();
            assert_eq!(next(&mut output).await["error"]["code"], -32602);
        }
        input
            .send(request(
                9,
                "thread/settings/update",
                json!({"threadId":arany::SessionId::new(),"permissionMode":"default"}),
            ))
            .await
            .unwrap();
        assert_eq!(
            next(&mut output).await["error"]["data"]["reason"],
            "threadNotLoaded"
        );
        input.send(ClientMessage::Eof).await.unwrap();
    };
    let (result, ()) = tokio::join!(server, client);
    result.unwrap();
}

#[tokio::test]
async fn session_lifecycle_releases_capacity_and_pages_only_workspace_metadata() {
    let _deadline = deadline();
    const PAGE_SIZE: usize = 2;
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let foreign = root.path().join("foreign");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&foreign).unwrap();
    let state = root.path().join("state");
    let engine = engine(&state);
    let hidden = engine
        .create_session(foreign.clone(), Some("Hidden".into()))
        .await
        .unwrap();
    let provider = scripted();
    let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let client = async {
        input.send(initialize()).await.unwrap();
        let initialized = next(&mut output).await;
        for capability in ["thread_unsubscribe_v1", "thread_list_v1"] {
            assert!(
                initialized["result"]["capabilities"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(capability))
            );
        }
        let mut ids = Vec::new();
        for _ in 0..threads::MAX_THREADS {
            input.send(thread_start(&workspace)).await.unwrap();
            ids.push(next(&mut output).await["result"]["thread"]["id"].clone());
            next(&mut output).await;
        }
        input.send(thread_start(&workspace)).await.unwrap();
        assert_eq!(
            next(&mut output).await["error"]["data"]["reason"],
            "threadLimit"
        );
        input
            .send(request(
                20,
                "thread/unsubscribe",
                json!({"threadId":ids[0]}),
            ))
            .await
            .unwrap();
        assert_eq!(next(&mut output).await["result"]["status"], "unsubscribed");
        input
            .send(request(
                20,
                "thread/unsubscribe",
                json!({"threadId":ids[0]}),
            ))
            .await
            .unwrap();
        assert_eq!(next(&mut output).await["result"]["status"], "notLoaded");
        input.send(thread_start(&workspace)).await.unwrap();
        let newest = next(&mut output).await["result"]["thread"]["id"].clone();
        next(&mut output).await;
        input
            .send(request(
                20,
                "thread/list",
                json!({"cwd":workspace,"limit":PAGE_SIZE}),
            ))
            .await
            .unwrap();
        let first = next(&mut output).await;
        let page = &first["result"];
        assert_eq!(page["data"].as_array().unwrap().len(), PAGE_SIZE);
        assert_eq!(page["data"][0]["id"], newest);
        let cursor = page["nextCursor"].as_str().unwrap();
        assert!(cursor.len() <= 128);
        input
            .send(request(
                20,
                "thread/list",
                json!({"cwd":workspace,"cursor":cursor,"limit":100}),
            ))
            .await
            .unwrap();
        let second = next(&mut output).await;
        assert_eq!(
            second["result"]["data"].as_array().unwrap().len(),
            threads::MAX_THREADS + 1 - PAGE_SIZE
        );
        assert!(second["result"]["nextCursor"].is_null());
        let mut listed = std::collections::BTreeSet::new();
        for entry in page["data"]
            .as_array()
            .unwrap()
            .iter()
            .chain(second["result"]["data"].as_array().unwrap())
        {
            assert!(listed.insert(entry["id"].as_str().unwrap().to_owned()));
            assert_eq!(entry["cwd"], json!(workspace));
            assert!(entry["updatedAt"].as_str().unwrap().ends_with('Z'));
            assert_eq!(entry.as_object().unwrap().len(), 5);
        }
        assert!(
            listed.contains(ids[0].as_str().unwrap()),
            "closing retains durable history"
        );
        for response in [&first, &second] {
            let encoded = serde_json::to_string(response).unwrap();
            assert!(!encoded.contains(foreign.to_str().unwrap()));
            assert!(!encoded.contains(state.to_str().unwrap()));
            assert!(!encoded.contains(&hidden.to_string()));
            let schema = protocol_schema();
            let part = json!({"$schema":schema["$schema"],"$defs":schema["$defs"],"$ref":schema["methods"]["thread/list"]["result"]["$ref"]});
            assert!(
                jsonschema::validator_for(&part)
                    .unwrap()
                    .is_valid(&response["result"])
            );
        }
        for params in [
            json!({"cwd":workspace,"limit":0}),
            json!({"cwd":workspace,"limit":101}),
            json!({"cwd":workspace,"cursor":"bad"}),
            json!({"cwd":workspace,"cursor":"x".repeat(129)}),
            json!({"cwd":foreign,"cursor":cursor}),
            json!({"cwd":"relative"}),
            json!({"cwd":root.path()}),
            json!({"cwd":workspace,"sortKey":"created_at"}),
        ] {
            input
                .send(request(20, "thread/list", params))
                .await
                .unwrap();
            assert_eq!(next(&mut output).await["error"]["code"], -32602);
        }
        let anchor = serde_json::from_value(page["data"][1]["id"].clone()).unwrap();
        arany::rename_session(
            StateRoot::open_existing(&state).unwrap(),
            workspace.clone(),
            anchor,
            "Changed".into(),
        )
        .await
        .unwrap();
        input
            .send(request(
                20,
                "thread/list",
                json!({"cwd":workspace,"cursor":cursor}),
            ))
            .await
            .unwrap();
        assert_eq!(
            next(&mut output).await["error"]["data"]["reason"],
            "invalidParams",
            "a changed anchor must not silently skip history"
        );
        input
            .send(request(
                20,
                "thread/list",
                json!({"cwd":foreign,"limit":100}),
            ))
            .await
            .unwrap();
        let other = next(&mut output).await;
        assert_eq!(other["result"]["data"].as_array().unwrap().len(), 1);
        assert_eq!(other["result"]["data"][0]["id"], json!(hidden));
        input.send(ClientMessage::Eof).await.unwrap();
        serde_json::from_value(ids[0].clone()).unwrap()
    };
    let (result, closed) = tokio::join!(
        server::run_with_threads(
            engine,
            state.clone(),
            inbound,
            outbound,
            threads::Threads::with_provider(provider.clone())
        ),
        client
    );
    result.unwrap();
    assert!(provider.requests.lock().unwrap().is_empty());
    let (events, view) = closed_view(&state, closed).await;
    assert_eq!(events.len(), 1);
    assert!(view.runs.is_empty());
}

use super::*;
use arany::test_support::scenario::{ScriptedProvider, WaitingProvider};
use arany::{Event, Provider, RunStatus, SessionId, Store};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

type Reader = BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>;
type Writer = tokio::io::WriteHalf<tokio::io::DuplexStream>;

fn schema() -> Value {
    serde_json::from_str(include_str!("../fixtures/acp/schema.json")).unwrap()
}

fn validate(value: &Value) {
    let schema = schema();
    assert!(
        jsonschema::validator_for(&schema).unwrap().is_valid(value),
        "{value}"
    );
    let definition = if value["method"] == "session/request_permission" {
        "RequestPermissionRequest"
    } else if value["method"] == "$/cancel_request" {
        "CancelRequestNotification"
    } else if value["method"] == "session/update" {
        "SessionNotification"
    } else if value.get("error").is_some() {
        "Error"
    } else if value["result"].get("protocolVersion").is_some() {
        "InitializeResponse"
    } else if value["result"].get("sessions").is_some() {
        "ListSessionsResponse"
    } else if value["result"].get("sessionId").is_some() {
        "NewSessionResponse"
    } else if value["result"].get("configOptions").is_some() {
        "SetSessionConfigOptionResponse"
    } else if value["result"].get("stopReason").is_some() {
        "PromptResponse"
    } else {
        "AuthenticateResponse"
    };
    let payload = if matches!(
        definition,
        "SessionNotification" | "RequestPermissionRequest" | "CancelRequestNotification"
    ) {
        &value["params"]
    } else if definition == "Error" {
        &value["error"]
    } else {
        &value["result"]
    };
    let part = json!({"$schema":schema["$schema"], "$defs":schema["$defs"], "$ref":format!("#/$defs/{definition}")});
    assert!(
        jsonschema::validator_for(&part).unwrap().is_valid(payload),
        "{definition}: {value}"
    );
    assert!(encode_line(value).unwrap().len() <= MAX_RPC_LINE_BYTES);
    assert!(
        value.get("method").is_none_or(|m| matches!(
            m.as_str(),
            Some("session/update" | "session/request_permission" | "$/cancel_request")
        )),
        "unexpected client call: {value}"
    );
}

async fn next(reader: &mut Reader) -> Value {
    let mut line = String::new();
    // Guarded turns on the macOS debug runner can exceed ten seconds per line.
    assert!(
        tokio::time::timeout(Duration::from_secs(60), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap()
            > 0
    );
    let value = serde_json::from_str(&line).unwrap();
    validate(&value);
    value
}

async fn send(writer: &mut Writer, id: Value, method: &str, params: Value) -> Value {
    let value = json!({"jsonrpc":"2.0", "id": id, "method":method, "params":params});
    writer
        .write_all(&encode_line(&value).unwrap())
        .await
        .unwrap();
    value
}

fn config(state: &std::path::Path) -> Config {
    Config {
        state: state.into(),
        provider: ProviderArg::Openai,
        model: "gpt-5.4".into(),
        effort: None,
        collaboration: Collaboration::Single,
        max_active_children: None,
        roster: None,
        tools: false,
        permission_mode: PermissionMode::Default,
        authenticated: Some(true),
    }
}

fn start(
    state: &std::path::Path,
    provider: Arc<dyn Provider>,
) -> (Writer, Reader, tokio::task::JoinHandle<Result<(), String>>) {
    start_configured(state, provider, false)
}

fn start_configured(
    state: &std::path::Path,
    provider: Arc<dyn Provider>,
    tools: bool,
) -> (Writer, Reader, tokio::task::JoinHandle<Result<(), String>>) {
    start_with_guard(state, provider, tools, std::env::current_exe().unwrap())
}

fn start_with_guard(
    state: &std::path::Path,
    provider: Arc<dyn Provider>,
    tools: bool,
    guard: PathBuf,
) -> (Writer, Reader, tokio::task::JoinHandle<Result<(), String>>) {
    let mut config = config(state);
    config.tools = tools;
    start_with_config(config, provider, guard)
}

fn start_with_config(
    config: Config,
    provider: Arc<dyn Provider>,
    guard: PathBuf,
) -> (Writer, Reader, tokio::task::JoinHandle<Result<(), String>>) {
    let state = &config.state;
    let engine = arany::test_support::engine::open(
        StateRoot::admit(state).expect("private State"),
        EngineConfig {
            guard_executable: Some(guard),
            ..Default::default()
        },
    )
    .unwrap();
    let state_dir = state.to_path_buf();
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(client);
    let (input, output) = tokio::io::split(server);
    let (inbound, receiver) = mpsc::channel(serve::INBOUND_CAPACITY);
    let (events, event_rx) = mpsc::channel(serve::OUTBOUND_CAPACITY);
    let (outbound, writer_rx) = mpsc::channel(serve::OUTBOUND_CAPACITY);
    let shared = Arc::new(Mutex::new(translate::State::default()));
    let (shutdown, _) = watch::channel(false);
    let reader_task = tokio::spawn(read_messages(
        input,
        config,
        shared.clone(),
        inbound.clone(),
        outbound.clone(),
        shutdown.clone(),
    ));
    let writer_task = tokio::spawn(serve::write_messages(output, writer_rx));
    let translation_input = inbound.clone();
    let translation_output = outbound.clone();
    let stopping = shutdown.clone();
    let task = tokio::spawn(async move {
        let serving = async move {
            let server = run_server(
                engine,
                state_dir,
                receiver,
                events,
                stopping.clone(),
                serve::threads::Threads::with_provider(provider),
            );
            let translator = translate::output(
                event_rx,
                translation_input,
                translation_output,
                shared,
                stopping.subscribe(),
            );
            {
                let (server, translator) = tokio::join!(server, translator);
                server.and(translator)
            }
        };
        serve::serve_transport(
            serving,
            std::future::pending(),
            inbound,
            outbound,
            reader_task,
            writer_task,
            shutdown,
        )
        .await
    });
    (writer, BufReader::new(reader), task)
}

async fn initialize(writer: &mut Writer, reader: &mut Reader) {
    send(
        writer,
        json!(1),
        "initialize",
        json!({"protocolVersion":1,"clientInfo":{"name":"test","version":"1"},
        "clientCapabilities":{"fs":{"readTextFile":true,"writeTextFile":true},"terminal":true}}),
    )
    .await;
    let initialized = next(reader).await;
    assert_eq!(initialized["result"]["protocolVersion"], 1);
    assert_eq!(
        initialized["result"]["agentCapabilities"]["loadSession"],
        true
    );
    assert_eq!(
        initialized["result"]["agentCapabilities"]["sessionCapabilities"],
        json!({"resume":{},"close":{},"list":{}})
    );
}

fn validate_session_result(value: &Value, method: &str) {
    validate_session_result_schema(value, method);
    assert_mode_state(&value["result"], "default");
}

fn validate_session_result_schema(value: &Value, method: &str) {
    let schema = schema();
    let definition = match method {
        "session/load" => "LoadSessionResponse",
        "session/resume" => "ResumeSessionResponse",
        _ => panic!("unexpected session method"),
    };
    let part = json!({"$schema":schema["$schema"], "$defs":schema["$defs"], "$ref":format!("#/$defs/{definition}")});
    assert!(
        jsonschema::validator_for(&part)
            .unwrap()
            .is_valid(&value["result"])
    );
}

async fn session(
    writer: &mut Writer,
    reader: &mut Reader,
    workspace: &std::path::Path,
) -> arany::SessionId {
    send(
        writer,
        json!(2),
        "session/new",
        json!({"cwd":workspace,"mcpServers":[]}),
    )
    .await;
    serde_json::from_value(next(reader).await["result"]["sessionId"].clone()).unwrap()
}

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

fn scripted() -> Arc<ScriptedProvider> {
    Arc::new(
        ScriptedProvider::new(vec![arany::ProviderOutcome::Answer(arany::Answer {
            text: "Hello".into(),
        })])
        .with_profile("openai", "gpt-5.4"),
    )
}

#[tokio::test]
async fn committed_text_turn_matches_upstream_schema_and_golden() {
    let _deadline = crate::cli::serve::tests::deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let provider = scripted();
    let (mut writer, mut reader, task) = start(&state, provider.clone());
    let mut transcript = Vec::new();
    transcript.push(json!({"client":send(&mut writer, json!(1), "initialize", json!({"protocolVersion":1,"clientInfo":{"name":"test","version":"1"}})).await}));
    transcript.push(json!({"server":next(&mut reader).await}));
    transcript.push(json!({"client":send(&mut writer, json!(2), "session/new", json!({"cwd":workspace,"mcpServers":[]})).await}));
    let new = next(&mut reader).await;
    let session: arany::SessionId =
        serde_json::from_value(new["result"]["sessionId"].clone()).unwrap();
    transcript.push(json!({"server":new}));
    transcript.push(json!({"client":send(&mut writer, json!("prompt"), "session/prompt", json!({"sessionId":session,"prompt":[{"type":"text","text":"Hi"}]})).await}));
    let update = next(&mut reader).await;
    assert_eq!(update["params"]["update"]["content"]["text"], "Hello");
    transcript.push(json!({"server":update}));
    let completion = next(&mut reader).await;
    assert_eq!(completion["result"]["stopReason"], "end_turn");
    transcript.push(json!({"server":completion}));
    writer.shutdown().await.unwrap();
    assert_eq!(reader.read(&mut [0; 1]).await.unwrap(), 0);
    task.await.unwrap().unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    let events = store.load_session(session).await.unwrap();
    let view = store.load_view(session).await.unwrap().unwrap();
    store.close().await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.event, Event::MessageCommitted { text, .. } if text == "Hello"))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, Event::RunFinished { .. }))
    );
    assert_eq!(view.runs.len(), 1);
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert!(provider.responses.lock().unwrap().is_empty());
    let actual: Vec<Value> = transcript
        .into_iter()
        .map(|value| {
            let bytes = serde_json::to_string(&value)
                .unwrap()
                .replace(&session.to_string(), "$session")
                .replace(&workspace.to_string_lossy().to_string(), "$workspace")
                .replace(env!("CARGO_PKG_VERSION"), "$version");
            serde_json::from_str(&bytes).unwrap()
        })
        .collect();
    let expected: Vec<Value> = include_str!("../fixtures/acp/basic.jsonl")
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(actual, expected);
}

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
async fn malformed_oversized_and_hostile_params_recover_over_stdio() {
    let _deadline = crate::cli::serve::tests::deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let (mut writer, mut reader, task) = start(&state, scripted());
    writer.write_all(b"{broken\n").await.unwrap();
    assert_eq!(next(&mut reader).await["error"]["code"], -32700);
    writer
        .write_all(&vec![b'x'; MAX_RPC_LINE_BYTES])
        .await
        .unwrap();
    writer.write_all(b"\n").await.unwrap();
    assert_eq!(
        next(&mut reader).await["error"]["data"]["reason"],
        "messageTooLarge"
    );
    for version in [json!("1"), json!(-1), json!(65536)] {
        send(
            &mut writer,
            json!(1),
            "initialize",
            json!({"protocolVersion":version}),
        )
        .await;
        assert_eq!(next(&mut reader).await["error"]["code"], -32602);
    }
    send(
        &mut writer,
        json!(1),
        "initialize",
        json!({"protocolVersion":99,"_meta":{"tools":true}}),
    )
    .await;
    assert_eq!(next(&mut reader).await["result"]["protocolVersion"], 1);
    send(
        &mut writer,
        json!(1),
        "initialize",
        json!({"protocolVersion":1}),
    )
    .await;
    assert_eq!(
        next(&mut reader).await["error"]["data"]["reason"],
        "alreadyInitialized"
    );
    for params in [
        json!({"cwd":"relative","mcpServers":[]}),
        json!({"cwd":workspace,"mcpServers":[],"additionalDirectories":["/tmp"]}),
    ] {
        send(&mut writer, json!(2), "session/new", params).await;
        assert_eq!(next(&mut reader).await["error"]["code"], -32602);
    }
    let session = session(&mut writer, &mut reader, &workspace).await;
    for prompt in [
        json!([{"type":"image","data":""}]),
        json!([{"type":"text","text":""}]),
        json!([{"type":"text","text":"x".repeat(65537)}]),
        json!(vec![json!({"type":"text","text":"x"}); 65]),
    ] {
        send(
            &mut writer,
            json!(3),
            "session/prompt",
            json!({"sessionId":session,"prompt":prompt}),
        )
        .await;
        assert_eq!(next(&mut reader).await["error"]["code"], -32602);
    }
    send(&mut writer, json!(9), "session/delete", json!({})).await;
    assert_eq!(next(&mut reader).await["error"]["code"], -32601);
    writer.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn transport_auth_and_schema_fixture_without_private_state() {
    let _deadline = crate::cli::serve::tests::deadline();
    assert_eq!(
        Sha256::digest(include_bytes!("../fixtures/acp/schema.json"))
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>(),
        "4bcd1d2051ff7ed379bd9b234fb9b09c5ab9245478698e9fab805bd9b52a33a6"
    );
    validate(&wire::response(RequestId::Integer(1), wire::initialized()));
    let mut config = config(std::path::Path::new("/unopened-state"));
    config.authenticated = Some(false);
    let (input, server) = tokio::io::duplex(64 * 1024);
    let (mut writer, mut reader) = (input, BufReader::new(server));
    writer.write_all(b"{\n").await.unwrap();
    let line = serve::read_bounded_line(&mut reader)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(wire::decode(&line).is_err());
    let (serve_tx, mut serve_rx) = mpsc::channel(64);
    let (output, mut output_rx) = mpsc::channel(256);
    let shared = Arc::new(Mutex::new(translate::State::default()));
    for (method, params, expected) in [
        ("session/prompt", json!({}), "notInitialized"),
        ("initialize", json!({"protocolVersion":1}), ""),
        ("authenticate", json!({"methodId":"other"}), "invalidParams"),
        (
            "authenticate",
            json!({"methodId":"arany-setup"}),
            "auth_required",
        ),
        (
            "session/prompt",
            json!({"sessionId":"01900000-0000-7000-8000-000000000001","prompt":[{"type":"text","text":"Hi"}]}),
            "auth_required",
        ),
    ] {
        let bytes =
            encode_line(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})).unwrap();
        translate::input(
            wire::decode(&bytes).unwrap(),
            &config,
            &shared,
            &serve_tx,
            &output,
        )
        .await
        .unwrap();
        if expected.is_empty() {
            assert!(serve_rx.recv().await.is_some());
        } else {
            let message = output_rx.recv().await.unwrap();
            validate(&message);
            assert_eq!(message["error"]["data"]["reason"], expected);
        }
    }
    for (prompt, expected) in [
        (
            json!([{"type":"resource_link","name":"é","uri":"x".repeat(65529)}]),
            "auth_required",
        ),
        (
            json!([{"type":"resource_link","name":"é","uri":"x".repeat(65530)}]),
            "invalidParams",
        ),
        (
            json!([{"type":"resource_link","name":"é","uri":"x".repeat(65529)}, {"type":"text","text":"x"}]),
            "invalidParams",
        ),
        (
            json!([{"type":"resource_link","name":"missing URI"}]),
            "invalidParams",
        ),
        (
            json!([{"type":"resource_link","uri":"file:///missing-name"}]),
            "invalidParams",
        ),
        (
            json!(vec![
                json!({"type":"resource_link","name":"a","uri":"b"});
                65
            ]),
            "invalidParams",
        ),
    ] {
        let bytes = encode_line(&json!({"jsonrpc":"2.0","id":2,"method":"session/prompt",
            "params":{"sessionId":"01900000-0000-7000-8000-000000000001","prompt":prompt}}))
        .unwrap();
        translate::input(
            wire::decode(&bytes).unwrap(),
            &config,
            &shared,
            &serve_tx,
            &output,
        )
        .await
        .unwrap();
        let message = output_rx.recv().await.unwrap();
        validate(&message);
        assert_eq!(message["error"]["data"]["reason"], expected);
    }
    assert!(
        serve_rx.try_recv().is_err(),
        "refused prompts never reach serve"
    );
    assert!(wire::decode(br#"{"jsonrpc":"2.0","id":123,"result":null}"#).is_ok());
    assert!(
        wire::decode(
            br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"Unknown request"}}"#
        )
        .is_ok()
    );
    for envelope in [
        json!({"jsonrpc":"1.0","id":1,"method":"initialize"}),
        json!({"id":1,"method":"initialize"}),
        json!({"jsonrpc":"2.0","id":1,"method":"x","result":{}}),
        json!([]),
    ] {
        assert!(wire::decode(&serde_json::to_vec(&envelope).unwrap()).is_err());
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
    events.send(reply(6, json!({"events":[{"sequence":20,"session_id":session.to_string(),"run_id":run.to_string(),"agent_run_id":null,"kind":"MessageCommitted","event_version":arany::test_support::payload_version("MessageCommitted"),"payload":{"run_id":run,"text":"Recovered"},"created_at_ms":0}],"nextSince":null}))).await.unwrap();
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
    events.send(Message::Notification(Notification { jsonrpc:Default::default(), method:"item/completed".into(), params:Some(json!({"threadId":session,"turnId":run,"completedAtMs":0,"item":{"type":"agentMessage","id":format!("{run}:answer"),"text":"Committed","phase":"final_answer"}})) })).await.unwrap();
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
    assert_eq!(answer["params"]["update"]["content"]["text"], "Committed");
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
async fn frame_recovery_uses_real_codec_and_stdio_without_private_state() {
    let _deadline = crate::cli::serve::tests::deadline();
    let (client, product) = tokio::io::duplex(64 * 1024);
    let (reader, mut source) = tokio::io::split(client);
    let (input, output) = tokio::io::split(product);
    let (serve_tx, mut serve_rx) = mpsc::channel(64);
    let (outbound, writer_rx) = mpsc::channel(256);
    let (shutdown, _) = watch::channel(false);
    let mut config = config(std::path::Path::new("/state"));
    config.authenticated = Some(false);
    let reader_task = tokio::spawn(read_messages(
        input,
        config,
        Arc::default(),
        serve_tx,
        outbound,
        shutdown,
    ));
    let writer_task = tokio::spawn(serve::write_messages(output, writer_rx));
    let producer = tokio::spawn(async move {
        source.write_all(b"{broken\n").await.unwrap();
        source
            .write_all(&vec![b'x'; MAX_RPC_LINE_BYTES])
            .await
            .unwrap();
        source.write_all(b"\n").await.unwrap();
        source.write_all(&encode_line(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}})).unwrap()).await.unwrap();
        source.write_all(&encode_line(&json!({"jsonrpc":"2.0","id":2,"method":"authenticate","params":{"methodId":"arany-setup"}})).unwrap()).await.unwrap();
        source.shutdown().await.unwrap();
    });
    let mut reader = BufReader::new(reader);
    assert_eq!(next(&mut reader).await["error"]["code"], -32700);
    assert_eq!(
        next(&mut reader).await["error"]["data"]["reason"],
        "messageTooLarge"
    );
    assert_eq!(next(&mut reader).await["error"]["code"], -32000);
    assert!(matches!(
        serve_rx.recv().await,
        Some(serve::server::ClientMessage::Message(Message::Request(_)))
    ));
    assert!(matches!(
        serve_rx.recv().await,
        Some(serve::server::ClientMessage::Eof)
    ));
    producer.await.unwrap();
    reader_task.await.unwrap().unwrap();
    writer_task.await.unwrap().unwrap();
    assert_eq!(reader.read(&mut [0; 1]).await.unwrap(), 0);
}

struct Adapter {
    mcp_probe: tempfile::TempDir,
    config: Config,
    shared: translate::Shared,
    serve: mpsc::Sender<serve::server::ClientMessage>,
    input: mpsc::Receiver<serve::server::ClientMessage>,
    events: mpsc::Sender<Message>,
    outbound: mpsc::Sender<Value>,
    output: mpsc::Receiver<Value>,
    worker: tokio::task::JoinHandle<Result<(), String>>,
    session: SessionId,
    run: arany::RunId,
}

impl Adapter {
    async fn new() -> Self {
        let shared: translate::Shared = Arc::default();
        let (serve, input) = mpsc::channel(64);
        let (events, receiver) = mpsc::channel(256);
        let (outbound, output) = mpsc::channel(256);
        let config = config(std::path::Path::new("/state"));
        let (_stop, stopping) = watch::channel(false);
        let worker = tokio::spawn(translate::output(
            receiver,
            serve.clone(),
            outbound.clone(),
            shared.clone(),
            stopping,
        ));
        let mut adapter = Self {
            mcp_probe: tempfile::tempdir().unwrap(),
            config,
            shared,
            serve,
            input,
            events,
            outbound,
            output,
            worker,
            session: SessionId::new(),
            run: arany::RunId::new(),
        };
        adapter
            .call(json!(1), "initialize", json!({"protocolVersion":1}))
            .await;
        adapter.respond(json!({})).await;
        assert_eq!(
            adapter.next().await["result"]["agentCapabilities"]["promptCapabilities"]["image"],
            true
        );
        adapter.config.tools = true;
        adapter.call(json!(2), "session/new", json!({"cwd":"/var/workspace","mcpServers":[{"name":"hostile","command":"/bin/sh","args":["-c","printf spawned > \"$1\"","acp-mcp-probe",adapter.mcp_probe.path().join("spawned")],"env":[]}]})).await;
        let start = adapter.request().await;
        assert_eq!(start.params.as_ref().unwrap()["tools"], true);
        adapter.events.send(Message::Response(Response { jsonrpc:Default::default(), id:start.id,
            result:json!({"thread":{"id":adapter.session,"workspace":"/private/var/workspace","title":"Test","createdAtMs":0}}) })).await.unwrap();
        let created = adapter.next().await;
        assert_eq!(created["result"]["sessionId"], adapter.session.to_string());
        assert_mode_state(&created["result"], "default");
        adapter
    }

    async fn call(&self, id: Value, method: &str, params: Value) {
        let message = wire::decode(
            &encode_line(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .unwrap(),
        )
        .unwrap();
        translate::input(
            message,
            &self.config,
            &self.shared,
            &self.serve,
            &self.outbound,
        )
        .await
        .unwrap();
    }

    async fn request(&mut self) -> Request {
        let Some(serve::server::ClientMessage::Message(Message::Request(request))) =
            self.input.recv().await
        else {
            panic!("expected serve request")
        };
        request
    }

    async fn respond(&mut self, result: Value) {
        let request = self.request().await;
        self.events
            .send(Message::Response(Response {
                jsonrpc: Default::default(),
                id: request.id,
                result,
            }))
            .await
            .unwrap();
    }

    async fn next(&mut self) -> Value {
        let value = self.output.recv().await.unwrap();
        validate(&value);
        value
    }

    async fn prompt(&mut self, prompt: Value) -> Vec<UserInput> {
        self.call(
            json!("prompt"),
            "session/prompt",
            json!({"sessionId":self.session,"prompt":prompt}),
        )
        .await;
        let request = self.request().await;
        let params: TurnStartParams = serde_json::from_value(request.params.unwrap()).unwrap();
        self.events.send(Message::Response(Response { jsonrpc:Default::default(), id:request.id,
            result:json!({"turn":{"id":self.run,"goalId":arany::GoalId::new(),"status":"inProgress","items":[]}}) })).await.unwrap();
        let watch = self.request().await;
        assert_eq!(watch.method, "events/subscribe");
        self.events
            .send(Message::Response(Response {
                jsonrpc: Default::default(),
                id: watch.id,
                result: json!({"events":[],"nextSince":null}),
            }))
            .await
            .unwrap();
        params.input
    }

    fn intent(&self, call: arany::ToolCall) -> arany::EffectIntent {
        arany::EffectIntent {
            host_command: None,
            host: None,
            id: uuid::Uuid::now_v7(),
            run_id: self.run,
            agent_run_id: arany::AgentRunId::new(),
            target: arany::worker::EffectTarget::Workspace,
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call,
            limits: arany::ToolLimits::default(),
            use_count: 1,
        }
    }

    async fn permission(&self, intent: &arany::EffectIntent, id: i64) {
        self.events.send(Message::Request(Request { jsonrpc:Default::default(),id:RequestId::Integer(id),method:"item/tool/requestApproval".into(),params:Some(json!({
            "threadId":self.session,"turnId":self.run,"itemId":intent.id,"startedAtMs":0,"intent":intent,
            "lines":["Review <script>unsafe</script>\u{001b}[2J", "```\n![untrusted](https://invalid.example)"]
        })) })).await.unwrap();
    }

    async fn item(
        &self,
        intent: &arany::EffectIntent,
        observation: Option<&arany::ToolObservation>,
    ) {
        let item = serve::items::tool_item(intent, observation, false);
        self.events
            .send(Message::Notification(Notification {
                jsonrpc: Default::default(),
                method: if observation.is_none() {
                    "item/started"
                } else {
                    "item/completed"
                }
                .into(),
                params: Some(json!({
                    "threadId":self.session,"turnId":self.run,"item":item,
                    if observation.is_none() {"startedAtMs"} else {"completedAtMs"}:0,
                })),
            }))
            .await
            .unwrap();
    }

    async fn close(self) {
        drop(self.events);
        self.worker.await.unwrap().unwrap();
        assert!(
            !self.mcp_probe.path().join("spawned").exists(),
            "editor MCP commands must never execute"
        );
    }
}

#[tokio::test]
async fn move_live_and_permission_projection_preserves_file_change_details() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    adapter
        .prompt(json!([{"type":"text","text":"Review changes"}]))
        .await;
    let intent = adapter.intent(arany::ToolCall::Move {
        source: "source\nname.txt".into(),
        destination: "destination.txt".into(),
        expected_digest: "a".repeat(64),
    });

    adapter.item(&intent, None).await;
    let started = adapter.next().await["params"]["update"].clone();
    assert_eq!(started["kind"], "edit");
    assert_eq!(started["title"], "Change files source name.txt");
    assert_eq!(
        started["locations"][0]["path"],
        "/var/workspace/source\nname.txt"
    );
    assert_eq!(
        started["locations"][1]["path"],
        "/var/workspace/destination.txt"
    );

    adapter.permission(&intent, 10).await;
    let request = adapter.next().await;
    let permission = request["params"]["toolCall"].clone();
    assert_eq!(permission["kind"], "edit");
    assert_eq!(permission["title"], "Change files source name.txt");
    assert_eq!(
        permission["locations"][0]["path"],
        "/var/workspace/source\nname.txt"
    );
    assert_eq!(
        permission["locations"][1]["path"],
        "/var/workspace/destination.txt"
    );
    let uncertain = arany::ToolObservation {
        intent: intent.clone(),
        disposition: arany::ToolDisposition::Uncertain,
        enforcement_cause: None,
        limit_cause: None,
        output: String::new(),
        attestation: None,
        approval_cause: None,
        cache_update: arany::CacheUpdate::NotApplicable,
    };
    adapter.item(&intent, Some(&uncertain)).await;
    let withdrawal = adapter.next().await;
    assert_eq!(withdrawal["method"], "$/cancel_request");
    assert_eq!(withdrawal["params"]["requestId"], request["id"]);
    let completed = adapter.next().await["params"]["update"].clone();
    assert_eq!(completed["kind"], "edit");
    assert_eq!(completed["status"], "failed");
    assert_eq!(completed["_meta"]["arany.status"], "uncertain");
    adapter.close().await;
}

#[tokio::test]
async fn move_completion_preserves_denial_projection() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    adapter
        .prompt(json!([{"type":"text","text":"Review changes"}]))
        .await;
    let intent = adapter.intent(arany::ToolCall::Move {
        source: "denied.txt".into(),
        destination: "denied-new.txt".into(),
        expected_digest: "b".repeat(64),
    });
    adapter.item(&intent, None).await;
    let _ = adapter.next().await;
    let observation = arany::ToolObservation {
        intent: intent.clone(),
        disposition: arany::ToolDisposition::Denied,
        enforcement_cause: None,
        limit_cause: None,
        output: String::new(),
        attestation: None,
        approval_cause: None,
        cache_update: arany::CacheUpdate::NotApplicable,
    };
    adapter.item(&intent, Some(&observation)).await;
    let update = adapter.next().await["params"]["update"].clone();
    assert_eq!(update["kind"], "edit");
    assert_eq!(update["status"], "failed");
    assert!(
        update["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("denied")
    );
    adapter.close().await;
}

#[tokio::test]
async fn move_history_projection_preserves_file_change_details() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut replay = Adapter::new().await;
    let intent = replay.intent(arany::ToolCall::Move {
        source: "old.txt".into(),
        destination: "new.txt".into(),
        expected_digest: "b".repeat(64),
    });
    replay
        .call(
            json!(3),
            "session/load",
            json!({"sessionId":replay.session,"cwd":"/var/workspace","mcpServers":[]}),
        )
        .await;
    let request = replay.request().await;
    assert_eq!(request.method, "thread/resume");
    replay
        .events
        .send(Message::Response(Response {
            jsonrpc: Default::default(),
            id: request.id,
            result: json!({
                "thread":{"id":replay.session,"title":"History","workspace":"/private/var/workspace","createdAtMs":0},
                "turns":[{"id":replay.run,"goalId":arany::GoalId::new(),"status":"interrupted","items":[
                    {"type":"userMessage","id":format!("{}:user", replay.run),"text":"Move","images":[]},
                    serve::items::tool_item(&intent, None, true)
                ]}]
            }),
        }))
        .await
        .unwrap();
    let _ = replay.next().await;
    let started = replay.next().await["params"]["update"].clone();
    assert_eq!(started["sessionUpdate"], "tool_call");
    assert_eq!(started["kind"], "edit");
    assert_eq!(started["title"], "Change files old.txt");
    assert_eq!(started["locations"][0]["path"], "/var/workspace/old.txt");
    assert_eq!(started["locations"][1]["path"], "/var/workspace/new.txt");
    let completed = replay.next().await["params"]["update"].clone();
    assert_eq!(completed["sessionUpdate"], "tool_call_update");
    assert_eq!(completed["status"], "failed");
    assert_eq!(completed["_meta"]["arany.status"], "uncertain");
    assert_eq!(replay.next().await["id"], 3);
    replay.close().await;
}

#[tokio::test]
async fn gh_pr_list_projects_as_bounded_read() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    adapter
        .prompt(json!([{"type":"text","text":"List pull requests"}]))
        .await;
    let repository = "github.com/team/\u{001b}[2J\nproject";
    let intent = adapter.intent(arany::ToolCall::GhPrList {
        repository: repository.into(),
        state: arany::PrState::Open,
        base: None,
        head: None,
        limit: 10,
    });
    adapter.item(&intent, None).await;
    let update = adapter.next().await["params"]["update"].clone();
    assert_eq!(update["kind"], "read");
    assert_eq!(
        update["title"],
        "List GitHub pull requests github.com/team/\\u{001b}[2J project"
    );
    assert!(
        !update["title"]
            .as_str()
            .unwrap()
            .chars()
            .any(char::is_control)
    );
    assert_eq!(update["rawInput"]["operation"], "gh_pr_list");
    assert_eq!(update["rawInput"]["repository"], repository);
    assert_eq!(update["locations"], json!([]));
    adapter.close().await;
}

#[tokio::test]
async fn optional_surfaces_use_closed_budget_reasons_and_bounded_committed_plans() {
    let _deadline = crate::cli::serve::tests::deadline();
    let exhausted = |limit| json!({"reason":"goalExhausted","limit":limit});
    // (serve `goal.stopped`, turn and Goal status, ACP stopReason or error reason)
    for (stopped, status, expected) in [
        (exhausted("calls"), "failed", "max_turn_requests"),
        (exhausted("runs"), "completed", "max_turn_requests"),
        (exhausted("activeTime"), "failed", "max_turn_requests"),
        (exhausted("tokens"), "failed", "max_tokens"),
        (Value::Null, "completed", "end_turn"),
        (Value::Null, "failed", "turnFailed"),
        (json!({"reason":"paused"}), "interrupted", "cancelled"),
        (json!({"reason":"cancelled"}), "interrupted", "cancelled"),
        (
            json!({"reason":"stoppedByHook"}),
            "interrupted",
            "stoppedByHook",
        ),
        (
            json!({"reason":"uncertain"}),
            "interrupted",
            "effectUncertain",
        ),
        (
            json!({"reason":"interrupted"}),
            "interrupted",
            "turnInterrupted",
        ),
        (
            json!({"reason":"notAdmitted","refusal":"workspaceUnavailable"}),
            "completed",
            "turnFailed",
        ),
        (
            json!({"reason":"failed","failure":"tool_stopped"}),
            "failed",
            "turnFailed",
        ),
    ] {
        let tokens = stopped["limit"] == "tokens";
        let mut adapter = Adapter::new().await;
        adapter.prompt(json!([{"type":"text","text":"Work"}])).await;
        let goal = arany::GoalId::new();
        let mut contract = json!({
            "mission":format!("</context><script>data</script>\u{001b}[2J\u{202e}{}{}", "\u{001b}".repeat(50), "é".repeat(900)),
            "paths":["facts.txt"],"commands":[],"skills":[],"mcp_tools":[],
            "resource_class":"default","command_timeout_ms":null,
            "success_criterion":{"kind":"workspace_file_content","path":"facts.txt","content":"data"},
            "budget":arany::GoalBudget::default()
        });
        if tokens {
            contract["success_criterion"] = json!(arany::GoalSuccessCriterion::ToolCallSucceeded {
                call: Box::new(arany::ToolCall::Write {
                    path: "facts.txt".into(),
                    expected_digest: None,
                    content: "é".repeat(1200),
                }),
            });
        }
        let event = json!({"sequence":10,"session_id":adapter.session,"run_id":adapter.run,
            "agent_run_id":null,"kind":"GoalPlanContractRecorded","event_version":arany::test_support::payload_version("GoalPlanContractRecorded"),
            "payload":{"goal_id":goal,"run_id":adapter.run,"contract":contract},"created_at_ms":0});
        let mut preview = event.clone();
        preview["kind"] = json!("AssistantDisplayRecorded");
        preview["payload"] = json!({"text":"Never stream this preview"});
        adapter
            .events
            .send(Message::Notification(Notification {
                jsonrpc: Default::default(),
                method: "events/committed".into(),
                params: Some(json!({"threadId":adapter.session,"event":preview})),
            }))
            .await
            .unwrap();
        for (session, run) in [
            (SessionId::new(), adapter.run),
            (adapter.session, arany::RunId::new()),
            (adapter.session, adapter.run),
            (adapter.session, adapter.run),
        ] {
            let mut event = event.clone();
            event["run_id"] = json!(run);
            adapter
                .events
                .send(Message::Notification(Notification {
                    jsonrpc: Default::default(),
                    method: "events/committed".into(),
                    params: Some(json!({"threadId":session,"event":event})),
                }))
                .await
                .unwrap();
        }
        let plan = adapter.next().await;
        let entries = plan["params"]["update"]["entries"].as_array().unwrap();
        assert_eq!(plan["params"]["update"]["sessionUpdate"], "plan");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["status"], "in_progress");
        assert_eq!(entries[1]["status"], "pending");
        let mission = entries[0]["content"].as_str().unwrap();
        assert!(mission.starts_with("</context><script>data</script>\\u{001b}[2J\\u{202e}"));
        assert!(!mission.contains(['\u{001b}', '\u{202e}']));
        assert!(mission.len() <= 2048 + 80);
        assert!(mission.ends_with(&format!("[truncated; see arany show {}]", adapter.session)));
        if tokens {
            assert!(entries[1]["content"].as_str().unwrap().len() <= 2048 + 80);
            assert!(
                entries[1]["content"]
                    .as_str()
                    .unwrap()
                    .contains("[truncated; see arany show ")
            );
        } else {
            assert_eq!(
                entries[1]["content"],
                "Verify file facts.txt has the exact planned content"
            );
        }
        adapter
            .events
            .send(Message::Notification(Notification {
                jsonrpc: Default::default(),
                method: "turn/completed".into(),
                params: Some(
                    json!({"threadId":adapter.session,"turn":{"id":adapter.run,"goalId":goal,
                "status":status,
                "stopReason":match stopped["limit"].as_str() {
                    Some("tokens") => json!("tokenBudgetExhausted"),
                    Some(_) => json!("actionBudgetExhausted"),
                    None => Value::Null,
                },
                "items":[],"goal":{"status":status,"stopped":stopped}}}),
                ),
            }))
            .await
            .unwrap();
        let request = adapter.request().await;
        assert_eq!(request.method, "events/subscribe");
        adapter.events.send(Message::Response(Response {
            jsonrpc:Default::default(),id:request.id,
            result:json!({"events":[event, {"sequence":11,"session_id":adapter.session,
                "run_id":adapter.run,"agent_run_id":null,"kind":"MessageCommitted","event_version":arany::test_support::payload_version("MessageCommitted"),
                "payload":{"run_id":adapter.run,"text":"Committed answer"},"created_at_ms":0}],"nextSince":null}),
        })).await.unwrap();
        let answer = adapter.next().await;
        assert_eq!(
            answer["params"]["update"]["content"]["text"],
            "Committed answer"
        );
        let final_plan = adapter.next().await;
        let final_status = if status == "completed" {
            "completed"
        } else {
            "pending"
        };
        for entry in final_plan["params"]["update"]["entries"]
            .as_array()
            .unwrap()
        {
            assert_eq!(entry["status"], final_status);
        }
        let result = adapter.next().await;
        if matches!(
            expected,
            "turnFailed" | "stoppedByHook" | "effectUncertain" | "turnInterrupted"
        ) {
            assert_eq!(result["error"]["code"], -32603, "{stopped}");
            assert_eq!(result["error"]["data"]["reason"], expected, "{stopped}");
        } else {
            assert_eq!(result["result"]["stopReason"], expected, "{stopped}");
        }
        assert!(
            adapter.output.try_recv().is_err(),
            "duplicates and foreign plans are suppressed"
        );
        for method in [
            "goal/pause",
            "goal/resume",
            "session/pause",
            "session/set_model",
            "terminal/create",
            "fs/write_text_file",
        ] {
            adapter
                .call(json!(8), method, json!({"sessionId":adapter.session}))
                .await;
            assert_eq!(adapter.next().await["error"]["code"], -32601);
            assert!(
                adapter.input.try_recv().is_err(),
                "unadvertised surfaces never reach serve"
            );
        }
        adapter.close().await;
    }
}

#[tokio::test]
async fn budget_stop_reasons_match_reopened_goal_receipts() {
    use arany::test_support::scenario::tool_outcome;
    use std::os::unix::fs::PermissionsExt;
    let _deadline = crate::cli::serve::tests::deadline();
    for mode in ["actions", "tokens", "finish"] {
        let tokens = mode == "tokens";
        let finish = mode == "finish";
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("facts.txt"), "Fact").unwrap();
        let state = root.path().join("state");
        let budget = arany::GoalBudget {
            max_provider_calls: Some(1),
            max_tokens: tokens.then_some(1),
            ..Default::default()
        };
        StateRoot::admit(&state)
            .unwrap()
            .record(arany::RecordKind::UiPreferences)
            .replace(
                &serde_json::to_vec(&json!({"version":6,"preferences":{
                    "completion":"tabs","view":"classic","goal_budget":budget,
                    "resources":arany::ToolResourceSettings::default(),
                    "copy_on_select":true,"newline":"shift_enter"
                }}))
                .unwrap(),
            )
            .unwrap();
        let tools = state.join("tools.json");
        std::fs::write(
            &tools,
            serde_json::to_vec(&json!({"version":1,
                "workspace_paths":["facts.txt"],"write":false,"commands":[],"skills":[],"mcp":[]
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(&tools, std::fs::Permissions::from_mode(0o600)).unwrap();
        let provider = Arc::new(
            ScriptedProvider::new(vec![if finish {
                arany::ProviderOutcome::Answer(arany::Answer {
                    text: "Answer".into(),
                })
            } else {
                tool_outcome(arany::ToolCall::Read {
                    path: "facts.txt".into(),
                    offset: None,
                    limit: Some(32),
                })
            }])
            .with_profile("openai", "gpt-5.4")
            .with_reported_usage(arany::Usage {
                input_tokens: Some(0),
                output_tokens: Some(1),
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            }),
        );
        let (mut writer, mut reader, task) = start_configured(&state, provider.clone(), true);
        initialize(&mut writer, &mut reader).await;
        let session = session(&mut writer, &mut reader, &workspace).await;
        send(
            &mut writer,
            json!(3),
            "session/prompt",
            json!({"sessionId":session,
            "prompt":[{"type":"text","text":"Read facts"}]}),
        )
        .await;
        let response = loop {
            let value = next(&mut reader).await;
            if value["id"] == 3 {
                assert_eq!(
                    value["result"]["stopReason"],
                    if finish {
                        "end_turn"
                    } else if tokens {
                        "max_tokens"
                    } else {
                        "max_turn_requests"
                    }
                );
                break value;
            }
        };
        writer.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        let events = store.load_session(session).await.unwrap();
        let view = arany::SessionView::replay(session, &events)
            .unwrap()
            .unwrap();
        assert_eq!(view.goals[0].budget_closed, !finish);
        assert_eq!(view.goals[0].budget_refusal.is_some(), !finish);
        if !finish {
            assert_eq!(
                response["result"]["_meta"]["arany.budgetRefusal"],
                json!(view.goals[0].budget_refusal)
            );
            assert_eq!(
                response["result"]["_meta"]["arany.status"],
                crate::presentation::session_run_feedback(&view, &view.runs[0]).unwrap()
            );
        }
        assert_eq!(view.goals[0].provider_calls, 1);
        assert_eq!(view.runs.len(), 1);
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        assert_eq!(
            events.iter().any(|e| matches!(
                e.event,
                Event::GoalFinished {
                    exhausted: true,
                    ..
                }
            )),
            !finish
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

#[tokio::test]
async fn tools_permissions_wait_and_withdraw_without_private_state() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    adapter.prompt(json!([{"type":"text","text":"Read"}])).await;
    let intent = adapter.intent(arany::ToolCall::Read {
        path: "notes/\u{001b}[2J\nfile".into(),
        offset: None,
        limit: Some(32),
    });
    adapter.item(&intent, None).await;
    let started = adapter.next().await;
    assert_eq!(started["params"]["update"]["sessionUpdate"], "tool_call");
    assert_eq!(started["params"]["update"]["kind"], "read");
    assert_eq!(started["params"]["update"]["status"], "pending");
    assert_eq!(
        started["params"]["update"]["locations"][0]["path"],
        "/var/workspace/notes/\u{001b}[2J\nfile"
    );
    assert!(
        !started["params"]["update"]["title"]
            .as_str()
            .unwrap()
            .chars()
            .any(char::is_control)
    );
    for (index, outcome, decision) in [
        (
            10,
            json!({"outcome":"selected","optionId":"allow_once"}),
            "accept",
        ),
        (
            11,
            json!({"outcome":"selected","optionId":"deny"}),
            "decline",
        ),
        (
            12,
            json!({"outcome":"selected","optionId":"allow_always"}),
            "unavailable",
        ),
        (13, json!({"outcome":"selected"}), "unavailable"),
        (14, Value::Null, "unavailable"),
        (
            15,
            json!({"outcome":"selected","optionId":"reject_once"}),
            "unavailable",
        ),
        (
            16,
            json!({"outcome":"selected","optionId":"unknown"}),
            "unavailable",
        ),
    ] {
        adapter.permission(&intent, index).await;
        let request = adapter.next().await;
        assert_eq!(request["method"], "session/request_permission");
        assert_eq!(
            request["params"]["toolCall"]["rawInput"],
            serde_json::to_value(&intent).unwrap()
        );
        assert_eq!(
            request["params"]["options"],
            json!([
                {"optionId":"deny","kind":"reject_once","name":"Deny"},
                {"optionId":"allow_once","kind":"allow_once","name":"Allow once"}
            ])
        );
        let content = request["params"]["toolCall"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(content.starts_with("````\n"));
        assert!(!content.contains('\u{001b}'));
        let reply = json!({"jsonrpc":"2.0","id":request["id"],"result":{"outcome":outcome}});
        translate::input(
            wire::decode(&encode_line(&reply).unwrap()).unwrap(),
            &adapter.config,
            &adapter.shared,
            &adapter.serve,
            &adapter.outbound,
        )
        .await
        .unwrap();
        match adapter.input.recv().await.unwrap() {
            serve::server::ClientMessage::Message(Message::Response(response)) => {
                assert_eq!(response.id, RequestId::Integer(index));
                assert_eq!(response.result["decision"], decision);
            }
            serve::server::ClientMessage::Message(Message::Error(error)) => {
                assert_eq!(decision, "unavailable");
                assert_eq!(error.id, Some(RequestId::Integer(index)));
                assert_eq!(error.error.message, "Approval channel unavailable");
            }
            _ => panic!("approval reply"),
        }
        // Duplicate and unknown IDs cannot resolve another intent.
        for id in [request["id"].clone(), json!(999)] {
            let reply = json!({"jsonrpc":"2.0","id":id,"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}});
            translate::input(
                wire::decode(&encode_line(&reply).unwrap()).unwrap(),
                &adapter.config,
                &adapter.shared,
                &adapter.serve,
                &adapter.outbound,
            )
            .await
            .unwrap();
        }
        assert!(adapter.input.try_recv().is_err());
    }
    adapter.permission(&intent, 20).await;
    let permission = adapter.next().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3600)).await;
    tokio::time::resume();
    assert!(adapter.output.try_recv().is_err());
    let late = json!({"jsonrpc":"2.0","id":permission["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}});
    translate::input(
        wire::decode(&encode_line(&late).unwrap()).unwrap(),
        &adapter.config,
        &adapter.shared,
        &adapter.serve,
        &adapter.outbound,
    )
    .await
    .unwrap();
    let Some(serve::server::ClientMessage::Message(Message::Response(reply))) =
        adapter.input.recv().await
    else {
        panic!("late allow")
    };
    assert_eq!(reply.result["decision"], "accept");
    for _ in 0..256 {
        adapter
            .outbound
            .try_send(wire::response(RequestId::Integer(99), json!({})))
            .unwrap();
    }
    let blocked = adapter.intent(intent.call.clone());
    adapter.item(&blocked, None).await;
    adapter.permission(&blocked, 21).await;
    let reply = tokio::time::timeout(Duration::from_secs(5), adapter.input.recv())
        .await
        .unwrap()
        .unwrap();
    let serve::server::ClientMessage::Message(Message::Error(error)) = reply else {
        panic!("unsent approval must be unavailable");
    };
    assert_eq!(error.id, Some(RequestId::Integer(21)));
    assert_eq!(error.error.message, "Approval channel unavailable");
    assert_eq!(adapter.outbound.capacity(), 0);
    let late = json!({"jsonrpc":"2.0","id":permission["id"].as_i64().unwrap()+1,"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}});
    translate::input(
        wire::decode(&encode_line(&late).unwrap()).unwrap(),
        &adapter.config,
        &adapter.shared,
        &adapter.serve,
        &adapter.outbound,
    )
    .await
    .unwrap();
    assert!(adapter.input.try_recv().is_err());
    for _ in 0..256 {
        assert_eq!(adapter.output.recv().await.unwrap()["id"], 99);
    }
    let update = adapter.next().await;
    assert_eq!(update["params"]["update"]["sessionUpdate"], "tool_call");
    assert_eq!(
        update["params"]["update"]["toolCallId"],
        blocked.id.to_string()
    );
    assert!(adapter.output.try_recv().is_err());
    let fresh = adapter.intent(intent.call.clone());
    adapter.permission(&fresh, 22).await;
    let permission = adapter.next().await;
    assert_eq!(
        permission["params"]["toolCall"]["rawInput"]["id"],
        fresh.id.to_string(),
        "an unavailable request must free the permission slot"
    );
    let error = json!({"jsonrpc":"2.0","id":permission["id"],"error":{"code":-32603,"message":"Client refused"}});
    translate::input(
        wire::decode(&encode_line(&error).unwrap()).unwrap(),
        &adapter.config,
        &adapter.shared,
        &adapter.serve,
        &adapter.outbound,
    )
    .await
    .unwrap();
    let Some(serve::server::ClientMessage::Message(Message::Error(reply))) =
        adapter.input.recv().await
    else {
        panic!("client error is unavailable")
    };
    assert_eq!(reply.id, Some(RequestId::Integer(22)));
    let observation = arany::ToolObservation {
        intent: intent.clone(),
        disposition: arany::ToolDisposition::Uncertain,
        enforcement_cause: None,
        limit_cause: None,
        output: "````\n<script>data</script>\u{001b}[2J".into(),
        attestation: None,
        approval_cause: None,
        cache_update: arany::CacheUpdate::NotApplicable,
    };
    adapter.item(&intent, Some(&observation)).await;
    let completed = adapter.next().await;
    assert_eq!(completed["params"]["update"]["status"], "failed");
    assert_eq!(
        completed["params"]["update"]["_meta"]["arany.status"],
        "uncertain"
    );
    let text = completed["params"]["update"]["content"][0]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(text.starts_with("`````\nOutcome unknown:"));
    assert!(!text.contains('\u{001b}'));
    adapter
        .events
        .send(Message::Notification(Notification {
            jsonrpc: Default::default(),
            method: "events/resync".into(),
            params: Some(json!({"threadId":adapter.session,"missed":1,"lastSequence":0})),
        }))
        .await
        .unwrap();
    adapter.events.send(Message::Notification(Notification { jsonrpc:Default::default(),method:"turn/completed".into(),params:Some(json!({"threadId":adapter.session,"turn":{"id":adapter.run,"goalId":arany::GoalId::new(),"status":"failed","items":[],"goal":{"status":"failed"}}})) })).await.unwrap();
    let events:Vec<Value> = [("ToolStarted",json!({"intent":intent})),("ToolFinished",{let mut p=serde_json::to_value(&observation).unwrap();p["checkpoint"]=Value::Null;p})].into_iter().enumerate().map(|(index,(kind,payload))| json!({
        "sequence":index+1,"session_id":adapter.session,"run_id":adapter.run,"agent_run_id":intent.agent_run_id,"kind":kind,"event_version":arany::test_support::payload_version(kind),"payload":payload,"created_at_ms":0
    })).collect();
    adapter
        .respond(json!({"events":events,"nextSince":null}))
        .await;
    let response = adapter.next().await;
    assert_eq!(
        response["error"]["data"]["reason"], "effectUncertain",
        "reconciliation must suppress duplicate Tool phases and preserve uncertainty"
    );
    adapter.close().await;
}

#[tokio::test]
async fn closed_permission_output_reports_unavailable_without_a_client_reply() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    adapter.prompt(json!([{"type":"text","text":"Read"}])).await;
    adapter.output.close();
    let intent = adapter.intent(arany::ToolCall::Read {
        path: "file".into(),
        offset: None,
        limit: Some(32),
    });
    adapter.permission(&intent, 1).await;
    let reply = tokio::time::timeout(Duration::from_secs(5), adapter.input.recv())
        .await
        .unwrap()
        .unwrap();
    let serve::server::ClientMessage::Message(Message::Error(error)) = reply else {
        panic!("unavailable permission")
    };
    assert_eq!(error.id, Some(RequestId::Integer(1)));
    assert_eq!(error.error.message, "Approval channel unavailable");
    let late = json!({"jsonrpc":"2.0","id":1,"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}});
    translate::input(
        wire::decode(&encode_line(&late).unwrap()).unwrap(),
        &adapter.config,
        &adapter.shared,
        &adapter.serve,
        &adapter.outbound,
    )
    .await
    .unwrap();
    assert!(adapter.input.try_recv().is_err());
    adapter.close().await;
}

#[tokio::test]
async fn blocked_projection_refuses_deferred_event_overflow() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    adapter.prompt(json!([{"type":"text","text":"Read"}])).await;
    for _ in 0..serve::OUTBOUND_CAPACITY {
        adapter
            .outbound
            .try_send(wire::response(RequestId::Integer(99), json!({})))
            .unwrap();
    }
    let intent = adapter.intent(arany::ToolCall::Read {
        path: "file".into(),
        offset: None,
        limit: Some(32),
    });
    adapter.item(&intent, None).await;
    for _ in 0..=serve::OUTBOUND_CAPACITY {
        adapter
            .events
            .send(Message::Notification(Notification {
                jsonrpc: Default::default(),
                method: "events/resync".into(),
                params: None,
            }))
            .await
            .unwrap();
    }
    assert!(
        adapter
            .worker
            .await
            .unwrap()
            .unwrap_err()
            .contains("ACP projection queue full")
    );
    assert_eq!(adapter.outbound.capacity(), 0);
}

#[tokio::test]
async fn blocked_projection_retires_withdrawn_permission_before_accepting_late_answers() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    adapter.prompt(json!([{"type":"text","text":"Read"}])).await;
    let intent = adapter.intent(arany::ToolCall::Read {
        path: "file".into(),
        offset: None,
        limit: Some(32),
    });
    adapter.permission(&intent, 1).await;
    let permission = adapter.next().await;
    assert_eq!(permission["method"], "session/request_permission");
    for _ in 0..serve::OUTBOUND_CAPACITY {
        adapter
            .outbound
            .try_send(wire::response(RequestId::Integer(99), json!({})))
            .unwrap();
    }
    adapter.item(&intent, None).await;
    adapter
        .events
        .send(Message::Notification(Notification {
            jsonrpc: Default::default(),
            method: "$/cancel_request".into(),
            params: Some(json!({"requestId":1})),
        }))
        .await
        .unwrap();
    adapter.permission(&intent, 2).await;
    let Some(serve::server::ClientMessage::Message(Message::Error(error))) =
        adapter.input.recv().await
    else {
        panic!("full output refuses the next permission without waiting")
    };
    assert_eq!(error.id, Some(RequestId::Integer(2)));
    assert_eq!(error.error.message, "Approval channel unavailable");
    let late = json!({"jsonrpc":"2.0","id":permission["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}});
    translate::input(
        wire::decode(&encode_line(&late).unwrap()).unwrap(),
        &adapter.config,
        &adapter.shared,
        &adapter.serve,
        &adapter.outbound,
    )
    .await
    .unwrap();
    assert!(adapter.input.try_recv().is_err());
    for _ in 0..serve::OUTBOUND_CAPACITY {
        assert_eq!(adapter.next().await["id"], 99);
    }
    let messages = [adapter.next().await, adapter.next().await];
    let withdrawal = messages
        .iter()
        .find(|message| message["method"] == "$/cancel_request")
        .expect("withdrawal reaches the client after output drains");
    assert_eq!(withdrawal["params"]["requestId"], permission["id"]);
    assert!(
        messages
            .iter()
            .any(|message| message["method"] == "session/update")
    );
    adapter.permission(&intent, 3).await;
    let next = adapter.next().await;
    assert_eq!(next["method"], "session/request_permission");
    assert_ne!(next["id"], permission["id"]);
    let reply = json!({"jsonrpc":"2.0","id":next["id"],"result":{"outcome":{"outcome":"selected","optionId":"deny"}}});
    translate::input(
        wire::decode(&encode_line(&reply).unwrap()).unwrap(),
        &adapter.config,
        &adapter.shared,
        &adapter.serve,
        &adapter.outbound,
    )
    .await
    .unwrap();
    let Some(serve::server::ClientMessage::Message(Message::Response(reply))) =
        adapter.input.recv().await
    else {
        panic!("next permission accepts its own answer")
    };
    assert_eq!(reply.id, RequestId::Integer(3));
    assert_eq!(reply.result["decision"], "decline");
    adapter.close().await;
}

#[tokio::test]
async fn permission_cancel_and_server_withdrawal_ignore_late_answers_without_private_state() {
    let _deadline = crate::cli::serve::tests::deadline();
    for cancel in ["client", "session", "server", "before_start_reply"] {
        let mut adapter = Adapter::new().await;
        if cancel == "before_start_reply" {
            adapter
                .call(
                    json!("prompt"),
                    "session/prompt",
                    json!({"sessionId":adapter.session,"prompt":[{"type":"text","text":"Read"}]}),
                )
                .await;
            assert_eq!(adapter.request().await.method, "turn/start");
        } else {
            adapter.prompt(json!([{"type":"text","text":"Read"}])).await;
        }

        let intent = adapter.intent(arany::ToolCall::Read {
            path: "file".into(),
            offset: None,
            limit: Some(32),
        });
        adapter.permission(&intent, 1).await;
        let permission = adapter.next().await;
        if cancel == "client" {
            let reply = json!({"jsonrpc":"2.0","id":permission["id"],"result":{"outcome":{"outcome":"cancelled"}}});
            translate::input(
                wire::decode(&encode_line(&reply).unwrap()).unwrap(),
                &adapter.config,
                &adapter.shared,
                &adapter.serve,
                &adapter.outbound,
            )
            .await
            .unwrap();
            let Some(serve::server::ClientMessage::Message(Message::Response(response))) =
                adapter.input.recv().await
            else {
                panic!("cancel reply")
            };
            assert_eq!(response.result["decision"], "cancel");
        } else {
            if cancel == "session" {
                let note = json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":adapter.session}});
                translate::input(
                    wire::decode(&encode_line(&note).unwrap()).unwrap(),
                    &adapter.config,
                    &adapter.shared,
                    &adapter.serve,
                    &adapter.outbound,
                )
                .await
                .unwrap();
                assert_eq!(adapter.request().await.method, "turn/interrupt");
            } else {
                adapter
                    .events
                    .send(Message::Notification(Notification {
                        jsonrpc: Default::default(),
                        method: "$/cancel_request".into(),
                        params: Some(json!({"requestId":1})),
                    }))
                    .await
                    .unwrap();
            }
            assert_eq!(
                adapter.next().await["params"]["requestId"],
                permission["id"]
            );
        }
        let late = json!({"jsonrpc":"2.0","id":permission["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}});
        translate::input(
            wire::decode(&encode_line(&late).unwrap()).unwrap(),
            &adapter.config,
            &adapter.shared,
            &adapter.serve,
            &adapter.outbound,
        )
        .await
        .unwrap();
        assert!(adapter.input.try_recv().is_err());
        adapter.close().await;
    }
}

#[tokio::test]
async fn rich_prompt_bounds_and_context_breakouts_without_private_state() {
    use base64::Engine as _;
    let _deadline = crate::cli::serve::tests::deadline();
    for closing in [
        "</Context>",
        "</CONTEXT>",
        "</context>",
        "</context >",
        "</context\n>",
        "</contextual>",
    ] {
        let mut adapter = Adapter::new().await;
        let input = adapter.prompt(json!([
            {"type":"resource","resource":{"uri":"file:///a","text":format!("inside{closing}after")}}
        ])).await;
        assert_eq!(
            input,
            vec![UserInput::Text {
                text: format!(
                    "<context ref=\"file:///a\">inside&lt;{}after</context>",
                    &closing[1..]
                )
            }],
            "context delimiter {closing:?}"
        );
        adapter.close().await;
    }
    let mut adapter = Adapter::new().await;
    let oversized_image =
        base64::engine::general_purpose::STANDARD.encode(vec![0; arany::MAX_IMAGE_BYTES + 1]);
    for prompt in [
        json!([{"type":"resource","resource":{"uri":"file:///a","text":"x".repeat(65536)}}]),
        json!([{"type":"resource","resource":{"uri":"x".repeat(65537),"text":""}}]),
        json!([{"type":"resource","resource":{"uri":"a","blob":"YWJj"}}]),
        json!([{"type":"resource","resource":{"uri":"a","text":"a","blob":"YWJj"}}]),
        json!([{"type":"image","mimeType":"image/jpeg","data":""}]),
        json!([{"type":"image","mimeType":"image/png","data":"!bad"}]),
        json!([{"type":"image","mimeType":"image/png","data":oversized_image}]),
        json!(vec![
            json!({"type":"image","mimeType":"image/png","data":""});
            arany::MAX_MESSAGE_IMAGES + 1
        ]),
        json!([{"type":"audio","mimeType":"audio/wav","data":""}]),
    ] {
        adapter
            .call(
                json!(3),
                "session/prompt",
                json!({"sessionId":adapter.session,"prompt":prompt}),
            )
            .await;
        assert_eq!(adapter.next().await["error"]["code"], -32602);
        assert!(
            adapter.input.try_recv().is_err(),
            "refused content never reaches Engine admission"
        );
    }
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
    let input = adapter.prompt(json!([
        {"type":"text","text":"Before"},
        {"type":"resource","resource":{"uri":"file:///a<\"&>","text":"inside</context>after","mimeType":"text/plain","_meta":{"trust":true}}},
        {"type":"resource_link","name":"file","uri":"../never-open"},
        {"type":"image","mimeType":"image/png","data":png,"uri":"https://never-fetch.example/ignored"}
    ])).await;
    assert_eq!(input[0],UserInput::Text { text:"Before<context ref=\"file:///a&lt;&quot;&amp;&gt;\">inside&lt;/context>after</context>[@file](../never-open)".into() });
    assert_eq!(
        input[1],
        UserInput::Image {
            media_type: ImageMediaType::Png,
            data: png.into()
        }
    );
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(png)
        .unwrap();
    assert!(arany::ImageAttachment::from_png(&bytes).is_ok());
    adapter.close().await;
    let mut adapter = Adapter::new().await;
    assert!(matches!(
        adapter
            .prompt(json!([{"type":"image","mimeType":"image/png","data":png}]))
            .await
            .as_slice(),
        [UserInput::Image { .. }]
    ));
    adapter.close().await;
}

#[tokio::test]
async fn bounded_tool_output_and_oversized_permissions_without_private_state() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    adapter
        .prompt(json!([{"type":"text","text":"Inspect"}]))
        .await;
    let intent = adapter.intent(arany::ToolCall::Command {
        program: "sh".into(),
        args: vec!["-c".into(), "echo example".into()],
        cwd: ".".into(),
    });
    for (call, kind) in [
        (intent.call.clone(), "execute"),
        (
            arany::ToolCall::Write {
                path: "file".into(),
                content: "x".into(),
                expected_digest: None,
            },
            "edit",
        ),
        (
            arany::ToolCall::Search {
                path: "notes".into(),
                query: "x".into(),
                regex: false,
                include: None,
                case_sensitive: true,
                offset: 0,
                limit: 100,
            },
            "search",
        ),
        (
            arany::ToolCall::Glob {
                pattern: "*.md".into(),
                path: "notes".into(),
                limit: 100,
            },
            "search",
        ),
        (
            arany::ToolCall::Skill {
                name: "guide".into(),
                resource: None,
            },
            "read",
        ),
        (
            arany::ToolCall::McpList {
                server: "local".into(),
            },
            "other",
        ),
    ] {
        let intent = adapter.intent(call);
        adapter.item(&intent, None).await;
        let update = adapter.next().await;
        assert_eq!(update["params"]["update"]["kind"], kind);
        assert!(update["params"]["update"].get("content").is_none());
    }
    let observation = arany::ToolObservation {
        intent: intent.clone(),
        disposition: arany::ToolDisposition::Succeeded,
        enforcement_cause: None,
        limit_cause: None,
        output: "é`\u{001b}".repeat(64 * 1024),
        attestation: None,
        approval_cause: None,
        cache_update: arany::CacheUpdate::NotApplicable,
    };
    adapter.item(&intent, Some(&observation)).await;
    let update = adapter.next().await;
    let text = update["params"]["update"]["content"][0]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("[truncated; see arany show "));
    assert!(!text.contains('\u{001b}'));
    let mut big = intent.clone();
    big.call = arany::ToolCall::Command {
        program: "sh".into(),
        args: vec!["x".repeat(MAX_RPC_LINE_BYTES)],
        cwd: ".".into(),
    };
    adapter.permission(&big, 100).await;
    let Some(serve::server::ClientMessage::Message(Message::Error(reply))) =
        adapter.input.recv().await
    else {
        panic!("oversized approval is unavailable")
    };
    assert_eq!(reply.id, Some(RequestId::Integer(100)));
    assert_eq!(reply.error.message, "Approval channel unavailable");
    assert!(
        adapter.output.try_recv().is_err(),
        "oversized approval must never be sent"
    );
    let unguarded = adapter.intent(arany::ToolCall::CommandUnguarded {
        program: "sh".into(),
        args: vec![],
        cwd: ".".into(),
        justification: "untrusted".into(),
        prefix_rule: None,
    });
    adapter.permission(&unguarded, 101).await;
    let Some(serve::server::ClientMessage::Message(Message::Error(reply))) =
        adapter.input.recv().await
    else {
        panic!("human-only approval is unavailable")
    };
    assert_eq!(reply.error.message, "Approval channel unavailable");
    assert!(adapter.output.try_recv().is_err());
    adapter.close().await;
}

#[tokio::test]
async fn permission_round_trip_dispatch_and_denial_survive_reopen() {
    let _deadline = crate::cli::serve::tests::deadline();
    for allow in [true, false] {
        permission_scenario(allow, None, PermissionCase::Read).await;
    }
    permission_scenario(true, None, PermissionCase::ModeSwitch).await;
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native ACP Guard journey; requires the arany executable and private State"
)]
fn guarded_acp_permissions_publish_once_and_refuse_hook_asks() {
    let _gate = crate::test_process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _deadline = crate::cli::serve::tests::deadline();
    // Cargo's integration targets build the product beside the deps directory in CI.
    let guard = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("arany");
    assert!(
        guard.is_file(),
        "build the arany binary before this native gate"
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            permission_scenario(false, Some(guard.clone()), PermissionCase::HookAsk).await;
            permission_scenario(true, Some(guard.clone()), PermissionCase::Write).await;
            permission_scenario(true, Some(guard), PermissionCase::LiveEdits).await;
        });
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PermissionCase {
    Read,
    Write,
    HookAsk,
    ModeSwitch,
    LiveEdits,
}

async fn permission_scenario(allow: bool, guard: Option<PathBuf>, case: PermissionCase) {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("facts.txt"), "Fact").unwrap();
    let state = root.path().join("state");
    StateRoot::admit(&state).unwrap();
    let live_edits = case == PermissionCase::LiveEdits;
    let write = matches!(case, PermissionCase::Write | PermissionCase::LiveEdits);
    let hook_ask = case == PermissionCase::HookAsk;
    let commands = if hook_ask {
        let shell = std::fs::canonicalize("/bin/sh").unwrap();
        let digest: String = Sha256::digest(std::fs::read(&shell).unwrap())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        json!([{"name":"sh","executable":shell,"sha256":digest,"interpreter":true,"inputs":[]}])
    } else {
        json!([])
    };
    let permissions = if hook_ask {
        json!({"permissions":{"defaultMode":"auto"},"hooks":{"PreToolUse":[{"matcher":"Read","hooks":[{
            "type":"command",
            "command":"printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"ask\"}}'"
        }]}]}})
    } else if live_edits {
        json!({"permissions":{"defaultMode":"acceptEdits"}})
    } else {
        json!({"permissions":{"ask":["Read","Edit"]}})
    };
    for (name, value) in [
        (
            "tools.json",
            json!({"version":1,"workspace_paths":if write {json!(["."])} else {json!(["facts.txt"])},"write":write || hook_ask,"commands":commands,"skills":[],"mcp":[]}),
        ),
        ("permissions.json", permissions),
    ] {
        let path = state.join(name);
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let call = if write {
        arany::ToolCall::Write {
            path: "new.txt".into(),
            expected_digest: None,
            content: "Written".into(),
        }
    } else {
        arany::ToolCall::Read {
            path: "facts.txt".into(),
            offset: None,
            limit: Some(32),
        }
    };
    let second = arany::ToolCall::Write {
        path: "next.txt".into(),
        expected_digest: None,
        content: "Next".into(),
    };
    let outcomes = permission_outcomes(case, allow, call, second);
    let provider = Arc::new(ScriptedProvider::new(outcomes).with_profile("openai", "gpt-5.4"));
    let (mut writer, mut reader, task) = if let Some(guard) = guard {
        start_with_guard(&state, provider.clone(), true, guard)
    } else {
        start_configured(&state, provider.clone(), true)
    };
    initialize(&mut writer, &mut reader).await;
    let session = session(&mut writer, &mut reader, &workspace).await;
    send(
        &mut writer,
        json!(3),
        "session/prompt",
        json!({"sessionId":session,"prompt":[{"type":"text","text":"Read facts"}]}),
    )
    .await;
    let mut intent = None;
    let mut permissions = 0;
    let mut projections = 0;
    loop {
        let value = next(&mut reader).await;
        if value["method"] == "session/request_permission" {
            permissions += 1;
            intent = Some(value["params"]["toolCall"]["rawInput"].clone());
            if matches!(case, PermissionCase::ModeSwitch | PermissionCase::LiveEdits)
                && permissions == 1
            {
                send(&mut writer, json!(4), "session/set_mode", json!({"sessionId":session,"modeId":if live_edits {"acceptEdits"} else {"auto"}})).await;
                let mut updates = 0;
                let mut seen = Vec::new();
                let mut omitted = 0_usize;
                loop {
                    let switched = next(&mut reader).await;
                    if seen.len() < 32 {
                        seen.push(switched.to_string().chars().take(256).collect::<String>());
                    } else {
                        omitted = omitted.saturating_add(1);
                    }
                    if switched["id"] == 4 {
                        assert_eq!(switched["result"], json!({}));
                        break;
                    }
                    assert_eq!(switched["method"], "session/update");
                    match switched["params"]["update"]["sessionUpdate"].as_str() {
                        Some("current_mode_update" | "config_option_update") => updates += 1,
                        Some("tool_call") => {
                            assert_eq!(switched["params"]["update"]["status"], "pending");
                            projections += 1;
                        }
                        _ => panic!(
                            "mode switch must leave the pending approval unanswered: {:?}; seen since switch: {:?}; omitted: {}",
                            switched.to_string().chars().take(2048).collect::<String>(),
                            seen,
                            omitted
                        ),
                    }
                }
                assert_eq!(updates, 2);
            }
            writer.write_all(&encode_line(&json!({"jsonrpc":"2.0","id":value["id"],"result":{"outcome":{"outcome":"selected","optionId":if allow {"allow_once"} else {"deny"}}}})).unwrap()).await.unwrap();
        } else if value["method"] == "session/update"
            && value["params"]["update"]["sessionUpdate"]
                .as_str()
                .unwrap()
                .starts_with("tool_call")
        {
            projections += 1;
        }
        if value["id"] == 3 {
            assert_eq!(value["result"]["stopReason"], "end_turn", "{case:?}");
            break;
        }
    }
    let calls = if matches!(case, PermissionCase::ModeSwitch | PermissionCase::LiveEdits) {
        2
    } else {
        1
    };
    assert_eq!(
        permissions,
        if hook_ask {
            0
        } else if live_edits {
            1
        } else {
            calls
        },
        "{case:?}"
    );
    assert_eq!(projections, calls * 2);
    writer.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    let events = store.load_session(session).await.unwrap();
    let view = store.load_view(session).await.unwrap().unwrap();
    store.close().await.unwrap();
    assert_eq!(
        arany::SessionView::replay(session, &events)
            .unwrap()
            .unwrap(),
        view
    );
    assert_permission_continuation(&view, &provider, allow, case);
    assert_eq!(view.runs[0].tools.len(), calls);
    if case == PermissionCase::ModeSwitch {
        assert_eq!(provider.requests.lock().unwrap().len(), 3);
        for tool in &view.runs[0].tools {
            let verdict = tool.permission_verdict.as_ref().unwrap();
            assert_eq!(verdict.source, arany::permissions::PermissionSource::Human);
            assert!(verdict.approved);
        }
    }
    let tool = view.runs[0].tools.last().unwrap();
    if hook_ask {
        assert!(intent.is_none());
        let verdict = tool.permission_verdict.as_ref().unwrap();
        assert_eq!(verdict.decision, arany::permissions::Decision::Ask);
        assert_eq!(
            verdict.hook_restriction,
            Some(arany::permissions::Decision::Ask)
        );
        assert_eq!(verdict.hooks, 1);
        assert_eq!(tool.hooks.len(), 1);
        assert!(matches!(
            tool.hooks[0].outcome.as_ref().unwrap().result,
            arany::permissions::HookResult::Restricted {
                decision: arany::permissions::Decision::Ask,
                ..
            }
        ));
        assert!(events.iter().any(|event| matches!(
            &event.event,
            Event::ToolPermissionDecided { verdict, .. }
                if verdict.source == arany::permissions::PermissionSource::UnavailableApproval
                    && !verdict.approved
        )));
        assert!(tool.observation.as_ref().unwrap().attestation.is_none());
    } else {
        let asked = if live_edits {
            &view.runs[0].tools[0]
        } else {
            tool
        };
        assert_eq!(Some(serde_json::to_value(&asked.intent).unwrap()), intent);
        assert_eq!(
            asked.permission_verdict.as_ref().unwrap().source,
            arany::permissions::PermissionSource::Human
        );
    }
    assert_eq!(tool.permission_verdict.as_ref().unwrap().approved, allow);
    assert_eq!(
        tool.permission_verdict.as_ref().unwrap().source,
        if hook_ask {
            arany::permissions::PermissionSource::UnavailableApproval
        } else if live_edits {
            arany::permissions::PermissionSource::Mode
        } else {
            arany::permissions::PermissionSource::Human
        }
    );
    assert_eq!(
        tool.observation.as_ref().unwrap().disposition,
        if allow {
            arany::ToolDisposition::Succeeded
        } else {
            arany::ToolDisposition::Denied
        }
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.event, Event::ToolFinished { .. }))
            .count(),
        calls
    );
    assert_eq!(provider.requests.lock().unwrap().len(), calls + 1);
    assert!(provider.responses.lock().unwrap().is_empty());
    if live_edits {
        assert_eq!(std::fs::read(workspace.join("next.txt")).unwrap(), b"Next");
    }
    if write {
        assert_eq!(
            std::fs::read(workspace.join("new.txt")).unwrap(),
            b"Written"
        );
        assert!(matches!(
            tool.observation.as_ref().unwrap().attestation,
            Some(arany::Attestation::Guard(_))
        ));
    }
}

fn permission_answer(case: PermissionCase, allow: bool) -> &'static str {
    if allow {
        "Finished"
    } else {
        match case {
            PermissionCase::HookAsk => {
                "The requested read needs an unavailable human approval channel."
            }
            PermissionCase::Read
            | PermissionCase::Write
            | PermissionCase::ModeSwitch
            | PermissionCase::LiveEdits => "The requested operation was declined and did not run.",
        }
    }
}

fn permission_outcomes(
    case: PermissionCase,
    allow: bool,
    call: arany::ToolCall,
    second: arany::ToolCall,
) -> Vec<arany::ProviderOutcome> {
    use arany::test_support::scenario::tool_outcome;
    let mut outcomes = vec![tool_outcome(call)];
    match case {
        PermissionCase::ModeSwitch => outcomes.push(outcomes[0].clone()),
        PermissionCase::LiveEdits => outcomes.push(tool_outcome(second)),
        PermissionCase::Read | PermissionCase::Write | PermissionCase::HookAsk => {}
    }
    outcomes.push(arany::ProviderOutcome::Answer(arany::Answer {
        text: permission_answer(case, allow).into(),
    }));
    outcomes
}

fn assert_permission_continuation(
    view: &arany::SessionView,
    provider: &ScriptedProvider,
    allow: bool,
    case: PermissionCase,
) {
    let run = &view.runs[0];
    assert_eq!(run.status, RunStatus::Finished, "{case:?}");
    assert!(run.failure.is_none());
    assert_eq!(
        run.assistant_message.as_deref(),
        Some(permission_answer(case, allow))
    );
    assert_eq!(view.goals[0].status, arany::GoalStatus::Completed);
    let requests = provider.requests.lock().unwrap();
    let context = requests.last().unwrap().input.tools.as_ref().unwrap();
    let step = context.steps.last().unwrap();
    let tool = run.tools.last().unwrap();
    let observation = tool.observation.as_ref().unwrap();
    assert_eq!(step.requests.len(), 1, "{case:?}: original request");
    assert_eq!(step.requests[0].id.as_str(), "call_1");
    assert_eq!(step.requests[0].call, tool.intent.call);
    assert_eq!(
        step.observations.as_slice(),
        std::slice::from_ref(observation)
    );
    if !allow {
        let label = match case {
            PermissionCase::HookAsk => "channel_unavailable",
            PermissionCase::Read
            | PermissionCase::Write
            | PermissionCase::ModeSwitch
            | PermissionCase::LiveEdits => "user_declined",
        };
        assert_eq!(
            observation.output,
            format!("{label}: no operation was dispatched")
        );
        assert!(observation.attestation.is_none());
        assert_eq!(run.tools.len(), 1, "{case:?}: no automatic retry");
    }
}

fn assert_mode_state(result: &Value, current: &str) {
    assert_eq!(result["modes"]["currentModeId"], current);
    let modes = result["modes"]["availableModes"].as_array().unwrap();
    let config = &result["configOptions"][0];
    assert_eq!(result["configOptions"].as_array().unwrap().len(), 1);
    assert_eq!(config["id"], "mode");
    assert_eq!(config["category"], "mode");
    assert_eq!(config["type"], "select");
    assert_eq!(config["currentValue"], current);
    assert_eq!(
        modes.iter().map(|m| &m["id"]).collect::<Vec<_>>(),
        config["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| &m["value"])
            .collect::<Vec<_>>()
    );
    assert_eq!(
        modes.iter().map(|m| &m["id"]).collect::<Vec<_>>(),
        vec![&json!("default"), &json!("acceptEdits"), &json!("auto")]
    );
}

#[tokio::test]
async fn modes_and_config_options_stay_in_sync_and_refuse_invalid_values_without_private_state() {
    let _deadline = crate::cli::serve::tests::deadline();
    let mut adapter = Adapter::new().await;
    for (method, mode) in [
        ("session/set_mode", "acceptEdits"),
        ("session/set_config_option", "auto"),
        ("session/set_config_option", "default"),
    ] {
        let params = if method == "session/set_mode" {
            json!({"sessionId":adapter.session,"modeId":mode,"_meta":{"tools":true}})
        } else {
            json!({"sessionId":adapter.session,"configId":"mode","value":mode})
        };
        adapter.call(json!(9), method, params).await;
        let request = adapter.request().await;
        assert_eq!(request.method, "thread/settings/update");
        assert_eq!(
            request.params.unwrap(),
            json!({"threadId":adapter.session,"permissionMode":mode})
        );
        adapter
            .call(
                json!(10),
                "session/resume",
                json!({"sessionId":adapter.session,"cwd":"/var/workspace"}),
            )
            .await;
        assert_eq!(adapter.next().await["error"]["data"]["reason"], "busy");
        assert!(adapter.input.try_recv().is_err());
        adapter
            .events
            .send(Message::Response(Response {
                jsonrpc: Default::default(),
                id: request.id,
                result: json!({"permissionMode":mode}),
            }))
            .await
            .unwrap();
        let current = adapter.next().await;
        assert_eq!(
            current["params"]["update"],
            json!({"sessionUpdate":"current_mode_update","currentModeId":mode})
        );
        let config = adapter.next().await;
        assert_eq!(
            config["params"]["update"]["configOptions"][0]["currentValue"],
            mode
        );
        let response = adapter.next().await;
        if method == "session/set_mode" {
            assert_eq!(response["result"], json!({}));
        } else {
            assert_eq!(
                response["result"]["configOptions"],
                config["params"]["update"]["configOptions"]
            );
        }
        for method in ["session/load", "session/resume"] {
            adapter
                .call(
                    json!(11),
                    method,
                    json!({"sessionId":adapter.session,"cwd":"/var/workspace","mcpServers":[]}),
                )
                .await;
            let request = adapter.request().await;
            assert_eq!(request.method, "thread/resume");
            assert_eq!(request.params.as_ref().unwrap()["permissionMode"], mode);
            adapter.events.send(Message::Response(Response {jsonrpc:Default::default(), id:request.id,
                result:json!({"thread":{"id":adapter.session,"workspace":"/private/var/workspace","title":"Test","createdAtMs":0},"turns":[]})})).await.unwrap();
            let loaded = adapter.next().await;
            validate_session_result_schema(&loaded, method);
            assert_mode_state(&loaded["result"], mode);
        }
    }
    for mode in [
        json!("plan"),
        json!("dontAsk"),
        json!("bypassPermissions"),
        json!("bypass"),
        json!("Default"),
        json!(""),
        json!("x".repeat(65536)),
        json!(true),
        json!({"auto":null}),
        json!(42),
        json!(null),
        json!("unknown"),
    ] {
        for (method, params) in [
            (
                "session/set_mode",
                json!({"sessionId":adapter.session,"modeId":mode}),
            ),
            (
                "session/set_config_option",
                json!({"sessionId":adapter.session,"configId":"mode","value":mode}),
            ),
        ] {
            adapter.call(json!(9), method, params).await;
            assert_eq!(adapter.next().await["error"]["code"], -32602);
            assert!(adapter.input.try_recv().is_err());
            assert!(adapter.output.try_recv().is_err());
            adapter
                .call(
                    json!(11),
                    "session/resume",
                    json!({"sessionId":adapter.session,"cwd":"/var/workspace"}),
                )
                .await;
            let request = adapter.request().await;
            assert_eq!(
                request.params.as_ref().unwrap()["permissionMode"],
                "default"
            );
            adapter.events.send(Message::Response(Response {jsonrpc:Default::default(), id:request.id,
                result:json!({"thread":{"id":adapter.session,"workspace":"/private/var/workspace","title":"Test","createdAtMs":0},"turns":[]})})).await.unwrap();
            assert_mode_state(&adapter.next().await["result"], "default");
        }
    }
    for params in [
        json!({"sessionId":adapter.session,"configId":"model","value":"auto"}),
        json!({"sessionId":adapter.session,"configId":"mode","value":true,"type":"boolean"}),
        json!({"sessionId":adapter.session,"configId":"mode","value":"auto","type":"boolean"}),
        json!({"sessionId":adapter.session,"configId":"x".repeat(65536),"value":"auto"}),
    ] {
        adapter
            .call(json!(9), "session/set_config_option", params)
            .await;
        assert_eq!(adapter.next().await["error"]["code"], -32602);
    }
    adapter
        .call(
            json!(9),
            "session/set_mode",
            json!({"sessionId":SessionId::new(),"modeId":"default"}),
        )
        .await;
    assert_eq!(adapter.next().await["error"]["code"], -32002);
    assert!(adapter.input.try_recv().is_err());
    adapter
        .call(
            json!(12),
            "session/set_mode",
            json!({"sessionId":adapter.session,"modeId":"auto"}),
        )
        .await;
    let request = adapter.request().await;
    adapter
        .events
        .send(Message::Error(ErrorResponse {
            jsonrpc: Default::default(),
            id: Some(request.id),
            error: ErrorObject {
                code: -32600,
                message: "unavailable".into(),
                data: Some(ErrorData {
                    reason: Reason::ThreadNotLoaded,
                }),
            },
        }))
        .await
        .unwrap();
    assert_eq!(adapter.next().await["error"]["code"], -32002);
    assert!(
        adapter.output.try_recv().is_err(),
        "failed update must not change either selector"
    );
    adapter
        .call(
            json!(13),
            "session/resume",
            json!({"sessionId":adapter.session,"cwd":"/var/workspace"}),
        )
        .await;
    let request = adapter.request().await;
    assert_eq!(request.params.unwrap()["permissionMode"], "default");
    adapter.events.send(Message::Response(Response {jsonrpc:Default::default(), id:request.id,
        result:json!({"thread":{"id":adapter.session,"workspace":"/private/var/workspace","title":"Test","createdAtMs":0},"turns":[]})})).await.unwrap();
    assert_mode_state(&adapter.next().await["result"], "default");
    for _ in 0..61 {
        adapter
            .call(
                json!(14),
                "session/set_config_option",
                json!({"sessionId":adapter.session,"configId":"mode","type":"id","value":"auto"}),
            )
            .await;
    }
    adapter
        .call(
            json!(15),
            "session/set_mode",
            json!({"sessionId":adapter.session,"modeId":"default"}),
        )
        .await;
    assert_eq!(adapter.next().await["error"]["data"]["reason"], "busy");
    for _ in 0..61 {
        let request = adapter.request().await;
        assert_eq!(request.method, "thread/settings/update");
        assert_eq!(
            request.params.unwrap(),
            json!({"threadId":adapter.session,"permissionMode":"auto"})
        );
    }
    assert!(adapter.input.try_recv().is_err());
    adapter.close().await;
}

#[test]
fn acp_cli_modes_are_closed_and_do_not_enable_tools() {
    use clap::Parser;
    for mode in [
        "default",
        "acceptEdits",
        "auto",
        "plan",
        "dontAsk",
        "bypassPermissions",
        "Auto",
    ] {
        let parsed = crate::Cli::try_parse_from([
            "arany",
            "acp",
            "--provider",
            "openai",
            "--model",
            "gpt-5.4",
            "--permission-mode",
            mode,
        ]);
        if matches!(mode, "default" | "acceptEdits" | "auto") {
            let crate::Command::Acp(args) = parsed.unwrap().command.unwrap() else {
                panic!("ACP args")
            };
            assert_eq!(serde_json::to_value(args.permission_mode).unwrap(), mode);
            assert!(!args.tools);
        } else {
            assert!(parsed.is_err(), "unsupported CLI mode: {mode}");
        }
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
        intent: intent.clone(),
        disposition: arany::ToolDisposition::Uncertain,
        enforcement_cause: None,
        limit_cause: None,
        output: "Outcome unknown".into(),
        attestation: None,
        approval_cause: None,
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
        intent: intent.clone(),
        disposition: arany::ToolDisposition::Uncertain,
        enforcement_cause: None,
        limit_cause: None,
        output: "Outcome unknown".into(),
        attestation: None,
        approval_cause: None,
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
    let events: Vec<Value> = [("ToolStarted",json!({"intent":intent})),("ToolFinished",{let mut p=serde_json::to_value(&observation).unwrap();p["checkpoint"]=Value::Null;p})].into_iter().enumerate().map(|(index,(kind,payload))| json!({
        "sequence":index+1,"session_id":adapter.session,"run_id":adapter.run,"agent_run_id":intent.agent_run_id,"kind":kind,"event_version":arany::test_support::payload_version(kind),"payload":payload,"created_at_ms":0
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
                "kind":"MessageCommitted","event_version":arany::test_support::payload_version("MessageCommitted"),"payload":{"text":"Late"},"created_at_ms":0
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
                "kind":"MessageCommitted","event_version":arany::test_support::payload_version("MessageCommitted"),"payload":{"text":"Late"},"created_at_ms":0
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

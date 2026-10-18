use super::*;
use arany::test_support::scenario::{ScriptedProvider, WaitingProvider};
use arany::{Event, Provider, RunStatus, Store};
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
    let definition = if value["method"] == "session/update" {
        "SessionNotification"
    } else if value.get("error").is_some() {
        "Error"
    } else if value["result"].get("protocolVersion").is_some() {
        "InitializeResponse"
    } else if value["result"].get("sessionId").is_some() {
        "NewSessionResponse"
    } else if value["result"].get("stopReason").is_some() {
        "PromptResponse"
    } else {
        "AuthenticateResponse"
    };
    let payload = if definition == "SessionNotification" {
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
    assert!(encode_line(value).unwrap().len() <= MAX_LINE_BYTES);
    assert!(
        value.get("method").is_none_or(|m| m == "session/update"),
        "unexpected client call: {value}"
    );
}

async fn next(reader: &mut Reader) -> Value {
    let mut line = String::new();
    assert!(
        tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
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

fn config(state: &std::path::Path, workspace: &std::path::Path) -> Config {
    Config {
        state: state.into(),
        workspace: workspace.into(),
        provider: ProviderArg::Openai,
        model: "gpt-5.4".into(),
        effort: None,
        collaboration: Collaboration::Single,
        max_active_children: None,
        roster: None,
        authenticated: Some(true),
    }
}

fn start(
    state: &std::path::Path,
    workspace: &std::path::Path,
    provider: Arc<dyn Provider>,
) -> (Writer, Reader, tokio::task::JoinHandle<Result<(), String>>) {
    let engine = Engine::open(
        StateRoot::admit(state).expect("private State"),
        EngineConfig::default(),
    )
    .unwrap();
    let config = config(state, workspace);
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
            let server = serve::server::run_with_threads(
                engine,
                state_dir,
                receiver,
                events,
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
    assert_eq!(next(reader).await["result"]["protocolVersion"], 1);
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

fn scripted() -> Arc<ScriptedProvider> {
    Arc::new(
        ScriptedProvider::new(vec![arany::ProviderOutcome::Finish(arany::Finish {
            summary: "Answer".into(),
            result: "Hello".into(),
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
    let (mut writer, mut reader, task) = start(&state, &workspace, provider.clone());
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
    for cancel in ["session/cancel", "$/cancel_request", "eof"] {
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
        let (mut writer, mut reader, task) = start(&state, &workspace, provider);
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
        if cancel != "eof" {
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
async fn malformed_oversized_and_hostile_params_recover_over_stdio() {
    let _deadline = crate::cli::serve::tests::deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let (mut writer, mut reader, task) = start(&state, &workspace, scripted());
    writer.write_all(b"{broken\n").await.unwrap();
    assert_eq!(next(&mut reader).await["error"]["code"], -32700);
    writer.write_all(&vec![b'x'; MAX_LINE_BYTES]).await.unwrap();
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
    send(&mut writer, json!(9), "session/load", json!({})).await;
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
    let mut config = config(
        std::path::Path::new("/unopened-state"),
        std::path::Path::new("/unopened-workspace"),
    );
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
    let config = config(
        std::path::Path::new("/state"),
        std::path::Path::new("/workspace"),
    );
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
    events.send(reply(6, json!({"events":[{"sequence":20,"session_id":session.to_string(),"run_id":run.to_string(),"agent_run_id":null,"kind":"MessageCommitted","event_version":1,"payload":{"run_id":run,"text":"Recovered"},"created_at_ms":0}],"nextSince":null}))).await.unwrap();
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
    let mut config = config(
        std::path::Path::new("/state"),
        std::path::Path::new("/workspace"),
    );
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
        source.write_all(&vec![b'x'; MAX_LINE_BYTES]).await.unwrap();
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

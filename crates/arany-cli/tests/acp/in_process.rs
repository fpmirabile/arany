use super::*;
use arany::test_support::scenario::{ScriptedProvider, WaitingProvider};
use arany::{Event, Provider, RunStatus, SessionId, Store};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

#[path = "goals.rs"]
mod goals;
#[path = "history.rs"]
mod history;
#[path = "lifecycle.rs"]
mod lifecycle;
#[path = "modes.rs"]
mod modes;
#[path = "permissions.rs"]
mod permissions;
#[path = "tool_projection.rs"]
mod tool_projection;
#[path = "transport.rs"]
mod transport;

type Reader = BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>;
type Writer = tokio::io::WriteHalf<tokio::io::DuplexStream>;
const RPC_WAIT: Duration = Duration::from_secs(60);

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
        tokio::time::timeout(RPC_WAIT, reader.read_line(&mut line))
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

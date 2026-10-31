use super::*;
use arany::test_support::scenario::{ScriptedProvider, WaitingProvider};
use arany::{Event, Provider, StateRoot, Store};
use arany_cli::protocol::protocol_schema;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{
    Arc, LazyLock, Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

static PROTOCOL_SCHEMA: LazyLock<Value> = LazyLock::new(protocol_schema);

struct TestDeadline {
    done: std::sync::mpsc::SyncSender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for TestDeadline {
    fn drop(&mut self) {
        let _ = self.done.send(());
        self.worker.take().unwrap().join().unwrap();
    }
}

pub(in crate::cli) fn deadline() -> impl Drop {
    let (done, receiver) = std::sync::mpsc::sync_channel(1);
    let name = std::thread::current()
        .name()
        .unwrap_or("serve test")
        .to_owned();
    // A runtime timer cannot bound a synchronous watch-lock deadlock.
    let worker = std::thread::spawn(move || {
        if receiver.recv_timeout(Duration::from_secs(30))
            == Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr().lock(),
                "{name} exceeded its 30-second deadline"
            );
            std::process::abort();
        }
    });
    TestDeadline {
        done,
        worker: Some(worker),
    }
}

fn request(id: i64, method: &str, params: Value) -> ClientMessage {
    ClientMessage::Message(Message::Request(Request {
        jsonrpc: Default::default(),
        id: RequestId::Integer(id),
        method: method.into(),
        params: Some(params),
    }))
}
fn initialize() -> ClientMessage {
    request(
        1,
        "initialize",
        json!({"clientInfo":{"name":"test","version":"1"}}),
    )
}
fn thread_start(root: &std::path::Path) -> ClientMessage {
    request(
        2,
        "thread/start",
        json!({"cwd":root,"provider":"openai","model":"gpt-5.4","collaboration":"single","title":"Conformance"}),
    )
}
async fn next(output: &mut mpsc::Receiver<Message>) -> Value {
    next_with_timeout(output, Duration::from_secs(10)).await
}
async fn next_with_timeout(output: &mut mpsc::Receiver<Message>, timeout: Duration) -> Value {
    let message = tokio::time::timeout(timeout, output.recv())
        .await
        .expect("server response deadline")
        .expect("server message");
    validate(&message);
    serde_json::to_value(message).unwrap()
}
fn validate(message: &Message) {
    static ENVELOPE: LazyLock<jsonschema::Validator> =
        LazyLock::new(|| jsonschema::validator_for(&PROTOCOL_SCHEMA).unwrap());
    let schema = &*PROTOCOL_SCHEMA;
    let value = serde_json::to_value(message).unwrap();
    assert!(ENVELOPE.is_valid(&value));
    let reference = match message {
        Message::Notification(note) => {
            Some(schema["notifications"][&note.method]["params"].clone())
        }
        Message::Response(response) => match &response.id {
            RequestId::Integer(1) => Some(schema["methods"]["initialize"]["result"].clone()),
            RequestId::Integer(2) => Some(schema["methods"]["thread/start"]["result"].clone()),
            RequestId::Integer(3) => Some(schema["methods"]["turn/start"]["result"].clone()),
            RequestId::Integer(6) => Some(schema["methods"]["thread/resume"]["result"].clone()),
            _ => None,
        },
        Message::Request(request) => {
            Some(schema["serverRequests"][&request.method]["params"].clone())
        }
        _ => None,
    };
    if let Some(reference) = reference {
        let payload = if matches!(message, Message::Notification(_) | Message::Request(_)) {
            &value["params"]
        } else {
            &value["result"]
        };
        assert!(payload_validator(&reference).is_valid(payload), "{value}");
    }
    assert!(encode_line(message).unwrap().len() <= MAX_RPC_LINE_BYTES);
}

fn payload_validator(reference: &Value) -> &'static jsonschema::Validator {
    static VALIDATORS: LazyLock<BTreeMap<String, OnceLock<jsonschema::Validator>>> =
        LazyLock::new(|| {
            ["methods", "notifications", "serverRequests"]
                .into_iter()
                .flat_map(|group| PROTOCOL_SCHEMA[group].as_object().unwrap().values())
                .flat_map(|entry| entry.as_object().unwrap().values())
                .map(|reference| {
                    (
                        reference["$ref"].as_str().unwrap().to_owned(),
                        OnceLock::new(),
                    )
                })
                .collect()
        });
    let reference = reference["$ref"].as_str().unwrap();
    VALIDATORS[reference].get_or_init(|| {
        let part = json!({"$schema":PROTOCOL_SCHEMA["$schema"],"$defs":PROTOCOL_SCHEMA["$defs"],"$ref":reference});
        jsonschema::validator_for(&part).unwrap()
    })
}
fn engine(state: &std::path::Path) -> Engine {
    Engine::open(
        StateRoot::admit(state).expect("private State"),
        EngineConfig::default(),
    )
    .unwrap()
}
fn scripted() -> Arc<ScriptedProvider> {
    Arc::new(
        ScriptedProvider::new(vec![arany::ProviderOutcome::Answer(arany::Answer {
            text: "Hello".into(),
        })])
        .with_profile("openai", "gpt-5.4"),
    )
}

async fn closed_view(
    state: &std::path::Path,
    id: arany::SessionId,
) -> (Vec<arany::EventEnvelope>, arany::SessionView) {
    let store = Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
    let events = store.load_session(id).await.unwrap();
    let view = store.load_view(id).await.unwrap().unwrap();
    store.close().await.unwrap();
    (events, view)
}

fn canonicalize(value: &mut Value, thread: &str, run: &str, goal: &str, workspace: &str) {
    match value {
        Value::String(text) => {
            for (from, to) in [
                (thread, "$thread"),
                (run, "$run"),
                (goal, "$goal"),
                (workspace, "$workspace"),
            ] {
                if !from.is_empty() {
                    *text = text.replace(from, to);
                }
            }
            if text.starts_with("arany/") {
                *text = "arany/$version".into();
            }
        }
        Value::Object(map) => {
            for (key, value) in map {
                if key == "platformOs" {
                    *value = json!("$os");
                    continue;
                }
                if matches!(
                    key.as_str(),
                    "createdAtMs" | "startedAtMs" | "completedAtMs"
                ) {
                    *value = json!(0);
                } else {
                    canonicalize(value, thread, run, goal, workspace);
                }
            }
        }
        Value::Array(array) => {
            for item in array {
                canonicalize(item, thread, run, goal, workspace);
            }
        }
        _ => {}
    }
}

async fn send_golden(
    input: &mpsc::Sender<ClientMessage>,
    transcript: &mut Vec<Value>,
    index: usize,
    workspace: &std::path::Path,
    thread: Option<arany::SessionId>,
) {
    let mut message = include_str!("../fixtures/serve/basic.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter_map(|line| line.get("client").cloned())
        .nth(index)
        .unwrap();
    if let Some(cwd) = message["params"].get_mut("cwd") {
        *cwd = json!(workspace);
    }
    if let Some(id) = message["params"].get_mut("threadId") {
        *id = json!(thread.unwrap());
    }
    transcript.push(json!({"client":message}));
    input
        .send(ClientMessage::Message(
            decode_line(&serde_json::to_vec(&message).unwrap()).unwrap(),
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn initialize_thread_start_turn_start_and_completion_golden() {
    let _deadline = deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let provider = scripted();
    let threads = threads::Threads {
        provider: Some(provider.clone()),
        ..Default::default()
    };
    let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let server =
        server::run_with_threads(engine(&state), state.clone(), inbound, outbound, threads);
    let client = async {
        let mut transcript = Vec::new();
        send_golden(&input, &mut transcript, 0, &workspace, None).await;
        transcript.push(json!({"server":next(&mut output).await}));
        send_golden(&input, &mut transcript, 1, &workspace, None).await;
        send_golden(&input, &mut transcript, 2, &workspace, None).await;
        let started = next(&mut output).await;
        let thread: arany::SessionId =
            serde_json::from_value(started["result"]["thread"]["id"].clone()).unwrap();
        transcript.push(json!({"server":started}));
        transcript.push(json!({"server":next(&mut output).await}));
        send_golden(&input, &mut transcript, 3, &workspace, Some(thread)).await;
        loop {
            let message = next(&mut output).await;
            let done = message["method"] == "turn/completed";
            transcript.push(json!({"server":message}));
            if done {
                break;
            }
        }
        send_golden(&input, &mut transcript, 4, &workspace, Some(thread)).await;
        let resumed = next(&mut output).await;
        assert_eq!(resumed["result"]["turns"][0]["status"], "completed");
        assert_eq!(resumed["result"]["turns"][0]["items"][1]["text"], "Hello");
        transcript.push(json!({"server":resumed}));
        input.send(ClientMessage::Eof).await.unwrap();
        (thread, transcript)
    };
    let (result, (thread, mut transcript)) = tokio::join!(server, client);
    result.unwrap();
    let (events, view) = closed_view(&state, thread).await;
    assert_eq!(view.runs.len(), 1);
    assert_eq!(view.runs[0].assistant_message.as_deref(), Some("Hello"));
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert_eq!(provider.responses.lock().unwrap().len(), 0);
    for kind in [
        "SessionStarted",
        "MessageAccepted",
        "MessageCommitted",
        "RunFinished",
    ] {
        assert_eq!(events.iter().filter(|e| e.event.kind() == kind).count(), 1);
    }
    for recorded in &transcript {
        let message = &recorded["server"];
        if matches!(
            message["method"].as_str(),
            Some("item/started" | "item/completed")
        ) {
            let time = message["params"]["startedAtMs"]
                .as_i64()
                .or_else(|| message["params"]["completedAtMs"].as_i64())
                .unwrap();
            assert!(events.iter().any(|e| e.created_at_ms == time));
        }
    }
    let run = view.runs[0].id.to_string();
    let goal = view.goals[0].id.to_string();
    for value in &mut transcript {
        canonicalize(
            value,
            &thread.to_string(),
            &run,
            &goal,
            workspace.to_str().unwrap(),
        );
    }
    let actual = transcript
        .iter()
        .map(|line| serde_json::to_string(line).unwrap() + "\n")
        .collect::<String>();
    assert_eq!(actual, include_str!("../fixtures/serve/basic.jsonl"));
}

struct FailingReader;

impl AsyncRead for FailingReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(std::io::Error::other("input failure")))
    }
}

fn private_json(path: &std::path::Path, value: Value) {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(&serde_json::to_vec(&value).unwrap())
        .unwrap();
}

#[path = "approvals.rs"]
mod approvals;
#[path = "display.rs"]
mod display;
#[path = "goal_controls.rs"]
mod goal_controls;
#[path = "history.rs"]
mod history_tests;
#[path = "lifecycle.rs"]
mod lifecycle;
#[path = "rewind.rs"]
mod rewind;
#[path = "tool_history.rs"]
mod tool_history;
#[path = "transport.rs"]
mod transport;

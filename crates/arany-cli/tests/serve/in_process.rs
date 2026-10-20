use super::*;
use arany::test_support::scenario::{ScriptedProvider, WaitingProvider};
use arany::{Event, Provider, StateRoot, Store};
use arany_cli::protocol::protocol_schema;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

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
            eprintln!("{name} exceeded its 30-second deadline");
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
    let message = tokio::time::timeout(Duration::from_secs(10), output.recv())
        .await
        .expect("server response deadline")
        .expect("server message");
    validate(&message);
    serde_json::to_value(message).unwrap()
}
fn validate(message: &Message) {
    let schema = protocol_schema();
    let value = serde_json::to_value(message).unwrap();
    assert!(jsonschema::validator_for(&schema).unwrap().is_valid(&value));
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
        let part =
            json!({"$schema":schema["$schema"],"$defs":schema["$defs"],"$ref":reference["$ref"]});
        let payload = if matches!(message, Message::Notification(_) | Message::Request(_)) {
            &value["params"]
        } else {
            &value["result"]
        };
        assert!(
            jsonschema::validator_for(&part).unwrap().is_valid(payload),
            "{value}"
        );
    }
    assert!(encode_line(message).unwrap().len() <= MAX_LINE_BYTES);
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

#[cfg(unix)]
#[test]
#[cfg_attr(not(arany_ci), ignore)]
fn rewind_serve_guarded_corpus_restores_refuses_drift_keeps_overwrites_and_blocks_uncertain() {
    use arany::test_support::goals::finish_with;
    use arany::test_support::scenario::tool_outcome;
    use sha2::{Digest, Sha256};
    let _gate = crate::test_process::NATIVE_GATE.lock().unwrap();
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
    arany::test_support::isolate_account_root();
    for case in [
        "success",
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
            json!({"version":1,"workspace_paths":["src"],"write":true,"commands":[],"skills":[],"mcp":[]}),
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
            let mut params =
                json!({"threadId":thread,"previewToken":preview["previewToken"],"restore":"code"});
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
        }
    }
    });
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

#[tokio::test]
async fn rewind_preview_tokens_bind_thread_head_expiry_and_fork_without_replaying_effects() {
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
            input
                .send(request(
                    3,
                    "turn/start",
                    json!({"threadId":thread,"input":[{"type":"text","text":prompt}]}),
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
        assert_eq!(preview["restore"], json!(["conversation"]));
        assert_eq!(preview["blockers"], json!(["noWriteGrant"]));
        input.send(request(12,"thread/start",json!({"cwd":workspace,"provider":"openai","model":"gpt-5.4","collaboration":"single"}))).await.unwrap();
        let foreign = next(&mut output).await["result"]["thread"]["id"].clone();
        next(&mut output).await;
        input.send(request(11,"thread/rewind",json!({"threadId":foreign,"previewToken":preview["previewToken"],"restore":"conversation"}))).await.unwrap();
        assert_eq!(
            next(&mut output).await["error"]["data"]["reason"],
            "previewExpired"
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
            "previewExpired"
        );
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(61)).await;
        tokio::time::resume();
        input.send(request(11,"thread/rewind",json!({"threadId":thread,"previewToken":preview["previewToken"],"restore":"conversation"}))).await.unwrap();
        assert_eq!(
            next(&mut output).await["error"]["data"]["reason"],
            "previewExpired"
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
        let source: arany::SessionId = serde_json::from_value(thread.clone()).unwrap();
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
            "previewExpired"
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
            "previewExpired"
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

#[tokio::test]
async fn requests_before_initialize_and_second_initialize_are_refused() {
    let _deadline = deadline();
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let client = async {
        for (message, expected) in [
            (request(0, "unknown", json!({})), Some("notInitialized")),
            (initialize(), None),
            (initialize(), Some("alreadyInitialized")),
            (request(4, "unknown", json!({})), Some("")),
        ] {
            input.send(message).await.unwrap();
            let value = next(&mut output).await;
            if let Some(reason) = expected {
                if reason.is_empty() {
                    assert_eq!(value["error"]["code"], -32601);
                } else {
                    assert_eq!(value["error"]["data"]["reason"], reason);
                }
            } else {
                assert!(value.get("result").is_some());
            }
        }
        input.send(ClientMessage::Eof).await.unwrap();
    };
    let (result, ()) = tokio::join!(
        server::run_with_threads(
            engine(&state),
            state,
            inbound,
            outbound,
            threads::Threads::default()
        ),
        client
    );
    result.unwrap();
}

#[tokio::test]
async fn oversized_and_malformed_lines_are_rejected_without_stopping_the_server() {
    let _deadline = deadline();
    let (mut client, transport) = tokio::io::duplex(4096);
    let (input, mut inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let reader = read_messages(transport, input, outbound);
    let peer = async {
        client
            .write_all(&vec![b'x'; MAX_LINE_BYTES + 33])
            .await
            .unwrap();
        client.write_all(b"\n[]\n{\"id\":1,\"method\":\"initialize\",\"params\":{\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n").await.unwrap();
        client.shutdown().await.unwrap();
        assert_eq!(
            next(&mut output).await["error"]["data"]["reason"],
            "messageTooLarge"
        );
        assert_eq!(next(&mut output).await["error"]["code"], -32700);
        assert!(matches!(
            inbound.recv().await,
            Some(ClientMessage::Message(Message::Request(_)))
        ));
        assert!(matches!(inbound.recv().await, Some(ClientMessage::Eof)));
    };
    let (result, ()) = tokio::join!(reader, peer);
    result.unwrap();
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

#[tokio::test]
async fn reader_failure_notifies_the_server_of_eof() {
    let _deadline = deadline();
    let (input, mut inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, _output) = mpsc::channel(OUTBOUND_CAPACITY);
    assert_eq!(
        read_messages(FailingReader, input, outbound).await,
        Err("protocol input failed".into())
    );
    assert!(matches!(inbound.try_recv(), Ok(ClientMessage::Eof)));
}
#[tokio::test]
async fn reader_failure_during_a_turn_cancels_and_closes_the_engine() {
    let _deadline = deadline();
    waiting_turn(TurnStop::ReaderFailure).await;
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
            transport_input,
            transport_output,
            reader,
            writer,
            shutdown,
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
            },
        );
    }
    let error = threads.check_capacity(None).err().unwrap().message(None);
    assert_eq!(
        serde_json::to_value(error).unwrap()["error"]["data"]["reason"],
        "threadLimit"
    );
    assert!(threads.check_capacity(first).is_ok());
    assert_eq!(threads.map.len(), 64);
}

#[tokio::test]
async fn bounded_reader_backpressures_after_64_pending_requests() {
    let _deadline = deadline();
    use std::{future::Future, task::Poll};
    let (input, mut inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, _output) = mpsc::channel(OUTBOUND_CAPACITY);
    let bytes = b"{\"id\":1,\"method\":\"x\"}\n".repeat(INBOUND_CAPACITY + 1);
    let mut reader = std::pin::pin!(read_messages(bytes.as_slice(), input, outbound));
    let ready = std::future::poll_fn(|cx| Poll::Ready(reader.as_mut().poll(cx).is_ready())).await;
    assert!(!ready);
    assert_eq!(inbound.len(), 64);
    inbound.recv().await.unwrap();
    let ready = std::future::poll_fn(|cx| Poll::Ready(reader.as_mut().poll(cx).is_ready())).await;
    assert!(!ready, "EOF also waits behind admitted requests");
    assert_eq!(inbound.len(), 64);
    for _ in 0..64 {
        inbound.recv().await.unwrap();
    }
    reader.await.unwrap();
    assert!(matches!(inbound.recv().await, Some(ClientMessage::Eof)));
}

#[tokio::test]
async fn bounded_writer_preserves_messages_and_backpressures_at_256() {
    let _deadline = deadline();
    use std::{future::Future, task::Poll};
    let (outbound, writer) = mpsc::channel(OUTBOUND_CAPACITY);
    let message = server::response(
        RequestId::Integer(1),
        json!({"text":"line\nnext\u{001b}[31m"}),
    );
    for _ in 0..OUTBOUND_CAPACITY {
        server::send(&outbound, message.clone()).await.unwrap();
    }
    let extra = outbound.clone();
    let extra_message = message.clone();
    let mut send = Box::pin(async move { server::send(&extra, extra_message).await });
    assert!(!std::future::poll_fn(|cx| Poll::Ready(send.as_mut().poll(cx).is_ready())).await);
    let (transport, mut client) = tokio::io::duplex(128);
    let writing = write_messages(transport, writer);
    let peer = async {
        send.await.unwrap();
        drop(outbound);
        let expected = encode_line(&message).unwrap().repeat(OUTBOUND_CAPACITY + 1);
        let mut actual = vec![0; expected.len()];
        tokio::io::AsyncReadExt::read_exact(&mut client, &mut actual)
            .await
            .unwrap();
        assert_eq!(actual, expected);
    };
    let (result, ()) = tokio::join!(writing, peer);
    result.unwrap();
}

#[test]
fn tool_items_preserve_journaled_dispositions_and_unfinished_history_is_uncertain() {
    use arany::{CacheUpdate, ToolCall, ToolDisposition, ToolObservation};
    let intent = arany::EffectIntent {
        target: arany::worker::EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id: arany::RunId::new(),
        agent_run_id: arany::AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: ToolCall::Command {
            program: "check".into(),
            args: vec!["--check".into()],
            cwd: ".".into(),
        },
        limits: arany::ToolLimits::default(),
        expires_at_ms: 1,
        use_count: 1,
    };
    for (disposition, status) in [
        (ToolDisposition::Succeeded, "completed"),
        (ToolDisposition::Denied, "declined"),
        (ToolDisposition::Failed, "failed"),
        (ToolDisposition::Conflict, "failed"),
        (ToolDisposition::Limit, "failed"),
        (ToolDisposition::Cancelled, "failed"),
        (ToolDisposition::Uncertain, "uncertain"),
    ] {
        let observation = ToolObservation {
            intent: intent.clone(),
            disposition,
            enforcement_cause: None,
            output: "bounded\noutput".into(),
            attestation: None,
            cache_update: CacheUpdate::NotApplicable,
        };
        let item =
            serde_json::to_value(items::tool_item(&intent, Some(&observation), false)).unwrap();
        assert_eq!(item["status"], status);
        assert_eq!(
            item["disposition"],
            serde_json::to_value(disposition).unwrap()
        );
        assert_eq!(item["output"], "bounded\noutput");
    }
    let observation = ToolObservation {
        intent: intent.clone(),
        disposition: ToolDisposition::Succeeded,
        enforcement_cause: None,
        output: String::new(),
        attestation: None,
        cache_update: CacheUpdate::Uncertain,
    };
    assert_eq!(
        serde_json::to_value(items::tool_item(&intent, Some(&observation), false)).unwrap()["status"],
        "uncertain"
    );
    assert_eq!(
        serde_json::to_value(items::tool_item(&intent, None, true)).unwrap()["status"],
        "uncertain"
    );
    let mut run = arany::RunView {
        supervisor: Default::default(),
        workers: Default::default(),
        id: intent.run_id,
        objective: "Inspect".into(),
        images: Vec::new(),
        config: None,
        provider_fallbacks: Vec::new(),
        agents: Vec::new(),
        assistant_message: None,
        status: arany::RunStatus::Interrupted,
        accepted_sequence: 1,
        finished_sequence: None,
        failure: None,
        tools: vec![arany::ToolView {
            checkpoint: None,
            intent: intent.clone(),
            observation: None,
            review_call_index: None,
            permission_verdict: None,
            hooks: Vec::new(),
            hook_sequence: None,
            finished_sequence: None,
        }],
        tool_compaction_summary: None,
        tool_compaction_covered_count: 0,
        definitions: Default::default(),
        display: Vec::new(),
        steering_notes: Vec::new(),
        lifecycle_hooks: Vec::new(),
        lifecycle_hook_sequence: None,
        provider_sequences: Default::default(),
    };
    assert_eq!(
        serde_json::to_value(items::history_items(&run)).unwrap()[1]["status"],
        "uncertain"
    );
    for (call, kind) in [
        (
            ToolCall::CommandUnguarded {
                program: "check".into(),
                args: vec!["--check".into()],
                cwd: ".".into(),
                justification: "Inspect the same command after explicit approval".into(),
            },
            "toolCall",
        ),
        (
            ToolCall::Write {
                path: "new".into(),
                expected_digest: None,
                content: "data".into(),
            },
            "fileChange",
        ),
        (ToolCall::Mkdir { path: "dir".into() }, "fileChange"),
        (
            ToolCall::McpList {
                server: "local".into(),
            },
            "mcpToolCall",
        ),
        (
            ToolCall::McpCall {
                server: "local".into(),
                tool: "echo".into(),
                schema_digest: "x".into(),
                arguments: "{\"text\":\"x\"}".into(),
            },
            "mcpToolCall",
        ),
        (
            ToolCall::Read {
                path: "file".into(),
                offset: 0,
                limit: 32,
            },
            "toolCall",
        ),
    ] {
        run.tools[0].intent.call = call;
        let item = serde_json::to_value(items::history_items(&run)).unwrap();
        assert_eq!(item[1]["type"], kind);
        assert_eq!(item[1]["status"], "uncertain");
        if matches!(run.tools[0].intent.call, ToolCall::CommandUnguarded { .. }) {
            assert_eq!(item[1]["call"]["operation"], "command_unguarded");
            assert!(item[1].get("attestation").is_none());
        }
    }
    let mut child_tool = run.tools.pop().unwrap();
    child_tool.intent.id = uuid::Uuid::now_v7();
    run.agents.push(arany::AgentView {
        profile: None,
        assignment: None,
        id: child_tool.intent.agent_run_id,
        role: arany::AgentRole::Child,
        ordinal: 1,
        objective: Some("Inspect".into()),
        summary: None,
        result: None,
        provider_calls: Vec::new(),
        grant: None,
        definition: None,
        tools: vec![child_tool],
        status: arany::AgentStatus::Interrupted,
    });
    let history = serde_json::to_value(items::history_items(&run)).unwrap();
    assert_eq!(history[1]["status"], "uncertain");
    assert_eq!(
        history[1]["id"],
        run.agents[0].tools[0].intent.id.to_string()
    );
}

#[cfg(unix)]
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

#[derive(Clone, Copy, Debug, PartialEq)]
enum ApprovalCase {
    Accept,
    Decline,
    Cancel,
    Interrupt,
    Eof,
    Timeout,
    Duplicate,
    Invalid,
    ClientError,
    Backpressure,
}

#[cfg(unix)]
#[tokio::test]
async fn approval_accept_dispatches_the_exact_journaled_intent_once() {
    let _deadline = deadline();
    approval_scenario(ApprovalCase::Accept).await;
}

#[cfg(unix)]
#[tokio::test]
async fn approval_decline_invalid_and_client_error_deny() {
    let _deadline = deadline();
    for case in [
        ApprovalCase::Decline,
        ApprovalCase::Invalid,
        ApprovalCase::ClientError,
    ] {
        approval_scenario(case).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn approval_cancel_interrupt_and_eof_deny() {
    let _deadline = deadline();
    for case in [
        ApprovalCase::Cancel,
        ApprovalCase::Interrupt,
        ApprovalCase::Eof,
    ] {
        approval_scenario(case).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn approval_timeout_denies_and_ignores_a_late_response() {
    let _deadline = deadline();
    approval_scenario(ApprovalCase::Timeout).await;
}

#[cfg(unix)]
#[tokio::test]
async fn approval_duplicate_and_unknown_responses_cannot_resolve_another_intent() {
    let _deadline = deadline();
    approval_scenario(ApprovalCase::Duplicate).await;
}

#[cfg(unix)]
#[tokio::test]
async fn approval_expires_and_journal_finishes_while_outbound_is_full() {
    let _deadline = deadline();
    approval_scenario(ApprovalCase::Backpressure).await;
}

fn decision(id: Value, result: Value) -> ClientMessage {
    ClientMessage::Message(Message::Response(Response {
        jsonrpc: Default::default(),
        id: serde_json::from_value(id).unwrap(),
        result,
    }))
}

#[cfg(unix)]
async fn approval_scenario(case: ApprovalCase) {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("facts.txt"), "private fact").unwrap();
    let state = root.path().join("state");
    let state_root = StateRoot::admit(&state).unwrap();
    private_json(
        &state.join("tools.json"),
        json!({"version":1,"workspace_paths":["facts.txt"],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    private_json(
        &state.join("permissions.json"),
        json!({"permissions":{"ask":["Read"]}}),
    );
    let engine = Engine::open(
        state_root,
        EngineConfig {
            guard_executable: Some(std::env::current_exe().unwrap()),
            ..Default::default()
        },
    )
    .unwrap();
    let mut events = engine.subscribe();
    let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let stalled = outbound.clone();
    let fill_output = outbound.clone();
    let read = arany::test_support::scenario::tool_outcome(arany::ToolCall::Read {
        path: "facts.txt".into(),
        offset: 0,
        limit: 32,
    });
    let finish = arany::ProviderOutcome::Answer(arany::Answer {
        text: "Finished".into(),
    });
    let outcomes = if case == ApprovalCase::Duplicate {
        vec![read.clone(), read, finish]
    } else {
        vec![read, finish]
    };
    let provider = Arc::new(
        ScriptedProvider::new(outcomes)
            .with_profile("openai", "gpt-5.4")
            .with_before_call(0, move || {
                if case == ApprovalCase::Backpressure {
                    for _ in 0..fill_output.capacity() {
                        fill_output
                            .try_send(server::response(RequestId::Integer(99), json!({})))
                            .unwrap();
                    }
                }
            }),
    );
    let requests = provider.requests.clone();
    let threads = threads::Threads {
        provider: Some(provider.clone()),
        ..Default::default()
    };
    let client = async {
        input.send(initialize()).await.unwrap();
        assert!(
            next(&mut output).await["result"]["capabilities"]
                .as_array()
                .unwrap()
                .contains(&json!("approvals_v1"))
        );
        let start = json!({"cwd":workspace,"provider":"openai","model":"gpt-5.4","tools":true,"permissionMode":"auto","collaboration":"single"});
        input.send(request(2, "thread/start", start)).await.unwrap();
        let thread = next(&mut output).await["result"]["thread"]["id"].clone();
        next(&mut output).await;
        input
            .send(request(
                3,
                "turn/start",
                json!({"threadId":thread,"input":[{"type":"text","text":"Read the fact"}]}),
            ))
            .await
            .unwrap();
        let mut wire_approval = None;
        let mut approval_count = 0;
        if case == ApprovalCase::Backpressure {
            loop {
                if let Some(arany::Tapped::Committed(event)) = events.next().await
                    && matches!(event.event, Event::ToolStarted { .. })
                {
                    break;
                }
            }
            tokio::time::pause();
            let mut denied = false;
            loop {
                if let Some(arany::Tapped::Committed(event)) = events.next().await {
                    if let Event::ToolPermissionDecided { verdict, .. } = &event.event {
                        assert!(!verdict.approved);
                        denied = true;
                    }
                    if matches!(event.event, Event::RunFinished { .. }) {
                        assert!(denied);
                        break;
                    }
                }
            }
            tokio::time::resume();
            assert_eq!(stalled.capacity(), 0);
        } else {
            let mut approval = None;
            let mut started = false;
            while approval.is_none() || !started {
                let message = next(&mut output).await;
                if message["id"] == 3 && message.get("result").is_some() {
                    started = true;
                }
                if message["method"] == "item/tool/requestApproval" {
                    approval = Some(message);
                }
            }
            let approval = approval.unwrap();
            approval_count += 1;
            assert_eq!(approval["id"], 1);
            let params = &approval["params"];
            assert_eq!(params["threadId"], thread);
            assert_eq!(params["turnId"], params["intent"]["run_id"]);
            assert_eq!(params["itemId"], params["intent"]["id"]);
            assert_eq!(params["expiresAtMs"], params["intent"]["expires_at_ms"]);
            let intent: arany::EffectIntent =
                serde_json::from_value(params["intent"].clone()).unwrap();
            assert_eq!(
                params["lines"],
                json!(crate::presentation::tool_approval_lines(&intent))
            );
            wire_approval = Some(approval.clone());
            match case {
                ApprovalCase::Accept => input
                    .send(decision(
                        approval["id"].clone(),
                        json!({"decision":"accept"}),
                    ))
                    .await
                    .unwrap(),
                ApprovalCase::Decline => input
                    .send(decision(
                        approval["id"].clone(),
                        json!({"decision":"decline"}),
                    ))
                    .await
                    .unwrap(),
                ApprovalCase::Cancel => input
                    .send(decision(
                        approval["id"].clone(),
                        json!({"decision":"cancel"}),
                    ))
                    .await
                    .unwrap(),
                ApprovalCase::Interrupt => input
                    .send(request(
                        5,
                        "turn/interrupt",
                        json!({"threadId":thread,"turnId":params["turnId"]}),
                    ))
                    .await
                    .unwrap(),
                ApprovalCase::Eof => input.send(ClientMessage::Eof).await.unwrap(),
                ApprovalCase::Timeout => {
                    tokio::time::pause();
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis();
                    let remaining = u128::from(intent.expires_at_ms).saturating_sub(now);
                    tokio::time::advance(Duration::from_millis(
                        u64::try_from(remaining).unwrap() + 1,
                    ))
                    .await;
                    tokio::time::resume();
                    loop {
                        if let Some(arany::Tapped::Committed(event)) = events.next().await
                            && matches!(event.event, Event::ToolPermissionDecided { .. })
                        {
                            break;
                        }
                    }
                    input
                        .send(decision(
                            approval["id"].clone(),
                            json!({"decision":"accept"}),
                        ))
                        .await
                        .unwrap();
                }
                ApprovalCase::Duplicate => {
                    input
                        .send(decision(json!(999), json!({"decision":"accept"})))
                        .await
                        .unwrap();
                    input
                        .send(decision(
                            approval["id"].clone(),
                            json!({"decision":"accept"}),
                        ))
                        .await
                        .unwrap();
                    input
                        .send(decision(
                            approval["id"].clone(),
                            json!({"decision":"accept"}),
                        ))
                        .await
                        .unwrap();
                }
                ApprovalCase::Invalid => {
                    input
                        .send(decision(
                            approval["id"].clone(),
                            json!({"decision":"acceptForSession"}),
                        ))
                        .await
                        .unwrap();
                }
                ApprovalCase::ClientError => input
                    .send(ClientMessage::Message(Message::Error(ErrorResponse {
                        jsonrpc: Default::default(),
                        id: Some(serde_json::from_value(approval["id"].clone()).unwrap()),
                        error: ErrorObject {
                            code: -32603,
                            message: "Human channel unavailable".into(),
                            data: None,
                        },
                    })))
                    .await
                    .unwrap(),
                ApprovalCase::Backpressure => unreachable!(),
            }
        }
        let mut completed = false;
        while !completed {
            let message = next(&mut output).await;
            assert!(message.get("error").is_none(), "{case:?}: {message}");
            if message
                .get("method")
                .and_then(Value::as_str)
                .is_some_and(|m| m.ends_with("requestApproval"))
            {
                approval_count += 1;
                assert_eq!(case, ApprovalCase::Duplicate);
                assert_eq!(message["id"], 2);
                input
                    .send(decision(
                        message["id"].clone(),
                        json!({"decision":"accept"}),
                    ))
                    .await
                    .unwrap();
            }
            if message["method"] == "turn/completed" {
                let interrupted = matches!(
                    case,
                    ApprovalCase::Cancel | ApprovalCase::Interrupt | ApprovalCase::Eof
                );
                assert_eq!(
                    message["params"]["turn"]["status"] == "interrupted",
                    interrupted,
                    "{case:?}: {message}"
                );
                completed = true;
            }
        }
        assert_eq!(
            approval_count,
            if case == ApprovalCase::Duplicate {
                2
            } else {
                usize::from(case != ApprovalCase::Backpressure)
            }
        );
        if case == ApprovalCase::Duplicate {
            input
                .send(decision(json!(2), json!({"decision":"accept"})))
                .await
                .unwrap();
        }
        if case != ApprovalCase::Eof {
            input.send(ClientMessage::Eof).await.unwrap();
        }
        (serde_json::from_value(thread).unwrap(), wire_approval)
    };
    let (result, (id, wire_approval)) = tokio::join!(
        server::run_with_threads(engine, state.clone(), inbound, outbound, threads),
        client
    );
    result.unwrap();
    while let Ok(message) = output.try_recv() {
        assert!(
            serde_json::to_value(&message)
                .unwrap()
                .get("error")
                .is_none(),
            "{case:?}: {message:?}"
        );
    }
    let (events, view) = closed_view(&state, id).await;
    let tools: Vec<_> = view.runs[0]
        .tools
        .iter()
        .chain(
            view.runs[0]
                .agents
                .iter()
                .flat_map(|agent| agent.tools.iter()),
        )
        .collect();
    let accepted = matches!(case, ApprovalCase::Accept | ApprovalCase::Duplicate);
    assert_eq!(
        tools.len(),
        if case == ApprovalCase::Duplicate {
            2
        } else {
            1
        }
    );
    assert_eq!(
        tools
            .iter()
            .filter(|tool| tool.observation.as_ref().unwrap().disposition
                == arany::ToolDisposition::Succeeded)
            .count(),
        if case == ApprovalCase::Duplicate {
            2
        } else {
            usize::from(accepted)
        }
    );
    for tool in &tools {
        let verdict = tool.permission_verdict.as_ref().unwrap();
        assert_eq!(verdict.source, arany::permissions::PermissionSource::Human);
        assert_eq!(
            verdict.approved,
            tool.observation.as_ref().unwrap().disposition == arany::ToolDisposition::Succeeded
        );
    }
    if let Some(approval) = wire_approval {
        let event = events.iter().find(|event| matches!(&event.event, Event::ToolStarted { intent, .. } if json!(intent.id) == approval["params"]["itemId"])).unwrap();
        let Event::ToolStarted { intent, .. } = &event.event else {
            unreachable!()
        };
        assert_eq!(approval["params"]["intent"], json!(intent));
        assert_eq!(
            approval["params"]["startedAtMs"],
            json!(event.created_at_ms)
        );
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::ToolFinished { .. }))
            .count(),
        tools.len()
    );
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.phase != arany::AgentPhase::ToolReview)
    );
    if !accepted {
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| !format!("{request:?}").contains("private fact"))
        );
    }
    assert_eq!(provider.responses.lock().unwrap().is_empty(), accepted);
}

#[cfg(unix)]
#[tokio::test]
async fn thread_resume_lists_turns_and_tool_items_with_journaled_dispositions_including_uncertain()
{
    let _deadline = deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("facts.txt"), "Fact").unwrap();
    let source = root.path().join("source");
    let source_root = StateRoot::admit(&source).unwrap();
    private_json(
        &source.join("tools.json"),
        json!({"version":1,"workspace_paths":["facts.txt"],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    let engine = Engine::open(
        source_root,
        EngineConfig {
            guard_executable: Some(std::env::current_exe().unwrap()),
            ..Default::default()
        },
    )
    .unwrap();
    let provider = Arc::new(
        ScriptedProvider::new(vec![
            arany::test_support::scenario::tool_outcome(arany::ToolCall::Read {
                path: "facts.txt".into(),
                offset: 0,
                limit: 32,
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
    let (events, view) = closed_view(&source, outcome.session_id).await;
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
    for envelope in events {
        let last = matches!(envelope.event, Event::ToolStarted { .. });
        store
            .append(outcome.session_id, envelope.event)
            .await
            .unwrap();
        if last {
            break;
        }
    }
    store.close().await.unwrap();
    for (state, status) in [(source, "completed"), (interrupted, "uncertain")] {
        let threads = threads::Threads {
            provider: Some(scripted()),
            ..Default::default()
        };
        let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
        let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
        let client = async {
            input.send(initialize()).await.unwrap();
            next(&mut output).await;
            input.send(super::tests::request(6,"thread/resume",json!({"threadId":outcome.session_id,"cwd":workspace,"provider":"openai","model":"gpt-5.4"}))).await.unwrap();
            let message = next(&mut output).await;
            assert_eq!(message["result"]["turns"][0]["items"][1]["status"], status);
            if status == "uncertain" {
                assert_eq!(message["result"]["turns"][0]["status"], "interrupted");
                assert!(
                    message["result"]["turns"][0]["items"][1]
                        .get("disposition")
                        .is_none()
                );
            }
            input.send(ClientMessage::Eof).await.unwrap();
        };
        let (result, ()) = tokio::join!(
            server::run_with_threads(
                super::tests::engine(&state),
                state,
                inbound,
                outbound,
                threads
            ),
            client
        );
        result.unwrap();
    }
}

#[tokio::test]
async fn failed_accepted_turns_complete_with_error_guidance_instead_of_rpc_errors() {
    let _deadline = deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let provider = Arc::new(ScriptedProvider::new(Vec::new()).with_profile("openai", "gpt-5.4"));
    provider
        .responses
        .lock()
        .unwrap()
        .push_back(Err(arany::ProviderError::Unavailable));
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
        crate::presentation::run_feedback(&view.runs[0]).unwrap()
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

#[test]
fn engine_errors_keep_closed_variant_reasons_and_compiled_messages() {
    for (error, reason) in [
        (
            arany::EngineError::GoalAdmissionUncertain,
            "goalAdmissionUncertain",
        ),
        (
            arany::EngineError::CancellationDrainTimeout,
            "cancellationDrainTimeout",
        ),
        (arany::EngineError::WorkspaceMismatch, "workspaceMismatch"),
    ] {
        let expected = error.to_string();
        let message =
            serde_json::to_value(server::Fault::engine(error).message(Some(RequestId::Integer(9))))
                .unwrap();
        assert_eq!(message["error"]["code"], -32000);
        assert_eq!(message["error"]["message"], expected);
        assert_eq!(message["error"]["data"]["reason"], reason);
    }
}

#[test]
fn events_resync_reports_the_gap_and_last_committed_sequence() {
    let thread = arany::SessionId::new();
    let mut state = items::TurnState {
        last_sequence: 41,
        ..Default::default()
    };
    let notes = items::project(&arany::Tapped::Resync { missed: 300 }, thread, &mut state);
    assert_eq!(notes.len(), 1);
    let message = Message::Notification(notes.into_iter().next().unwrap());
    validate(&message);
    assert_eq!(
        serde_json::to_value(message).unwrap(),
        json!({"jsonrpc":"2.0","method":"events/resync","params":{"threadId":thread,"missed":300,"lastSequence":41}})
    );
    assert_eq!(state.last_sequence, 41);
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
        tokio::time::advance(super::super::exec::SHUTDOWN_GRACE).await;
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
    assert_eq!(started.elapsed(), super::super::exec::SHUTDOWN_GRACE);
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

#[path = "goal_controls.rs"]
mod goal_controls;
#[path = "history.rs"]
mod history_tests;

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
        for _ in 0..64 {
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
                json!({"cwd":workspace,"limit":2}),
            ))
            .await
            .unwrap();
        let first = next(&mut output).await;
        let page = &first["result"];
        assert_eq!(page["data"].as_array().unwrap().len(), 2);
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
        assert_eq!(second["result"]["data"].as_array().unwrap().len(), 63);
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

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
    assert!(encode_line(message).unwrap().len() <= MAX_RPC_LINE_BYTES);
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

struct DisplayProvider {
    scripted: ScriptedProvider,
    preview: Mutex<Option<arany::ResponsePreview>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl Provider for DisplayProvider {
    fn identity(&self) -> &arany::ProviderIdentity {
        self.scripted.identity()
    }

    async fn invoke(
        &self,
        request: arany::ProviderRequest,
    ) -> Result<arany::ProviderResponse, arany::ProviderError> {
        self.entered.notify_one();
        self.release.notified().await;
        self.scripted.invoke(request).await
    }

    async fn invoke_with_preview(
        &self,
        request: arany::ProviderRequest,
        preview: arany::ResponsePreview,
    ) -> Result<arany::ProviderResponse, arany::ProviderError> {
        *self.preview.lock().unwrap() = Some(preview);
        self.invoke(request).await
    }
}

async fn start_display_turn(
    input: &mpsc::Sender<ClientMessage>,
    output: &mut mpsc::Receiver<Message>,
    workspace: &std::path::Path,
    enabled: bool,
    opt_out: bool,
) -> (Value, Value) {
    input.send(request(1, "initialize", json!({"clientInfo":{"name":"test","version":"1"},"capabilities":{"provisionalDisplay":enabled,"optOutNotificationMethods":if opt_out {vec!["turn/display"]} else {vec![]}}}))).await.unwrap();
    next(output).await;
    input.send(thread_start(workspace)).await.unwrap();
    let thread = next(output).await["result"]["thread"]["id"].clone();
    next(output).await;
    input
        .send(request(
            3,
            "turn/start",
            json!({"threadId":thread,"input":[{"type":"text","text":"Hi"}]}),
        ))
        .await
        .unwrap();
    let mut run = Value::Null;
    loop {
        let message = next(output).await;
        if message["id"] == 3 {
            run = message["result"]["turn"]["id"].clone();
        }
        if message["method"] == "item/completed" {
            return (thread, run);
        }
    }
}

async fn publish_display(
    preview: Option<arany::ResponsePreview>,
    output: &mut mpsc::Receiver<Message>,
    thread: &Value,
    run: &Value,
    stalled: bool,
) {
    if let Some(preview) = preview {
        preview
            .publish(arany::PreviewText {
                answer: "Provisional draft".into(),
                tentative: true,
                ..Default::default()
            })
            .unwrap();
        if !stalled {
            let message = next(output).await;
            assert_eq!(message["method"], "turn/display");
            assert_eq!(&message["params"]["threadId"], thread);
            assert_eq!(&message["params"]["turnId"], run);
            assert_eq!(
                message["params"]["update"]["preview"]["answer"],
                "Provisional draft"
            );
            assert_eq!(message["params"]["update"]["preview"]["tentative"], true);
        }
        preview.publish(arany::PreviewText::default()).unwrap();
        if !stalled {
            assert_eq!(
                next(output).await["params"]["update"]["preview"]["answer"],
                ""
            );
        }
    }
}

#[tokio::test]
async fn provisional_display_is_opt_in_lossy_and_withdrawn_tentative_text_is_not_replayed() {
    let _deadline = deadline();
    for (enabled, opt_out, stalled) in [
        (false, false, false),
        (true, true, false),
        (true, false, false),
        (true, false, true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = root.path().join("state");
        let provider = Arc::new(DisplayProvider {
            scripted: ScriptedProvider::new(vec![arany::ProviderOutcome::Answer(arany::Answer {
                text: "Hello".into(),
            })])
            .with_profile("openai", "gpt-5.4"),
            preview: Mutex::new(None),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let engine = engine(&state);
        let mut events = engine.subscribe();
        let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
        let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
        let fill = outbound.clone();
        let server = tokio::spawn(server::run_with_threads(
            engine,
            state.clone(),
            inbound,
            outbound,
            threads::Threads::with_provider(provider.clone()),
        ));
        let (thread, run) =
            start_display_turn(&input, &mut output, &workspace, enabled, opt_out).await;
        provider.entered.notified().await;
        let preview = provider.preview.lock().unwrap().clone();
        assert_eq!(preview.is_some(), enabled && !opt_out);
        if stalled {
            for _ in 0..fill.capacity() {
                fill.try_send(server::response(RequestId::Integer(99), json!({})))
                    .unwrap();
            }
        }
        publish_display(preview, &mut output, &thread, &run, stalled).await;
        provider.release.notify_one();
        if stalled {
            loop {
                if let Some(arany::Tapped::Committed(event)) = events.next().await
                    && matches!(event.event, Event::RunFinished { .. })
                {
                    break;
                }
            }
            assert_eq!(
                fill.capacity(),
                0,
                "Journal settles while output stays full"
            );
        }
        loop {
            let message = next(&mut output).await;
            if message["method"] == "turn/display" {
                assert!(enabled && !opt_out);
                let preview = &message["params"]["update"]["preview"];
                assert!(preview.is_null() || preview["answer"] == "");
            }
            if message["method"] == "turn/completed" {
                assert_eq!(message["params"]["turn"]["status"], "completed");
                break;
            }
        }
        input.send(ClientMessage::Eof).await.unwrap();
        server.await.unwrap().unwrap();
        assert!(output.try_recv().is_err());
        let (events, view) = closed_view(&state, serde_json::from_value(thread).unwrap()).await;
        assert_eq!(view.runs[0].assistant_message.as_deref(), Some("Hello"));
        assert!(
            !events.iter().any(|event| event
                .event
                .payload()
                .unwrap()
                .contains("Provisional draft"))
        );
        assert_eq!(provider.scripted.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn provisional_display_preserves_durable_chunks_commentary_and_settlement_tails() {
    let chunked = format!("{}completion tail", "Published checkpoint ".repeat(256));
    for (snapshot, cancel) in [
        (
            arany::PreviewText {
                answer: chunked,
                ..Default::default()
            },
            false,
        ),
        (
            arany::PreviewText {
                commentary: vec!["Public commentary".into()],
                ..Default::default()
            },
            false,
        ),
        (
            arany::PreviewText {
                answer: "Completion tail".into(),
                tentative: true,
                ..Default::default()
            },
            false,
        ),
        (
            arany::PreviewText {
                answer: "Cancellation tail".into(),
                ..Default::default()
            },
            true,
        ),
        (
            arany::PreviewText {
                answer: "Tentative cancellation tail".into(),
                tentative: true,
                ..Default::default()
            },
            true,
        ),
    ] {
        display_checkpoint_scenario(snapshot, cancel).await;
    }
}

async fn display_checkpoint_scenario(snapshot: arany::PreviewText, cancel: bool) {
    let _deadline = deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let answer = format!("{} confirmed", snapshot.answer);
    let provider = Arc::new(DisplayProvider {
        scripted: ScriptedProvider::new(vec![arany::ProviderOutcome::Answer(arany::Answer {
            text: answer.clone(),
        })])
        .with_profile("openai", "gpt-5.4"),
        preview: Mutex::new(None),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let server = tokio::spawn(server::run_with_threads(
        engine(&state),
        state.clone(),
        inbound,
        outbound,
        threads::Threads::with_provider(provider.clone()),
    ));
    let (thread, run) = start_display_turn(&input, &mut output, &workspace, true, false).await;
    provider.entered.notified().await;
    let preview = provider.preview.lock().unwrap().clone().unwrap();
    preview.publish(snapshot.clone()).unwrap();
    let message = next(&mut output).await;
    assert_eq!(message["method"], "turn/display");
    assert_eq!(
        message["params"]["update"]["preview"]["answer"],
        snapshot.answer
    );
    assert_eq!(
        message["params"]["update"]["preview"]["commentary"],
        json!(snapshot.commentary)
    );
    let thread_id = serde_json::from_value(thread.clone()).unwrap();
    let (_, before) = closed_view(&state, thread_id).await;
    if cancel {
        input
            .send(request(
                4,
                "turn/interrupt",
                json!({"threadId":thread,"turnId":run}),
            ))
            .await
            .unwrap();
    } else {
        provider.release.notify_one();
    }
    loop {
        let message = next(&mut output).await;
        assert!(message.get("error").is_none(), "{message}");
        if message["method"] == "turn/completed" {
            assert_eq!(
                message["params"]["turn"]["status"],
                if cancel { "interrupted" } else { "completed" }
            );
            break;
        }
    }
    input.send(ClientMessage::Eof).await.unwrap();
    server.await.unwrap().unwrap();
    assert_reopened_display_checkpoints(&state, &snapshot, &before, cancel, &answer).await;
    assert_eq!(
        provider.scripted.requests.lock().unwrap().len(),
        usize::from(!cancel)
    );
}

async fn assert_reopened_display_checkpoints(
    state: &std::path::Path,
    snapshot: &arany::PreviewText,
    before: &arany::SessionView,
    cancel: bool,
    answer: &str,
) {
    let (events, after) = closed_view(state, before.id).await;
    assert_display_checkpoints(snapshot, &before.runs[0], &after.runs[0], cancel);
    let recorded: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::AssistantDisplayRecorded { chunk, .. } => Some(chunk.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(recorded, after.runs[0].display);
    assert_eq!(
        after.runs[0].assistant_message.as_deref(),
        (!cancel).then_some(answer)
    );
    assert_eq!(
        after.runs[0].status,
        if cancel {
            arany::RunStatus::Cancelled
        } else {
            arany::RunStatus::Finished
        }
    );
}

fn assert_display_checkpoints(
    snapshot: &arany::PreviewText,
    before: &arany::RunView,
    after: &arany::RunView,
    cancel: bool,
) {
    let text = |run: &arany::RunView, category| {
        run.display
            .iter()
            .filter(|chunk| chunk.category == category)
            .map(|chunk| chunk.text.as_str())
            .collect::<String>()
    };
    let streamed = text(before, arany::DisplayCategory::Answer);
    if snapshot.tentative || cancel {
        assert!(
            streamed.is_empty(),
            "small or tentative tails wait for settlement"
        );
    } else if !snapshot.answer.is_empty() {
        assert!(
            !streamed.is_empty(),
            "non-tentative chunks commit before notification delivery"
        );
        assert!(snapshot.answer.starts_with(&streamed));
        assert!(
            streamed.len() < snapshot.answer.len(),
            "the completion tail stays pending"
        );
    }
    assert_eq!(
        text(before, arany::DisplayCategory::Commentary),
        snapshot.commentary.concat()
    );
    let promoted = cancel && snapshot.tentative;
    assert_eq!(
        text(after, arany::DisplayCategory::Answer),
        if promoted { "" } else { &snapshot.answer }
    );
    let commentary = format!(
        "{}{}",
        snapshot.commentary.concat(),
        if promoted { &snapshot.answer } else { "" }
    );
    assert_eq!(text(after, arany::DisplayCategory::Commentary), commentary);
}

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
            .write_all(&vec![b'x'; MAX_RPC_LINE_BYTES + 33])
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
        host: None,
        host_command: None,
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
            limit_cause: (disposition == arany::ToolDisposition::Limit)
                .then_some(arany::LimitCause::Resource),
            output: "bounded\noutput".into(),
            attestation: None,
            approval_cause: None,
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
        limit_cause: None,
        output: String::new(),
        attestation: None,
        approval_cause: None,
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
        skill: None,
        supervisor: Default::default(),
        workers: Default::default(),
        id: intent.run_id,
        objective: "Inspect".into(),
        images: Vec::new(),
        config: None,
        provider_fallbacks: Vec::new(),
        provider_retries: Vec::new(),
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
        tool_results_cleared_count: 0,
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
                prefix_rule: None,
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
                offset: None,
                limit: Some(32),
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
    let canary = "historical protocol hook canary";
    run.tools[0].observation = Some(ToolObservation {
        intent: run.tools[0].intent.clone(),
        disposition: ToolDisposition::Denied,
        enforcement_cause: None,
        limit_cause: None,
        output: format!("Hook denied this Tool: {canary}"),
        attestation: None,
        approval_cause: None,
        cache_update: CacheUpdate::NotApplicable,
    });
    run.tools[0].permission_verdict = Some(arany::permissions::PermissionVerdict {
        host: None,
        intent_digest: [1; 32],
        decision: arany::permissions::Decision::Deny,
        mode: None,
        basis: arany::permissions::PermissionSource::Hook,
        source: arany::permissions::PermissionSource::Hook,
        approved: false,
        hook_restriction: Some(arany::permissions::Decision::Deny),
        hooks: 1,
        narrowed: false,
    });
    let history = serde_json::to_value(items::history_items(&run)).unwrap();
    assert_eq!(history[1]["status"], "declined");
    assert_eq!(
        history[1]["output"],
        "hook_denied: no operation was dispatched"
    );
    assert!(!history.to_string().contains(canary));

    assert_admission_history_items(&run);
    run.tools[0].observation = None;
    run.tools[0].permission_verdict = None;

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
        steering_notes: Vec::new(),
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

fn assert_admission_history_items(run: &arany::RunView) {
    let mut run = run.clone();
    for tool in arany::test_support::host_admission_fixtures()
        .into_iter()
        .chain(std::iter::once(
            arany::test_support::uncertain_hook_fixture(),
        ))
    {
        let output = tool.observation.as_ref().unwrap().output.clone();
        run.tools = vec![tool];
        let history = serde_json::to_value(items::history_items(&run)).unwrap();
        assert_eq!(history[1]["status"], "declined");
        assert_eq!(history[1]["output"], output);
        let expected = serde_json::to_string(&output).unwrap();
        assert!(arany::test_support::history_tool_block(&run).contains(&expected));
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Executor {
    Tool,
    Host,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ApprovalCase {
    Accept,
    Decline,
    Cancel,
    Interrupt,
    Eof,
    LateAccept,
    Duplicate,
    Invalid,
    ClientError,
    Unavailable,
    Backpressure,
    BackpressureInterrupt,
    Undelivered,
    Session,
    SessionBusyResume,
    Remember,
}

#[cfg(unix)]
#[tokio::test]
async fn approval_accept_dispatches_the_exact_journaled_intent_once() {
    let _deadline = deadline();
    approval_scenario(ApprovalCase::Accept, Executor::Tool).await;
}

#[cfg(unix)]
#[tokio::test]
async fn approval_decline_invalid_and_client_error_deny() {
    let _deadline = deadline();
    for case in [
        ApprovalCase::Decline,
        ApprovalCase::Invalid,
        ApprovalCase::ClientError,
        ApprovalCase::Unavailable,
    ] {
        approval_scenario(case, Executor::Tool).await;
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
        approval_scenario(case, Executor::Tool).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn approval_remains_answerable_after_the_former_deadline() {
    let _deadline = deadline();
    approval_scenario(ApprovalCase::LateAccept, Executor::Tool).await;
}

#[cfg(unix)]
#[tokio::test]
async fn approval_duplicate_and_unknown_responses_cannot_resolve_another_intent() {
    let _deadline = deadline();
    approval_scenario(ApprovalCase::Duplicate, Executor::Tool).await;
}

#[cfg(unix)]
#[tokio::test]
async fn approval_cancel_journals_while_outbound_is_full() {
    let _deadline = deadline();
    for case in [
        ApprovalCase::Backpressure,
        ApprovalCase::BackpressureInterrupt,
    ] {
        approval_scenario(case, Executor::Tool).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn approval_never_delivered_is_unavailable_and_rejects_late_acceptance() {
    let _deadline = deadline();
    approval_scenario(ApprovalCase::Undelivered, Executor::Tool).await;
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native serve host commands; requires the arany executable"
)]
fn host_approval_once_refusal_and_transport_failures_replay() {
    let _gate = crate::test_process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for case in [
                ApprovalCase::Accept,
                ApprovalCase::Decline,
                ApprovalCase::Cancel,
                ApprovalCase::Interrupt,
                ApprovalCase::Eof,
                ApprovalCase::LateAccept,
                ApprovalCase::Duplicate,
                ApprovalCase::Invalid,
                ApprovalCase::ClientError,
                ApprovalCase::Backpressure,
                ApprovalCase::BackpressureInterrupt,
                ApprovalCase::Undelivered,
                ApprovalCase::Session,
                ApprovalCase::SessionBusyResume,
                ApprovalCase::Remember,
            ] {
                let _deadline = deadline();
                approval_scenario(case, Executor::Host).await;
            }
            let _deadline = deadline();
            approval_scenario_channel(
                ApprovalCase::Remember,
                Executor::Host,
                server::ApprovalChannel::Terminal,
            )
            .await;
        });
}

fn decision(id: Value, result: Value) -> ClientMessage {
    ClientMessage::Message(Message::Response(Response {
        jsonrpc: Default::default(),
        id: serde_json::from_value(id).unwrap(),
        result,
    }))
}

const BUSY_RESUME_OBJECTIVE: &str = "Hold a tools-disabled turn";

struct ResumeProvider {
    scripted: Arc<ScriptedProvider>,
    waiting: WaitingProvider,
}

#[async_trait::async_trait]
impl Provider for ResumeProvider {
    fn identity(&self) -> &arany::ProviderIdentity {
        self.scripted.identity()
    }

    async fn invoke(
        &self,
        request: arany::ProviderRequest,
    ) -> Result<arany::ProviderResponse, arany::ProviderError> {
        if request.input.objective == BUSY_RESUME_OBJECTIVE {
            self.waiting.invoke(request).await
        } else {
            self.scripted.invoke(request).await
        }
    }
}

async fn resume_host_thread(
    input: &mpsc::Sender<ClientMessage>,
    output: &mut mpsc::Receiver<Message>,
    thread: &Value,
    workspace: &std::path::Path,
) {
    input.send(request(6, "thread/resume", json!({"threadId":thread,"cwd":workspace,"provider":"openai","model":"gpt-5.4","tools":true,"permissionMode":"default","collaboration":"single"}))).await.unwrap();
    loop {
        let message = next(output).await;
        assert!(message.get("error").is_none(), "{message}");
        if message["id"] == 6 {
            break;
        }
    }
}

async fn resume_during_tools_disabled_turn(
    input: &mpsc::Sender<ClientMessage>,
    output: &mut mpsc::Receiver<Message>,
    thread: &Value,
    workspace: &std::path::Path,
    started: &mut mpsc::UnboundedReceiver<()>,
) {
    input
        .send(request(
            8,
            "thread/start",
            json!({"cwd":workspace,"provider":"openai","model":"gpt-5.4","collaboration":"single"}),
        ))
        .await
        .unwrap();
    let other = next(output).await["result"]["thread"]["id"].clone();
    assert!(!other.is_null());
    assert_eq!(next(output).await["method"], "thread/started");
    input
        .send(request(
            3,
            "turn/start",
            json!({"threadId":other,"input":[{"type":"text","text":BUSY_RESUME_OBJECTIVE}]}),
        ))
        .await
        .unwrap();
    let run = loop {
        let message = next(output).await;
        assert!(message.get("error").is_none(), "{message}");
        if message["id"] == 3 {
            break message["result"]["turn"]["id"].clone();
        }
    };
    started.recv().await.unwrap();
    resume_host_thread(input, output, thread, workspace).await;
    input
        .send(request(
            9,
            "turn/interrupt",
            json!({"threadId":other,"turnId":run}),
        ))
        .await
        .unwrap();
    let mut acknowledged = false;
    let mut completed = false;
    while !acknowledged || !completed {
        let message = next(output).await;
        assert!(message.get("error").is_none(), "{message}");
        if message["id"] == 9 {
            assert_eq!(message["result"], json!({}));
            acknowledged = true;
        }
        if message["method"] == "turn/completed" {
            assert_eq!(message["params"]["threadId"], other);
            assert_eq!(message["params"]["turn"]["status"], "interrupted");
            completed = true;
        }
    }
}

async fn session_host_turn(
    input: &mpsc::Sender<ClientMessage>,
    output: &mut mpsc::Receiver<Message>,
    thread: &Value,
    expected_prompts: usize,
) {
    input
        .send(request(
            3,
            "turn/start",
            json!({"threadId":thread,"input":[{"type":"text","text":"Inspect host value"}]}),
        ))
        .await
        .unwrap();
    let mut prompts = 0;
    loop {
        let message = next(output).await;
        assert!(message.get("error").is_none(), "{message}");
        if message["method"] == "item/commandExecution/requestApproval" {
            prompts += 1;
            assert!(prompts <= expected_prompts);
            input
                .send(decision(
                    message["id"].clone(),
                    json!({"decision":"accept"}),
                ))
                .await
                .unwrap();
        }
        if message["method"] == "turn/completed" {
            assert_eq!(message["params"]["turn"]["status"], "completed");
            break;
        }
    }
    assert_eq!(prompts, expected_prompts);
}

#[cfg(unix)]
async fn approval_scenario(case: ApprovalCase, executor: Executor) {
    approval_scenario_channel(case, executor, server::ApprovalChannel::Human).await;
}

#[cfg(unix)]
async fn approval_scenario_channel(
    case: ApprovalCase,
    executor: Executor,
    channel: server::ApprovalChannel,
) {
    let terminal_prefix =
        channel == server::ApprovalChannel::Terminal && case == ApprovalCase::Remember;
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
    let engine = arany::test_support::engine::open(
        state_root,
        EngineConfig {
            guard_executable: Some(if executor == Executor::Host {
                std::env::current_exe()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join("arany")
            } else {
                std::env::current_exe().unwrap()
            }),
            ..Default::default()
        },
    )
    .unwrap();
    let mut events = engine.subscribe();
    let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let stalled = outbound.clone();
    let fill_output = outbound.clone();
    let read = arany::test_support::scenario::tool_outcome(if executor == Executor::Host {
        arany::ToolCall::CommandUnguarded {
            program: "printf".into(),
            args: vec!["host value".into()],
            cwd: String::new(),
            justification: "Inspect a synthetic host value".into(),
            prefix_rule: Some(vec!["printf".into(), "host value".into()]),
        }
    } else {
        arany::ToolCall::Read {
            path: "facts.txt".into(),
            offset: None,
            limit: Some(32),
        }
    });
    let finish = arany::ProviderOutcome::Answer(arany::Answer {
        text: "Finished".into(),
    });
    let outcomes = if matches!(
        case,
        ApprovalCase::Session | ApprovalCase::SessionBusyResume
    ) {
        vec![
            read.clone(),
            read.clone(),
            finish.clone(),
            read.clone(),
            finish.clone(),
            read,
            finish,
        ]
    } else if case == ApprovalCase::Duplicate {
        vec![read.clone(), read, finish]
    } else {
        vec![read, finish]
    };
    let provider = Arc::new(
        ScriptedProvider::new(outcomes)
            .with_profile("openai", "gpt-5.4")
            .with_before_call(0, move || {
                if case == ApprovalCase::Undelivered {
                    for _ in 0..fill_output.capacity() {
                        fill_output
                            .try_send(server::response(RequestId::Integer(99), json!({})))
                            .unwrap();
                    }
                }
            }),
    );
    let requests = provider.requests.clone();
    let (busy_started, mut busy_start) = mpsc::unbounded_channel();
    let busy_requests = Arc::new(Mutex::new(Vec::new()));
    let busy_dropped = Arc::new(AtomicUsize::new(0));
    let selected: Arc<dyn Provider> = if case == ApprovalCase::SessionBusyResume {
        Arc::new(ResumeProvider {
            scripted: provider.clone(),
            waiting: WaitingProvider {
                started: busy_started,
                dropped: busy_dropped.clone(),
                requests: busy_requests.clone(),
                identity: provider.identity().clone(),
            },
        })
    } else {
        provider.clone()
    };
    let threads = threads::Threads {
        provider: Some(selected),
        approval_channel: channel,
        ..Default::default()
    };
    let approval_method = if executor == Executor::Host {
        "item/commandExecution/requestApproval"
    } else {
        "item/tool/requestApproval"
    };
    let client = async {
        input.send(if matches!(case, ApprovalCase::Session | ApprovalCase::SessionBusyResume | ApprovalCase::Remember) {
            request(1, "initialize", json!({"clientInfo":{"name":"test","version":"1"},"capabilities":{"hostCommandSessionApproval":true}}))
        } else { initialize() }).await.unwrap();
        assert!(
            next(&mut output).await["result"]["capabilities"]
                .as_array()
                .unwrap()
                .contains(&json!("approvals_v2"))
        );
        let start = json!({"cwd":workspace,"provider":"openai","model":"gpt-5.4","tools":true,"permissionMode":if matches!(case, ApprovalCase::Session | ApprovalCase::SessionBusyResume | ApprovalCase::Remember) {"default"} else {"auto"},"collaboration":"single"});
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
        if case == ApprovalCase::Undelivered {
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut unavailable = false;
                loop {
                    if let Some(arany::Tapped::Committed(event)) = events.next().await {
                        if let Event::ToolFinished { observation, .. } = &event.event {
                            assert_eq!(
                                observation.approval_cause,
                                Some(arany::ApprovalCause::ChannelUnavailable)
                            );
                            assert!(observation.attestation.is_none());
                            unavailable = true;
                        }
                        if matches!(event.event, Event::RunFinished { .. }) {
                            assert!(unavailable);
                            break;
                        }
                    }
                }
            })
            .await
            .expect("undelivered approval must settle without draining output");
            assert_eq!(stalled.capacity(), 0);
            input
                .send(decision(json!(1), json!({"decision":"accept"})))
                .await
                .unwrap();
            for _ in 0..OUTBOUND_CAPACITY {
                let message = serde_json::to_value(output.try_recv().unwrap()).unwrap();
                assert_ne!(message["method"], approval_method);
                assert_ne!(message["method"], "turn/completed");
            }
        } else {
            let mut approval = None;
            let mut started = false;
            while approval.is_none() || !started {
                let message = next(&mut output).await;
                if message["id"] == 3 && message.get("result").is_some() {
                    started = true;
                }
                if message["method"] == approval_method {
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
            assert!(params.get("expiresAtMs").is_none());
            assert!(params["intent"].get("expires_at_ms").is_none());
            let intent: arany::EffectIntent =
                serde_json::from_value(params["intent"].clone()).unwrap();
            if executor == Executor::Host {
                assert!(
                    params["lines"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|line| line.as_str().unwrap().contains("Provider"))
                );
                assert_eq!(
                    params.get("availableDecisions").is_some(),
                    matches!(
                        case,
                        ApprovalCase::Session
                            | ApprovalCase::SessionBusyResume
                            | ApprovalCase::Remember
                    )
                );
            } else {
                assert_eq!(
                    params["lines"],
                    json!(crate::presentation::tool_approval_lines(&intent))
                );
            }
            wire_approval = Some(approval.clone());
            match case {
                ApprovalCase::Session
                | ApprovalCase::SessionBusyResume
                | ApprovalCase::Remember => {
                    input
                        .send(decision(json!(999), json!({"decision":"acceptForSession"})))
                        .await
                        .unwrap();
                    let reply = if matches!(
                        case,
                        ApprovalCase::Session | ApprovalCase::SessionBusyResume
                    ) {
                        "acceptForSession"
                    } else {
                        "allowPrefixInFolder"
                    };
                    input
                        .send(decision(approval["id"].clone(), json!({"decision":reply})))
                        .await
                        .unwrap();
                    input
                        .send(decision(
                            approval["id"].clone(),
                            json!({"decision":"acceptForSession"}),
                        ))
                        .await
                        .unwrap();
                }
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
                ApprovalCase::Cancel => {
                    input
                        .send(decision(
                            approval["id"].clone(),
                            json!({"decision":"cancel"}),
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
                ApprovalCase::Interrupt => input
                    .send(request(
                        5,
                        "turn/interrupt",
                        json!({"threadId":thread,"turnId":params["turnId"]}),
                    ))
                    .await
                    .unwrap(),
                ApprovalCase::Eof => input.send(ClientMessage::Eof).await.unwrap(),
                ApprovalCase::LateAccept => {
                    tokio::time::pause();
                    tokio::time::advance(Duration::from_secs(3600)).await;
                    tokio::time::resume();
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
                ApprovalCase::Unavailable => input
                    .send(decision(
                        approval["id"].clone(),
                        json!({"decision":"unavailable"}),
                    ))
                    .await
                    .unwrap(),
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
                ApprovalCase::Backpressure | ApprovalCase::BackpressureInterrupt => {
                    for _ in 0..stalled.capacity() {
                        stalled
                            .try_send(server::response(RequestId::Integer(99), json!({})))
                            .unwrap();
                    }
                    input
                        .send(request(8, "thread/list", json!({})))
                        .await
                        .unwrap();
                    drop(input.reserve_many(INBOUND_CAPACITY).await.unwrap());
                    if case == ApprovalCase::Backpressure {
                        input.send(ClientMessage::Eof).await.unwrap();
                    } else {
                        input
                            .send(request(
                                9,
                                "turn/interrupt",
                                json!({"threadId":thread,"turnId":params["turnId"]}),
                            ))
                            .await
                            .unwrap();
                    }
                    tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            if let Some(arany::Tapped::Committed(event)) = events.next().await
                                && let Event::RunFinished { disposition, .. } = event.event
                            {
                                assert_eq!(disposition, arany::RunDisposition::Cancelled);
                                break;
                            }
                        }
                    })
                    .await
                    .expect("cancellation must commit while output is full");
                    assert_eq!(stalled.capacity(), 0);
                    return (serde_json::from_value(thread).unwrap(), wire_approval, None);
                }
                ApprovalCase::Undelivered => unreachable!(),
            }
        }
        let mut completed = false;
        let mut completed_goal = Value::Null;
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
                    ApprovalCase::Cancel
                        | ApprovalCase::Interrupt
                        | ApprovalCase::Eof
                        | ApprovalCase::Backpressure
                );
                assert_eq!(
                    message["params"]["turn"]["status"] == "interrupted",
                    interrupted,
                    "{case:?}: {message}"
                );
                completed_goal = message["params"]["turn"]["goalId"].clone();
                completed = true;
            }
        }
        assert_eq!(
            approval_count,
            if case == ApprovalCase::Duplicate {
                2
            } else {
                usize::from(case != ApprovalCase::Undelivered)
            }
        );
        if case == ApprovalCase::Duplicate {
            input
                .send(decision(json!(2), json!({"decision":"accept"})))
                .await
                .unwrap();
        }
        let mut receipt = None;
        if matches!(
            case,
            ApprovalCase::Session | ApprovalCase::SessionBusyResume
        ) {
            session_host_turn(&input, &mut output, &thread, 0).await;
            if case == ApprovalCase::SessionBusyResume {
                resume_during_tools_disabled_turn(
                    &input,
                    &mut output,
                    &thread,
                    &workspace,
                    &mut busy_start,
                )
                .await;
            } else {
                resume_host_thread(&input, &mut output, &thread, &workspace).await;
            }
            session_host_turn(&input, &mut output, &thread, 1).await;
            input
                .send(request(
                    7,
                    "receipt/get",
                    json!({"threadId":thread,"goalId":completed_goal}),
                ))
                .await
                .unwrap();
            loop {
                let message = next(&mut output).await;
                assert!(message.get("error").is_none(), "{message}");
                if message["id"] == 7 {
                    receipt = Some(message["result"]["goal"].clone());
                    break;
                }
            }
        }
        if !matches!(case, ApprovalCase::Eof | ApprovalCase::Backpressure) {
            input.send(ClientMessage::Eof).await.unwrap();
        }
        (
            serde_json::from_value(thread).unwrap(),
            wire_approval,
            receipt,
        )
    };
    let (result, (id, wire_approval, wire_receipt)) = tokio::join!(
        server::run_with_threads(engine, state.clone(), inbound, outbound, threads),
        client
    );
    if matches!(
        case,
        ApprovalCase::Backpressure | ApprovalCase::BackpressureInterrupt
    ) {
        assert!(result.unwrap_err().contains("output blocked"));
    } else {
        result.unwrap();
    }
    while let Ok(message) = output.try_recv() {
        assert!(
            serde_json::to_value(&message)
                .unwrap()
                .get("error")
                .is_none(),
            "{case:?}: {message:?}"
        );
    }
    if case == ApprovalCase::SessionBusyResume {
        assert_eq!(busy_dropped.load(Ordering::SeqCst), 1);
        let busy_session = {
            let busy = busy_requests.lock().unwrap();
            assert_eq!(busy.len(), 1);
            busy[0].session_id
        };
        let (events, view) = closed_view(&state, busy_session).await;
        assert!(view.runs[0].config.as_ref().unwrap().tool_policy.is_none());
        assert_eq!(view.runs[0].status, arany::RunStatus::Cancelled);
        for prefix in 1..=events.len() {
            arany::SessionView::replay(view.id, &events[..prefix]).unwrap();
        }
    }
    let (events, view) = closed_view(&state, id).await;
    if executor == Executor::Host {
        if terminal_prefix {
            let saved: Value =
                serde_json::from_slice(&std::fs::read(state.join("permissions.json")).unwrap())
                    .unwrap();
            assert_eq!(
                saved["permissions"]["hostPrefixes"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        } else {
            assert_eq!(
                std::fs::read(state.join("permissions.json")).unwrap(),
                serde_json::to_vec(&json!({"permissions":{"ask":["Read"]}})).unwrap()
            );
        }
        for prefix in 1..=events.len() {
            arany::SessionView::replay(id, &events[..prefix]).unwrap();
        }
    }
    if let Some(receipt) = wire_receipt {
        let goal = view
            .goals
            .iter()
            .find(|goal| json!(goal.id) == receipt["id"])
            .unwrap();
        let report = goal.report.as_ref().unwrap();
        assert_eq!(
            receipt["report"],
            json!({
                "summary":report.summary,"changes":report.changes,"evidence":report.evidence,
                "unresolved":report.unresolved,"cachedInputTokens":report.cached_input_tokens,
            })
        );
        assert!(receipt["report"].to_string().contains("unguarded host run"));
    }
    if matches!(
        case,
        ApprovalCase::Session | ApprovalCase::SessionBusyResume
    ) {
        assert_eq!(view.runs.len(), 3);
        let second = &view.runs[1].tools[0];
        let third = &view.runs[2].tools[0];
        assert_eq!(
            second.permission_verdict.as_ref().unwrap().source,
            arany::permissions::PermissionSource::SessionApproval
        );
        assert_eq!(
            third.permission_verdict.as_ref().unwrap().source,
            arany::permissions::PermissionSource::Human
        );
        for tool in [second, third] {
            assert_eq!(
                tool.observation.as_ref().unwrap().disposition,
                arany::ToolDisposition::Succeeded
            );
        }
        let epoch = |run: &arany::RunView| {
            run.config
                .as_ref()
                .unwrap()
                .tool_policy
                .as_ref()
                .unwrap()
                .host_epoch
        };
        assert_eq!(epoch(&view.runs[0]), epoch(&view.runs[1]));
        assert_ne!(epoch(&view.runs[1]), epoch(&view.runs[2]));
    }
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
    let accepted = terminal_prefix
        || matches!(
            case,
            ApprovalCase::Accept
                | ApprovalCase::Duplicate
                | ApprovalCase::LateAccept
                | ApprovalCase::Session
                | ApprovalCase::SessionBusyResume
        );
    let continued = accepted
        || matches!(
            case,
            ApprovalCase::Decline
                | ApprovalCase::Invalid
                | ApprovalCase::ClientError
                | ApprovalCase::Unavailable
                | ApprovalCase::Undelivered
                | ApprovalCase::Remember
        );
    assert_eq!(
        tools.len(),
        if matches!(
            case,
            ApprovalCase::Duplicate | ApprovalCase::Session | ApprovalCase::SessionBusyResume
        ) {
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
        if matches!(
            case,
            ApprovalCase::Duplicate | ApprovalCase::Session | ApprovalCase::SessionBusyResume
        ) {
            2
        } else {
            usize::from(accepted)
        }
    );
    for (index, tool) in tools.iter().enumerate() {
        assert_eq!(
            tool.observation.as_ref().unwrap().approval_cause,
            match case {
                ApprovalCase::Remember if terminal_prefix => None,
                ApprovalCase::Decline => Some(arany::ApprovalCause::Declined),
                ApprovalCase::Cancel
                | ApprovalCase::Interrupt
                | ApprovalCase::Eof
                | ApprovalCase::Backpressure
                | ApprovalCase::BackpressureInterrupt => Some(arany::ApprovalCause::Cancelled),
                ApprovalCase::Invalid
                | ApprovalCase::ClientError
                | ApprovalCase::Unavailable
                | ApprovalCase::Undelivered
                | ApprovalCase::Remember => Some(arany::ApprovalCause::ChannelUnavailable),
                ApprovalCase::Accept
                | ApprovalCase::LateAccept
                | ApprovalCase::Duplicate
                | ApprovalCase::Session
                | ApprovalCase::SessionBusyResume => None,
            }
        );
        let verdict = tool.permission_verdict.as_ref().unwrap();
        assert_eq!(
            verdict.source,
            if matches!(
                case,
                ApprovalCase::Session | ApprovalCase::SessionBusyResume
            ) && index == 1
            {
                arany::permissions::PermissionSource::SessionApproval
            } else if terminal_prefix
                || matches!(
                    case,
                    ApprovalCase::Accept
                        | ApprovalCase::LateAccept
                        | ApprovalCase::Decline
                        | ApprovalCase::Duplicate
                        | ApprovalCase::Session
                        | ApprovalCase::SessionBusyResume
                )
            {
                arany::permissions::PermissionSource::Human
            } else {
                arany::permissions::PermissionSource::UnavailableApproval
            }
        );
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
            + if matches!(
                case,
                ApprovalCase::Session | ApprovalCase::SessionBusyResume
            ) {
                2
            } else {
                0
            }
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
    if continued && !accepted {
        let requests = requests.lock().unwrap();
        let result = requests[1]
            .input
            .tools
            .as_ref()
            .unwrap()
            .steps
            .last()
            .unwrap()
            .observations
            .last()
            .unwrap();
        let reason = if case == ApprovalCase::Decline {
            "user_declined"
        } else {
            "channel_unavailable"
        };
        assert!(
            result.output.contains(reason),
            "{case:?} {executor:?} {channel:?}: missing correlated denial cause {reason}"
        );
        assert!(!format!("{:?}", requests).contains("Human channel unavailable"));
    }
    assert_eq!(provider.responses.lock().unwrap().is_empty(), continued);
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
    for budget_stop in [false, true] {
        let _deadline = deadline();
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = root.path().join("state");
        StateRoot::admit(&state).unwrap().record(arany::RecordKind::UiPreferences).replace(&serde_json::to_vec(&json!({"version":6,"preferences":{
        "completion":"tabs", "view":"fullscreen", "goal_budget":arany::GoalBudget { max_provider_calls:Some(1), ..Default::default() },
        "resources":arany::ToolResourceSettings::default(), "copy_on_select":true,"newline":"shift_enter"
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

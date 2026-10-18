use super::*;
use arany::{GoalId, SessionId};
use arany_cli::{
    presentation::{Output, render_session},
    protocol::EventLine,
};

async fn start_thread(
    input: &mpsc::Sender<ClientMessage>,
    output: &mut mpsc::Receiver<Message>,
    workspace: &std::path::Path,
) -> SessionId {
    input.send(initialize()).await.unwrap();
    next(output).await;
    input.send(thread_start(workspace)).await.unwrap();
    let result = next(output).await;
    let id = serde_json::from_value(result["result"]["thread"]["id"].clone()).unwrap();
    assert_eq!(next(output).await["method"], "thread/started");
    id
}

async fn finish_turn(
    input: &mpsc::Sender<ClientMessage>,
    output: &mut mpsc::Receiver<Message>,
    thread: SessionId,
    text: &str,
) -> GoalId {
    input
        .send(request(
            3,
            "turn/start",
            json!({"threadId":thread,"input":[{"type":"text","text":text}]}),
        ))
        .await
        .unwrap();
    let result = next(output).await;
    let goal = serde_json::from_value(result["result"]["turn"]["goalId"].clone()).unwrap();
    while next(output).await["method"] != "turn/completed" {}
    goal
}

#[tokio::test]
async fn events_subscribe_pages_strictly_after_since_and_streams_live_commits_in_order() {
    let _deadline = deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let setup = engine(&state);
    let thread = setup
        .create_session(workspace.clone(), Some("Replay".into()))
        .await
        .unwrap();
    setup.close().await.unwrap();
    let store = Store::open(StateRoot::open_existing(&state).unwrap()).unwrap();
    for index in 0..1_005 {
        store
            .append(
                thread,
                Event::SessionRenamed {
                    title: format!("Replay {index}"),
                },
            )
            .await
            .unwrap();
    }
    store.close().await.unwrap();
    let (before, _) = closed_view(&state, thread).await;
    let head = before.last().unwrap().sequence;
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
        next(&mut output).await;
        let constrained = Message::Request(Request {
            jsonrpc: Default::default(),
            id: RequestId::String("i".repeat(MAX_LINE_BYTES - 2048)),
            method: "events/subscribe".into(),
            params: Some(json!({"threadId":thread,"since":0})),
        });
        assert!(encode_line(&constrained).unwrap().len() <= MAX_LINE_BYTES);
        input
            .send(ClientMessage::Message(constrained))
            .await
            .unwrap();
        let constrained = next(&mut output).await;
        let page = constrained["result"]["events"].as_array().unwrap();
        assert!(!page.is_empty() && page.len() < 1_000);
        assert_eq!(
            constrained["result"]["nextSince"],
            page.last().unwrap()["sequence"]
        );
        input
            .send(request(
                10,
                "events/subscribe",
                json!({"threadId":thread,"since":head + 1}),
            ))
            .await
            .unwrap();
        assert_eq!(
            next(&mut output).await["error"]["data"]["reason"],
            "invalidParams"
        );
        input
            .send(request(
                10,
                "events/subscribe",
                json!({"threadId":thread,"since":before[0].sequence}),
            ))
            .await
            .unwrap();
        let first = next(&mut output).await;
        assert_eq!(first["result"]["events"].as_array().unwrap().len(), 1_000);
        let cursor = first["result"]["nextSince"].as_u64().unwrap();
        input
            .send(request(
                11,
                "events/subscribe",
                json!({"threadId":thread,"since":cursor}),
            ))
            .await
            .unwrap();
        let last = next(&mut output).await;
        assert!(last["result"]["nextSince"].is_null());
        assert_eq!(last["result"]["events"].as_array().unwrap().len(), 5);
        let mut replay = first["result"]["events"].as_array().unwrap().clone();
        replay.extend(last["result"]["events"].as_array().unwrap().iter().cloned());
        input
            .send(request(
                3,
                "turn/start",
                json!({"threadId":thread,"input":[{"type":"text","text":"Hi"}]}),
            ))
            .await
            .unwrap();
        let mut live = Vec::new();
        let mut done = false;
        loop {
            let message = next(&mut output).await;
            if message["method"] == "events/committed" {
                assert_eq!(message["params"]["threadId"], json!(thread));
                let event = message["params"]["event"].clone();
                let terminal = event["kind"] == "GoalFinished";
                live.push(event);
                if terminal && done {
                    break;
                }
            }
            if message["method"] == "turn/completed" {
                done = true;
                if live
                    .last()
                    .is_some_and(|event| event["kind"] == "GoalFinished")
                {
                    break;
                }
            }
        }
        input.send(ClientMessage::Eof).await.unwrap();
        (replay, live)
    };
    let (result, (replay, live)) = tokio::join!(
        server::run_with_threads(
            engine(&state),
            state.clone(),
            inbound,
            outbound,
            threads::Threads {
                provider: Some(scripted()),
                ..Default::default()
            }
        ),
        client
    );
    result.unwrap();
    let (events, view) = closed_view(&state, thread).await;
    let expected: Vec<Value> = render_session(&view, &events, Output::Jsonl)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(replay, expected[1..before.len()]);
    assert_eq!(live, expected[before.len()..]);
    let rendered = render_session(&view, &events, Output::Jsonl);
    for (actual, line) in replay.iter().chain(&live).zip(rendered.lines().skip(1)) {
        let event: EventLine = serde_json::from_value(actual.clone()).unwrap();
        assert_eq!(serde_json::to_string(&event).unwrap(), line);
    }
}

#[tokio::test]
async fn receipt_get_matches_goal_finished_report_and_bounds_the_entire_response() {
    let _deadline = deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let objective = "x".repeat(64 * 1024);
    let client = async {
        let thread = start_thread(&input, &mut output, &workspace).await;
        let goal = finish_turn(&input, &mut output, thread, &objective).await;
        for (method, params) in [
            (
                "receipt/get",
                json!({"threadId":SessionId::new(),"goalId":goal}),
            ),
            (
                "events/subscribe",
                json!({"threadId":SessionId::new(),"since":0}),
            ),
            (
                "thread/fork",
                json!({"threadId":SessionId::new(),"cwd":workspace}),
            ),
        ] {
            input.send(request(10, method, params)).await.unwrap();
            assert_eq!(
                next(&mut output).await["error"]["data"]["reason"],
                "threadNotLoaded"
            );
        }
        input
            .send(request(
                10,
                "receipt/get",
                json!({"threadId":thread,"goalId":GoalId::new()}),
            ))
            .await
            .unwrap();
        assert_eq!(
            next(&mut output).await["error"]["data"]["reason"],
            "goalUnavailable"
        );
        let params = json!({"threadId":thread,"goalId":goal});
        let oversized = Message::Request(Request {
            jsonrpc: Default::default(),
            id: RequestId::String("i".repeat(MAX_LINE_BYTES - 256)),
            method: "receipt/get".into(),
            params: Some(params.clone()),
        });
        assert!(encode_line(&oversized).unwrap().len() <= MAX_LINE_BYTES);
        input.send(ClientMessage::Message(oversized)).await.unwrap();
        assert_eq!(
            next(&mut output).await["error"]["data"]["reason"],
            "contextTooLarge"
        );
        input
            .send(request(11, "receipt/get", params))
            .await
            .unwrap();
        let receipt = next(&mut output).await["result"]["goal"].clone();
        assert!(
            !serde_json::to_string(&receipt)
                .unwrap()
                .contains(workspace.to_str().unwrap())
        );
        assert_eq!(
            receipt
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["budget", "id", "objective", "report", "status", "usage"]
        );
        input.send(ClientMessage::Eof).await.unwrap();
        (thread, goal, receipt)
    };
    let (result, (thread, goal, receipt)) = tokio::join!(
        server::run_with_threads(
            engine(&state),
            state.clone(),
            inbound,
            outbound,
            threads::Threads {
                provider: Some(scripted()),
                ..Default::default()
            }
        ),
        client
    );
    result.unwrap();
    let (events, view) = closed_view(&state, thread).await;
    let committed = view.goals.iter().find(|item| item.id == goal).unwrap();
    assert_eq!(receipt["objective"], objective);
    assert_eq!(receipt["status"], committed.status.as_str());
    assert_eq!(
        receipt["budget"],
        serde_json::to_value(committed.budget).unwrap()
    );
    assert_eq!(receipt["usage"]["providerCalls"], committed.provider_calls);
    assert_eq!(receipt["usage"]["activeMs"], committed.active_ms);
    assert_eq!(
        receipt["report"],
        serde_json::to_value(committed.report.as_ref().unwrap()).unwrap()
    );
    assert!(events.iter().any(|event| matches!(&event.event, Event::GoalFinished { goal_id, report, .. } if *goal_id == goal && serde_json::to_value(report).unwrap() == receipt["report"])));
}

#[tokio::test]
async fn thread_fork_requires_a_committed_run_boundary_and_preserves_lineage() {
    let _deadline = deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let other = root.path().join("other");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&other).unwrap();
    let state = root.path().join("state");
    let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let client = async {
        let thread = start_thread(&input, &mut output, &workspace).await;
        input
            .send(request(
                10,
                "thread/fork",
                json!({"threadId":thread,"cwd":workspace}),
            ))
            .await
            .unwrap();
        assert_eq!(
            next(&mut output).await["error"]["data"]["reason"],
            "forkNotAtRunBoundary"
        );
        finish_turn(&input, &mut output, thread, "Hi").await;
        input
            .send(request(
                10,
                "thread/fork",
                json!({"threadId":thread,"cwd":other}),
            ))
            .await
            .unwrap();
        assert_eq!(
            next(&mut output).await["error"]["data"]["reason"],
            "workspaceMismatch"
        );
        input
            .send(request(
                10,
                "thread/fork",
                json!({"threadId":thread,"cwd":workspace,"title":"Fork"}),
            ))
            .await
            .unwrap();
        let reply = next(&mut output).await;
        assert_eq!(reply["result"]["thread"]["forkedFromId"], json!(thread));
        assert_eq!(reply["result"]["thread"]["title"], "Fork");
        assert_eq!(next(&mut output).await["params"], reply["result"]);
        let fork: SessionId =
            serde_json::from_value(reply["result"]["thread"]["id"].clone()).unwrap();
        input
            .send(request(
                11,
                "events/subscribe",
                json!({"threadId":fork,"since":0}),
            ))
            .await
            .unwrap();
        let replay = next(&mut output).await;
        assert!(
            replay["result"]["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event["kind"] == "SessionForked")
        );
        input.send(ClientMessage::Eof).await.unwrap();
        (thread, fork)
    };
    let (result, (thread, fork)) = tokio::join!(
        server::run_with_threads(
            engine(&state),
            state.clone(),
            inbound,
            outbound,
            threads::Threads {
                provider: Some(scripted()),
                ..Default::default()
            }
        ),
        client
    );
    result.unwrap();
    let (_, source) = closed_view(&state, thread).await;
    let (_, forked) = closed_view(&state, fork).await;
    assert_eq!(forked.lineage.unwrap().source_session_id, thread);
    assert_eq!(forked.runs, source.runs);
}

#[tokio::test]
async fn thread_fork_of_a_busy_session_is_refused_by_engine_locking() {
    let _deadline = deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let (started, mut start) = mpsc::unbounded_channel();
    let provider = Arc::new(WaitingProvider {
        started,
        dropped: Arc::new(AtomicUsize::new(0)),
        requests: Arc::new(Mutex::new(Vec::new())),
        identity: arany::ProviderIdentity::new("openai", "gpt-5.4", arany::Provenance::native()),
    });
    let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let client = async {
        let thread = start_thread(&input, &mut output, &workspace).await;
        input
            .send(request(
                3,
                "turn/start",
                json!({"threadId":thread,"input":[{"type":"text","text":"Wait"}]}),
            ))
            .await
            .unwrap();
        next(&mut output).await;
        start.recv().await.unwrap();
        input
            .send(request(
                10,
                "thread/fork",
                json!({"threadId":thread,"cwd":workspace}),
            ))
            .await
            .unwrap();
        loop {
            let message = next(&mut output).await;
            if message["id"] == 10 {
                assert_eq!(message["error"]["data"]["reason"], "sessionBusy");
                break;
            }
        }
        input.send(ClientMessage::Eof).await.unwrap();
        while output.recv().await.is_some() {}
        thread
    };
    let (result, thread) = tokio::join!(
        server::run_with_threads(
            engine(&state),
            state.clone(),
            inbound,
            outbound,
            threads::Threads {
                provider: Some(provider),
                ..Default::default()
            }
        ),
        client
    );
    result.unwrap();
    let (events, view) = closed_view(&state, thread).await;
    assert_eq!(view.runs[0].status, arany::RunStatus::Cancelled);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, Event::SessionForked { .. }))
    );
}

#[tokio::test]
async fn events_resync_is_emitted_when_the_tap_lags_behind_a_stalled_client() {
    use std::{future::Future, task::Poll};
    let _deadline = deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let app = engine(&state);
    let thread = app.create_session(workspace.clone(), None).await.unwrap();
    let provider = scripted();
    let providers = arany::AdmittedProviders::single(provider);
    let mut loaded = threads::Threads::default();
    loaded.map.insert(
        thread,
        threads::LoadedThread {
            workspace: workspace.clone(),
            providers: providers.clone(),
            grant: arany::ToolGrant::None,
            approval_mode: arany::ApprovalMode::Request,
            include_paths: Vec::new(),
            policy: arany::CollaborationPolicy::Single,
            goal_budget: arany::GoalBudget::default(),
            resource_settings: arany::ToolResourceSettings::default(),
        },
    );
    let mut history = super::super::history::History::default();
    let (outbound, mut output) = mpsc::channel(1);
    let subscription = Request {
        jsonrpc: Default::default(),
        id: RequestId::Integer(10),
        method: "events/subscribe".into(),
        params: Some(json!({"threadId":thread,"since":0})),
    };
    let replay = history
        .dispatch(&app, &state, &mut loaded, subscription, &outbound, &[])
        .await
        .unwrap()
        .unwrap();
    let replay_head = replay["events"].as_array().unwrap().last().unwrap()["sequence"]
        .as_u64()
        .unwrap();
    let mut tap = app.subscribe();
    let mut run = arany::test_support::scenario::request(&workspace, "Hi");
    run.session_id = Some(thread);
    run.title = None;
    run.include_paths.clear();
    app.submit(providers, run, arany::RunControls::default())
        .unwrap()
        .result
        .await
        .unwrap();
    let first = tap.next().await.unwrap();
    outbound
        .send(server::response(RequestId::Integer(99), json!({})))
        .await
        .unwrap();
    {
        let mut delivery = std::pin::pin!(history.observe(first, &outbound, &[]));
        assert!(
            !std::future::poll_fn(|cx| Poll::Ready(delivery.as_mut().poll(cx).is_ready())).await
        );
        for _ in 0..300 {
            app.create_session(workspace.clone(), None).await.unwrap();
        }
        output.recv().await.unwrap();
        delivery.await.unwrap();
    }
    let first = next(&mut output).await;
    assert_eq!(first["method"], "events/committed");
    let cursor = first["params"]["event"]["sequence"].as_u64().unwrap();
    assert!(cursor > replay_head);
    let missed = tap.next().await.unwrap();
    assert!(matches!(missed, arany::Tapped::Resync { missed } if missed > 0));
    history.observe(missed, &outbound, &[]).await.unwrap();
    let resync = next(&mut output).await;
    assert_eq!(resync["method"], "events/resync");
    assert_eq!(resync["params"]["threadId"], json!(thread));
    assert_eq!(resync["params"]["lastSequence"], cursor);
    let subscription = Request {
        jsonrpc: Default::default(),
        id: RequestId::Integer(11),
        method: "events/subscribe".into(),
        params: Some(json!({"threadId":thread,"since":cursor})),
    };
    let recovered = history
        .dispatch(&app, &state, &mut loaded, subscription, &outbound, &[])
        .await
        .unwrap()
        .unwrap();
    app.close().await.unwrap();
    let (events, _) = closed_view(&state, thread).await;
    assert_eq!(
        recovered["events"],
        serde_json::to_value(
            events
                .iter()
                .filter(|event| event.sequence > cursor)
                .map(EventLine::from)
                .collect::<Vec<_>>()
        )
        .unwrap()
    );
}

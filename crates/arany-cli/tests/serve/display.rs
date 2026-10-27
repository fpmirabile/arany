use super::*;

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

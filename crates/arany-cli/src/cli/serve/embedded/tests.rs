use super::*;
use arany::test_support::scenario::{WaitingProvider, request};
use arany::{AdmittedProviders, ApprovalMode, EngineConfig, RunStatus, StateRoot};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

async fn waiting_connection() -> (
    tempfile::TempDir,
    Arc<Engine>,
    Controls,
    Connection,
    arany::SessionId,
    Arc<AtomicUsize>,
) {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).unwrap();
    std::fs::write(workspace.join("notes/facts.txt"), "Fact").unwrap();
    let state = root.path().join("state");
    let engine = Arc::new(
        arany::test_support::engine::open(
            StateRoot::admit(&state).unwrap(),
            EngineConfig::default(),
        )
        .unwrap(),
    );
    let session = engine
        .create_session(workspace.clone(), None)
        .await
        .unwrap();
    let (started, mut starts) = mpsc::unbounded_channel();
    let dropped = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(WaitingProvider {
        started,
        dropped: dropped.clone(),
        requests: Arc::new(Mutex::new(Vec::new())),
        identity: arany::ProviderIdentity::new("openai", "test-model", arany::Provenance::native()),
    });
    let (controls, bootstrap) = Controls::new(ApprovalMode::Request, true);
    let mut request = request(&workspace, "Inspect the folder");
    request.session_id = Some(session);
    request.launch_directory = Some(workspace);
    let connection = Connection::start(
        engine.clone(),
        state,
        controls.clone(),
        bootstrap,
        move |controls, inbox| async move {
            Ok(Submission {
                providers: AdmittedProviders::single(provider),
                request,
                controls,
                inbox,
            })
        },
    );
    starts.recv().await.unwrap();
    (root, engine, controls, connection, session, dropped)
}

#[tokio::test]
async fn steering_withdrawal_pause_cancel_and_resync_use_the_server_connection() {
    let _deadline = crate::cli::serve::tests::deadline();
    let (root, engine, controls, mut connection, session, dropped) = waiting_connection().await;
    let state = root.path().join("state");
    let mut view = controls.view(session).await.unwrap();
    assert_eq!(view.runs.last().unwrap().status, RunStatus::Active);
    let run = view.runs.last().unwrap();
    let target = arany::ChildSteeringTarget {
        session_id: session,
        run_id: run.id,
        agent_run_id: run.agents[0].id,
    };
    assert_pending_controls(&controls, target).await;
    assert_burst_bound(&controls, &mut view, &state).await;
    connection
        .reduce(&mut view, Tapped::Resync { missed: 1 })
        .await
        .unwrap();
    assert_eq!(view.runs.last().unwrap().status, RunStatus::Active);
    assert!(controls.pause().await.unwrap());
    assert!(!controls.pause().await.unwrap());
    assert!(controls.cancel().await.unwrap());
    assert!(!controls.cancel().await.unwrap());
    let mut sequence = 0;
    let result = loop {
        match connection.next().await.unwrap() {
            Event::Committed(Tapped::Committed(event)) => {
                assert!(event.sequence > sequence);
                sequence = event.sequence;
            }
            Event::Finished(result) => break (*result).unwrap(),
            _ => {}
        }
    };
    assert_eq!(result.run.run.status, RunStatus::Cancelled);
    assert_eq!(
        controls.view(session).await.unwrap().runs.last().unwrap(),
        &result.run.run
    );
    connection.close().await.unwrap();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    Arc::try_unwrap(engine)
        .ok()
        .expect("Engine released")
        .close()
        .await
        .unwrap();
    let store = arany::Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    let events = store.load_session(session).await.unwrap();
    let replayed = SessionView::replay(session, &events).unwrap().unwrap();
    assert_eq!(replayed.runs.last().unwrap(), &result.run.run);
    assert!(!events.iter().any(|event| matches!(
        event.event,
        arany::Event::GoalSteeringNoteRecorded { .. }
            | arany::Event::AgentSteeringNoteRecorded { .. }
    )));
    store.close().await.unwrap();
}

#[tokio::test]
async fn server_loss_during_a_run_never_fabricates_a_settlement() {
    let _deadline = crate::cli::serve::tests::deadline();
    let (_root, engine, _controls, mut connection, _session, dropped) = waiting_connection().await;
    connection.server.abort();
    while let Ok(event) = connection.next().await {
        assert!(!matches!(event, Event::Finished(_)));
    }
    assert!(
        connection
            .close()
            .await
            .unwrap_err()
            .contains("Server task failed")
    );
    tokio::time::timeout(crate::cli::run_outcome::SHUTDOWN_GRACE, async {
        while dropped.load(Ordering::SeqCst) != 1 || Arc::strong_count(&engine) != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled worker releases the Engine");
    Arc::try_unwrap(engine)
        .ok()
        .expect("Engine released")
        .close()
        .await
        .unwrap();
}

async fn assert_pending_controls(controls: &Controls, target: arany::ChildSteeringTarget) {
    assert_eq!(
        controls
            .offer_child_note(target, "child note".into())
            .await
            .unwrap(),
        Err(arany::SteeringRefused::Unavailable)
    );
    assert!(
        controls
            .offer_note("first note".into())
            .await
            .unwrap()
            .is_ok()
    );
    assert!(
        controls
            .offer_note("second note".into())
            .await
            .unwrap()
            .is_ok()
    );
    assert_eq!(
        controls.withdraw_note().await.unwrap().as_deref(),
        Some("first note\n\nsecond note")
    );
    assert_eq!(controls.delivered_notes().await.unwrap(), 0);
    assert!(controls.withdraw_note().await.unwrap().is_none());
    for mode in [
        ApprovalMode::AutoEdits,
        ApprovalMode::Auto,
        ApprovalMode::Request,
    ] {
        controls.set_mode(mode).await.unwrap();
    }
}

async fn assert_burst_bound(controls: &Controls, view: &mut SessionView, state: &std::path::Path) {
    let store = arany::Store::open_read_only(StateRoot::open_existing(state).unwrap()).unwrap();
    let history = store.load_session(view.id).await.unwrap();
    store.close().await.unwrap();
    let event = Tapped::Committed(Arc::new(history[0].clone()));
    let (sender, mut events) = mpsc::channel(super::super::OUTBOUND_CAPACITY);
    sender
        .try_send(Event::Message(Message::Notification(
            super::super::items::notification(
                "$/cancel_request",
                serde_json::json!({"requestId":1}),
            ),
        )))
        .unwrap_or_else(|_| panic!("bounded client queue"));
    for _ in 0..EVENT_DRAIN_LIMIT {
        sender
            .try_send(Event::Committed(event.clone()))
            .unwrap_or_else(|_| panic!("bounded client queue"));
    }
    let mut pending = VecDeque::new();
    let sequence = view.last_sequence;
    reduce_burst(controls, view, event, &mut events, &mut pending)
        .await
        .unwrap();
    assert_eq!(
        events.len(),
        1,
        "the next committed Event waits for another input poll"
    );
    assert_eq!(pending.len(), 1, "non-Event notifications stay queued");
    assert_eq!(
        view.last_sequence, sequence,
        "repeated committed Events are ignored"
    );
}

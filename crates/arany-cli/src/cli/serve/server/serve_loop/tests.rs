use super::*;
use arany::test_support::scenario::{ScriptedProvider, request};
use arany::{AdmittedProviders, ApprovalMode, EngineConfig, StateRoot, Store};

const SETTLEMENT_CAPACITY: usize = 1;

struct SettlingRun {
    _root: tempfile::TempDir,
    server: ServerLoop,
    incoming: mpsc::Sender<ClientMessage>,
    output: mpsc::Receiver<embedded::Event>,
    _rpc: mpsc::Receiver<ServerMessage>,
    result: arany::RunResult,
    provider: Arc<ScriptedProvider>,
    first_sequence: u64,
}

async fn completed_run_with_unread_events() -> SettlingRun {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let state = root.path().join("state");
    let engine =
        Arc::new(Engine::open(StateRoot::admit(&state).unwrap(), EngineConfig::default()).unwrap());
    let session = engine
        .create_session(workspace.clone(), None)
        .await
        .unwrap();
    let first_sequence = threads::load_view(&state, session)
        .await
        .unwrap_or_else(|_| panic!("Session view"))
        .last_sequence
        + 1;
    let provider = Arc::new(ScriptedProvider::new(vec![arany::ProviderOutcome::Answer(
        arany::Answer {
            text: "Finished".into(),
        },
    )]));
    let (approval_mode, _inbox) = ToolApprovals::new(ApprovalMode::Request);
    let controls = RunControls {
        steering: Some(RunSteering::new()),
        approvals: Some(approval_mode.clone()),
        ..Default::default()
    };
    let mut input = request(&workspace, "Complete the task");
    input.session_id = Some(session);
    input.title = None;
    input.include_paths.clear();
    let (incoming, inbound) = mpsc::channel(super::super::super::INBOUND_CAPACITY);
    let (outbound, _rpc) = mpsc::channel(super::super::super::OUTBOUND_CAPACITY);
    let (native, output) = mpsc::channel(SETTLEMENT_CAPACITY);
    let (stopping, _) = watch::channel(false);
    let mut server = ServerLoop::configured(
        engine,
        state.clone(),
        inbound,
        outbound,
        Threads {
            approval_channel: ApprovalChannel::Terminal,
            ..Default::default()
        },
        stopping,
        Some(embedded::Native {
            pending: None,
            output: native,
            start_error: None,
        }),
    );
    server.initialized = true;
    server.inbound.local = Some((controls.clone(), state));
    let result = complete_turn(
        &mut server,
        provider.clone(),
        input,
        controls,
        first_sequence,
    )
    .await;
    SettlingRun {
        _root: root,
        server,
        incoming,
        output,
        _rpc,
        result,
        provider,
        first_sequence,
    }
}

async fn complete_turn(
    server: &mut ServerLoop,
    provider: Arc<ScriptedProvider>,
    input: RunRequest,
    controls: RunControls,
    first_sequence: u64,
) -> arany::RunResult {
    let thread_id = input.session_id.expect("materialized Session");
    let steering = controls.steering.clone().expect("steering");
    let approval_mode = controls.approvals.clone().expect("approvals");
    let SubmittedRun {
        result: finished,
        events,
        cancel,
    } = server
        .submit_run(
            AdmittedProviders::single(provider),
            input,
            controls,
            RunCancellation::new(),
        )
        .await
        .unwrap_or_else(|_| panic!("Run admission"));
    let result = server.workers.join_next().await.unwrap().unwrap().unwrap();
    server.active = Some(ActiveTurn {
        thread_id,
        events,
        cancel,
        result: finished,
        display: None,
        approvals: Arc::new(approvals::Approvals::empty_for_test()),
        approval_mode,
        steering,
        pending_start: Some(RequestId::Integer(2)),
        result_done: false,
        pending_close: None,
        state: TurnState {
            first_sequence: Some(first_sequence),
            ..Default::default()
        },
    });
    result
}

async fn receive_settlement(
    incoming: &mpsc::Sender<ClientMessage>,
    output: &mut mpsc::Receiver<embedded::Event>,
) -> (Vec<u64>, bool) {
    let mut sequences = Vec::new();
    let mut resync = false;
    loop {
        match output.recv().await.expect("settlement delivery") {
            embedded::Event::Committed(event) => {
                if sequences.is_empty() && !resync {
                    let (reply, delivered) = tokio::sync::oneshot::channel();
                    incoming
                        .send(ClientMessage::Control(embedded::Control::Delivered(reply)))
                        .await
                        .unwrap();
                    assert_eq!(
                        delivered.await.unwrap(),
                        0,
                        "controls remain serviceable while the tail waits for output"
                    );
                }
                match event {
                    Tapped::Committed(event) => sequences.push(event.sequence),
                    Tapped::Resync { .. } => resync = true,
                }
            }
            embedded::Event::Finished(result) => {
                assert!(result.is_ok());
                return (sequences, resync);
            }
            embedded::Event::Message(_) => panic!("canonical settlement delivery"),
        }
    }
}

#[tokio::test]
async fn result_ready_unread_tail_is_delivered_before_finished_under_backpressure() {
    let _deadline = crate::cli::serve::tests::deadline();
    let SettlingRun {
        _root,
        mut server,
        incoming,
        mut output,
        _rpc,
        result,
        provider,
        first_sequence,
    } = completed_run_with_unread_events().await;
    let session = result.run.session_id;
    let (settled, (sequences, resync)) = tokio::join!(
        server.on_run_finished(Ok(result)),
        receive_settlement(&incoming, &mut output)
    );
    settled.unwrap();
    let state = server.state.clone();
    let engine = server.engine.clone();
    server.on_stop(Ok(())).await.unwrap();
    Arc::try_unwrap(engine)
        .ok()
        .expect("Engine released")
        .close()
        .await
        .unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    let events = store.load_session(session).await.unwrap();
    store.close().await.unwrap();
    let expected = events
        .iter()
        .filter(|event| event.sequence >= first_sequence)
        .map(|event| event.sequence)
        .collect::<Vec<_>>();
    assert!(!expected.is_empty());
    assert!(
        resync || sequences == expected,
        "every committed sequence or an explicit resync must precede Finished"
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert!(provider.responses.lock().unwrap().is_empty());
}

mod controls;
mod serve_loop;

pub(in crate::cli) use serve_loop::{run_configured, run_embedded};

use super::{
    embedded,
    items::{self, TurnState, notification},
    threads::{self, Threads},
};
use arany::{
    Engine, EngineError, Event, RunCancellation, RunSteering, SessionId, Tapped, ToolApprovals,
};
use arany_cli::protocol::{methods::*, rpc::*};
use base64::Engine as _;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{collections::VecDeque, future::Future, path::PathBuf, sync::Arc, task::Poll};
use tokio::sync::{mpsc, watch};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(in crate::cli) enum ApprovalChannel {
    #[default]
    Human,
    Terminal,
    Restricted,
}

pub(in crate::cli) enum ClientMessage {
    Message(Message),
    Eof,
    Control(embedded::Control),
}
pub(in crate::cli) type ServerMessage = Message;

#[derive(Debug)]
pub(super) struct Fault {
    code: i64,
    message: String,
    reason: Option<Reason>,
}
impl Fault {
    pub fn state(reason: Reason) -> Self {
        Self {
            code: -32600,
            message: "invalid protocol state".into(),
            reason: Some(reason),
        }
    }
    pub fn params() -> Self {
        Self {
            code: -32602,
            message: "invalid parameters".into(),
            reason: Some(Reason::InvalidParams),
        }
    }
    fn refusal(reason: Reason) -> Self {
        Self {
            code: -32602,
            message: "steering note refused".into(),
            reason: Some(reason),
        }
    }
    pub fn selection(message: &str, reason: Reason) -> Self {
        Self {
            code: -32001,
            message: message.into(),
            reason: Some(reason),
        }
    }
    pub fn engine(error: EngineError) -> Self {
        let reason = Reason::from(&error);
        Self {
            code: if matches!(error, EngineError::Busy) {
                -32600
            } else {
                -32000
            },
            message: error.to_string(),
            reason: Some(reason),
        }
    }
    pub fn message(self, id: Option<RequestId>) -> Message {
        Message::Error(ErrorResponse {
            jsonrpc: Default::default(),
            id,
            error: ErrorObject {
                code: self.code,
                message: self.message,
                data: self.reason.map(|reason| ErrorData { reason }),
            },
        })
    }
}

struct ActiveTurn {
    thread_id: Option<SessionId>,
    result: tokio::task::AbortHandle,
    events: arany::Subscription,
    display: Option<DisplayFollower>,
    cancel: RunCancellation,
    approvals: Arc<super::approvals::Approvals>,
    approval_mode: Option<ToolApprovals>,
    steering: Option<RunSteering>,
    pending_start: Option<RequestId>,
    result_done: bool,
    pending_close: Option<RequestId>,
    state: TurnState,
}

struct DisplayFollower {
    previews: arany::RunPreview,
    waiting: arany::RunHints,
}

impl DisplayFollower {
    fn new(hints: arany::RunHints) -> Self {
        Self {
            previews: hints.previews(),
            waiting: hints,
        }
    }

    async fn next(&mut self) -> (Option<arany::RunId>, DisplayUpdate) {
        tokio::select! {
            preview = self.previews.changed() => {
                let run = preview.as_ref().map(|preview| preview.run_id);
                let preview = preview.map(|preview| DisplayPreview {
                    agent_run_id: preview.agent_run_id,
                    call_index: preview.call_index,
                    answer: preview.text.answer.clone(),
                    commentary: preview.text.commentary.clone(),
                    commentary_limited: preview.text.commentary_limited,
                    tentative: preview.text.tentative,
                });
                (run, DisplayUpdate::Preview { preview })
            }
            intent_id = self.waiting.waiting_changed() => (None, DisplayUpdate::Waiting { intent_id }),
        }
    }
}

impl Drop for ActiveTurn {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.result.abort();
    }
}

#[derive(Clone)]
struct OutputControl {
    thread_id: Option<SessionId>,
    run_id: Option<arany::RunId>,
    pending_completion: bool,
    result: tokio::task::AbortHandle,
    cancel: RunCancellation,
    approvals: Arc<super::approvals::Approvals>,
}

impl From<&ActiveTurn> for OutputControl {
    fn from(turn: &ActiveTurn) -> Self {
        Self {
            thread_id: turn.thread_id,
            run_id: turn.state.run_id,
            pending_completion: turn.state.pending_completion,
            result: turn.result.clone(),
            cancel: turn.cancel.clone(),
            approvals: turn.approvals.clone(),
        }
    }
}

impl OutputControl {
    fn interrupt(&self, request: &Request) -> bool {
        match request.method.as_str() {
            "turn/interrupt" => {
                params::<TurnInterruptParams>(request.params.clone()).is_ok_and(|p| {
                    Some(p.thread_id) == self.thread_id
                        && Some(p.turn_id) == self.run_id
                        && !self.pending_completion
                        && !self.result.is_finished()
                })
            }
            "thread/unsubscribe" => params::<ThreadUnsubscribeParams>(request.params.clone())
                .is_ok_and(|p| Some(p.thread_id) == self.thread_id),
            _ => false,
        }
    }

    fn cancel(&self) {
        self.cancel.cancel();
        self.approvals.close();
    }
}

struct Inbound {
    receiver: mpsc::Receiver<ClientMessage>,
    pending: VecDeque<ClientMessage>,
    stopping: watch::Sender<bool>,
    ended: bool,
    local: Option<(arany::RunControls, PathBuf)>,
}

impl Inbound {
    async fn next(&mut self) -> Option<ClientMessage> {
        if let Some(message) = self.pending.pop_front() {
            return Some(message);
        }
        if self.ended {
            return None;
        }
        self.receiver.recv().await
    }

    async fn wait<T, E: From<String>>(
        &mut self,
        control: Option<OutputControl>,
        outbound: &mpsc::Sender<Message>,
        action: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        tokio::pin!(action);
        let mut cancellation = control.as_ref().map(|c| c.cancel.clone());
        let mut stopping = self.stopping.subscribe();
        let mut stopped = false;
        loop {
            let reason = tokio::select! { biased;
                result = std::future::poll_fn(|cx| {
                    match action.as_mut().poll(cx) {
                        Poll::Pending if stopped && outbound.capacity() == 0 => Poll::Ready(Err("output blocked after cancellation; Run may be interrupted; check Session history".to_owned().into())),
                        result => result,
                    }
                }) => return result,
                () = outbound.closed() => "protocol writer closed",
                () = async { cancellation.as_mut().expect("active cancellation").cancelled().await }, if cancellation.is_some() && !stopped => {
                    stopped = true;
                    continue;
                },
                _ = async { let _ = stopping.wait_for(|stopping| *stopping).await; }, if !stopped => {
                    if let Some(control) = &control { control.cancel(); }
                    stopped = true;
                    continue;
                },
                message = self.receiver.recv(), if !self.ended => match message {
                    None | Some(ClientMessage::Eof) => {
                        self.ended = true;
                        if let Some(control) = &control { control.cancel(); }
                        self.stopping.send_replace(true);
                        stopped = true;
                        continue;
                    }
                    Some(ClientMessage::Control(command)) => {
                        if let Some((local, state)) = &self.local { embedded::dispatch(command, local, state).await; }
                        continue;
                    }
                    Some(ClientMessage::Message(Message::Request(ref request)))
                        if outbound.capacity() == 0 && control.as_ref().is_some_and(|c| c.interrupt(request)) => "output blocked at cancellation",
                    Some(ClientMessage::Message(Message::Response(reply))) if control.is_some() => {
                        control.as_ref().expect("active control").approvals.respond(reply.id, Some(reply.result));
                        continue;
                    }
                    Some(ClientMessage::Message(Message::Error(reply))) if control.is_some() => {
                        if let Some(id) = reply.id {
                            control.as_ref().expect("active control").approvals.respond(id, None);
                        }
                        continue;
                    }
                    Some(message) if self.pending.len() < super::INBOUND_CAPACITY => {
                        self.pending.push_back(message);
                        continue;
                    }
                    Some(_) => "pending input bound exceeded while output blocked",
                },
            };
            if let Some(control) = &control {
                control.cancel();
            }
            return Err(format!("{reason}; Run may be interrupted; check Session history").into());
        }
    }
}

impl From<String> for Fault {
    fn from(message: String) -> Self {
        internal(message)
    }
}

pub(in crate::cli) async fn run(
    engine: Engine,
    state: PathBuf,
    inbound: mpsc::Receiver<ClientMessage>,
    outbound: mpsc::Sender<ServerMessage>,
    shutdown: watch::Sender<bool>,
) -> Result<(), String> {
    run_configured(
        engine,
        state,
        inbound,
        outbound,
        Threads::default(),
        shutdown,
    )
    .await
}

#[cfg(test)]
pub(in crate::cli) async fn run_with_threads(
    engine: Engine,
    state: PathBuf,
    inbound: mpsc::Receiver<ClientMessage>,
    outbound: mpsc::Sender<ServerMessage>,
    threads: Threads,
) -> Result<(), String> {
    let (shutdown, receiver) = watch::channel(false);
    super::shutdown_grace(
        receiver,
        run_configured(engine, state, inbound, outbound, threads, shutdown),
    )
    .await
}

async fn observe(
    turn: &mut ActiveTurn,
    tapped: Tapped,
    state: &std::path::Path,
    outbound: &mpsc::Sender<Message>,
    opt_out: &[String],
) -> Result<(), String> {
    let thread_id = turn.thread_id.ok_or("Session identity unavailable")?;
    if let Tapped::Committed(envelope) = &tapped {
        if envelope.session_id != thread_id {
            return Ok(());
        }
        if turn.state.pending_completion && matches!(envelope.event, Event::MessageAccepted { .. })
        {
            complete(turn, state, outbound, opt_out, &Completion::Progress).await?;
        }
        if let Event::MessageAccepted { run_id, .. } = envelope.event
            && turn.state.goal_id.is_none()
        {
            let view = threads::load_view(state, thread_id)
                .await
                .map_err(|e| e.message)?;
            turn.state.goal_id = Some(
                view.goals
                    .iter()
                    .find(|g| g.run_ids.contains(&run_id))
                    .ok_or("accepted Run has no Goal linkage")?
                    .id,
            );
        }
    }
    let notes = items::project(&tapped, thread_id, &mut turn.state);
    if let Tapped::Committed(envelope) = tapped
        && let Event::MessageAccepted { run_id, .. } = envelope.event
        && let Some(id) = turn.pending_start.take()
    {
        let goal = turn
            .state
            .goal_id
            .ok_or("accepted Run has no Goal linkage")?;
        send(
            outbound,
            response(
                id,
                TurnResult {
                    turn: items::in_progress(run_id, goal),
                },
            ),
        )
        .await?;
    }
    for note in notes {
        emit(outbound, opt_out, note).await?;
    }
    Ok(())
}

// The next autonomous Run's refusal after a committed Run; no Event records it.
pub(super) struct Refused {
    run: arany::RunId,
    reason: Reason,
    message: String,
}

impl Refused {
    fn from_result(result: &arany::RunResult) -> Option<Self> {
        result.continuation_error.as_ref().map(|error| Self {
            run: result.run.run.id,
            reason: Reason::from(error),
            message: error.to_string(),
        })
    }
}

enum Completion {
    Progress,
    // `settled`: the submission returned, so a final Run without a terminal receipt is reported once.
    Final {
        settled: bool,
        refused: Option<Refused>,
    },
}

async fn complete(
    turn: &mut ActiveTurn,
    state: &std::path::Path,
    outbound: &mpsc::Sender<Message>,
    opt_out: &[String],
    completion: &Completion,
) -> Result<(), String> {
    let (finalizing, settled, refused) = match completion {
        Completion::Progress => (false, false, None),
        Completion::Final { settled, refused } => (true, *settled, refused.as_ref()),
    };
    let thread_id = turn.thread_id.ok_or("Session identity unavailable")?;
    let view = threads::load_view(state, thread_id)
        .await
        .map_err(|e| e.message)?;
    if finalizing && let Some(id) = turn.pending_start.take() {
        let run = view
            .runs
            .iter()
            .find(|run| {
                turn.state
                    .first_sequence
                    .is_some_and(|first| run.accepted_sequence >= first)
            })
            .ok_or("submission has no accepted Run")?;
        let goal = view
            .goals
            .iter()
            .find(|goal| goal.run_ids.contains(&run.id))
            .ok_or("Run has no Goal")?;
        send(
            outbound,
            response(
                id,
                TurnResult {
                    turn: items::turn_summary(&view, run, goal, false),
                },
            ),
        )
        .await?;
    }
    for run in &view.runs {
        if !finalizing && Some(run.id) != turn.state.run_id {
            continue;
        }
        // A settled submission's last Run may have no terminal receipt; its acceptance dedupes it.
        let unfinished = (settled && run.status == arany::RunStatus::Interrupted)
            .then_some(run.accepted_sequence);
        let Some(finished) = run.finished_sequence.or(unfinished) else {
            continue;
        };
        if finished <= turn.state.completed_sequence
            || !turn
                .state
                .first_sequence
                .is_some_and(|first| run.accepted_sequence >= first)
        {
            continue;
        }
        let goal = view
            .goals
            .iter()
            .find(|goal| goal.run_ids.contains(&run.id))
            .ok_or("Run has no Goal")?;
        let mut summary = items::turn_summary(&view, run, goal, false);
        if let Some(refused) = refused.filter(|refused| refused.run == run.id)
            && let Some(goal) = &mut summary.goal
        {
            goal.stopped = Some(GoalStop::NotAdmitted {
                refusal: refused.reason.clone(),
            });
            summary.error = Some(TurnError {
                message: refused.message.clone(),
            });
        }
        if !finalizing {
            summary.stop_reason = None;
            summary.goal = Some(GoalSummary {
                status: if matches!(
                    run.status,
                    arany::RunStatus::Cancelled | arany::RunStatus::StoppedByHook
                ) {
                    GoalStatus::Interrupted
                } else {
                    GoalStatus::Running
                },
                stopped: None,
                budget_refusal: None,
            });
        }
        emit(
            outbound,
            opt_out,
            notification(
                "turn/completed",
                TurnNotification {
                    thread_id,
                    turn: summary,
                },
            ),
        )
        .await?;
        turn.state.completed_sequence = finished;
    }
    turn.state.pending_completion = false;
    Ok(())
}

pub(super) fn params<T: DeserializeOwned>(params: Option<Value>) -> Result<T, Fault> {
    let params = params.unwrap_or_else(|| json!({}));
    let unsupported = params
        .get("permissionMode")
        .and_then(Value::as_str)
        .is_some_and(|s| !matches!(s, "default" | "acceptEdits" | "auto"));
    serde_json::from_value(params).map_err(|_| {
        if unsupported {
            Fault {
                code: -32602,
                message: "unsupported permission mode".into(),
                reason: Some(Reason::UnsupportedPermissionMode),
            }
        } else {
            Fault::params()
        }
    })
}

fn input_parts(input: Vec<UserInput>) -> Result<(String, Vec<arany::ImageAttachment>), Fault> {
    let mut text = String::new();
    let mut images = Vec::new();
    let mut bytes = 0;
    for item in input {
        match item {
            UserInput::Text { text: value } => text = value,
            UserInput::Image { data, .. } => {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|_| Fault::params())?;
                bytes += decoded.len();
                if bytes > arany::MAX_IMAGE_BYTES {
                    return Err(Fault::params());
                }
                images
                    .push(arany::ImageAttachment::from_png(&decoded).map_err(|_| Fault::params())?);
            }
        }
    }
    if text.trim().is_empty() && images.is_empty() {
        return Err(Fault::params());
    }
    Ok((text, images))
}

fn internal(message: String) -> Fault {
    Fault {
        code: -32603,
        message,
        reason: None,
    }
}
pub(super) fn response(id: RequestId, result: impl Serialize) -> Message {
    Message::Response(Response {
        jsonrpc: Default::default(),
        id,
        result: serde_json::to_value(result).expect("typed result"),
    })
}
const REWIND_BUSY_QUEUE: usize = 64;

pub(super) async fn send(outbound: &mpsc::Sender<Message>, message: Message) -> Result<(), String> {
    outbound
        .send(message)
        .await
        .map_err(|_| "protocol writer closed".into())
}
pub(super) async fn emit(
    outbound: &mpsc::Sender<Message>,
    opt_out: &[String],
    note: Notification,
) -> Result<(), String> {
    if !opt_out.contains(&note.method) {
        send(outbound, Message::Notification(note)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arany::RunControls;
    use arany::test_support::scenario::{ScriptedProvider, request};

    #[tokio::test]
    async fn blocked_output_bounds_deferred_input_without_dropping_the_queued_message() {
        let _deadline = super::super::tests::deadline();
        let (sender, receiver) = mpsc::channel(super::super::INBOUND_CAPACITY);
        let (stopping, _) = watch::channel(false);
        let mut inbound = Inbound {
            receiver,
            pending: VecDeque::new(),
            stopping,
            ended: false,
            local: Some((
                RunControls {
                    steering: Some(RunSteering::new()),
                    ..Default::default()
                },
                PathBuf::new(),
            )),
        };
        let (outbound, mut output) = mpsc::channel(1);
        outbound
            .try_send(response(RequestId::Integer(1), json!({})))
            .unwrap();
        let fill = async {
            let (reply, paused) = tokio::sync::oneshot::channel();
            sender
                .send(ClientMessage::Control(embedded::Control::Pause(reply)))
                .await
                .unwrap();
            assert!(
                paused.await.unwrap(),
                "local controls are serviced while output is full"
            );
            for id in 0..=super::super::INBOUND_CAPACITY {
                sender
                    .send(ClientMessage::Message(Message::Request(Request {
                        jsonrpc: Default::default(),
                        id: RequestId::Integer(i64::try_from(id).unwrap()),
                        method: "thread/list".into(),
                        params: Some(json!({})),
                    })))
                    .await
                    .unwrap();
            }
        };
        let (result, ()) = tokio::join!(
            inbound.wait(
                None,
                &outbound,
                send(&outbound, response(RequestId::Integer(2), json!({})))
            ),
            fill,
        );
        assert!(result.unwrap_err().contains("pending input bound exceeded"));
        assert_eq!(inbound.pending.len(), super::super::INBOUND_CAPACITY);
        for id in 0..super::super::INBOUND_CAPACITY {
            let Some(ClientMessage::Message(Message::Request(request))) = inbound.next().await
            else {
                panic!("deferred request")
            };
            assert_eq!(request.id, RequestId::Integer(i64::try_from(id).unwrap()));
        }
        let Message::Response(reply) = output.try_recv().unwrap() else {
            panic!("queued reply")
        };
        assert_eq!(reply.id, RequestId::Integer(1));
        assert!(output.try_recv().is_err());
    }

    #[tokio::test]
    async fn lost_message_accepted_tap_answers_from_the_first_new_committed_run() {
        let _deadline = super::super::tests::deadline();
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = temp.path().join("state");
        let engine = Engine::open(
            arany::StateRoot::admit(&state).unwrap(),
            arany::EngineConfig::default(),
        )
        .unwrap();
        let finish = arany::ProviderOutcome::Answer(arany::Answer {
            text: "Hello".into(),
        });
        let providers = arany::AdmittedProviders::single(Arc::new(ScriptedProvider::new(vec![
            finish.clone(),
            finish,
        ])));
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap();
        let mut input = request(&workspace, "Hi");
        input.session_id = Some(session);
        input.title = None;
        input.include_paths.clear();
        engine
            .submit(providers.clone(), input, RunControls::default())
            .unwrap()
            .result
            .await
            .unwrap();
        let first_sequence = threads::load_view(&state, session)
            .await
            .unwrap_or_else(|_| panic!("committed view"))
            .last_sequence
            + 1;
        let mut input = request(&workspace, "Again");
        input.session_id = Some(session);
        input.title = None;
        input.include_paths.clear();
        let arany::RunHandle {
            result,
            events,
            cancel,
            ..
        } = engine
            .submit(providers, input, RunControls::default())
            .unwrap();
        let result = result.await.unwrap();
        let worker = tokio::spawn(async {});
        let mut turn = ActiveTurn {
            thread_id: Some(session),
            result: worker.abort_handle(),
            events,
            display: None,
            cancel,
            approvals: Arc::new(super::super::approvals::Approvals::closed()),
            approval_mode: Some(ToolApprovals::new(arany::ApprovalMode::Request).0),
            steering: Some(RunSteering::new()),
            pending_start: Some(RequestId::Integer(3)),
            result_done: true,
            pending_close: None,
            state: TurnState {
                first_sequence: Some(first_sequence),
                ..Default::default()
            },
        };
        let (outbound, mut output) = mpsc::channel(super::super::OUTBOUND_CAPACITY);
        complete(
            &mut turn,
            &state,
            &outbound,
            &[],
            &Completion::Final {
                settled: true,
                refused: None,
            },
        )
        .await
        .unwrap();
        let response = serde_json::to_value(output.recv().await.unwrap()).unwrap();
        assert_eq!(response["id"], 3);
        assert_eq!(response["result"]["turn"]["id"], json!(result.run.run.id));
        assert_eq!(response["result"]["turn"]["status"], "completed");
        let completed = serde_json::to_value(output.recv().await.unwrap()).unwrap();
        assert_eq!(completed["method"], "turn/completed");
        assert_eq!(completed["params"]["turn"]["id"], json!(result.run.run.id));
        assert!(output.try_recv().is_err());
        assert!(turn.pending_start.is_none());
        worker.await.unwrap();
        engine.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_settled_final_run_without_a_terminal_receipt_completes_once_with_its_stop() {
        let _deadline = super::super::tests::deadline();
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let source = temp.path().join("source");
        let engine = Engine::open(
            arany::StateRoot::admit(&source).unwrap(),
            arany::EngineConfig::default(),
        )
        .unwrap();
        let finish = arany::ProviderOutcome::Answer(arany::Answer {
            text: "Hello".into(),
        });
        let providers = arany::AdmittedProviders::single(Arc::new(ScriptedProvider::new(vec![
            finish.clone(),
            finish,
        ])));
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap();
        for text in ["Hi", "Again"] {
            let mut input = request(&workspace, text);
            input.session_id = Some(session);
            input.title = None;
            input.include_paths.clear();
            engine
                .submit(providers.clone(), input, RunControls::default())
                .unwrap()
                .result
                .await
                .unwrap();
        }
        engine.close().await.unwrap();
        let store = arany::Store::open_read_only(arany::StateRoot::open_existing(&source).unwrap())
            .unwrap();
        let events = store.load_session(session).await.unwrap();
        store.close().await.unwrap();
        // A crash right after the second Run is accepted: no terminal receipt and no effect.
        let accepted = events
            .iter()
            .rposition(|e| matches!(e.event, Event::MessageAccepted { .. }))
            .unwrap();
        let state = temp.path().join("state");
        let store = arany::Store::open(arany::StateRoot::admit(&state).unwrap()).unwrap();
        for envelope in &events[..=accepted] {
            store.append(session, envelope.event.clone()).await.unwrap();
        }
        store.close().await.unwrap();
        let engine = Engine::open(
            arany::StateRoot::open_existing(&state).unwrap(),
            arany::EngineConfig::default(),
        )
        .unwrap();
        let worker = tokio::spawn(async {});
        let mut turn = ActiveTurn {
            thread_id: Some(session),
            result: worker.abort_handle(),
            events: engine.subscribe(),
            display: None,
            cancel: RunCancellation::new(),
            approvals: Arc::new(super::super::approvals::Approvals::closed()),
            approval_mode: Some(ToolApprovals::new(arany::ApprovalMode::Request).0),
            steering: Some(RunSteering::new()),
            pending_start: None,
            result_done: true,
            pending_close: None,
            state: TurnState {
                first_sequence: Some(events[accepted].sequence),
                ..Default::default()
            },
        };
        let (outbound, mut output) = mpsc::channel(super::super::OUTBOUND_CAPACITY);
        let unsettled = Completion::Final {
            settled: false,
            refused: None,
        };
        complete(&mut turn, &state, &outbound, &[], &unsettled)
            .await
            .unwrap();
        assert!(
            output.try_recv().is_err(),
            "an Engine error keeps the fatal recovery path"
        );
        let settled = Completion::Final {
            settled: true,
            refused: None,
        };
        complete(&mut turn, &state, &outbound, &[], &settled)
            .await
            .unwrap();
        let completed = serde_json::to_value(output.recv().await.unwrap()).unwrap();
        assert_eq!(completed["method"], "turn/completed");
        assert_eq!(completed["params"]["turn"]["status"], "interrupted");
        assert_eq!(
            completed["params"]["turn"]["goal"]["stopped"],
            json!({"reason":"interrupted"})
        );
        complete(&mut turn, &state, &outbound, &[], &settled)
            .await
            .unwrap();
        assert!(
            output.try_recv().is_err(),
            "the stopped turn completes once"
        );
        worker.await.unwrap();
        engine.close().await.unwrap();
    }
}

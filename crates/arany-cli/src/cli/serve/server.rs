use super::{
    items::{self, TurnState, notification},
    threads::{self, Threads},
};
use arany::{
    Engine, EngineError, Event, GoalRequest, RunCancellation, RunControls, RunRequest, RunSteering,
    SessionId, Tapped, ToolApprovals,
};
use arany_cli::protocol::{methods::*, rpc::*};
use base64::Engine as _;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{future::Future, path::PathBuf, sync::Arc, task::Poll};
use tokio::sync::{mpsc, watch};

pub(super) enum ClientMessage {
    Message(Message),
    Eof,
}
pub(super) type ServerMessage = Message;

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
    pub fn selection(message: &str, reason: Reason) -> Self {
        Self {
            code: -32001,
            message: message.into(),
            reason: Some(reason),
        }
    }
    pub fn engine(error: EngineError) -> Self {
        let reason = engine_reason(&error);
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

fn engine_reason(error: &EngineError) -> Reason {
    match error {
        EngineError::IncompatibleProviderChain => Reason::IncompatibleProviderChain,
        EngineError::InvalidRequest => Reason::InvalidRequest,
        EngineError::MissingSession => Reason::MissingSession,
        EngineError::NoSessionForWorkspace => Reason::NoSessionForWorkspace,
        EngineError::TooManySessions => Reason::TooManySessions,
        EngineError::SessionBusy => Reason::SessionBusy,
        EngineError::WorkspaceBusy => Reason::WorkspaceBusy,
        EngineError::GoalUnavailable => Reason::GoalUnavailable,
        EngineError::GoalAdmissionUncertain => Reason::GoalAdmissionUncertain,
        EngineError::GoalBudgetExhausted => Reason::GoalBudgetExhausted,
        EngineError::GoalNotContinuable => Reason::GoalNotContinuable,
        EngineError::Busy => Reason::Busy,
        EngineError::GuardUnavailable => Reason::GuardUnavailable,
        EngineError::SessionNotIdle => Reason::SessionNotIdle,
        EngineError::ForkNotAtRunBoundary => Reason::ForkNotAtRunBoundary,
        EngineError::WorkspaceMismatch => Reason::WorkspaceMismatch,
        EngineError::InvalidWorkspacePath => Reason::InvalidWorkspacePath,
        EngineError::InputNotRegular => Reason::InputNotRegular,
        EngineError::InputTooLarge => Reason::InputTooLarge,
        EngineError::InputNotUtf8 => Reason::InputNotUtf8,
        EngineError::MissingInclude => Reason::MissingInclude,
        EngineError::TooManyIncludes => Reason::TooManyIncludes,
        EngineError::StateOverlap => Reason::StateOverlap,
        EngineError::WorkspaceUnavailable => Reason::WorkspaceUnavailable,
        EngineError::InvalidHistory => Reason::InvalidHistory,
        EngineError::ContextTooLarge => Reason::ContextTooLarge,
        EngineError::ImagesNotAdmitted => Reason::ImagesNotAdmitted,
        EngineError::CompactionInputEmpty => Reason::CompactionInputEmpty,
        EngineError::CompactionInputTooLarge => Reason::CompactionInputTooLarge,
        EngineError::CompactionNotIdle => Reason::CompactionNotIdle,
        EngineError::CancellationDrainTimeout => Reason::CancellationDrainTimeout,
        EngineError::CoordinatorFailed => Reason::CoordinatorFailed,
        EngineError::CancelledBeforeStart => Reason::CancelledBeforeStart,
        EngineError::Store(_) => Reason::Store,
        EngineError::Tool(_) => Reason::Tool,
    }
}

struct ActiveTurn {
    thread_id: SessionId,
    result: tokio::task::AbortHandle,
    events: arany::Subscription,
    cancel: RunCancellation,
    approvals: super::approvals::Approvals,
    pending_start: Option<RequestId>,
    result_done: bool,
    state: TurnState,
}

impl Drop for ActiveTurn {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.result.abort();
        self.approvals.close();
    }
}

pub(super) async fn run(
    engine: Engine,
    state: PathBuf,
    inbound: mpsc::Receiver<ClientMessage>,
    outbound: mpsc::Sender<ServerMessage>,
    shutdown: watch::Sender<bool>,
) -> Result<(), String> {
    run_inner(
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
pub(super) async fn run_with_threads(
    engine: Engine,
    state: PathBuf,
    inbound: mpsc::Receiver<ClientMessage>,
    outbound: mpsc::Sender<ServerMessage>,
    threads: Threads,
) -> Result<(), String> {
    let (shutdown, receiver) = watch::channel(false);
    super::shutdown_grace(
        receiver,
        run_inner(engine, state, inbound, outbound, threads, shutdown),
    )
    .await
}

async fn run_inner(
    engine: Engine,
    state: PathBuf,
    mut inbound: mpsc::Receiver<ClientMessage>,
    outbound: mpsc::Sender<ServerMessage>,
    mut threads: Threads,
    stopping: watch::Sender<bool>,
) -> Result<(), String> {
    let engine = Arc::new(engine);
    let mut active: Option<ActiveTurn> = None;
    let mut workers = tokio::task::JoinSet::<Result<arany::RunResult, EngineError>>::new();
    let next_server_id = Arc::new(std::sync::atomic::AtomicI64::new(1));
    let mut initialized = false;
    let mut opt_out = Vec::<String>::new();
    let mut shutdown = false;
    let serving = async {
        loop {
            enum Next {
                Input(Option<ClientMessage>),
                Event(Option<Tapped>),
                Done(Result<Option<String>, EngineError>),
            }
            let next = if let Some(turn) = active.as_mut() {
                tokio::select! { biased;
                    input = inbound.recv(), if !shutdown => Next::Input(input),
                    event = turn.events.next() => Next::Event(event),
                    result = workers.join_next() => Next::Done(result.and_then(Result::ok).unwrap_or(Err(EngineError::CoordinatorFailed)).map(|result| result.stopped.map(|reason| reason.to_string()))),
                }
            } else {
                if shutdown {
                    break;
                }
                Next::Input(inbound.recv().await)
            };
            match next {
                Next::Input(None | Some(ClientMessage::Eof)) => {
                    if let Some(turn) = &active {
                        turn.approvals.close();
                        turn.cancel.cancel();
                    }
                    shutdown = true;
                    stopping.send_replace(true);
                }
                Next::Input(Some(ClientMessage::Message(Message::Request(request)))) => {
                    let id = request.id.clone();
                    let outcome: Result<Option<Value>, Fault> = async {
                        if request.method == "initialize" {
                            if initialized {
                                return Err(Fault::state(Reason::AlreadyInitialized));
                            }
                            let params: InitializeParams = params(request.params)?;
                            opt_out = params
                                .capabilities
                                .and_then(|c| c.opt_out_notification_methods)
                                .unwrap_or_default();
                            initialized = true;
                            return Ok(Some(json!(InitializeResult {
                                user_agent: format!("arany/{}", env!("CARGO_PKG_VERSION")),
                                platform_os: if cfg!(target_os = "macos") {
                                    PlatformOs::Macos
                                } else {
                                    PlatformOs::Linux
                                },
                                capabilities: CAPABILITIES.iter().map(|s| (*s).into()).collect()
                            })));
                        }
                        if !initialized {
                            return Err(Fault::state(Reason::NotInitialized));
                        }
                        match request.method.as_str() {
                            "thread/start" => {
                                let result = threads::start(
                                    &engine,
                                    &state,
                                    &mut threads,
                                    params(request.params)?,
                                )
                                .await?;
                                send(&outbound, response(id.clone(), &result))
                                    .await
                                    .map_err(internal)?;
                                emit(&outbound, &opt_out, notification("thread/started", result))
                                    .await
                                    .map_err(internal)?;
                                Ok(None)
                            }
                            "thread/resume" => {
                                let input: ThreadResumeParams = params(request.params)?;
                                if active
                                    .as_ref()
                                    .is_some_and(|turn| turn.thread_id == input.thread_id)
                                {
                                    return Err(Fault::state(Reason::Busy));
                                }
                                let thread_id = input.thread_id;
                                let (result, loaded) =
                                    threads::resume(&state, &threads, input).await?;
                                let response = response(id.clone(), &result);
                                if encode_line(&response).is_err() {
                                    return Err(Fault::engine(EngineError::ContextTooLarge));
                                }
                                threads.map.insert(thread_id, loaded);
                                send(&outbound, response).await.map_err(internal)?;
                                Ok(None)
                            }
                            "turn/start" => {
                                let input: TurnStartParams = params(request.params)?;
                                if active.is_some() {
                                    return Err(Fault::state(Reason::Busy));
                                }
                                let loaded = threads
                                    .map
                                    .get(&input.thread_id)
                                    .ok_or_else(|| Fault::state(Reason::ThreadNotLoaded))?;
                                let (objective, images) = input_parts(input.input)?;
                                let mut goal = match input.goal {
                                    Some(GoalInput {
                                        resume_goal_id: Some(_),
                                        ..
                                    }) => return Err(Fault::params()),
                                    Some(GoalInput {
                                        autonomous: true, ..
                                    }) => GoalRequest::autonomous(
                                        loaded.goal_budget,
                                        arany::GoalPhase::Implementation,
                                    ),
                                    _ => GoalRequest::default(),
                                };
                                goal.budget = Some(loaded.goal_budget);
                                goal.resource_settings = loaded.resource_settings;
                                let (approvals, inbox) = ToolApprovals::new(loaded.approval_mode);
                                let view = threads::load_view(&state, input.thread_id).await?;
                                let first_sequence = view
                                    .last_sequence
                                    .checked_add(1)
                                    .ok_or_else(|| Fault::engine(EngineError::InvalidHistory))?;
                                let worker_engine = engine.clone();
                                let providers = loaded.providers.clone();
                                let request = RunRequest {
                                    session_id: Some(input.thread_id),
                                    title: None,
                                    objective,
                                    images,
                                    workspace: loaded.workspace.clone(),
                                    include_paths: loaded.include_paths.clone(),
                                    policy: loaded.policy,
                                    goal,
                                };
                                let controls = RunControls {
                                    tools: loaded.grant.clone(),
                                    approvals: Some(approvals),
                                    steering: Some(RunSteering::new()),
                                    cancellation: RunCancellation::new(),
                                    progress: None,
                                };
                                let (started, ready) = tokio::sync::oneshot::channel();
                                let approval_end = RunCancellation::new();
                                let finished = approval_end.clone();
                                let result = workers.spawn(async move {
                                    let result =
                                        match worker_engine.submit(providers, request, controls) {
                                            Ok(handle) => {
                                                let _ = started
                                                    .send(Ok((handle.events, handle.cancel)));
                                                handle.result.await
                                            }
                                            Err(error) => {
                                                let _ = started.send(Err(error));
                                                Err(EngineError::InvalidRequest)
                                            }
                                        };
                                    finished.cancel();
                                    result
                                });
                                let (events, cancel) = match ready.await {
                                    Ok(Ok(handle)) => handle,
                                    Ok(Err(error)) => {
                                        let _ = workers.join_next().await;
                                        return Err(Fault::engine(error));
                                    }
                                    Err(_) => {
                                        let _ = workers.join_next().await;
                                        return Err(Fault::engine(EngineError::CoordinatorFailed));
                                    }
                                };
                                let approvals = super::approvals::Approvals::start(
                                    inbox,
                                    cancel.clone(),
                                    state.clone(),
                                    input.thread_id,
                                    outbound.clone(),
                                    next_server_id.clone(),
                                    approval_end,
                                );
                                active = Some(ActiveTurn {
                                    thread_id: input.thread_id,
                                    result,
                                    events,
                                    cancel,
                                    approvals,
                                    pending_start: Some(id.clone()),
                                    result_done: false,
                                    state: TurnState {
                                        first_sequence: Some(first_sequence),
                                        ..Default::default()
                                    },
                                });
                                Ok(None)
                            }
                            "turn/interrupt" => {
                                let input: TurnInterruptParams = params(request.params)?;
                                let turn = active
                                    .as_ref()
                                    .filter(|turn| {
                                        turn.thread_id == input.thread_id
                                            && turn.state.run_id == Some(input.turn_id)
                                            && !turn.state.pending_completion
                                            && !turn.result.is_finished()
                                    })
                                    .ok_or_else(|| Fault::state(Reason::TurnNotActive))?;
                                turn.approvals.close();
                                turn.cancel.cancel();
                                Ok(Some(json!({})))
                            }
                            _ => Err(Fault {
                                code: -32601,
                                message: "method not found".into(),
                                reason: None,
                            }),
                        }
                    }
                    .await;
                    match outcome {
                        Ok(Some(value)) => send(&outbound, response(id, value)).await?,
                        Ok(None) => {}
                        Err(error) => send(&outbound, error.message(Some(id))).await?,
                    }
                }
                Next::Input(Some(ClientMessage::Message(Message::Notification(_)))) => {}
                Next::Input(Some(ClientMessage::Message(
                    message @ (Message::Response(_) | Message::Error(_)),
                ))) => {
                    let id = match &message {
                        Message::Response(reply) => Some(reply.id.clone()),
                        Message::Error(reply) => reply.id.clone(),
                        _ => unreachable!(),
                    };
                    if let Some(id) = id {
                        let outcome = active
                            .as_ref()
                            .filter(|turn| !turn.result.is_finished())
                            .ok_or_else(|| Fault::state(Reason::ApprovalNotPending))
                            .and_then(|turn| turn.approvals.respond(message));
                        if let Err(error) = outcome {
                            send(&outbound, error.message(Some(id))).await?;
                        }
                    }
                }
                Next::Event(Some(event)) => {
                    observe(
                        active.as_mut().expect("active turn"),
                        event,
                        &state,
                        &outbound,
                        &opt_out,
                    )
                    .await?
                }
                Next::Event(None) => return Err("Event stream closed during a Run".into()),
                Next::Done(result) => {
                    let turn = active.as_mut().expect("active turn");
                    turn.result_done = true;
                    turn.approvals.close();
                    loop {
                        let event = {
                            let mut next = std::pin::pin!(turn.events.next());
                            std::future::poll_fn(|cx| {
                                Poll::Ready(match next.as_mut().poll(cx) {
                                    Poll::Ready(event) => event,
                                    Poll::Pending => None,
                                })
                            })
                            .await
                        };
                        let Some(event) = event else {
                            break;
                        };
                        observe(turn, event, &state, &outbound, &opt_out).await?;
                    }
                    let failure = if turn.pending_start.is_none() {
                        result.as_ref().err().map(ToString::to_string)
                    } else {
                        None
                    };
                    let stopped = result.as_ref().ok().cloned().flatten();
                    if result.is_err()
                        && let Some(id) = turn.pending_start.take()
                    {
                        let error = result.expect_err("failed Run");
                        send(&outbound, Fault::engine(error).message(Some(id))).await?;
                    }
                    complete(turn, &state, &outbound, &opt_out, true, stopped).await?;
                    active = None;
                    if let Some(failure) = failure {
                        return Err(format!(
                            "{failure}; Run may be interrupted; check Session history"
                        ));
                    }
                }
            }
        }
        Ok(())
    };
    let result = serving.await;
    stopping.send_replace(true);
    if let Some(turn) = active.take() {
        turn.cancel.cancel();
        if !turn.result_done {
            let _ = workers.join_next().await;
        }
    }
    let engine =
        Arc::try_unwrap(engine).map_err(|_| "Run did not release the Engine".to_owned())?;
    let closed = engine
        .close()
        .await
        .map_err(|_| "store shutdown failed".to_owned());
    result.and(closed)
}

async fn observe(
    turn: &mut ActiveTurn,
    tapped: Tapped,
    state: &std::path::Path,
    outbound: &mpsc::Sender<Message>,
    opt_out: &[String],
) -> Result<(), String> {
    if let Tapped::Committed(envelope) = &tapped {
        if envelope.session_id != turn.thread_id {
            return Ok(());
        }
        if turn.state.pending_completion && matches!(envelope.event, Event::MessageAccepted { .. })
        {
            complete(turn, state, outbound, opt_out, false, None).await?;
        }
        if let Event::MessageAccepted { run_id, .. } = envelope.event
            && turn.state.goal_id.is_none()
        {
            let view = threads::load_view(state, turn.thread_id)
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
    let notes = items::project(&tapped, turn.thread_id, &mut turn.state);
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

async fn complete(
    turn: &mut ActiveTurn,
    state: &std::path::Path,
    outbound: &mpsc::Sender<Message>,
    opt_out: &[String],
    finalizing: bool,
    stopped: Option<String>,
) -> Result<(), String> {
    let view = threads::load_view(state, turn.thread_id)
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
                    turn: items::turn_summary(run, goal, false),
                },
            ),
        )
        .await?;
    }
    for run in &view.runs {
        if !finalizing && Some(run.id) != turn.state.run_id {
            continue;
        }
        let Some(finished) = run.finished_sequence else {
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
        let mut summary = items::turn_summary(run, goal, false);
        if finalizing && let Some(goal) = &mut summary.goal {
            goal.stopped = stopped.clone();
        }
        if !finalizing {
            summary.goal = Some(GoalSummary {
                status: if run.status == arany::RunStatus::Cancelled {
                    GoalStatus::Interrupted
                } else {
                    GoalStatus::Running
                },
                stopped: None,
            });
        }
        emit(
            outbound,
            opt_out,
            notification(
                "turn/completed",
                TurnNotification {
                    thread_id: turn.thread_id,
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

fn params<T: DeserializeOwned>(params: Option<Value>) -> Result<T, Fault> {
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
pub(super) async fn send(outbound: &mpsc::Sender<Message>, message: Message) -> Result<(), String> {
    outbound
        .send(message)
        .await
        .map_err(|_| "protocol writer closed".into())
}
async fn emit(
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
    use arany::test_support::scenario::{ScriptedProvider, request};

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
        let finish = arany::ProviderOutcome::Finish(arany::Finish {
            summary: "Answer".into(),
            result: "Hello".into(),
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
            thread_id: session,
            result: worker.abort_handle(),
            events,
            cancel,
            approvals: super::super::approvals::Approvals::empty_for_test(),
            pending_start: Some(RequestId::Integer(3)),
            result_done: true,
            state: TurnState {
                first_sequence: Some(first_sequence),
                ..Default::default()
            },
        };
        let (outbound, mut output) = mpsc::channel(super::super::OUTBOUND_CAPACITY);
        complete(&mut turn, &state, &outbound, &[], true, None)
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
}

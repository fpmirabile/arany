use super::server::{ClientMessage, ServerMessage};
use crate::cli::provider::{routing::RoutingError, select::SelectionError};
use arany::{Engine, RunControls, RunRequest, RunResult, SessionView, Tapped};
use arany_cli::protocol::{methods::*, rpc::*};
use std::{collections::VecDeque, path::PathBuf, sync::Arc};
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Debug)]
pub(in crate::cli) enum RunAttemptError {
    InterruptedBeforeRun,
    Stopped {
        outcome: Box<arany::RunOutcome>,
        reason: String,
    },
    Failed(RunFailure),
}

#[derive(Debug, thiserror::Error)]
pub(in crate::cli) enum RunFailure {
    #[error(transparent)]
    Selection(SelectionError),
    #[error("{0}")]
    Roster(String),
    #[error(transparent)]
    Routing(RoutingError),
    #[error("state directory unavailable or unsafe")]
    StateUnavailable,
    #[error(transparent)]
    Engine(arany::EngineError),
    #[error("{0}")]
    Unsettled(&'static str),
    #[error("{0}")]
    Server(String),
}

pub(in crate::cli) fn classify_run_error(error: arany::EngineError) -> RunAttemptError {
    match error {
        arany::EngineError::CancelledBeforeStart => RunAttemptError::InterruptedBeforeRun,
        error => RunAttemptError::Failed(RunFailure::Engine(error)),
    }
}

pub(in crate::cli) enum Event {
    Committed(Tapped),
    Message(ServerMessage),
    Finished(Box<Result<RunResult, RunAttemptError>>),
}

impl Event {
    pub(in crate::cli) fn finished(result: Result<RunResult, RunAttemptError>) -> Self {
        Self::Finished(Box::new(result))
    }
}

pub(in crate::cli) enum Control {
    Cancel(oneshot::Sender<bool>),
    Pause(oneshot::Sender<bool>),
    Mode(arany::ApprovalMode, oneshot::Sender<()>),
    Offer(
        Option<arany::ChildSteeringTarget>,
        String,
        oneshot::Sender<Result<(), arany::SteeringRefused>>,
    ),
    Withdraw(
        Option<arany::ChildSteeringTarget>,
        oneshot::Sender<Option<String>>,
    ),
    Delivered(oneshot::Sender<u64>),
    View(
        arany::SessionId,
        oneshot::Sender<Result<SessionView, String>>,
    ),
}

impl Control {
    pub(super) fn apply(self, state: &RunControls) {
        let steering = state.steering.as_ref();
        match self {
            Self::Cancel(reply) => {
                let _ = reply.send(state.cancellation.cancel());
            }
            Self::Pause(reply) => {
                let _ = reply.send(steering.is_some_and(arany::RunSteering::pause));
            }
            Self::Mode(mode, reply) => {
                if let Some(approvals) = &state.approvals {
                    approvals.set_mode(mode);
                }
                let _ = reply.send(());
            }
            Self::Offer(target, text, reply) => {
                let _ = reply.send(
                    steering
                        .ok_or(arany::SteeringRefused::Unavailable)
                        .and_then(|steering| match target {
                            Some(target) => steering.offer_child_note(target, text),
                            None => steering.offer_note(text),
                        }),
                );
            }
            Self::Withdraw(target, reply) => {
                let _ = reply.send(steering.and_then(|steering| match target {
                    Some(target) => steering.withdraw_child_note(target),
                    None => steering.withdraw_note(),
                }));
            }
            Self::Delivered(reply) => {
                let _ = reply.send(steering.map_or(0, arany::RunSteering::delivered_notes));
            }
            Self::View(_, _) => unreachable!("view requests use strict replay"),
        }
    }
}

#[derive(Clone)]
pub(in crate::cli) struct Controls {
    inbound: mpsc::Sender<ClientMessage>,
}

pub(in crate::cli) struct Bootstrap {
    inbound: mpsc::Receiver<ClientMessage>,
    pub controls: RunControls,
    pub inbox: arany::ApprovalInbox,
}

impl Controls {
    pub fn new(mode: arany::ApprovalMode, previews: bool) -> (Self, Bootstrap) {
        let (inbound, receiver) = mpsc::channel(super::INBOUND_CAPACITY);
        let (approvals, inbox) = arany::ToolApprovals::new(mode);
        let controls = RunControls {
            approvals: Some(approvals),
            steering: Some(arany::RunSteering::new()),
            hints: Some(if previews {
                arany::RunHints::with_previews()
            } else {
                arany::RunHints::new()
            }),
            ..Default::default()
        };
        (
            Self { inbound },
            Bootstrap {
                inbound: receiver,
                controls,
                inbox,
            },
        )
    }

    async fn call<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<T>) -> Control,
    ) -> Result<T, String> {
        let (reply, receive) = oneshot::channel();
        self.inbound
            .send(ClientMessage::Control(build(reply)))
            .await
            .map_err(|_| "Run event stream closed".to_owned())?;
        receive
            .await
            .map_err(|_| "Run event stream closed".to_owned())
    }

    pub async fn cancel(&self) -> Result<bool, String> {
        self.call(Control::Cancel).await
    }
    pub async fn pause(&self) -> Result<bool, String> {
        self.call(Control::Pause).await
    }
    pub async fn set_mode(&self, mode: arany::ApprovalMode) -> Result<(), String> {
        self.call(|reply| Control::Mode(mode, reply)).await
    }
    pub async fn offer_note(
        &self,
        text: String,
    ) -> Result<Result<(), arany::SteeringRefused>, String> {
        self.call(|reply| Control::Offer(None, text, reply)).await
    }
    pub async fn offer_child_note(
        &self,
        target: arany::ChildSteeringTarget,
        text: String,
    ) -> Result<Result<(), arany::SteeringRefused>, String> {
        self.call(|reply| Control::Offer(Some(target), text, reply))
            .await
    }
    pub async fn withdraw_note(&self) -> Result<Option<String>, String> {
        self.call(|reply| Control::Withdraw(None, reply)).await
    }
    pub async fn withdraw_child_note(
        &self,
        target: arany::ChildSteeringTarget,
    ) -> Result<Option<String>, String> {
        self.call(|reply| Control::Withdraw(Some(target), reply))
            .await
    }
    pub fn child_withdrawal(
        &self,
    ) -> impl AsyncFnMut(arany::ChildSteeringTarget) -> Option<String> {
        async |target| self.withdraw_child_note(target).await.ok().flatten()
    }
    pub async fn delivered_notes(&self) -> Result<u64, String> {
        self.call(Control::Delivered).await
    }
    pub async fn view(&self, id: arany::SessionId) -> Result<SessionView, String> {
        self.call(|reply| Control::View(id, reply)).await?
    }
    pub async fn respond(&self, id: RequestId, decision: ApprovalDecision) -> Result<(), String> {
        self.inbound
            .send(ClientMessage::Message(Message::Response(Response {
                jsonrpc: Default::default(),
                id,
                result: serde_json::json!(ApprovalResult { decision }),
            })))
            .await
            .map_err(|_| "Run event stream closed".to_owned())
    }
}

pub(in crate::cli) struct Submission {
    pub providers: arany::AdmittedProviders,
    pub request: RunRequest,
    pub controls: RunControls,
    pub inbox: arany::ApprovalInbox,
}

pub(in crate::cli) struct Native {
    pub pending: Option<Submission>,
    pub output: mpsc::Sender<Event>,
    pub start_error: Option<RunAttemptError>,
}

pub(in crate::cli) struct Connection {
    pub events: mpsc::Receiver<Event>,
    pending: VecDeque<Event>,
    controls: Controls,
    server: tokio::task::JoinHandle<Result<(), String>>,
    writer: tokio::task::JoinHandle<Result<(), String>>,
}

impl Connection {
    pub fn start<F>(
        engine: Arc<Engine>,
        state: PathBuf,
        controls: Controls,
        bootstrap: Bootstrap,
        admit: impl FnOnce(RunControls, arany::ApprovalInbox) -> F + Send + 'static,
    ) -> Self
    where
        F: std::future::Future<Output = Result<Submission, RunAttemptError>> + Send + 'static,
    {
        let (output, events) = mpsc::channel(super::OUTBOUND_CAPACITY);
        let (rpc, mut messages) = mpsc::channel(super::OUTBOUND_CAPACITY);
        let writer_output = output.clone();
        let writer = tokio::spawn(async move {
            while let Some(message) = messages.recv().await {
                let relevant = match &message {
                    Message::Request(_) => true,
                    Message::Notification(note) => note.method == "$/cancel_request",
                    _ => false,
                };
                if !relevant {
                    continue;
                }
                writer_output
                    .send(Event::Message(message))
                    .await
                    .map_err(|_| "terminal client closed".to_owned())?;
            }
            Ok(())
        });
        let server = tokio::spawn(async move {
            let Some((inbound, submission)) = bootstrap.prepare(&state, &output, admit).await?
            else {
                return Ok(());
            };
            let (stopping, stopped) = watch::channel(false);
            super::shutdown_grace(
                stopped,
                super::server::run_embedded(
                    engine,
                    state,
                    inbound,
                    rpc,
                    Native {
                        pending: Some(submission),
                        output,
                        start_error: None,
                    },
                    stopping,
                ),
            )
            .await
        });
        Self {
            events,
            pending: VecDeque::new(),
            controls,
            server,
            writer,
        }
    }

    pub async fn next(&mut self) -> Result<Event, String> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(event);
        }
        self.events
            .recv()
            .await
            .ok_or_else(|| "Run event stream closed".to_owned())
    }

    pub async fn close(mut self) -> Result<(), String> {
        let result = tokio::time::timeout(crate::cli::run_outcome::SHUTDOWN_GRACE, async {
            let _ = self.controls.inbound.send(ClientMessage::Eof).await;
            drain_task(&mut self.server, &mut self.events).await?;
            drain_task(&mut self.writer, &mut self.events).await
        })
        .await
        .map_err(|_| "shutdown timed out; Run may be interrupted; check Session history".to_owned())
        .and_then(|result| result);
        self.stop_tasks().await;
        result
    }

    pub async fn abort(mut self) {
        self.stop_tasks().await;
    }

    async fn stop_tasks(&mut self) {
        for task in [&mut self.server, &mut self.writer] {
            if !task.is_finished() {
                task.abort();
                let _ = task.await;
            }
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.server.abort();
        self.writer.abort();
    }
}

pub(in crate::cli) fn failed(reason: &'static str) -> RunAttemptError {
    RunAttemptError::Failed(RunFailure::Unsettled(reason))
}

pub(in crate::cli) struct Admission {
    pub state: PathBuf,
    pub profile: String,
    pub model: String,
    pub effort: Option<arany::Effort>,
    pub account_id: Option<uuid::Uuid>,
    pub request: RunRequest,
    pub configured_tools: bool,
    pub permissions: Option<arany::WorkspacePermissions>,
    pub roster: Option<crate::cli::provider::roster::SelectedRoster>,
}

impl Admission {
    pub async fn admit(
        self,
        engine: Arc<Engine>,
        mut controls: RunControls,
        inbox: arany::ApprovalInbox,
    ) -> Result<Submission, RunAttemptError> {
        let selection = crate::cli::provider::select::Selection {
            workspace: &self.request.workspace,
            profile: &self.profile,
            model: &self.model,
            effort: self.effort,
            account_id: self.account_id,
        };
        let mut cancellation = controls.cancellation.clone();
        let admit = async {
            engine
                .readmit_state()
                .map_err(|_| RunFailure::StateUnavailable)?;
            if let Some(roster) = &self.roster {
                roster
                    .matches_selection(selection)
                    .map_err(RunFailure::Roster)?;
                return roster
                    .admit(&self.state, &self.request.workspace)
                    .await
                    .map_err(RunFailure::Roster);
            }
            let provider = crate::cli::provider::select::select_provider(&self.state, selection)
                .await
                .map_err(RunFailure::Selection)?;
            engine
                .readmit_state()
                .map_err(|_| RunFailure::StateUnavailable)?;
            crate::cli::provider::routing::configured_providers(
                &self.state,
                &self.request.workspace,
                provider,
            )
            .await
            .map_err(RunFailure::Routing)
        };
        let providers = tokio::select! { biased;
            () = cancellation.cancelled() => return Err(RunAttemptError::InterruptedBeforeRun),
            result = admit => result.map_err(RunAttemptError::Failed)?,
        };
        engine
            .readmit_state()
            .map_err(|_| RunAttemptError::Failed(RunFailure::StateUnavailable))?;
        controls.tools = tool_grant(&providers, self.configured_tools, self.permissions.as_ref())?;
        Ok(Submission {
            providers,
            request: self.request,
            controls,
            inbox,
        })
    }
}

fn tool_grant(
    providers: &arany::AdmittedProviders,
    configured: bool,
    permissions: Option<&arany::WorkspacePermissions>,
) -> Result<arany::ToolGrant, RunAttemptError> {
    if configured {
        return Ok(arany::ToolGrant::Configured);
    }
    let trusted_tools = crate::cli::attached::permissions::supports_tools(Some(
        &providers.primary[0].identity().profile,
    ));
    Ok(
        match permissions.filter(|permissions| trusted_tools && permissions.is_trusted()) {
            Some(permissions) => arany::ToolGrant::Trusted(permissions.clone()),
            None => arany::ToolGrant::None,
        },
    )
}

pub(in crate::cli) struct Approval {
    pub request_id: RequestId,
    pub prompt: crate::presentation::ToolApprovalPrompt,
}

impl Approval {
    pub fn from_request(request: Request) -> Result<Self, String> {
        use crate::presentation::ToolApprovalChoice as Choice;
        let params: ApprovalParams = serde_json::from_value(request.params.unwrap_or_default())
            .map_err(|_| "invalid Server approval".to_owned())?;
        let intent: arany::EffectIntent = serde_json::from_value(params.intent)
            .map_err(|_| "invalid Server intent".to_owned())?;
        let decisions = params.available_decisions.unwrap_or_default();
        let mut choices = vec![Choice::Yes];
        if decisions.contains(&ApprovalDecision::AcceptForSession) {
            choices.push(Choice::ExactCommand);
        } else if params.allow_auto_edits && !intent.call.is_host() {
            choices.push(Choice::AutoEdits);
        }
        if decisions.contains(&ApprovalDecision::AllowPrefixInFolder) {
            use arany::{ToolCall, permissions::TypedGitPrefix as Prefix};
            choices.push(match intent.call {
                ToolCall::GitCommit { .. } => Choice::GitPrefix(Prefix::Commit),
                ToolCall::GitBranch { .. } => Choice::GitPrefix(Prefix::Branch),
                ToolCall::GitSwitch { .. } => Choice::GitPrefix(Prefix::Switch),
                ToolCall::GhPrList { .. } => Choice::GitPrefix(Prefix::PrList),
                ToolCall::GhPrView { .. } => Choice::GitPrefix(Prefix::PrView),
                ToolCall::CommandUnguarded { .. } => Choice::HostPrefix,
                _ => return Err("invalid Server prefix choice".into()),
            });
        }
        choices.push(Choice::No);
        Ok(Self {
            request_id: request.id,
            prompt: crate::presentation::ToolApprovalPrompt {
                intent_id: intent.id,
                lines: params.lines,
                choices,
                unguarded: matches!(intent.call, arany::ToolCall::CommandUnguarded { .. }),
            },
        })
    }

    pub async fn decide(
        self,
        controls: &Controls,
        reply: arany::ApprovalReply,
    ) -> Result<(), String> {
        controls
            .respond(
                self.request_id,
                match reply {
                    arany::ApprovalReply::AllowOnce => ApprovalDecision::Accept,
                    arany::ApprovalReply::AllowForSession => ApprovalDecision::AcceptForSession,
                    arany::ApprovalReply::AllowPrefixInFolder => {
                        ApprovalDecision::AllowPrefixInFolder
                    }
                    arany::ApprovalReply::Deny => ApprovalDecision::Decline,
                    arany::ApprovalReply::Unavailable => ApprovalDecision::Unavailable,
                },
            )
            .await
    }
}

const EVENT_DRAIN_LIMIT: usize = 64;

impl Connection {
    pub async fn reduce(&mut self, view: &mut SessionView, event: Tapped) -> Result<(), String> {
        reduce_burst(
            &self.controls,
            view,
            event,
            &mut self.events,
            &mut self.pending,
        )
        .await
    }
}

async fn reduce_burst(
    controls: &Controls,
    view: &mut SessionView,
    event: Tapped,
    events: &mut mpsc::Receiver<Event>,
    pending: &mut VecDeque<Event>,
) -> Result<(), String> {
    reduce_one(controls, view, event).await?;
    let mut committed = 1;
    for _ in 0..super::OUTBOUND_CAPACITY {
        if committed == EVENT_DRAIN_LIMIT || pending.len() == super::OUTBOUND_CAPACITY {
            break;
        }
        let Ok(event) = events.try_recv() else {
            break;
        };
        match event {
            Event::Committed(event) => {
                reduce_one(controls, view, event).await?;
                committed += 1;
            }
            other => pending.push_back(other),
        }
    }
    Ok(())
}

async fn reduce_one(
    controls: &Controls,
    view: &mut SessionView,
    event: Tapped,
) -> Result<(), String> {
    match event {
        Tapped::Committed(event)
            if event.session_id != view.id || event.sequence <= view.last_sequence =>
        {
            Ok(())
        }
        Tapped::Committed(event)
            if event.sequence == view.last_sequence + 1
                && arany::reduce_into(view, &event).is_ok() =>
        {
            Ok(())
        }
        _ => {
            *view = controls
                .view(view.id)
                .await
                .map_err(|_| "Run history unavailable; check Session history".to_owned())?;
            Ok(())
        }
    }
}

pub(super) async fn dispatch(command: Control, controls: &RunControls, state: &std::path::Path) {
    if let Control::View(id, reply) = command {
        let _ = reply.send(snapshot(state, id).await);
    } else {
        command.apply(controls);
    }
}

#[cfg(test)]
pub(in crate::cli) async fn run_with_providers(
    engine: &Arc<Engine>,
    providers: arany::AdmittedProviders,
    mut request: RunRequest,
    state: &std::path::Path,
    configured: bool,
    permissions: Option<&arany::WorkspacePermissions>,
    mode: arany::ApprovalMode,
) -> Result<RunResult, RunAttemptError> {
    engine
        .readmit_state()
        .map_err(|_| RunAttemptError::Failed(RunFailure::StateUnavailable))?;
    let grant = tool_grant(&providers, configured, permissions)?;
    if request.session_id.is_none() {
        request.session_id = Some(
            engine
                .create_session(request.workspace.clone(), request.title.take())
                .await
                .map_err(classify_run_error)?,
        );
    }
    let (controls, bootstrap) = Controls::new(mode, false);
    let mut connection = Connection::start(
        engine.clone(),
        state.to_owned(),
        controls,
        bootstrap,
        move |mut controls, inbox| async move {
            controls.tools = grant;
            Ok(Submission {
                providers,
                request,
                controls,
                inbox,
            })
        },
    );
    let result = loop {
        if let Event::Finished(result) = connection
            .next()
            .await
            .map_err(|_| failed("embedded connection failed"))?
        {
            break *result;
        }
    };
    connection
        .close()
        .await
        .map_err(|_| failed("Server shutdown failed"))?;
    result
}

async fn snapshot(state: &std::path::Path, id: arany::SessionId) -> Result<SessionView, String> {
    let root = arany::StateRoot::open_existing(state)
        .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
    let store =
        arany::Store::open_read_only(root).map_err(|_| "state store unavailable".to_owned())?;
    let view = store
        .load_open_view(id)
        .await
        .map_err(|_| "Session history unavailable or invalid".to_owned());
    let closed = store.close().await;
    let view = view?.ok_or_else(|| "Session not found".to_owned())?;
    closed.map_err(|_| "store shutdown failed".to_owned())?;
    Ok(view)
}

#[cfg(test)]
mod tests;

async fn drain_task(
    task: &mut tokio::task::JoinHandle<Result<(), String>>,
    events: &mut mpsc::Receiver<Event>,
) -> Result<(), String> {
    let mut open = true;
    loop {
        tokio::select! { biased;
            result = &mut *task => return result.map_err(|_| "Server task failed".to_owned())?,
            event = events.recv(), if open => open = event.is_some(),
        }
    }
}

impl Bootstrap {
    async fn prepare<F>(
        self,
        state: &std::path::Path,
        output: &mpsc::Sender<Event>,
        admit: impl FnOnce(RunControls, arany::ApprovalInbox) -> F,
    ) -> Result<Option<(mpsc::Receiver<ClientMessage>, Submission)>, String>
    where
        F: std::future::Future<Output = Result<Submission, RunAttemptError>>,
    {
        let Bootstrap {
            mut inbound,
            controls,
            inbox,
        } = self;
        let admission = admit(controls.clone(), inbox);
        tokio::pin!(admission);
        let mut ended = false;
        let submission = loop {
            tokio::select! { biased;
                message = inbound.recv(), if !ended => match message {
                    Some(ClientMessage::Control(command)) => dispatch(command, &controls, state).await,
                    None | Some(ClientMessage::Eof) => { controls.cancellation.cancel(); ended = true; }
                    Some(_) => return Err("unexpected input during admission".into()),
                },
                result = &mut admission => break result,
            }
        };
        let submission = match submission {
            Ok(submission) => submission,
            Err(error) => {
                output
                    .send(Event::finished(Err(error)))
                    .await
                    .map_err(|_| "terminal client closed".to_owned())?;
                if ended {
                    return Ok(None);
                }
                loop {
                    match inbound.recv().await {
                        Some(ClientMessage::Control(command)) => {
                            dispatch(command, &controls, state).await
                        }
                        None | Some(ClientMessage::Eof) => return Ok(None),
                        Some(_) => {}
                    }
                }
            }
        };
        Ok(Some((inbound, submission)))
    }
}

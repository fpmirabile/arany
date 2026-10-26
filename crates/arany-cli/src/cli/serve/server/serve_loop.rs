use super::super::{
    approvals, embedded, history,
    items::{TurnState, notification},
    rewind,
    threads::{self, Threads},
};
use super::{
    ActiveTurn, ApprovalChannel, ClientMessage, Completion, DisplayFollower, Fault, Inbound,
    OutputControl, REWIND_BUSY_QUEUE, Refused, ServerMessage, complete, controls, emit, internal,
    observe, params, response, send,
};
use arany::{
    Engine, EngineError, Event, RunCancellation, RunControls, RunRequest, RunSteering, SessionId,
    Tapped, ToolApprovals,
};
use arany_cli::protocol::{methods::*, rpc::*};
use serde_json::{Value, json};
use std::{collections::VecDeque, future::Future, path::PathBuf, pin::Pin, sync::Arc, task::Poll};
use tokio::sync::{mpsc, watch};

const MAX_REWIND_OTHER_RESULT_BYTES: usize = 256 * 1024;

struct ServerLoop {
    shutdown: bool,
    opt_out: Vec<String>,
    initialized: bool,
    host_session_approval: bool,
    provisional_display: bool,
    next_server_id: Arc<std::sync::atomic::AtomicI64>,
    // Drop workers and the active turn before releasing the Engine.
    workers: tokio::task::JoinSet<Result<arany::RunResult, EngineError>>,
    active: Option<ActiveTurn>,
    rewind_preview: Option<rewind::Pending>,
    history: history::History,
    committed: arany::Subscription,
    engine: Arc<Engine>,
    inbound: Inbound,
    stopping: watch::Sender<bool>,
    threads: Threads,
    outbound: mpsc::Sender<ServerMessage>,
    state: PathBuf,
    native: Option<embedded::Native>,
}

struct SubmittedRun {
    result: tokio::task::AbortHandle,
    events: arany::Subscription,
    cancel: RunCancellation,
}

impl ServerLoop {
    async fn run(&mut self) -> Result<(), String> {
        loop {
            enum Next {
                Input(Option<ClientMessage>),
                OutputClosed,
                Event(Option<Tapped>),
                Committed(Option<Tapped>),
                Done(Box<Result<arany::RunResult, EngineError>>),
                Display(Option<arany::RunId>, DisplayUpdate),
            }
            let next = if let Some(turn) = self.active.as_mut() {
                tokio::select! { biased;
                    () = self.outbound.closed() => Next::OutputClosed,
                    input = self.inbound.next(), if !self.shutdown => Next::Input(input),
                    event = turn.events.next() => Next::Event(event),
                    event = self.committed.next() => Next::Committed(event),
                    result = self.workers.join_next() => Next::Done(Box::new(result.and_then(Result::ok).unwrap_or(Err(EngineError::CoordinatorFailed)))),
                    (run, update) = async { turn.display.as_mut().expect("display follower").next().await }, if turn.display.is_some() => Next::Display(run, update),
                }
            } else {
                if self.shutdown {
                    self.history
                        .drain(&mut self.committed, &self.outbound, &self.opt_out)
                        .await?;
                    break;
                }
                tokio::select! { biased;
                    () = self.outbound.closed() => Next::OutputClosed,
                    input = self.inbound.next() => Next::Input(input),
                    event = self.committed.next() => Next::Committed(event),
                }
            };
            match next {
                Next::OutputClosed => {
                    return Err(
                        "protocol writer closed; Run may be interrupted; check Session history"
                            .into(),
                    );
                }
                Next::Input(message) => self.on_client_message(message).await?,
                Next::Event(Some(event)) => self.on_run_event(event).await?,
                Next::Event(None) => return Err("Event stream closed during a Run".into()),
                Next::Committed(Some(event)) => self.on_committed_event(event).await?,
                Next::Committed(None) => return Err("committed Event stream closed".into()),
                Next::Done(result) => self.on_run_finished(*result).await?,
                Next::Display(run, update) => self.on_display(run, update),
            }
        }
        Ok(())
    }

    async fn on_client_message(&mut self, message: Option<ClientMessage>) -> Result<(), String> {
        match message {
            Some(ClientMessage::Control(command)) => {
                if let Some((local, state)) = &self.inbound.local {
                    embedded::dispatch(command, local, state).await;
                }
            }
            None | Some(ClientMessage::Eof) => {
                if let Some(turn) = &self.active {
                    turn.approvals.close();
                    turn.cancel.cancel();
                }
                self.shutdown = true;
                self.stopping.send_replace(true);
            }
            Some(ClientMessage::Message(Message::Request(request))) => {
                let control = self.active.as_ref().map(OutputControl::from);
                let id = request.id.clone();
                let outcome = self.on_request(request, control.clone()).await;
                match outcome {
                    Ok(Some(value)) => {
                        self.inbound
                            .wait(
                                control.clone(),
                                &self.outbound,
                                send(&self.outbound, response(id, value)),
                            )
                            .await?
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.inbound
                            .wait(
                                control.clone(),
                                &self.outbound,
                                send(&self.outbound, error.message(Some(id))),
                            )
                            .await?
                    }
                }
            }
            Some(ClientMessage::Message(Message::Notification(_))) => {}
            Some(ClientMessage::Message(message @ (Message::Response(_) | Message::Error(_)))) => {
                let (id, result) = match message {
                    Message::Response(reply) => (Some(reply.id), Some(reply.result)),
                    Message::Error(reply) => (reply.id, None),
                    _ => (None, None),
                };
                if let (Some(id), Some(turn)) = (id, self.active.as_ref()) {
                    turn.approvals.respond(id, result);
                }
            }
        }
        Ok(())
    }

    async fn on_request(
        &mut self,
        request: Request,
        control: Option<OutputControl>,
    ) -> Result<Option<Value>, Fault> {
        if request.method == "initialize" {
            return self.initialize(request);
        }
        if !self.initialized {
            return Err(Fault::state(Reason::NotInitialized));
        }
        match request.method.as_str() {
            "thread/rewind/preview" => self.preview_rewind(request).await,
            "thread/rewind" => self.restore_rewind(request, control).await,
            "events/subscribe" | "receipt/get" | "thread/fork" => {
                self.inbound
                    .wait(
                        control.clone(),
                        &self.outbound,
                        self.history.dispatch(
                            &self.engine,
                            &self.state,
                            &mut self.threads,
                            request,
                            &self.outbound,
                            &self.opt_out,
                        ),
                    )
                    .await
            }
            "thread/start" => self.start_thread(request, control).await,
            "thread/list" => self.list_threads(request, control).await,
            "thread/unsubscribe" => self.unsubscribe_thread(request),
            "thread/settings/update" => self.update_settings(request),
            "thread/resume" => self.resume_thread(request, control).await,
            "turn/start" | "goal/resume" => self.start_turn(request).await,
            "turn/interrupt" => self.interrupt_turn(request),
            "turn/steer" => controls::steer(self.active.as_ref(), request.params).map(Some),
            "goal/pause" => controls::pause(self.active.as_ref(), request.params).map(Some),
            _ => Err(Fault {
                code: -32601,
                message: "method not found".into(),
                reason: None,
            }),
        }
    }

    fn initialize(&mut self, request: Request) -> Result<Option<Value>, Fault> {
        if self.initialized {
            return Err(Fault::state(Reason::AlreadyInitialized));
        }
        let params: InitializeParams = params(request.params)?;
        self.provisional_display = params
            .capabilities
            .as_ref()
            .is_some_and(|c| c.provisional_display);
        self.host_session_approval = params
            .capabilities
            .as_ref()
            .is_some_and(|capabilities| capabilities.host_command_session_approval)
            && self.threads.approval_channel == ApprovalChannel::Human;
        self.opt_out = params
            .capabilities
            .and_then(|c| c.opt_out_notification_methods)
            .unwrap_or_default();
        self.initialized = true;
        Ok(Some(json!(InitializeResult {
            user_agent: format!("arany/{}", env!("CARGO_PKG_VERSION")),
            platform_os: if cfg!(target_os = "macos") {
                PlatformOs::Macos
            } else {
                PlatformOs::Linux
            },
            capabilities: CAPABILITIES.iter().map(|s| (*s).into()).collect()
        })))
    }

    async fn preview_rewind(&mut self, request: Request) -> Result<Option<Value>, Fault> {
        let id = request.id.clone();
        if self.active.is_some() {
            return Err(Fault::state(Reason::Busy));
        }
        self.rewind_preview = None;
        let (result, pending) = rewind::preview(
            &self.engine,
            &self.state,
            &self.threads,
            params(request.params)?,
        )
        .await?;
        encode_line(&response(id.clone(), &result))
            .map_err(|_| Fault::engine(EngineError::ContextTooLarge))?;
        self.rewind_preview = Some(pending);
        Ok(Some(json!(result)))
    }

    async fn restore_rewind(
        &mut self,
        request: Request,
        control: Option<OutputControl>,
    ) -> Result<Option<Value>, Fault> {
        let id = request.id.clone();
        if self.active.is_some() {
            return Err(Fault::state(Reason::Busy));
        }
        let reservation = format!(
            "{}{}",
            "x".repeat(MAX_REWIND_OTHER_RESULT_BYTES),
            "\u{1}".repeat(arany::MAX_USER_MESSAGE_BYTES),
        );
        // Control bytes reserve the prompt's worst-case JSON escaping.
        encode_line(&response(id.clone(), reservation))
            .map_err(|_| Fault::engine(EngineError::ContextTooLarge))?;
        let cancellation = RunCancellation::new();
        let mut restoration = Box::pin(rewind::restore(
            &self.engine,
            &self.state,
            &mut self.threads,
            &mut self.rewind_preview,
            params(request.params)?,
            cancellation.clone(),
        ));
        let (result, busy) = Self::restore_while_serving(
            &mut self.inbound,
            &self.outbound,
            &mut self.shutdown,
            restoration.as_mut(),
            &cancellation,
        )
        .await;
        for reply in busy {
            self.inbound
                .wait(control.clone(), &self.outbound, send(&self.outbound, reply))
                .await
                .map_err(internal)?;
        }
        let result = result?;
        self.inbound
            .wait(
                control.clone(),
                &self.outbound,
                send(&self.outbound, response(id.clone(), &result)),
            )
            .await
            .map_err(internal)?;
        if let Some(thread) = result.thread {
            self.inbound
                .wait(
                    control.clone(),
                    &self.outbound,
                    emit(
                        &self.outbound,
                        &self.opt_out,
                        notification("thread/started", ThreadResult { thread }),
                    ),
                )
                .await
                .map_err(internal)?;
        }
        Ok(None)
    }

    async fn restore_while_serving(
        inbound: &mut Inbound,
        outbound: &mpsc::Sender<Message>,
        shutdown: &mut bool,
        mut restoration: Pin<&mut impl Future<Output = Result<ThreadRewindResult, Fault>>>,
        cancellation: &RunCancellation,
    ) -> (Result<ThreadRewindResult, Fault>, Vec<Message>) {
        let mut busy = Vec::new();
        loop {
            let stop = tokio::select! {
                result = restoration.as_mut() => return (result, busy),
                message = inbound.next(), if !*shutdown => match message {
                    None | Some(ClientMessage::Eof) => true,
                    Some(ClientMessage::Message(Message::Request(request))) => {
                        Self::queue_rewind_busy(outbound, &mut busy, request)
                    }
                    _ => false,
                }
            };
            if stop {
                cancellation.cancel();
                *shutdown = true;
                inbound.stopping.send_replace(true);
            }
        }
    }

    fn queue_rewind_busy(
        outbound: &mpsc::Sender<Message>,
        busy: &mut Vec<Message>,
        request: Request,
    ) -> bool {
        let reply = Fault::state(Reason::Busy).message(Some(request.id));
        if let Err(mpsc::error::TrySendError::Full(reply)) = outbound.try_send(reply) {
            if busy.len() < REWIND_BUSY_QUEUE {
                busy.push(reply);
            } else {
                return true;
            }
        }
        false
    }

    async fn start_thread(
        &mut self,
        request: Request,
        control: Option<OutputControl>,
    ) -> Result<Option<Value>, Fault> {
        let id = request.id.clone();
        let result = threads::start(
            &self.engine,
            &self.state,
            &mut self.threads,
            params(request.params)?,
        )
        .await?;
        self.inbound
            .wait(
                control.clone(),
                &self.outbound,
                send(&self.outbound, response(id.clone(), &result)),
            )
            .await
            .map_err(internal)?;
        self.inbound
            .wait(
                control.clone(),
                &self.outbound,
                emit(
                    &self.outbound,
                    &self.opt_out,
                    notification("thread/started", result),
                ),
            )
            .await
            .map_err(internal)?;
        Ok(None)
    }

    async fn list_threads(
        &mut self,
        request: Request,
        control: Option<OutputControl>,
    ) -> Result<Option<Value>, Fault> {
        let id = request.id.clone();
        let result = threads::list(&self.state, params(request.params)?).await?;
        let reply = response(id.clone(), result);
        encode_line(&reply).map_err(|_| Fault::engine(EngineError::ContextTooLarge))?;
        self.inbound
            .wait(control.clone(), &self.outbound, send(&self.outbound, reply))
            .await
            .map_err(internal)?;
        Ok(None)
    }

    fn unsubscribe_thread(&mut self, request: Request) -> Result<Option<Value>, Fault> {
        let id = request.id.clone();
        let input: ThreadUnsubscribeParams = params(request.params)?;
        if self
            .rewind_preview
            .as_ref()
            .is_some_and(|p| p.belongs_to(input.thread_id))
        {
            self.rewind_preview = None;
        }
        if let Some(turn) = self
            .active
            .as_mut()
            .filter(|t| t.thread_id == Some(input.thread_id))
        {
            if turn.pending_close.is_some() {
                return Err(Fault::state(Reason::Busy));
            }
            turn.pending_close = Some(id.clone());
            turn.approvals.close();
            turn.cancel.cancel();
            return Ok(None);
        }
        self.history.unsubscribe(input.thread_id);
        let status = if self.threads.map.remove(&input.thread_id).is_some() {
            ThreadUnsubscribeStatus::Unsubscribed
        } else {
            ThreadUnsubscribeStatus::NotLoaded
        };
        Ok(Some(json!(ThreadUnsubscribeResult { status })))
    }

    fn update_settings(&mut self, request: Request) -> Result<Option<Value>, Fault> {
        let input: ThreadSettingsUpdateParams = params(request.params)?;
        let loaded = self
            .threads
            .map
            .get_mut(&input.thread_id)
            .ok_or_else(|| Fault::state(Reason::ThreadNotLoaded))?;
        loaded.approval_mode = threads::approval_mode(&input.permission_mode);
        if let Some(turn) = self
            .active
            .as_ref()
            .filter(|turn| turn.thread_id == Some(input.thread_id))
            && let Some(approvals) = &turn.approval_mode
        {
            approvals.set_mode(loaded.approval_mode);
        }
        Ok(Some(
            serde_json::to_value(ThreadSettingsUpdateResult {
                permission_mode: input.permission_mode,
            })
            .expect("typed settings result"),
        ))
    }

    async fn resume_thread(
        &mut self,
        request: Request,
        control: Option<OutputControl>,
    ) -> Result<Option<Value>, Fault> {
        let id = request.id.clone();
        let input: ThreadResumeParams = params(request.params)?;
        if self
            .active
            .as_ref()
            .is_some_and(|turn| turn.thread_id == Some(input.thread_id))
        {
            return Err(Fault::state(Reason::Busy));
        }
        let thread_id = input.thread_id;
        let (result, loaded) = threads::resume(&self.state, &self.threads, input).await?;
        let response = response(id.clone(), &result);
        if encode_line(&response).is_err() {
            return Err(Fault::engine(EngineError::ContextTooLarge));
        }
        self.engine.revoke_host_approvals().map_err(Fault::engine)?;
        self.threads.map.insert(thread_id, loaded);
        self.inbound
            .wait(
                control.clone(),
                &self.outbound,
                send(&self.outbound, response),
            )
            .await
            .map_err(internal)?;
        Ok(None)
    }

    async fn start_turn(&mut self, request: Request) -> Result<Option<Value>, Fault> {
        let id = request.id.clone();
        let (providers, request, controls, inbox, first_sequence) =
            self.prepare_turn(&request.method, request.params).await?;
        let thread_id = request.session_id;
        let submitted_goal = request.goal.id;
        let display = controls.hints.clone().map(DisplayFollower::new);
        let approval_mode = controls.approvals.clone();
        let steering = controls.steering.clone();
        let approval_end = RunCancellation::new();
        let SubmittedRun {
            result,
            events,
            cancel,
        } = self
            .submit_run(providers, request, controls, approval_end.clone())
            .await?;
        let approvals = match &approval_mode {
            Some(_) => self.start_approvals(
                inbox,
                cancel.clone(),
                thread_id.expect("human approval Session"),
                approval_end,
            ),
            None => approvals::Approvals::closed(),
        };
        self.active = Some(ActiveTurn {
            thread_id,
            result,
            events,
            display,
            cancel,
            approvals: Arc::new(approvals),
            approval_mode,
            steering,
            pending_start: Some(id.clone()),
            result_done: false,
            pending_close: None,
            state: TurnState {
                goal_id: submitted_goal,
                first_sequence: Some(first_sequence),
                ..Default::default()
            },
        });
        Ok(None)
    }

    async fn prepare_turn(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> Result<
        (
            arany::AdmittedProviders,
            RunRequest,
            RunControls,
            arany::ApprovalInbox,
            u64,
        ),
        Fault,
    > {
        if let Some(native) = &mut self.native
            && let Some(submission) = native.pending.take()
        {
            let first = match submission
                .request
                .session_id
                .filter(|_| submission.controls.approvals.is_some())
            {
                Some(id) => threads::load_view(&self.state, id)
                    .await?
                    .last_sequence
                    .checked_add(1)
                    .ok_or_else(|| Fault::engine(EngineError::InvalidHistory))?,
                None => 1,
            };
            return Ok((
                submission.providers,
                submission.request,
                submission.controls,
                submission.inbox,
                first,
            ));
        }
        let mut input = controls::Submission::parse(method, params)?;
        if self.active.is_some() {
            return Err(Fault::state(Reason::Busy));
        }
        self.rewind_preview = None;
        let loaded = self
            .threads
            .map
            .get(&input.thread_id)
            .ok_or_else(|| Fault::state(Reason::ThreadNotLoaded))?;
        let (objective, images) = input.parts()?;
        let view = threads::load_view(&self.state, input.thread_id).await?;
        if view.provider_setup_blocks_continuation() {
            return Err(Fault::engine(EngineError::ProviderSetupBlocked));
        }
        let goal = input.goal(loaded, &view)?;
        let (approvals, inbox) = ToolApprovals::new(loaded.approval_mode);
        let approvals = if self.threads.approval_channel == ApprovalChannel::Terminal {
            approvals
        } else if self.host_session_approval {
            approvals.without_prefix_rules()
        } else {
            approvals.once_only()
        };
        let approvals = if self.threads.approval_channel == ApprovalChannel::Restricted {
            approvals.without_human_only()
        } else {
            approvals
        };
        let first_sequence = view
            .last_sequence
            .checked_add(1)
            .ok_or_else(|| Fault::engine(EngineError::InvalidHistory))?;

        let request = RunRequest {
            skill: None,
            session_id: Some(input.thread_id),
            title: None,
            objective,
            images,
            workspace: loaded.workspace.clone(),
            launch_directory: None,
            include_paths: loaded.include_paths.clone(),
            policy: loaded.policy,
            goal,
        };
        let steering = RunSteering::new();
        let hints = (self.provisional_display
            && !self.opt_out.iter().any(|method| method == "turn/display"))
        .then(arany::RunHints::with_previews);
        let controls = RunControls {
            tools: loaded.grant.clone(),
            approvals: Some(approvals),
            steering: Some(steering),
            cancellation: RunCancellation::new(),
            hints,
        };
        Ok((
            loaded.providers.clone(),
            request,
            controls,
            inbox,
            first_sequence,
        ))
    }

    async fn submit_run(
        &mut self,
        providers: arany::AdmittedProviders,
        request: RunRequest,
        controls: RunControls,
        finished: RunCancellation,
    ) -> Result<SubmittedRun, Fault> {
        let grants_tools = !matches!(controls.tools, arany::ToolGrant::None);
        let worker_engine = self.engine.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let result = self.workers.spawn(async move {
            let result = match worker_engine.submit(providers, request, controls) {
                Ok(handle) => {
                    let _ = started.send(Ok((handle.events, handle.cancel)));
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
                let _ = self.workers.join_next().await;
                if let Some(native) = &mut self.native {
                    if grants_tools {
                        arany::record_development_failure(arany::DevelopmentFailure::ToolAdmission);
                    }
                    native.start_error = Some(embedded::classify_run_error(error));
                    return Err(Fault::engine(EngineError::CoordinatorFailed));
                }
                return Err(Fault::engine(error));
            }
            Err(_) => {
                let _ = self.workers.join_next().await;
                return Err(Fault::engine(EngineError::CoordinatorFailed));
            }
        };
        Ok(SubmittedRun {
            result,
            events,
            cancel,
        })
    }

    fn start_approvals(
        &self,
        inbox: arany::ApprovalInbox,
        cancel: RunCancellation,
        thread_id: SessionId,
        approval_end: RunCancellation,
    ) -> approvals::Approvals {
        approvals::Approvals::start(
            inbox,
            cancel,
            thread_id,
            approvals::Channel {
                outbound: self.outbound.clone(),
                next_id: self.next_server_id.clone(),
                approval_channel: self.threads.approval_channel,
            },
            approval_end,
            {
                let state = self.state.clone();
                move |intent| approvals::started_at(state.clone(), thread_id, intent)
            },
        )
    }

    fn interrupt_turn(&mut self, request: Request) -> Result<Option<Value>, Fault> {
        let input: TurnInterruptParams = params(request.params)?;
        let turn = self
            .active
            .as_ref()
            .filter(|turn| {
                turn.thread_id == Some(input.thread_id)
                    && turn.state.run_id == Some(input.turn_id)
                    && !turn.state.pending_completion
                    && !turn.result.is_finished()
            })
            .ok_or_else(|| Fault::state(Reason::TurnNotActive))?;
        turn.approvals.close();
        turn.cancel.cancel();
        Ok(Some(json!({})))
    }

    fn on_display(&self, run: Option<arany::RunId>, update: DisplayUpdate) {
        let turn = self.active.as_ref().expect("active turn");
        let Some(turn_id) = run.or(turn.state.run_id) else {
            return;
        };
        let message = Message::Notification(notification(
            "turn/display",
            TurnDisplayNotification {
                thread_id: turn.thread_id.expect("display Session"),
                turn_id,
                update,
            },
        ));
        if encode_line(&message).is_ok() {
            if let Some(native) = &self.native {
                let _ = native.output.try_send(embedded::Event::Message(message));
            } else {
                let _ = self.outbound.try_send(message);
            }
        }
    }

    async fn on_run_event(&mut self, event: Tapped) -> Result<(), String> {
        if let Some(native) = &self.native {
            self.inbound
                .wait(
                    self.active.as_ref().map(OutputControl::from),
                    &self.outbound,
                    async {
                        native
                            .output
                            .send(embedded::Event::Committed(event.clone()))
                            .await
                            .map_err(|_| "terminal client closed".to_owned())
                    },
                )
                .await?;
        }
        if self.native.is_some()
            && self
                .active
                .as_ref()
                .is_some_and(|turn| turn.approval_mode.is_none())
        {
            return Ok(());
        }
        let mut control = self.active.as_ref().map(OutputControl::from);
        if let (Some(control), Tapped::Committed(event)) = (&mut control, &event)
            && Some(event.session_id) == control.thread_id
            && let Event::MessageAccepted { run_id, .. } = event.event
        {
            control.run_id = Some(run_id);
            control.pending_completion = false;
        }
        self.inbound
            .wait(
                control,
                &self.outbound,
                observe(
                    self.active.as_mut().expect("active turn"),
                    event,
                    &self.state,
                    &self.outbound,
                    &self.opt_out,
                ),
            )
            .await
    }

    async fn on_committed_event(&mut self, event: Tapped) -> Result<(), String> {
        let control = self.active.as_ref().map(OutputControl::from);
        self.inbound
            .wait(
                control,
                &self.outbound,
                self.history.observe(event, &self.outbound, &self.opt_out),
            )
            .await
    }

    async fn on_run_finished(
        &mut self,
        result: Result<arany::RunResult, EngineError>,
    ) -> Result<(), String> {
        let turn = self.active.as_mut().expect("active turn");
        turn.result_done = true;
        turn.approvals.close();
        self.drain_run_events().await?;
        let refused = result.as_ref().ok().and_then(Refused::from_result);
        let control = self.active.as_ref().map(OutputControl::from);
        let Self {
            inbound,
            outbound,
            active,
            state,
            opt_out,
            history,
            threads,
            native,
            ..
        } = self;
        inbound
            .wait(control, outbound, async {
                let turn = active.as_mut().expect("active turn");
                let failure = if turn.pending_start.is_none() {
                    result.as_ref().err().map(ToString::to_string)
                } else {
                    None
                };
                let ending = Completion::Final {
                    settled: result.is_ok(),
                    refused,
                };
                if let Some(native) = native {
                    native
                        .output
                        .send(embedded::Event::finished(
                            result.map_err(embedded::classify_run_error),
                        ))
                        .await
                        .map_err(|_| "terminal client closed".to_owned())?;
                    *active = None;
                    return Ok(());
                }
                Self::report_start_failure(turn, result.map(|_| ()), state, outbound).await?;
                complete(turn, state, outbound, opt_out, &ending).await?;
                if let Some(id) = turn.pending_close.take() {
                    let thread_id = turn.thread_id.ok_or("Session identity unavailable")?;
                    history.unsubscribe(thread_id);
                    threads.map.remove(&thread_id);
                    send(
                        outbound,
                        response(
                            id,
                            ThreadUnsubscribeResult {
                                status: ThreadUnsubscribeStatus::Unsubscribed,
                            },
                        ),
                    )
                    .await?;
                }
                *active = None;
                if let Some(failure) = failure {
                    return Err(format!(
                        "{failure}; Run may be interrupted; check Session history"
                    ));
                }
                Ok(())
            })
            .await
    }

    async fn drain_run_events(&mut self) -> Result<(), String> {
        loop {
            let event = {
                let turn = self.active.as_mut().expect("active turn");
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
            self.on_run_event(event).await?;
        }
        Ok(())
    }

    async fn report_start_failure(
        turn: &mut ActiveTurn,
        result: Result<(), EngineError>,
        state: &std::path::Path,
        outbound: &mpsc::Sender<Message>,
    ) -> Result<(), String> {
        if result.is_err()
            && let Some(id) = turn.pending_start.take()
        {
            let error = result.expect_err("failed Run");
            let committed_budget = matches!(error, EngineError::GoalBudgetExhausted);
            let mut fault = Fault::engine(error);
            if committed_budget
                && let Ok(view) =
                    threads::load_view(state, turn.thread_id.ok_or("Session identity unavailable")?)
                        .await
                && let Some(feedback) =
                    crate::presentation::budget_refusal_feedback(&view, turn.state.goal_id)
            {
                fault.message = feedback;
            }
            send(outbound, fault.message(Some(id))).await?;
        }
        Ok(())
    }

    async fn on_stop(mut self, result: Result<(), String>) -> Result<(), String> {
        self.stopping.send_replace(true);
        if let Some(turn) = self.active.take() {
            turn.cancel.cancel();
            if !turn.result_done {
                let _ = self.workers.join_next().await;
            }
        }
        if self.native.is_some() {
            return result;
        }
        let engine = Arc::try_unwrap(self.engine)
            .map_err(|_| "Run did not release the Engine".to_owned())?;
        let closed = engine
            .close()
            .await
            .map_err(|_| "store shutdown failed".to_owned());
        result.and(closed)
    }
}

pub(in crate::cli) async fn run_configured(
    engine: Engine,
    state: PathBuf,
    inbound: mpsc::Receiver<ClientMessage>,
    outbound: mpsc::Sender<ServerMessage>,
    threads: Threads,
    stopping: watch::Sender<bool>,
) -> Result<(), String> {
    run_shared(
        Arc::new(engine),
        state,
        inbound,
        outbound,
        threads,
        stopping,
        None,
    )
    .await
}

pub(in crate::cli) async fn run_embedded(
    engine: Arc<Engine>,
    state: PathBuf,
    inbound: mpsc::Receiver<ClientMessage>,
    outbound: mpsc::Sender<ServerMessage>,
    native: embedded::Native,
    stopping: watch::Sender<bool>,
) -> Result<(), String> {
    run_shared(
        engine,
        state,
        inbound,
        outbound,
        Threads {
            approval_channel: ApprovalChannel::Terminal,
            ..Default::default()
        },
        stopping,
        Some(native),
    )
    .await
}

async fn run_shared(
    engine: Arc<Engine>,
    state: PathBuf,
    inbound: mpsc::Receiver<ClientMessage>,
    outbound: mpsc::Sender<ServerMessage>,
    threads: Threads,
    stopping: watch::Sender<bool>,
    native: Option<embedded::Native>,
) -> Result<(), String> {
    let mut server =
        ServerLoop::configured(engine, state, inbound, outbound, threads, stopping, native);
    server.start_embedded().await?;
    let result = server.run().await;
    server.on_stop(result).await
}

impl ServerLoop {
    async fn start_embedded(&mut self) -> Result<(), String> {
        if self.native.is_some() {
            let submission = self
                .native
                .as_ref()
                .and_then(|native| native.pending.as_ref())
                .expect("embedded submission");
            let thread_id = submission.request.session_id;
            let previews = submission.controls.hints.is_some();
            let request = Request {
                jsonrpc: Default::default(),
                id: RequestId::Integer(1),
                method: "initialize".into(),
                params: Some(
                    json!({"clientInfo":{"name":"arany-terminal","version":env!("CARGO_PKG_VERSION")},"capabilities":{"provisionalDisplay":previews}}),
                ),
            };
            self.initialize(request).map_err(|fault| fault.message)?;
            let request = Request {
                jsonrpc: Default::default(),
                id: RequestId::Integer(2),
                method: "turn/start".into(),
                params: Some(json!({"threadId":thread_id})),
            };
            if let Err(fault) = self.start_turn(request).await {
                let native = self.native.as_mut().expect("attached transport");
                let error = native.start_error.take().unwrap_or_else(|| {
                    embedded::RunAttemptError::Failed(embedded::RunFailure::Server(fault.message))
                });
                native
                    .output
                    .send(embedded::Event::finished(Err(error)))
                    .await
                    .map_err(|_| "terminal client closed".to_owned())?;
            }
        }
        Ok(())
    }
}

impl ServerLoop {
    fn configured(
        engine: Arc<Engine>,
        state: PathBuf,
        inbound: mpsc::Receiver<ClientMessage>,
        outbound: mpsc::Sender<ServerMessage>,
        threads: Threads,
        stopping: watch::Sender<bool>,
        native: Option<embedded::Native>,
    ) -> Self {
        let local = native
            .as_ref()
            .and_then(|native| native.pending.as_ref())
            .map(|pending| (pending.controls.clone(), state.clone()));
        let inbound = Inbound {
            receiver: inbound,
            pending: VecDeque::new(),
            stopping: stopping.clone(),
            ended: false,
            local,
        };
        let committed = engine.subscribe();
        Self {
            engine,
            state,
            inbound,
            outbound,
            threads,
            stopping,
            committed,
            history: history::History::default(),
            rewind_preview: None,
            active: None,
            workers: tokio::task::JoinSet::new(),
            next_server_id: Arc::new(std::sync::atomic::AtomicI64::new(1)),
            initialized: false,
            host_session_approval: false,
            provisional_display: false,
            opt_out: Vec::new(),
            shutdown: false,
            native,
        }
    }
}

#[cfg(test)]
mod tests;

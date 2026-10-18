use super::{Config, tools, wire};
use crate::cli::serve::server::ClientMessage;
use arany::{RunId, SessionId, now_ms};
use arany_cli::protocol::{methods::*, rpc::*};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{Notify, mpsc},
    time::Instant,
};

pub(super) type Shared = Arc<Mutex<State>>;

#[derive(Default)]
pub(super) struct State {
    initialized: bool,
    authenticated: bool,
    mcp_notice: bool,
    next_id: i64,
    pending: BTreeMap<i64, Pending>,
    sessions: BTreeMap<SessionId, Session>,
    next_permission: i64,
    permission: Option<Permission>,
    changed: Arc<Notify>,
    active: Option<Active>,
}

struct Session {
    sequence: u64,
    cwd: String,
}

struct Permission {
    id: RequestId,
    serve_id: RequestId,
    deadline: Instant,
    expires_at_ms: u64,
    item_id: String,
    sent: bool,
}

struct Active {
    id: RequestId,
    session: SessionId,
    turn: Option<RunId>,
    cancelled: bool,
    answered: bool,
    completion: Option<Turn>,
    resync: bool,
    projected: BTreeSet<(String, bool)>,
    uncertain: usize,
    incomplete: bool,
}

enum Pending {
    Initialize(RequestId),
    New(RequestId, String),
    Load {
        id: RequestId,
        cwd: String,
        replay: bool,
    },
    Start,
    Interrupt,
    Recover,
}

fn parse<T: DeserializeOwned>(value: Option<Value>) -> Result<T, ()> {
    serde_json::from_value(value.unwrap_or_else(|| json!({}))).map_err(|_| ())
}

async fn send_request(
    state: &Shared,
    serve: &mpsc::Sender<ClientMessage>,
    method: &str,
    params: impl Serialize,
    pending: Pending,
) -> Result<(), String> {
    let message = {
        let mut state = state.lock().expect("translator state");
        if state.pending.len() >= 64 {
            return Err("ACP pending request bound exceeded".into());
        }
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or("ACP request IDs exhausted")?;
        let id = state.next_id;
        state.pending.insert(id, pending);
        Message::Request(Request {
            jsonrpc: Default::default(),
            id: RequestId::Integer(id),
            method: method.into(),
            params: Some(serde_json::to_value(params).expect("typed serve request")),
        })
    };
    serve
        .send(ClientMessage::Message(message))
        .await
        .map_err(|_| "serve input closed".into())
}

pub(super) fn immediate(output: &mpsc::Sender<Value>, value: Value) -> Result<(), String> {
    encode_line(&value).map_err(|_| "ACP output exceeds its bound")?;
    output
        .try_send(value)
        .map_err(|_| "ACP output queue full or closed".into())
}

async fn emit(output: &mpsc::Sender<Value>, value: Value) -> Result<(), String> {
    encode_line(&value).map_err(|_| "ACP output exceeds its bound")?;
    output
        .send(value)
        .await
        .map_err(|_| "ACP output closed".into())
}

pub(super) async fn input(
    message: wire::Input,
    config: &Config,
    state: &Shared,
    serve: &mpsc::Sender<ClientMessage>,
    output: &mpsc::Sender<Value>,
) -> Result<(), String> {
    let request = match message {
        wire::Input::Request(request) => request,
        wire::Input::Notification(note) => {
            let cancel = {
                let mut state = state.lock().expect("translator state");
                if let Some(active) = &mut state.active {
                    let matches = match note.method.as_str() {
                        "session/cancel" => parse::<wire::Cancel>(note.params)
                            .is_ok_and(|p| p.session_id == active.session),
                        "$/cancel_request" => parse::<wire::CancelRequest>(note.params)
                            .is_ok_and(|p| p.request_id == active.id),
                        _ => false,
                    };
                    if matches && !active.cancelled {
                        active.cancelled = true;
                        active
                            .turn
                            .filter(|_| active.completion.is_none())
                            .map(|turn_id| TurnInterruptParams {
                                thread_id: active.session,
                                turn_id,
                            })
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            state.lock().expect("translator state").changed.notify_one();
            if let Some(cancel) = cancel {
                send_request(state, serve, "turn/interrupt", cancel, Pending::Interrupt).await?;
            }
            return Ok(());
        }
        wire::Input::Response(reply) => {
            return permission_reply(reply.id, Some(reply.result), state, serve).await;
        }
        wire::Input::Error(reply) => {
            if let Some(id) = reply.id {
                permission_reply(id, None, state, serve).await?;
            }
            return Ok(());
        }
    };
    let id = request.id;
    if matches!(&id, RequestId::String(value) if value.len() > 256) {
        return immediate(
            output,
            wire::error(
                None,
                -32600,
                "Request ID exceeds 256 bytes",
                "invalidRequest",
            ),
        );
    }
    let invalid = || {
        wire::error(
            Some(id.clone()),
            -32602,
            "Invalid parameters",
            "invalidParams",
        )
    };
    let refusal = {
        let state = state.lock().expect("translator state");
        if request.method == "initialize" && state.initialized {
            Some(wire::error(
                Some(id.clone()),
                -32600,
                "Already initialized",
                "alreadyInitialized",
            ))
        } else if request.method != "initialize" && !state.initialized {
            Some(wire::error(
                Some(id.clone()),
                -32600,
                "Initialize first",
                "notInitialized",
            ))
        } else if state.pending.len() >= 62 {
            Some(wire::error(Some(id.clone()), -32600, "Server busy", "busy"))
        } else {
            None
        }
    };
    if let Some(error) = refusal {
        return immediate(output, error);
    }
    match request.method.as_str() {
        "initialize" => {
            let Ok(client) =
                parse::<wire::Initialize>(request.params).and_then(wire::Initialize::client)
            else {
                return immediate(output, invalid());
            };
            state.lock().expect("translator state").initialized = true;
            send_request(
                state,
                serve,
                "initialize",
                InitializeParams {
                    client_info: client,
                    capabilities: Some(ClientCapabilities {
                        opt_out_notification_methods: Some(vec!["events/committed".into()]),
                    }),
                },
                Pending::Initialize(id),
            )
            .await
        }
        "authenticate" => {
            if state.lock().expect("translator state").active.is_some() {
                return immediate(output, wire::error(Some(id), -32600, "Server busy", "busy"));
            }
            let Ok(params) = parse::<wire::Authenticate>(request.params) else {
                return immediate(output, invalid());
            };
            if params.method_id != "arany-setup" {
                return immediate(output, invalid());
            }
            let authenticated = config.authenticate().await;
            state.lock().expect("translator state").authenticated = authenticated;
            immediate(
                output,
                if authenticated {
                    wire::response(id, json!({}))
                } else {
                    wire::auth_required(id)
                },
            )
        }
        "session/new" => {
            let Ok(params) = parse::<wire::NewSession>(request.params) else {
                return immediate(output, invalid());
            };
            if !params.additional_directories.is_empty() {
                return immediate(output, invalid());
            }
            notice_mcp(state, &params.mcp_servers);
            let cwd = params.cwd;
            let params = config.thread(cwd.clone());
            send_request(state, serve, "thread/start", params, Pending::New(id, cwd)).await
        }
        "session/load" | "session/resume" => {
            let replay = request.method == "session/load";
            let params = if replay {
                parse::<wire::LoadSession>(request.params).map(|p| wire::ResumeSession {
                    session_id: p.session_id,
                    cwd: p.session.cwd,
                    mcp_servers: p.session.mcp_servers,
                    additional_directories: p.session.additional_directories,
                })
            } else {
                parse::<wire::ResumeSession>(request.params)
            };
            let Ok(params) = params else {
                return immediate(output, invalid());
            };
            if !params.additional_directories.is_empty() {
                return immediate(output, invalid());
            }
            if state
                .lock()
                .expect("translator state")
                .active
                .as_ref()
                .is_some_and(|active| active.session == params.session_id)
            {
                return immediate(output, wire::error(Some(id), -32600, "Server busy", "busy"));
            }
            notice_mcp(state, &params.mcp_servers);
            let cwd = params.cwd;
            let selection = config.thread(cwd.clone());
            send_request(
                state,
                serve,
                "thread/resume",
                ThreadResumeParams {
                    thread_id: params.session_id,
                    cwd: selection.cwd,
                    provider: selection.provider,
                    model: selection.model,
                    effort: selection.effort,
                    collaboration: selection.collaboration,
                    max_active_children: selection.max_active_children,
                    tools: selection.tools,
                    permission_mode: selection.permission_mode,
                    include_paths: selection.include_paths,
                },
                Pending::Load { id, cwd, replay },
            )
            .await
        }
        "session/prompt" => {
            let Ok(params) = parse::<wire::Prompt>(request.params) else {
                return immediate(output, invalid());
            };
            let session_id = params.session_id;
            let Ok(input) = params.input() else {
                return immediate(output, invalid());
            };
            let refusal = {
                let mut state = state.lock().expect("translator state");
                if !state.authenticated {
                    Some(wire::auth_required(id.clone()))
                } else if !state.sessions.contains_key(&session_id) {
                    Some(wire::error(
                        Some(id.clone()),
                        -32002,
                        "Session not found",
                        "resource_not_found",
                    ))
                } else if state.active.is_some() {
                    Some(wire::error(Some(id.clone()), -32600, "Server busy", "busy"))
                } else {
                    state.active = Some(Active {
                        id: id.clone(),
                        session: session_id,
                        turn: None,
                        cancelled: false,
                        answered: false,
                        completion: None,
                        resync: false,
                        projected: BTreeSet::new(),
                        uncertain: 0,
                        incomplete: false,
                    });
                    None
                }
            };
            if let Some(error) = refusal {
                return immediate(output, error);
            }
            send_request(
                state,
                serve,
                "turn/start",
                TurnStartParams {
                    thread_id: session_id,
                    input,
                    goal: None,
                },
                Pending::Start,
            )
            .await
        }
        _ => immediate(
            output,
            wire::error(Some(id), -32601, "Method not found", "methodNotFound"),
        ),
    }
}

fn notice_mcp(state: &Shared, servers: &[Value]) {
    if !servers.is_empty() {
        let mut state = state.lock().expect("translator state");
        if !state.mcp_notice {
            eprintln!("Arany ignores editor MCP servers.");
            state.mcp_notice = true;
        }
    }
}

fn history_updates<'a>(
    result: &'a ThreadResumeResult,
    cwd: &'a str,
) -> impl Iterator<Item = Value> + 'a {
    let session = result.thread.id;
    result.turns.iter().flat_map(move |turn| {
        turn.items.iter().flat_map(move |item| match item {
            Item::UserMessage { text, images, .. } => vec![json!({
                "jsonrpc":"2.0", "method":"session/update", "params":{
                    "sessionId":session, "update":{
                        "sessionUpdate":"user_message_chunk",
                        "content":{"type":"text", "text":text},
                        "_meta":{"arany.turnId":turn.id, "arany.turnStatus":turn.status,
                            "arany.images":images}
                    }
                }
            })],
            Item::AgentMessage { text, .. } => vec![wire::answer(session, text)],
            _ => [true, false]
                .into_iter()
                .filter_map(|started| tools::project(item, cwd, session, started))
                .map(|update| tools::notification(session, update, turn.id))
                .collect(),
        })
    })
}

pub(super) async fn output(
    mut inbound: mpsc::Receiver<Message>,
    serve: mpsc::Sender<ClientMessage>,
    outbound: mpsc::Sender<Value>,
    state: Shared,
    stopping: tokio::sync::watch::Receiver<bool>,
) -> Result<(), String> {
    loop {
        if let Some(withdrawal) = withdraw_permission(&state, None, false) {
            emit(&outbound, withdrawal).await?;
        }
        let (deadline, changed) = {
            let state = state.lock().expect("translator state");
            (
                state.permission.as_ref().map(|p| p.deadline),
                state.changed.clone(),
            )
        };
        let message = tokio::select! { biased;
            () = changed.notified() => continue,
            () = async { tokio::time::sleep_until(deadline.expect("permission deadline")).await }, if deadline.is_some() => continue,
            message = inbound.recv() => message,
        };
        let Some(message) = message else { break };
        match message {
            Message::Response(reply) => {
                let RequestId::Integer(id) = reply.id else {
                    continue;
                };
                let pending = state.lock().expect("translator state").pending.remove(&id);
                match pending {
                    Some(Pending::Initialize(id)) => {
                        emit(&outbound, wire::response(id, wire::initialized())).await?
                    }
                    Some(Pending::New(id, cwd)) => {
                        let result: ThreadResult = serde_json::from_value(reply.result)
                            .map_err(|_| "invalid serve thread")?;
                        {
                            let mut state = state.lock().expect("translator state");
                            state
                                .sessions
                                .insert(result.thread.id, Session { sequence: 0, cwd });
                            state.authenticated = true;
                        }
                        emit(
                            &outbound,
                            wire::response(id, json!({"sessionId": result.thread.id})),
                        )
                        .await?;
                    }
                    Some(Pending::Load { id, cwd, replay }) => {
                        let result: ThreadResumeResult = serde_json::from_value(reply.result)
                            .map_err(|_| "invalid serve history")?;
                        if replay {
                            for update in history_updates(&result, &cwd) {
                                emit_answer(&outbound, update).await?;
                            }
                        }
                        {
                            let mut state = state.lock().expect("translator state");
                            state
                                .sessions
                                .insert(result.thread.id, Session { sequence: 0, cwd });
                            state.authenticated = true;
                        }
                        emit(&outbound, wire::response(id, json!({}))).await?;
                    }
                    Some(Pending::Start) => {
                        let result: TurnResult = serde_json::from_value(reply.result)
                            .map_err(|_| "invalid serve turn")?;
                        let cancel = {
                            let mut state = state.lock().expect("translator state");
                            let active = state.active.as_mut().ok_or("no active ACP prompt")?;
                            active.turn = Some(result.turn.id);
                            (active.cancelled && !*stopping.borrow()).then_some(
                                TurnInterruptParams {
                                    thread_id: active.session,
                                    turn_id: result.turn.id,
                                },
                            )
                        };
                        if let Some(params) = cancel {
                            send_request(
                                &state,
                                &serve,
                                "turn/interrupt",
                                params,
                                Pending::Interrupt,
                            )
                            .await?;
                        }
                    }
                    Some(Pending::Recover) => {
                        let page: EventsSubscribeResult = serde_json::from_value(reply.result)
                            .map_err(|_| "invalid serve events")?;
                        let (session, run) = {
                            let state = state.lock().expect("translator state");
                            let active = state.active.as_ref().ok_or("no active ACP prompt")?;
                            (active.session, active.turn.ok_or("no active Run")?)
                        };
                        for event in &page.events {
                            if event.run_id.as_deref() != Some(run.to_string().as_str()) {
                                continue;
                            }
                            let item = match event.kind.as_str() {
                                "MessageCommitted" => Some((
                                    Item::AgentMessage {
                                        id: format!("{run}:answer"),
                                        text: event.payload["text"]
                                            .as_str()
                                            .ok_or("invalid committed answer")?
                                            .into(),
                                        phase: AgentMessagePhase::FinalAnswer,
                                    },
                                    false,
                                )),
                                "ToolStarted" => {
                                    let intent: arany::EffectIntent =
                                        serde_json::from_value(event.payload["intent"].clone())
                                            .map_err(|_| "invalid committed Tool intent")?;
                                    Some((
                                        crate::cli::serve::items::tool_item(&intent, None, false),
                                        true,
                                    ))
                                }
                                "ToolFinished" => {
                                    let observation: arany::ToolObservation =
                                        serde_json::from_value(
                                            event.payload["observation"].clone(),
                                        )
                                        .map_err(|_| "invalid committed Tool observation")?;
                                    Some((
                                        crate::cli::serve::items::tool_item(
                                            &observation.intent,
                                            Some(&observation),
                                            false,
                                        ),
                                        false,
                                    ))
                                }
                                _ => None,
                            };
                            if let Some((item, started)) = item
                                && let Some(update) = project(&state, session, run, item, started)
                            {
                                emit_answer(&outbound, update).await?;
                            }
                        }
                        if let Some(last) = page.events.last() {
                            state
                                .lock()
                                .expect("translator state")
                                .sessions
                                .get_mut(&session)
                                .expect("loaded Session")
                                .sequence = last.sequence;
                        }
                        if let Some(since) = page.next_since.filter(|_| !*stopping.borrow()) {
                            recover(
                                &state,
                                &serve,
                                &outbound,
                                EventsSubscribeParams {
                                    thread_id: session,
                                    since,
                                },
                            )
                            .await?;
                        } else {
                            finish(&state, &outbound, page.next_since.is_some()).await?;
                        }
                    }
                    _ => (),
                }
            }
            Message::Error(reply) => {
                let Some(RequestId::Integer(id)) = reply.id else {
                    continue;
                };
                let pending = state.lock().expect("translator state").pending.remove(&id);
                match pending {
                    Some(
                        Pending::Initialize(id) | Pending::New(id, _) | Pending::Load { id, .. },
                    ) => emit(&outbound, remap(id, reply.error)).await?,
                    Some(Pending::Start) => {
                        let active = state
                            .lock()
                            .expect("translator state")
                            .active
                            .take()
                            .ok_or("no active ACP prompt")?;
                        let response = if active.cancelled {
                            wire::response(active.id, json!({"stopReason":"cancelled"}))
                        } else {
                            remap(active.id, reply.error)
                        };
                        emit(&outbound, response).await?;
                    }
                    Some(Pending::Recover) => {
                        eprintln!(
                            "Arany could not reconcile all committed ACP updates; check Session history"
                        );
                        finish(&state, &outbound, true).await?;
                    }
                    _ => (),
                }
            }
            Message::Notification(note)
                if matches!(note.method.as_str(), "item/started" | "item/completed") =>
            {
                let started = note.method == "item/started";
                let (thread, turn, item) = if started {
                    let item: ItemStartedNotification =
                        serde_json::from_value(note.params.unwrap_or(Value::Null))
                            .map_err(|_| "invalid serve item")?;
                    (item.thread_id, item.turn_id, item.item)
                } else {
                    let item: ItemCompletedNotification =
                        serde_json::from_value(note.params.unwrap_or(Value::Null))
                            .map_err(|_| "invalid serve item")?;
                    (item.thread_id, item.turn_id, item.item)
                };
                if !started {
                    let withdrawal_id = {
                        let state = state.lock().expect("translator state");
                        state
                            .permission
                            .as_ref()
                            .filter(|p| item.id() == p.item_id)
                            .map(|p| p.serve_id.clone())
                    };
                    if let Some(id) = withdrawal_id
                        && let Some(note) = withdraw_permission(&state, Some(&id), true)
                    {
                        emit(&outbound, note).await?;
                    }
                }
                if let Some(update) = project(&state, thread, turn, item, started) {
                    emit_answer(&outbound, update).await?;
                }
            }
            Message::Notification(note) if note.method == "turn/completed" => {
                let result: TurnNotification =
                    serde_json::from_value(note.params.unwrap_or(Value::Null))
                        .map_err(|_| "invalid serve completion")?;
                let since = {
                    let mut state = state.lock().expect("translator state");
                    if let Some(active) = &mut state.active
                        && active.session == result.thread_id
                        && active.turn == Some(result.turn.id)
                    {
                        let recover = active.resync
                            || (!active.answered && result.turn.status == TurnStatus::Completed);
                        active.completion = Some(result.turn);
                        recover.then_some(
                            state
                                .sessions
                                .get(&result.thread_id)
                                .map_or(0, |s| s.sequence),
                        )
                    } else {
                        None
                    }
                };
                if let Some(withdrawal) = withdraw_permission(&state, None, true) {
                    emit(&outbound, withdrawal).await?;
                }
                if let Some(since) = since.filter(|_| !*stopping.borrow()) {
                    recover(
                        &state,
                        &serve,
                        &outbound,
                        EventsSubscribeParams {
                            thread_id: result.thread_id,
                            since,
                        },
                    )
                    .await?;
                } else {
                    finish(&state, &outbound, since.is_some()).await?;
                }
            }
            Message::Notification(note) if note.method == "events/resync" => {
                if let Some(active) = &mut state.lock().expect("translator state").active {
                    active.resync = true;
                }
            }
            Message::Notification(note) if note.method == "$/cancel_request" => {
                if let Ok(params) = parse::<wire::CancelRequest>(note.params)
                    && let Some(withdrawal) =
                        withdraw_permission(&state, Some(&params.request_id), true)
                {
                    emit(&outbound, withdrawal).await?;
                }
            }
            Message::Request(request) => {
                permission_request(&state, &serve, &outbound, request).await?
            }
            _ => (),
        }
    }
    let completed = state
        .lock()
        .expect("translator state")
        .active
        .as_ref()
        .is_some_and(|active| active.completion.is_some());
    if completed {
        finish(&state, &outbound, true).await?;
    }
    Ok(())
}

async fn recover(
    state: &Shared,
    serve: &mpsc::Sender<ClientMessage>,
    outbound: &mpsc::Sender<Value>,
    params: EventsSubscribeParams,
) -> Result<(), String> {
    match send_request(state, serve, "events/subscribe", params, Pending::Recover).await {
        Err(_) if serve.is_closed() => finish(state, outbound, true).await,
        result => result,
    }
}

async fn emit_answer(outbound: &mpsc::Sender<Value>, mut answer: Value) -> Result<(), String> {
    if encode_line(&answer).is_err() {
        let text = answer["params"]["update"]["content"]["text"]
            .as_str()
            .ok_or("invalid ACP answer")?;
        let mut end = text.len().min(128 * 1024);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let text = format!(
            "{}\n[truncated; see arany show {}]",
            &text[..end],
            answer["params"]["sessionId"].as_str().unwrap_or_default()
        );
        answer["params"]["update"]["content"]["text"] = json!(text);
    }
    emit(outbound, answer).await
}

async fn finish(
    state: &Shared,
    output: &mpsc::Sender<Value>,
    incomplete: bool,
) -> Result<(), String> {
    let active = state
        .lock()
        .expect("translator state")
        .active
        .take()
        .ok_or("no active ACP prompt")?;
    let turn = active.completion.ok_or("no committed turn completion")?;
    let incomplete = incomplete
        || active.incomplete
        || (turn.status == TurnStatus::Completed && !active.answered);
    let mut response = if active.cancelled
        || turn.status == TurnStatus::Interrupted
            && turn.error.is_none()
            && turn.goal.as_ref().is_some_and(|g| g.stopped.is_none())
    {
        wire::response(active.id, json!({"stopReason":"cancelled"}))
    } else if turn.status == TurnStatus::Completed
        && turn
            .goal
            .as_ref()
            .is_some_and(|g| g.status == GoalStatus::Completed && g.stopped.is_none())
    {
        wire::response(active.id, json!({"stopReason":"end_turn"}))
    } else {
        let reason = if active.uncertain > 0 {
            "effectUncertain"
        } else if turn.goal.as_ref().and_then(|g| g.stopped.as_deref())
            == Some(arany::EngineError::GoalBudgetExhausted.to_string().as_str())
        {
            "goalBudgetExhausted"
        } else if turn.status == TurnStatus::Interrupted {
            "stoppedByHook"
        } else {
            "turnFailed"
        };
        wire::error(
            Some(active.id),
            -32603,
            if active.uncertain > 0 {
                tools::UNKNOWN
            } else {
                turn.error
                    .as_ref()
                    .map_or("Run stopped; check Session history", |e| e.message.as_str())
            },
            reason,
        )
    };
    if active.uncertain > 0 && response.get("result").is_some() {
        response["result"]["_meta"]["arany.uncertainEffects"] = json!(active.uncertain);
    }
    if incomplete && response.get("result").is_some() {
        response["result"]["_meta"]["arany.incompleteUpdates"] = json!(true);
    }
    emit(output, response).await
}

fn remap(id: RequestId, error: ErrorObject) -> Value {
    if error
        .data
        .as_ref()
        .is_some_and(|d| d.reason == Reason::MissingSession)
    {
        return wire::error(Some(id), -32002, "Session not found", "resource_not_found");
    }
    if error.code == -32001
        && error
            .data
            .as_ref()
            .is_some_and(|d| d.reason == Reason::ProviderUnavailable)
    {
        return wire::auth_required(id);
    }
    let code = if matches!(error.code, -32000 | -32001) {
        -32603
    } else {
        error.code
    };
    let mut value = json!({"jsonrpc":"2.0", "id": id, "error": error});
    value["error"]["code"] = json!(code);
    value
}

pub(super) fn cancel_active(state: &Shared) {
    let mut state = state.lock().expect("translator state");
    if let Some(active) = &mut state.active {
        active.cancelled = true;
    }
    state.changed.notify_one();
}

fn project(
    state: &Shared,
    session: SessionId,
    run: RunId,
    item: Item,
    started: bool,
) -> Option<Value> {
    let mut state = state.lock().expect("translator state");
    let cwd = &state.sessions.get(&session)?.cwd.clone();
    let active = state
        .active
        .as_mut()
        .filter(|a| a.session == session && a.turn == Some(run))?;
    if let Item::AgentMessage { text, .. } = item {
        if active.answered {
            return None;
        }
        active.answered = true;
        return Some(wire::answer(session, &text));
    }
    let mut update = tools::project(&item, cwd, session, started)?;
    let id = update["toolCallId"].as_str()?.to_owned();
    if !started && !active.projected.contains(&(id.clone(), true)) {
        update["sessionUpdate"] = json!("tool_call");
    }
    if started && active.projected.contains(&(id.clone(), false)) {
        return None;
    }
    if active.projected.contains(&(id.clone(), started)) {
        return None;
    }
    if active.projected.len() >= 10_000 {
        active.incomplete = true;
        return None;
    }
    active.projected.insert((id, started));
    if update["_meta"]["arany.status"] == "uncertain" {
        active.uncertain += 1;
    }
    Some(tools::notification(session, update, run))
}

async fn approval_response(
    serve: &mpsc::Sender<ClientMessage>,
    id: RequestId,
    decision: &str,
) -> Result<(), String> {
    serve
        .send(ClientMessage::Message(Message::Response(Response {
            jsonrpc: Default::default(),
            id,
            result: json!({"decision":decision}),
        })))
        .await
        .map_err(|_| "serve input closed".into())
}

async fn permission_reply(
    id: RequestId,
    result: Option<Value>,
    state: &Shared,
    serve: &mpsc::Sender<ClientMessage>,
) -> Result<(), String> {
    let decision = match result.as_ref().map(|r| &r["outcome"]) {
        Some(outcome)
            if outcome["outcome"] == "selected" && outcome["optionId"] == "allow_once" =>
        {
            "accept"
        }
        Some(outcome) if outcome["outcome"] == "cancelled" => "cancel",
        _ => "decline",
    };
    let pending = {
        let mut state = state.lock().expect("translator state");
        if state.active.as_ref().is_some_and(|a| a.cancelled) {
            return Ok(());
        }
        let pending = state.permission.take_if(|p| {
            p.id == id && p.sent && Instant::now() < p.deadline && now_ms() < p.expires_at_ms
        });
        if decision == "cancel"
            && pending.is_some()
            && let Some(active) = &mut state.active
        {
            active.cancelled = true;
        }
        pending
    };
    if let Some(pending) = pending {
        approval_response(serve, pending.serve_id, decision).await?;
    }
    Ok(())
}

fn withdraw_permission(state: &Shared, id: Option<&RequestId>, force: bool) -> Option<Value> {
    let mut state = state.lock().expect("translator state");
    let cancelled = state.active.as_ref().is_none_or(|a| a.cancelled);
    let pending = state.permission.take_if(|p| {
        id.is_none_or(|id| *id == p.serve_id)
            && (force || cancelled || Instant::now() >= p.deadline || now_ms() >= p.expires_at_ms)
    })?;
    pending.sent.then(
        || json!({"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":pending.id}}),
    )
}

async fn permission_request(
    state: &Shared,
    serve: &mpsc::Sender<ClientMessage>,
    output: &mpsc::Sender<Value>,
    request: Request,
) -> Result<(), String> {
    let Ok(params) = parse::<ApprovalParams>(request.params) else {
        return approval_response(serve, request.id, "decline").await;
    };
    let Ok(expires_at_ms) = u64::try_from(params.expires_at_ms) else {
        return approval_response(serve, request.id, "decline").await;
    };
    let intent: arany::EffectIntent = serde_json::from_value(params.intent.clone())
        .map_err(|_| "invalid serve approval intent")?;
    let prepared = {
        let mut state = state.lock().expect("translator state");
        let valid = state.active.as_ref().is_some_and(|a| {
            a.session == params.thread_id
                && a.turn.is_none_or(|run| run == params.turn_id)
                && !a.cancelled
                && a.completion.is_none()
        });
        let remaining = expires_at_ms.saturating_sub(now_ms());
        if !valid || state.permission.is_some() || remaining == 0 || intent.call.is_unguarded() {
            None
        } else {
            let cwd = &state
                .sessions
                .get(&params.thread_id)
                .ok_or("missing Session")?
                .cwd;
            let item = crate::cli::serve::items::tool_item(&intent, None, false);
            let mut tool = tools::project(&item, cwd, params.thread_id, true)
                .ok_or("invalid approval Tool")?;
            tool.as_object_mut()
                .expect("Tool update")
                .remove("sessionUpdate");
            tool["rawInput"] = params.intent;
            tool["content"] = tools::approval_content(&params.lines);
            state.next_permission = state
                .next_permission
                .checked_add(1)
                .ok_or("ACP permission IDs exhausted")?;
            let id = RequestId::Integer(state.next_permission);
            let message = json!({"jsonrpc":"2.0","id":id,"method":"session/request_permission","params":{
                "sessionId":params.thread_id,"toolCall":tool,"options":[
                    {"optionId":"deny","kind":"reject_once","name":"Deny"},
                    {"optionId":"allow_once","kind":"allow_once","name":"Allow once"}
                ]
            }});
            if encode_line(&message).is_err() {
                None
            } else {
                let deadline = Instant::now() + Duration::from_millis(remaining);
                state.permission = Some(Permission {
                    id,
                    serve_id: request.id.clone(),
                    deadline,
                    expires_at_ms,
                    item_id: params.item_id.clone(),
                    sent: false,
                });
                Some((message, deadline, state.changed.clone()))
            }
        }
    };
    let Some((message, deadline, changed)) = prepared else {
        return approval_response(serve, request.id, "decline").await;
    };
    loop {
        let permit = tokio::select! { biased;
            () = changed.notified() => None,
            () = tokio::time::sleep_until(deadline) => None,
            permit = output.reserve() => Some(permit.map_err(|_| "ACP output closed")?),
        };
        let mut state = state.lock().expect("translator state");
        let cancelled = state.active.as_ref().is_none_or(|a| a.cancelled);
        let live = state
            .permission
            .as_ref()
            .is_some_and(|p| Instant::now() < p.deadline && now_ms() < p.expires_at_ms);
        if cancelled || !live {
            return Ok(());
        }
        if let Some(permit) = permit {
            state.permission.as_mut().expect("live permission").sent = true;
            permit.send(message);
            return Ok(());
        }
    }
}

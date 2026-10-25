use super::{Config, tools, wire};
use crate::cli::serve::server::ClientMessage;
use arany::{RunId, SessionId};
use arany_cli::protocol::{methods::*, rpc::*};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::sync::{Notify, mpsc};

pub(super) type Shared = Arc<Mutex<State>>;

struct InputContext<'a> {
    config: &'a Config,
    state: &'a Shared,
    serve: &'a mpsc::Sender<ClientMessage>,
    output: &'a mpsc::Sender<Value>,
}

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
    closing: BTreeSet<SessionId>,
    next_generation: u64,
}

struct Session {
    mode: PermissionMode,
    sequence: u64,
    cwd: String,
}

struct Permission {
    id: RequestId,
    serve_id: RequestId,
    item_id: String,
    sent: bool,
}

struct Active {
    id: RequestId,
    generation: u64,
    session: SessionId,
    turn: Option<RunId>,
    cancelled: bool,
    answered: bool,
    completion: Option<Turn>,
    resync: bool,
    projected: BTreeSet<(String, bool)>,
    uncertain: usize,
    incomplete: bool,
    close: Option<RequestId>,
    close_task: Option<tokio::task::AbortHandle>,
    watch_plan: bool,
    plan: Option<(u64, Value)>,
}

enum Pending {
    Initialize(RequestId),
    New(RequestId, String, PermissionMode),
    Load {
        id: RequestId,
        cwd: String,
        replay: bool,
        mode: PermissionMode,
    },
    Mode {
        id: RequestId,
        session: SessionId,
        config: bool,
    },
    Close(RequestId, SessionId),
    List(RequestId),
    Start,
    Interrupt,
    Recover,
    Watch {
        session: SessionId,
        run: RunId,
    },
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
        wire::Input::Notification(note) => return input_notification(note, state, serve).await,
        wire::Input::Response(reply) => return input_response(reply, state, serve).await,
        wire::Input::Error(reply) => return input_error(reply, state, serve).await,
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
        } else if state.pending.len() >= 61 {
            Some(wire::error(Some(id.clone()), -32600, "Server busy", "busy"))
        } else {
            None
        }
    };
    if let Some(error) = refusal {
        return immediate(output, error);
    }
    let context = InputContext {
        config,
        state,
        serve,
        output,
    };
    match request.method.as_str() {
        "initialize" => initialize_input(id, request.params, state, output, serve).await,
        "authenticate" => authenticate_input(id, request.params, config, state, output).await,
        "session/new" => {
            let Ok(params) = parse::<wire::NewSession>(request.params) else {
                return immediate(output, invalid_params(&id));
            };
            if !params.additional_directories.is_empty() {
                return immediate(output, invalid_params(&id));
            }
            new_session_input(id, params, config, state, serve).await
        }
        "session/load" | "session/resume" => {
            load_input(
                id,
                request.method == "session/load",
                request.params,
                &context,
            )
            .await
        }
        "session/set_mode" | "session/set_config_option" => {
            mode_input(
                id,
                request.method == "session/set_config_option",
                request.params,
                &context,
            )
            .await
        }
        "session/close" => close_input(id, request.params, &context).await,
        "session/list" => list_input(id, request.params, &context).await,
        "session/prompt" => prompt_input(id, request.params, &context).await,
        _ => immediate(
            output,
            wire::error(Some(id), -32601, "Method not found", "methodNotFound"),
        ),
    }
}

fn invalid_params(id: &RequestId) -> Value {
    wire::error(
        Some(id.clone()),
        -32602,
        "Invalid parameters",
        "invalidParams",
    )
}

async fn close_input(
    id: RequestId,
    value: Option<Value>,
    context: &InputContext<'_>,
) -> Result<(), String> {
    let Ok(params) = parse::<wire::Cancel>(value) else {
        return immediate(context.output, invalid_params(&id));
    };
    let shared = context.state;
    let cancel = {
        let mut state = context.state.lock().expect("translator state");
        if !state.sessions.contains_key(&params.session_id) {
            return immediate(
                context.output,
                wire::error(Some(id), -32002, "Session not found", "resource_not_found"),
            );
        }
        if !state.closing.insert(params.session_id) {
            return immediate(
                context.output,
                wire::error(Some(id), -32600, "Server busy", "busy"),
            );
        }
        let cancel = match state
            .active
            .as_mut()
            .filter(|a| a.session == params.session_id)
        {
            Some(active) => {
                active.close = Some(id.clone());
                active.close_task = Some(
                    tokio::spawn(expire_close(
                        shared.clone(),
                        context.serve.clone(),
                        context.output.clone(),
                        active.generation,
                    ))
                    .abort_handle(),
                );
                let first = !active.cancelled;
                active.cancelled = true;
                Some(
                    active
                        .turn
                        .filter(|_| first && active.completion.is_none())
                        .map(|turn_id| TurnInterruptParams {
                            thread_id: active.session,
                            turn_id,
                        }),
                )
            }
            None => None,
        };
        state.changed.notify_one();
        cancel
    };
    match cancel {
        Some(interrupt) => {
            if let Some(interrupt) = interrupt {
                send_request(
                    context.state,
                    context.serve,
                    "turn/interrupt",
                    interrupt,
                    Pending::Interrupt,
                )
                .await?;
            }
            Ok(())
        }
        None => unload(context.state, context.serve, id, params.session_id).await,
    }
}

async fn list_input(
    id: RequestId,
    value: Option<Value>,
    context: &InputContext<'_>,
) -> Result<(), String> {
    let Ok(params) = parse::<wire::ListSessions>(value) else {
        return immediate(context.output, invalid_params(&id));
    };
    send_request(
        context.state,
        context.serve,
        "thread/list",
        ThreadListParams {
            cwd: params.cwd,
            cursor: params.cursor,
            limit: None,
        },
        Pending::List(id),
    )
    .await
}

async fn prompt_input(
    id: RequestId,
    value: Option<Value>,
    context: &InputContext<'_>,
) -> Result<(), String> {
    let Ok(params) = parse::<wire::Prompt>(value) else {
        return immediate(context.output, invalid_params(&id));
    };
    let session_id = params.session_id;
    let Ok(input) = params.input() else {
        return immediate(context.output, invalid_params(&id));
    };
    let refusal = {
        let mut state = context.state.lock().expect("translator state");
        if !state.authenticated {
            Some(wire::auth_required(id.clone()))
        } else if !state.sessions.contains_key(&session_id) {
            Some(wire::error(
                Some(id.clone()),
                -32002,
                "Session not found",
                "resource_not_found",
            ))
        } else if state.active.is_some() || state.closing.contains(&session_id) {
            Some(wire::error(Some(id.clone()), -32600, "Server busy", "busy"))
        } else {
            state.next_generation += 1;
            let generation = state.next_generation;
            state.active = Some(Active {
                id: id.clone(),
                generation,
                session: session_id,
                turn: None,
                cancelled: false,
                answered: false,
                completion: None,
                resync: false,
                projected: BTreeSet::new(),
                uncertain: 0,
                incomplete: false,
                close: None,
                close_task: None,
                watch_plan: context.config.tools,
                plan: None,
            });
            None
        }
    };
    if let Some(error) = refusal {
        return immediate(context.output, error);
    }
    send_request(
        context.state,
        context.serve,
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

async fn load_input(
    id: RequestId,
    replay: bool,
    value: Option<Value>,
    context: &InputContext<'_>,
) -> Result<(), String> {
    let params = if replay {
        parse::<wire::LoadSession>(value).map(|p| wire::ResumeSession {
            session_id: p.session_id,
            cwd: p.session.cwd,
            mcp_servers: p.session.mcp_servers,
            additional_directories: p.session.additional_directories,
        })
    } else {
        parse::<wire::ResumeSession>(value)
    };
    let Ok(params) = params else {
        return immediate(context.output, invalid_params(&id));
    };
    if !params.additional_directories.is_empty() {
        return immediate(context.output, invalid_params(&id));
    }
    let busy = {
        let state = context.state.lock().expect("translator state");
        state
            .active
            .as_ref()
            .is_some_and(|active| active.session == params.session_id)
            || state.pending.values().any(|pending| {
                matches!(pending, Pending::Mode { session, .. } if *session == params.session_id)
            })
    };
    if busy {
        return immediate(
            context.output,
            wire::error(Some(id), -32600, "Server busy", "busy"),
        );
    }
    notice_mcp(context.state, &params.mcp_servers);
    let cwd = params.cwd;
    let mut selection = context.config.thread(cwd.clone());
    let mode = context
        .state
        .lock()
        .expect("translator state")
        .sessions
        .get(&params.session_id)
        .map(|session| session.mode.clone())
        .unwrap_or_else(|| context.config.permission_mode.clone());
    selection.permission_mode = Some(mode.clone());
    send_request(
        context.state,
        context.serve,
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
        Pending::Load {
            id,
            cwd,
            replay,
            mode,
        },
    )
    .await
}

async fn mode_input(
    id: RequestId,
    config_option: bool,
    value: Option<Value>,
    context: &InputContext<'_>,
) -> Result<(), String> {
    let params = if config_option {
        parse::<wire::SetConfigOption>(value).and_then(|p| {
            if p.config_id != "mode" || p.value_type.as_deref() == Some("boolean") {
                return Err(());
            }
            Ok(wire::SetMode {
                session_id: p.session_id,
                mode_id: p.value,
            })
        })
    } else {
        parse::<wire::SetMode>(value)
    };
    let Ok(params) = params else {
        return immediate(context.output, invalid_params(&id));
    };
    if !context
        .state
        .lock()
        .expect("translator state")
        .sessions
        .contains_key(&params.session_id)
    {
        return immediate(
            context.output,
            wire::error(Some(id), -32002, "Session not found", "resource_not_found"),
        );
    }
    send_request(
        context.state,
        context.serve,
        "thread/settings/update",
        ThreadSettingsUpdateParams {
            thread_id: params.session_id,
            permission_mode: params.mode_id,
        },
        Pending::Mode {
            id,
            session: params.session_id,
            config: config_option,
        },
    )
    .await
}

async fn input_notification(
    note: wire::AcpNotification,
    state: &Shared,
    serve: &mpsc::Sender<ClientMessage>,
) -> Result<(), String> {
    let cancel = {
        let mut state = state.lock().expect("translator state");
        if let Some(active) = &mut state.active {
            let matches = match note.method.as_str() {
                "session/cancel" => {
                    parse::<wire::Cancel>(note.params).is_ok_and(|p| p.session_id == active.session)
                }
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
    Ok(())
}

async fn input_error(
    reply: wire::AcpError,
    state: &Shared,
    serve: &mpsc::Sender<ClientMessage>,
) -> Result<(), String> {
    if let Some(id) = reply.id {
        permission_reply(id, None, state, serve).await?;
    }
    Ok(())
}

async fn input_response(
    reply: wire::AcpResponse,
    state: &Shared,
    serve: &mpsc::Sender<ClientMessage>,
) -> Result<(), String> {
    permission_reply(reply.id, Some(reply.result), state, serve).await
}

async fn initialize_input(
    id: RequestId,
    params: Option<Value>,
    state: &Shared,
    output: &mpsc::Sender<Value>,
    serve: &mpsc::Sender<ClientMessage>,
) -> Result<(), String> {
    let Ok(client) = parse::<wire::Initialize>(params).and_then(wire::Initialize::client) else {
        return immediate(output, invalid_params(&id));
    };
    state.lock().expect("translator state").initialized = true;
    send_request(
        state,
        serve,
        "initialize",
        InitializeParams {
            client_info: client,
            capabilities: None,
        },
        Pending::Initialize(id),
    )
    .await
}

async fn authenticate_input(
    id: RequestId,
    params: Option<Value>,
    config: &Config,
    state: &Shared,
    output: &mpsc::Sender<Value>,
) -> Result<(), String> {
    if state.lock().expect("translator state").active.is_some() {
        return immediate(output, wire::error(Some(id), -32600, "Server busy", "busy"));
    }
    let Ok(params) = parse::<wire::Authenticate>(params) else {
        return immediate(output, invalid_params(&id));
    };
    if params.method_id != "arany-setup" {
        return immediate(output, invalid_params(&id));
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

async fn new_session_input(
    id: RequestId,
    params: wire::NewSession,
    config: &Config,
    state: &Shared,
    serve: &mpsc::Sender<ClientMessage>,
) -> Result<(), String> {
    notice_mcp(state, &params.mcp_servers);
    let cwd = params.cwd;
    let params = config.thread(cwd.clone());
    send_request(
        state,
        serve,
        "thread/start",
        params,
        Pending::New(id, cwd, config.permission_mode.clone()),
    )
    .await
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
            Item::UserMessage { text, images, .. } => {
                let mut update = json!({
                "jsonrpc":"2.0", "method":"session/update", "params":{
                    "sessionId":session, "update":{
                        "sessionUpdate":"user_message_chunk",
                        "content":{"type":"text", "text":text},
                        "_meta":{"arany.turnId":turn.id, "arany.turnStatus":turn.status,
                            "arany.images":images}
                    }
                }
                });
                if turn.goal.as_ref().is_some_and(|goal| {
                    matches!(goal.stopped, Some(GoalStop::GoalExhausted { .. }))
                }) {
                    update["params"]["update"]["_meta"]["arany.budgetRefusal"] =
                        json!(turn.goal.as_ref().and_then(|goal| goal.budget_refusal));
                    update["params"]["update"]["_meta"]["arany.status"] =
                        json!(turn.error.as_ref().map(|error| &error.message));
                }
                vec![update]
            }
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
    let mut pending = VecDeque::new();
    let mut closed = false;
    loop {
        if let Some(withdrawal) = withdraw_permission(&state, None, false) {
            emit(&outbound, withdrawal).await?;
        }
        let changed = state.lock().expect("translator state").changed.clone();
        let message = if let Some(message) = pending.pop_front() {
            Some(message)
        } else if closed {
            None
        } else {
            tokio::select! { biased;
                () = changed.notified() => continue,
                message = inbound.recv() => message,
            }
        };
        let Some(message) = message else { break };
        let projection = output_message(
            message,
            serve.clone(),
            outbound.clone(),
            state.clone(),
            stopping.clone(),
        );
        tokio::pin!(projection);
        let mut projected = false;
        let mut withdrawal = None;
        while !projected || withdrawal.is_some() {
            tokio::select! { biased;
                permit = outbound.reserve(), if withdrawal.is_some() => {
                    permit.map_err(|_| "ACP output closed")?
                        .send(withdrawal.take().expect("pending withdrawal"));
                },
                result = &mut projection, if !projected => { result?; projected = true; },
                message = inbound.recv(), if !closed => match message {
                    Some(Message::Request(request)) => {
                        if withdrawal.is_some() {
                            approval_response(&serve, request.id, "unavailable").await?;
                        } else {
                            permission_request(&state, &serve, &outbound, request).await?;
                        }
                    },
                    Some(Message::Notification(note)) if note.method == "$/cancel_request" => {
                        if let Ok(params) = parse::<wire::CancelRequest>(note.params)
                            && let Some(cancel) = withdraw_permission(&state, Some(&params.request_id), true)
                        {
                            // Admit no new permission until this withdrawal drains.
                            withdrawal = Some(cancel);
                        }
                    },
                    Some(message) if pending.len() < crate::cli::serve::OUTBOUND_CAPACITY => pending.push_back(message),
                    Some(_) => return Err("ACP projection queue full; check Session history".into()),
                    None => closed = true,
                },
            }
        }
    }

    let completed = state
        .lock()
        .expect("translator state")
        .active
        .as_ref()
        .filter(|active| active.completion.is_some())
        .map(|active| active.generation);
    if let Some(prompt) = completed {
        finish(&state, &serve, &outbound, prompt, true).await?;
    }
    Ok(())
}

async fn recover(
    state: &Shared,
    serve: &mpsc::Sender<ClientMessage>,
    outbound: &mpsc::Sender<Value>,
    prompt: u64,
    params: EventsSubscribeParams,
) -> Result<(), String> {
    if !state
        .lock()
        .expect("translator state")
        .active
        .as_ref()
        .is_some_and(|a| a.generation == prompt)
    {
        return Ok(());
    }
    match send_request(state, serve, "events/subscribe", params, Pending::Recover).await {
        Err(_) if serve.is_closed() => finish(state, serve, outbound, prompt, true).await,
        result => result,
    }
}

struct ResponseContext<'a> {
    state: &'a Shared,
    outbound: &'a mpsc::Sender<Value>,
}

struct ActiveResponseContext<'a> {
    state: &'a Shared,
    serve: &'a mpsc::Sender<ClientMessage>,
    outbound: &'a mpsc::Sender<Value>,
    stopping: &'a tokio::sync::watch::Receiver<bool>,
}

struct LoadResponse {
    id: RequestId,
    cwd: String,
    replay: bool,
    mode: PermissionMode,
}

async fn initialize_response(id: RequestId, context: &ResponseContext<'_>) -> Result<(), String> {
    emit(context.outbound, wire::response(id, wire::initialized())).await
}

async fn new_session_response(
    id: RequestId,
    cwd: String,
    mode: PermissionMode,
    result: Value,
    context: &ResponseContext<'_>,
) -> Result<(), String> {
    let result: ThreadResult =
        serde_json::from_value(result).map_err(|_| "invalid serve thread")?;
    {
        let mut state = context.state.lock().expect("translator state");
        state.sessions.insert(
            result.thread.id,
            Session {
                sequence: 0,
                cwd,
                mode: mode.clone(),
            },
        );
        state.authenticated = true;
    }
    emit(
        context.outbound,
        wire::response(id, {
            let mut settings = wire::mode_state(&mode);
            settings["sessionId"] = json!(result.thread.id);
            settings
        }),
    )
    .await
}

async fn load_session_response(
    response: LoadResponse,
    result: Value,
    context: &ResponseContext<'_>,
) -> Result<(), String> {
    let result: ThreadResumeResult =
        serde_json::from_value(result).map_err(|_| "invalid serve history")?;
    if response.replay {
        for update in history_updates(&result, &response.cwd) {
            emit_answer(context.outbound, update).await?;
        }
    }
    {
        let mut state = context.state.lock().expect("translator state");
        state.sessions.insert(
            result.thread.id,
            Session {
                sequence: 0,
                cwd: response.cwd,
                mode: response.mode.clone(),
            },
        );
        state.authenticated = true;
    }
    emit(
        context.outbound,
        wire::response(response.id, wire::mode_state(&response.mode)),
    )
    .await
}

async fn mode_response(
    id: RequestId,
    session: SessionId,
    config: bool,
    result: Value,
    context: &ResponseContext<'_>,
) -> Result<(), String> {
    let result: ThreadSettingsUpdateResult =
        serde_json::from_value(result).map_err(|_| "invalid serve settings")?;
    context
        .state
        .lock()
        .expect("translator state")
        .sessions
        .get_mut(&session)
        .ok_or("no loaded ACP Session")?
        .mode = result.permission_mode.clone();
    for update in wire::mode_updates(session, &result.permission_mode) {
        emit(context.outbound, update).await?;
    }
    let response = if config {
        json!({"configOptions":wire::mode_state(&result.permission_mode)["configOptions"]})
    } else {
        json!({})
    };
    emit(context.outbound, wire::response(id, response)).await
}

async fn close_session_response(
    id: RequestId,
    session: SessionId,
    context: &ResponseContext<'_>,
) -> Result<(), String> {
    {
        let mut state = context.state.lock().expect("translator state");
        state.sessions.remove(&session);
        state.closing.remove(&session);
    }
    emit(context.outbound, wire::response(id, json!({}))).await
}

async fn list_sessions_response(
    id: RequestId,
    result: Value,
    context: &ResponseContext<'_>,
) -> Result<(), String> {
    let result: ThreadListResult =
        serde_json::from_value(result).map_err(|_| "invalid serve Session list")?;
    let sessions: Vec<_> = result
        .data
        .into_iter()
        .map(|item| {
            let mut session = json!({
                "sessionId": item.id, "cwd": item.cwd,
                "title": crate::presentation::escape_terminal(&item.title),
            });
            if item.updated_at != "Unknown" {
                session["updatedAt"] = json!(item.updated_at);
            }
            session
        })
        .collect();
    emit(
        context.outbound,
        wire::response(
            id,
            json!({"sessions": sessions, "nextCursor": result.next_cursor}),
        ),
    )
    .await
}

async fn start_turn_response(
    result: Value,
    context: &ActiveResponseContext<'_>,
) -> Result<(), String> {
    let result: TurnResult = serde_json::from_value(result).map_err(|_| "invalid serve turn")?;
    let cancel = {
        let mut state = context.state.lock().expect("translator state");
        let active = state.active.as_mut().ok_or("no active ACP prompt")?;
        active.turn = Some(result.turn.id);
        (active.cancelled && !*context.stopping.borrow()).then_some(TurnInterruptParams {
            thread_id: active.session,
            turn_id: result.turn.id,
        })
    };
    if let Some(params) = cancel {
        send_request(
            context.state,
            context.serve,
            "turn/interrupt",
            params,
            Pending::Interrupt,
        )
        .await?;
    }
    let watch = {
        let state = context.state.lock().expect("translator state");
        state.active.as_ref().filter(|a| a.watch_plan).map(|a| {
            (
                a.session,
                result.turn.id,
                state.sessions.get(&a.session).map_or(0, |s| s.sequence),
            )
        })
    };
    if let Some((session, run, since)) = watch.filter(|_| !*context.stopping.borrow()) {
        send_request(
            context.state,
            context.serve,
            "events/subscribe",
            EventsSubscribeParams {
                thread_id: session,
                since,
            },
            Pending::Watch { session, run },
        )
        .await?;
    }
    Ok(())
}

async fn watch_turn_response(
    session: SessionId,
    run: RunId,
    result: Value,
    context: &ActiveResponseContext<'_>,
) -> Result<(), String> {
    if !context
        .state
        .lock()
        .expect("translator state")
        .active
        .as_ref()
        .is_some_and(|a| a.session == session && a.turn == Some(run))
    {
        return Ok(());
    }
    let page: EventsSubscribeResult =
        serde_json::from_value(result).map_err(|_| "invalid serve events")?;
    for event in &page.events {
        if let Some(update) = project_plan(context.state, session, event)? {
            emit(context.outbound, update).await?;
        }
    }
    if let Some(since) = page.next_since.filter(|_| !*context.stopping.borrow()) {
        send_request(
            context.state,
            context.serve,
            "events/subscribe",
            EventsSubscribeParams {
                thread_id: session,
                since,
            },
            Pending::Watch { session, run },
        )
        .await?;
    }
    Ok(())
}

async fn recover_turn_response(
    result: Value,
    context: &ActiveResponseContext<'_>,
) -> Result<(), String> {
    let page: EventsSubscribeResult =
        serde_json::from_value(result).map_err(|_| "invalid serve events")?;
    let Some((session, run, prompt)) = ({
        let state = context.state.lock().expect("translator state");
        state
            .active
            .as_ref()
            .and_then(|active| Some((active.session, active.turn?, active.generation)))
    }) else {
        return Ok(());
    };
    for event in &page.events {
        if let Some(update) = project_plan(context.state, session, event)? {
            emit(context.outbound, update).await?;
        }
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
                let mut payload = event.payload.clone();
                if let Some(fields) = payload.as_object_mut() {
                    fields.remove("checkpoint");
                }
                let observation: arany::ToolObservation = serde_json::from_value(payload)
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
            && let Some(update) = project(context.state, session, run, item, started)
        {
            emit_answer(context.outbound, update).await?;
        }
    }
    if let Some(last) = page.events.last()
        && let Some(loaded) = context
            .state
            .lock()
            .expect("translator state")
            .sessions
            .get_mut(&session)
    {
        loaded.sequence = last.sequence;
    }
    if let Some(since) = page.next_since.filter(|_| !*context.stopping.borrow()) {
        recover(
            context.state,
            context.serve,
            context.outbound,
            prompt,
            EventsSubscribeParams {
                thread_id: session,
                since,
            },
        )
        .await?;
    } else {
        finish(
            context.state,
            context.serve,
            context.outbound,
            prompt,
            page.next_since.is_some(),
        )
        .await?;
    }
    Ok(())
}

async fn output_message(
    message: Message,
    serve: mpsc::Sender<ClientMessage>,
    outbound: mpsc::Sender<Value>,
    state: Shared,
    stopping: tokio::sync::watch::Receiver<bool>,
) -> Result<(), String> {
    match message {
        Message::Response(reply) => {
            let RequestId::Integer(id) = reply.id else {
                return Ok(());
            };
            let pending = state.lock().expect("translator state").pending.remove(&id);
            let context = ResponseContext {
                state: &state,
                outbound: &outbound,
            };
            let active_context = ActiveResponseContext {
                state: &state,
                serve: &serve,
                outbound: &outbound,
                stopping: &stopping,
            };
            match pending {
                Some(Pending::Initialize(id)) => initialize_response(id, &context).await?,
                Some(Pending::New(id, cwd, mode)) => {
                    new_session_response(id, cwd, mode, reply.result, &context).await?
                }
                Some(Pending::Load {
                    id,
                    cwd,
                    replay,
                    mode,
                }) => {
                    load_session_response(
                        LoadResponse {
                            id,
                            cwd,
                            replay,
                            mode,
                        },
                        reply.result,
                        &context,
                    )
                    .await?;
                }
                Some(Pending::Mode {
                    id,
                    session,
                    config,
                }) => {
                    mode_response(id, session, config, reply.result, &context).await?;
                }
                Some(Pending::Close(id, session)) => {
                    close_session_response(id, session, &context).await?
                }
                Some(Pending::List(id)) => {
                    list_sessions_response(id, reply.result, &context).await?
                }
                Some(Pending::Start) => start_turn_response(reply.result, &active_context).await?,
                Some(Pending::Watch { session, run }) => {
                    watch_turn_response(session, run, reply.result, &active_context).await?
                }
                Some(Pending::Recover) => {
                    recover_turn_response(reply.result, &active_context).await?
                }
                _ => (),
            }
        }
        Message::Error(reply) => {
            let Some(RequestId::Integer(id)) = reply.id else {
                return Ok(());
            };
            let pending = state.lock().expect("translator state").pending.remove(&id);
            match pending {
                Some(
                    Pending::Initialize(id)
                    | Pending::New(id, ..)
                    | Pending::Load { id, .. }
                    | Pending::Mode { id, .. }
                    | Pending::List(id),
                ) => emit(&outbound, remap(id, reply.error)).await?,
                Some(Pending::Close(id, session)) => {
                    state
                        .lock()
                        .expect("translator state")
                        .closing
                        .remove(&session);
                    emit(&outbound, remap(id, reply.error)).await?
                }
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
                    if let Some(close) = active.close {
                        unload(&state, &serve, close, active.session).await?;
                    }
                }
                Some(Pending::Recover) => {
                    eprintln!(
                        "Arany could not reconcile all committed ACP updates; check Session history"
                    );
                    let prompt = state
                        .lock()
                        .expect("translator state")
                        .active
                        .as_ref()
                        .map(|a| a.generation);
                    if let Some(prompt) = prompt {
                        finish(&state, &serve, &outbound, prompt, true).await?;
                    }
                }
                Some(Pending::Watch { session, run }) => {
                    if let Some(active) = &mut state.lock().expect("translator state").active
                        && active.session == session
                        && active.turn == Some(run)
                    {
                        active.resync = true;
                    }
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
                    let recover = active.watch_plan
                        || active.resync
                        || (!active.answered && result.turn.status == TurnStatus::Completed);
                    active.completion = Some(result.turn);
                    let prompt = active.generation;
                    Some((
                        prompt,
                        recover.then_some(
                            state
                                .sessions
                                .get(&result.thread_id)
                                .map_or(0, |s| s.sequence),
                        ),
                    ))
                } else {
                    None
                }
            };
            let Some((prompt, since)) = since else {
                return Ok(());
            };
            if let Some(withdrawal) = withdraw_permission(&state, None, true) {
                emit(&outbound, withdrawal).await?;
            }
            if let Some(since) = since.filter(|_| !*stopping.borrow()) {
                recover(
                    &state,
                    &serve,
                    &outbound,
                    prompt,
                    EventsSubscribeParams {
                        thread_id: result.thread_id,
                        since,
                    },
                )
                .await?;
            } else {
                finish(&state, &serve, &outbound, prompt, since.is_some()).await?;
            }
        }
        Message::Notification(note) if note.method == "events/resync" => {
            if let Some(active) = &mut state.lock().expect("translator state").active {
                active.resync = true;
            }
        }
        Message::Notification(note) if note.method == "events/committed" => {
            let params: EventsCommittedNotification =
                serde_json::from_value(note.params.unwrap_or(Value::Null))
                    .map_err(|_| "invalid serve event")?;
            if let Some(update) = project_plan(&state, params.thread_id, &params.event)? {
                emit(&outbound, update).await?;
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
        Message::Request(request) => permission_request(&state, &serve, &outbound, request).await?,
        _ => (),
    }
    Ok(())
}

fn project_plan(
    state: &Shared,
    session: SessionId,
    event: &arany_cli::protocol::EventLine,
) -> Result<Option<Value>, String> {
    if event.kind != "GoalPlanContractRecorded" {
        return Ok(None);
    }
    let mut state = state.lock().expect("translator state");
    let Some(active) = state.active.as_mut().filter(|a| {
        a.session == session
            && a.turn
                .is_some_and(|run| event.run_id.as_deref() == Some(run.to_string().as_str()))
            && a.plan
                .as_ref()
                .is_none_or(|(sequence, _)| event.sequence > *sequence)
    }) else {
        return Ok(None);
    };
    let contract: arany::GoalPlanContract =
        serde_json::from_value(event.payload["contract"].clone())
            .map_err(|_| "invalid committed Goal plan")?;
    let criterion = match &contract.success_criterion {
        arany::GoalSuccessCriterion::ToolCallSucceeded { call } => {
            format!(
                "Verify successful planned Tool: {}",
                serde_json::to_string(call).expect("Tool call")
            )
        }
        arany::GoalSuccessCriterion::WorkspaceFileDigest { path, sha256 } => {
            format!("Verify file {path} has SHA-256 {sha256}")
        }
        arany::GoalSuccessCriterion::WorkspaceFileContent { path, .. } => {
            format!("Verify file {path} has the exact planned content")
        }
    };
    let plan = json!({"jsonrpc":"2.0","method":"session/update","params":{
        "sessionId":session,"update":{"sessionUpdate":"plan","entries":[
            {"content":tools::bounded(&contract.mission, 2048, session),"priority":"medium","status":"in_progress"},
            {"content":tools::bounded(&criterion, 2048, session),"priority":"medium","status":"pending"}
        ]}
    }});
    active.plan = Some((event.sequence, plan.clone()));
    Ok(Some(plan))
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

async fn expire_close(
    state: Shared,
    serve: mpsc::Sender<ClientMessage>,
    output: mpsc::Sender<Value>,
    prompt: u64,
) {
    tokio::time::sleep(crate::cli::run_outcome::SHUTDOWN_GRACE).await;
    let active = {
        let mut state = state.lock().expect("translator state");
        if !state
            .active
            .as_ref()
            .is_some_and(|a| a.generation == prompt && a.close.is_some())
        {
            return;
        }
        state
            .pending
            .retain(|_, pending| !matches!(pending, Pending::Start | Pending::Recover));
        state
            .active
            .take()
            .expect("expired close has an active prompt")
    };
    if let Some(id) = active.close {
        let _ = unload(&state, &serve, id, active.session).await;
    }
    let response = if active.uncertain > 0 {
        wire::error(
            Some(active.id),
            -32603,
            tools::UNKNOWN_ACP_TOOL_OUTCOME,
            "effectUncertain",
        )
    } else {
        wire::response(
            active.id,
            json!({"stopReason":"cancelled","_meta":{"arany.incompleteUpdates":true}}),
        )
    };
    let _ = emit(&output, response).await;
}

async fn unload(
    state: &Shared,
    serve: &mpsc::Sender<ClientMessage>,
    id: RequestId,
    session: SessionId,
) -> Result<(), String> {
    state
        .lock()
        .expect("translator state")
        .closing
        .insert(session);
    send_request(
        state,
        serve,
        "thread/unsubscribe",
        ThreadUnsubscribeParams { thread_id: session },
        Pending::Close(id, session),
    )
    .await
}

async fn finish(
    state: &Shared,
    serve: &mpsc::Sender<ClientMessage>,
    output: &mpsc::Sender<Value>,
    prompt: u64,
    incomplete: bool,
) -> Result<(), String> {
    let active = {
        let mut state = state.lock().expect("translator state");
        if !state
            .active
            .as_ref()
            .is_some_and(|a| a.generation == prompt)
        {
            return Ok(());
        }
        state.active.take().expect("matched active prompt")
    };
    let close = active.close.clone();
    let session = active.session;
    if let Some(id) = close {
        match unload(state, serve, id, session).await {
            Err(_) if serve.is_closed() => {}
            result => result?,
        }
    }
    if let Some(task) = &active.close_task {
        task.abort();
    }
    let turn = active.completion.ok_or("no committed turn completion")?;
    if let Some((_, mut plan)) = active.plan {
        let status = if turn
            .goal
            .as_ref()
            .is_some_and(|g| g.status == GoalStatus::Completed)
        {
            "completed"
        } else {
            "pending"
        };
        for entry in plan["params"]["update"]["entries"]
            .as_array_mut()
            .expect("plan entries")
        {
            entry["status"] = json!(status);
        }
        emit(output, plan).await?;
    }
    let incomplete = incomplete
        || active.incomplete
        || (turn.status == TurnStatus::Completed && !active.answered);
    let mut response = match prompt_outcome(active.cancelled, active.uncertain > 0, &turn) {
        Ok(stop_reason) => wire::response(active.id, json!({ "stopReason": stop_reason })),
        Err(reason) => wire::error(
            Some(active.id),
            -32603,
            if reason == "effectUncertain" {
                tools::UNKNOWN_ACP_TOOL_OUTCOME
            } else {
                turn.error
                    .as_ref()
                    .map_or("Run stopped; check Session history", |e| e.message.as_str())
            },
            reason,
        ),
    };
    if active.uncertain > 0 && response.get("result").is_some() {
        response["result"]["_meta"]["arany.uncertainEffects"] = json!(active.uncertain);
    }
    if turn
        .goal
        .as_ref()
        .is_some_and(|goal| matches!(goal.stopped, Some(GoalStop::GoalExhausted { .. })))
        && response.get("result").is_some()
    {
        response["result"]["_meta"]["arany.budgetRefusal"] =
            json!(turn.goal.as_ref().and_then(|goal| goal.budget_refusal));
        response["result"]["_meta"]["arany.status"] =
            json!(turn.error.as_ref().map(|error| &error.message));
    }
    if incomplete && response.get("result").is_some() {
        response["result"]["_meta"]["arany.incompleteUpdates"] = json!(true);
    }
    emit(output, response).await
}

// The ACP stopReason for a committed turn, or the `data.reason` of its -32603 error.
fn prompt_outcome(
    cancelled: bool,
    uncertain_observed: bool,
    turn: &Turn,
) -> Result<&'static str, &'static str> {
    if cancelled {
        return Ok("cancelled");
    }
    if uncertain_observed {
        return Err("effectUncertain");
    }
    match turn.goal.as_ref().and_then(|goal| goal.stopped.as_ref()) {
        None if turn.status == TurnStatus::Completed
            && turn
                .goal
                .as_ref()
                .is_some_and(|goal| goal.status == GoalStatus::Completed) =>
        {
            Ok("end_turn")
        }
        None | Some(GoalStop::Failed { .. } | GoalStop::NotAdmitted { .. }) => Err("turnFailed"),
        Some(GoalStop::GoalExhausted {
            limit: GoalLimit::Tokens,
        }) => Ok("max_tokens"),
        Some(GoalStop::GoalExhausted {
            limit: GoalLimit::Calls | GoalLimit::Runs | GoalLimit::ActiveTime,
        }) => Ok("max_turn_requests"),
        Some(GoalStop::Paused | GoalStop::Cancelled) => Ok("cancelled"),
        Some(GoalStop::StoppedByHook) => Err("stoppedByHook"),
        Some(GoalStop::Uncertain) => Err("effectUncertain"),
        Some(GoalStop::Interrupted) => Err("turnInterrupted"),
    }
}

fn remap(id: RequestId, error: ErrorObject) -> Value {
    if error
        .data
        .as_ref()
        .is_some_and(|d| matches!(d.reason, Reason::MissingSession | Reason::ThreadNotLoaded))
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
    let message = if decision == "unavailable" {
        Message::Error(ErrorResponse {
            jsonrpc: Default::default(),
            id: Some(id),
            error: ErrorObject {
                code: -32603,
                message: "Approval channel unavailable".into(),
                data: None,
            },
        })
    } else {
        Message::Response(Response {
            jsonrpc: Default::default(),
            id,
            result: json!({"decision":decision}),
        })
    };
    serve
        .send(ClientMessage::Message(message))
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
        Some(outcome) if outcome["outcome"] == "selected" && outcome["optionId"] == "deny" => {
            "decline"
        }
        Some(outcome) if outcome["outcome"] == "cancelled" => "cancel",
        _ => "unavailable",
    };
    let pending = {
        let mut state = state.lock().expect("translator state");
        if state.active.as_ref().is_some_and(|a| a.cancelled) {
            return Ok(());
        }
        let pending = state.permission.take_if(|p| p.id == id && p.sent);
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
    let pending = state
        .permission
        .take_if(|p| id.is_none_or(|id| *id == p.serve_id) && (force || cancelled))?;
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
        return approval_response(serve, request.id, "unavailable").await;
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
        if !valid
            || state.permission.is_some()
            || intent.call.is_unguarded()
            || intent.call.is_host()
        {
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
                state.permission = Some(Permission {
                    id,
                    serve_id: request.id.clone(),
                    item_id: params.item_id.clone(),
                    sent: false,
                });
                Some(message)
            }
        }
    };
    let Some(message) = prepared else {
        return approval_response(serve, request.id, "unavailable").await;
    };
    let delivered = {
        let mut state = state.lock().expect("translator state");
        let cancelled = state.active.as_ref().is_none_or(|a| a.cancelled);
        let live = state
            .permission
            .as_ref()
            .is_some_and(|p| p.serve_id == request.id);
        if cancelled || !live {
            return Ok(());
        }
        if let Ok(permit) = output.try_reserve() {
            state.permission.as_mut().expect("live permission").sent = true;
            permit.send(message);
            true
        } else {
            state.permission = None;
            false
        }
    };
    if !delivered {
        approval_response(serve, request.id, "unavailable").await?;
    }
    Ok(())
}

use super::{Config, wire};
use crate::cli::serve::server::ClientMessage;
use arany::{RunId, SessionId};
use arany_cli::protocol::{methods::*, rpc::*};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;

pub(super) type Shared = Arc<Mutex<State>>;

#[derive(Default)]
pub(super) struct State {
    initialized: bool,
    authenticated: bool,
    mcp_notice: bool,
    next_id: i64,
    pending: BTreeMap<i64, Pending>,
    sessions: BTreeMap<SessionId, u64>,
    active: Option<Active>,
}

struct Active {
    id: RequestId,
    session: SessionId,
    turn: Option<RunId>,
    cancelled: bool,
    answered: bool,
    completion: Option<Turn>,
    resync: bool,
}

enum Pending {
    Initialize(RequestId),
    New(RequestId),
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
            if let Some(cancel) = cancel {
                send_request(state, serve, "turn/interrupt", cancel, Pending::Interrupt).await?;
            }
            return Ok(());
        }
        wire::Input::Response(_reply) => return Ok(()),
        wire::Input::Error(_reply) => return Ok(()),
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
            if !params.mcp_servers.is_empty() {
                let mut state = state.lock().expect("translator state");
                if !state.mcp_notice {
                    eprintln!("Arany ignores editor MCP servers.");
                    state.mcp_notice = true;
                }
            }
            let params = config.thread(params.cwd);
            send_request(state, serve, "thread/start", params, Pending::New(id)).await
        }
        "session/prompt" => {
            let Ok(params) = parse::<wire::Prompt>(request.params) else {
                return immediate(output, invalid());
            };
            let Ok(text) = params.text() else {
                return immediate(output, invalid());
            };
            let refusal = {
                let mut state = state.lock().expect("translator state");
                if !state.authenticated {
                    Some(wire::auth_required(id.clone()))
                } else if !state.sessions.contains_key(&params.session_id) {
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
                        session: params.session_id,
                        turn: None,
                        cancelled: false,
                        answered: false,
                        completion: None,
                        resync: false,
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
                    thread_id: params.session_id,
                    input: vec![UserInput::Text { text }],
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

pub(super) async fn output(
    mut inbound: mpsc::Receiver<Message>,
    serve: mpsc::Sender<ClientMessage>,
    outbound: mpsc::Sender<Value>,
    state: Shared,
    stopping: tokio::sync::watch::Receiver<bool>,
) -> Result<(), String> {
    while let Some(message) = inbound.recv().await {
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
                    Some(Pending::New(id)) => {
                        let result: ThreadResult = serde_json::from_value(reply.result)
                            .map_err(|_| "invalid serve thread")?;
                        {
                            let mut state = state.lock().expect("translator state");
                            state.sessions.insert(result.thread.id, 0);
                            state.authenticated = true;
                        }
                        emit(
                            &outbound,
                            wire::response(id, json!({"sessionId": result.thread.id})),
                        )
                        .await?;
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
                        let mut answer = None;
                        let session = {
                            let mut state = state.lock().expect("translator state");
                            let active = state.active.as_mut().ok_or("no active ACP prompt")?;
                            for event in &page.events {
                                if event.kind == "MessageCommitted"
                                    && event.run_id.as_deref()
                                        == active.turn.as_ref().map(|id| id.to_string()).as_deref()
                                    && !active.answered
                                {
                                    let text = event.payload["text"]
                                        .as_str()
                                        .ok_or("invalid committed answer")?;
                                    active.answered = true;
                                    answer = Some(wire::answer(active.session, text));
                                }
                            }
                            let session = active.session;
                            if let Some(last) = page.events.last() {
                                state.sessions.insert(session, last.sequence);
                            }
                            session
                        };
                        if let Some(answer) = answer {
                            emit_answer(&outbound, answer).await?;
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
                    Some(Pending::Initialize(id) | Pending::New(id)) => {
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
            Message::Notification(note) if note.method == "item/completed" => {
                let item: ItemCompletedNotification =
                    serde_json::from_value(note.params.unwrap_or(Value::Null))
                        .map_err(|_| "invalid serve item")?;
                let answer = {
                    let mut state = state.lock().expect("translator state");
                    state
                        .active
                        .as_mut()
                        .filter(|a| {
                            a.session == item.thread_id
                                && a.turn == Some(item.turn_id)
                                && !a.answered
                        })
                        .and_then(|active| {
                            if let Item::AgentMessage { text, .. } = item.item {
                                active.answered = true;
                                Some(wire::answer(active.session, &text))
                            } else {
                                None
                            }
                        })
                };
                if let Some(answer) = answer {
                    emit_answer(&outbound, answer).await?;
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
                        recover.then_some(*state.sessions.get(&result.thread_id).unwrap_or(&0))
                    } else {
                        None
                    }
                };
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
            Message::Request(_) => {
                let _ = serve.send(ClientMessage::Eof).await;
                return Err("approvals unavailable in ACP core text adapter".into());
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
    let incomplete = incomplete || (turn.status == TurnStatus::Completed && !active.answered);
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
        let reason = if turn.goal.as_ref().and_then(|g| g.stopped.as_deref())
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
            turn.error
                .as_ref()
                .map_or("Run stopped; check Session history", |e| e.message.as_str()),
            reason,
        )
    };
    if incomplete && response.get("result").is_some() {
        response["result"]["_meta"]["arany.incompleteUpdates"] = json!(true);
    }
    emit(output, response).await
}

fn remap(id: RequestId, error: ErrorObject) -> Value {
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
    if let Some(active) = &mut state.lock().expect("translator state").active {
        active.cancelled = true;
    }
}

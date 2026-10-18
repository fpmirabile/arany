use super::server::Fault;
use arany::{
    ApprovalInbox, Event, RunCancellation, SessionId, StateRoot, Store, ToolApproval, ToolCall,
};
use arany_cli::protocol::{methods::*, rpc::*};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{Notify, mpsc},
    time::Instant,
};

struct Pending {
    id: RequestId,
    approval: ToolApproval,
    deadline: Instant,
    sent: bool,
}

#[derive(Default)]
struct State {
    pending: Option<Pending>,
    closed: bool,
}

pub(super) struct Approvals {
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
    cancel: RunCancellation,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl Approvals {
    pub fn start(
        mut inbox: ApprovalInbox,
        mut cancel: RunCancellation,
        state_dir: PathBuf,
        thread_id: SessionId,
        outbound: mpsc::Sender<Message>,
        next_id: Arc<AtomicI64>,
        mut finished: RunCancellation,
    ) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let changed = Arc::new(Notify::new());
        let mut owner = Self {
            state: state.clone(),
            changed: changed.clone(),
            cancel: cancel.clone(),
            worker: None,
        };
        owner.worker = Some(tokio::spawn(async move {
            let mut outgoing = None;
            let mut deadline = None;
            loop {
                tokio::select! { biased;
                    () = cancel.cancelled() => break,
                    () = finished.cancelled() => break,
                    () = outbound.closed() => break,
                    () = changed.notified() => {
                        let state = state.lock().expect("approval state");
                        if state.closed { break; }
                        if state.pending.is_none() {
                            outgoing = None;
                            deadline = None;
                        }
                    }
                    () = async { tokio::time::sleep_until(deadline.expect("pending expiry")).await }, if deadline.is_some() => {
                        state.lock().expect("approval state").pending.take();
                        outgoing = None;
                        deadline = None;
                    }
                    approval = inbox.next() => {
                        let Some(approval) = approval else { break };
                        if state.lock().expect("approval state").pending.is_some() {
                            approval.decide(false);
                            continue;
                        }
                        let remaining = approval.intent.expires_at_ms.saturating_sub(now_ms());
                        if remaining == 0 { approval.decide(false); continue; }
                        let expires = Instant::now() + Duration::from_millis(remaining);
                        let message = tokio::select! { biased;
                            () = cancel.cancelled() => break,
                            () = finished.cancelled() => break,
                            () = outbound.closed() => break,
                            () = tokio::time::sleep_until(expires) => None,
                            message = approval_request(&state_dir, thread_id, &approval, &next_id) => message,
                        };
                        let Some(message) = message else { approval.decide(false); continue; };
                        let mut state = state.lock().expect("approval state");
                        if state.closed { approval.decide(false); break; }
                        state.pending = Some(Pending { id: message.id.clone(), approval, deadline: expires, sent: false });
                        outgoing = Some(Message::Request(message));
                        deadline = Some(expires);
                    }
                    permit = outbound.reserve(), if outgoing.is_some() => {
                        let Ok(permit) = permit else { break };
                        let mut state = state.lock().expect("approval state");
                        if let Some(pending) = &mut state.pending {
                            if Instant::now() < pending.deadline {
                                pending.sent = true;
                                permit.send(outgoing.take().expect("queued approval"));
                            } else {
                                state.pending.take();
                                outgoing = None;
                                deadline = None;
                            }
                        }
                    }
                }
            }
            let mut state = state.lock().expect("approval state");
            state.closed = true;
            state.pending.take();
        }));
        owner
    }

    pub fn respond(&self, message: Message) -> Result<(), Fault> {
        let (id, result) = match message {
            Message::Response(reply) => (reply.id, Some(reply.result)),
            Message::Error(reply) => (reply.id.expect("non-null response"), None),
            _ => unreachable!(),
        };
        let mut state = self.state.lock().expect("approval state");
        let Some(pending) = &state.pending else {
            return Err(Fault::state(Reason::ApprovalNotPending));
        };
        if pending.id != id
            || !pending.sent
            || Instant::now() >= pending.deadline
            || pending.approval.intent.expires_at_ms <= now_ms()
            || state.closed
        {
            return Err(Fault::state(Reason::ApprovalNotPending));
        }
        let pending = state.pending.take().expect("matched pending approval");
        let decision = result
            .map(serde_json::from_value::<ApprovalResult>)
            .transpose();
        let cancel = matches!(
            &decision,
            Ok(Some(ApprovalResult {
                decision: ApprovalDecision::Cancel
            }))
        );
        pending.approval.decide(matches!(
            &decision,
            Ok(Some(ApprovalResult {
                decision: ApprovalDecision::Accept
            }))
        ));
        if cancel {
            self.cancel.cancel();
        }
        self.changed.notify_one();
        decision.map(|_| ()).map_err(|_| Fault::params())
    }

    pub fn close(&self) {
        let mut state = self.state.lock().expect("approval state");
        state.closed = true;
        state.pending.take();
        self.changed.notify_one();
    }

    #[cfg(test)]
    pub fn empty_for_test() -> Self {
        Self {
            state: Default::default(),
            changed: Default::default(),
            cancel: RunCancellation::new(),
            worker: None,
        }
    }
}

impl Drop for Approvals {
    fn drop(&mut self) {
        self.close();
        if let Some(worker) = &self.worker {
            worker.abort();
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

async fn approval_request(
    state_dir: &std::path::Path,
    thread_id: SessionId,
    approval: &ToolApproval,
    next_id: &AtomicI64,
) -> Option<Request> {
    let store = Store::open_read_only(StateRoot::open_existing(state_dir).ok()?).ok()?;
    let events = store.load_session(thread_id).await;
    store.close().await.ok()?;
    let intent = &approval.intent;
    let started = events.ok()?.into_iter().find(|event| matches!(&event.event, Event::ToolStarted { intent: started, .. } if started == intent))?;
    let method = match intent.call {
        ToolCall::Command { .. } | ToolCall::CommandUnguarded { .. } => {
            "item/commandExecution/requestApproval"
        }
        ToolCall::Write { .. }
        | ToolCall::Edit { .. }
        | ToolCall::Mkdir { .. }
        | ToolCall::ApplyChangeset { .. } => "item/fileChange/requestApproval",
        _ => "item/tool/requestApproval",
    };
    let lines = match &approval.unguarded {
        Some(prompt) => crate::presentation::unguarded_approval_lines(intent, prompt),
        None => crate::presentation::tool_approval_lines(intent),
    };
    Some(Request {
        jsonrpc: Default::default(),
        id: RequestId::Integer(
            next_id
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .ok()?,
        ),
        method: method.into(),
        params: Some(
            serde_json::to_value(ApprovalParams {
                thread_id,
                turn_id: intent.run_id,
                item_id: intent.id.to_string(),
                started_at_ms: started.created_at_ms,
                expires_at_ms: intent.expires_at_ms.try_into().ok()?,
                intent: serde_json::to_value(intent).ok()?,
                lines,
            })
            .ok()?,
        ),
    })
}

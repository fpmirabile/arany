use arany::{
    ApprovalInbox, EffectIntent, Event, RunCancellation, SessionId, StateRoot, Store, ToolApproval,
    ToolCall, UnguardedPrompt, now_ms,
};
use arany_cli::protocol::{methods::*, rpc::*};
use serde_json::Value;
use std::{
    future::Future,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Notify, mpsc, oneshot},
    time::Instant,
};

struct Pending {
    id: RequestId,
    deadline: Instant,
    expires_ms: u64,
    sent: bool,
    decide: oneshot::Sender<bool>,
}

#[derive(Default)]
struct State {
    pending: Option<Pending>,
    closed: bool,
}

pub(super) struct Channel {
    pub outbound: mpsc::Sender<Message>,
    pub next_id: Arc<AtomicI64>,
    pub withdrawals: bool,
}

pub(super) struct Approvals {
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
    cancel: RunCancellation,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl Approvals {
    pub fn start<Lookup, Started>(
        mut inbox: ApprovalInbox,
        mut cancel: RunCancellation,
        thread_id: SessionId,
        channel: Channel,
        mut finished: RunCancellation,
        started_at: Lookup,
    ) -> Self
    where
        Lookup: Fn(EffectIntent) -> Started + Send + 'static,
        Started: Future<Output = Option<i64>> + Send,
    {
        let Channel {
            outbound,
            next_id,
            withdrawals,
        } = channel;
        let state = Arc::new(Mutex::new(State::default()));
        let changed = Arc::new(Notify::new());
        let mut owner = Self {
            state: state.clone(),
            changed: changed.clone(),
            cancel: cancel.clone(),
            worker: None,
        };
        owner.worker = Some(tokio::spawn(async move {
            let mut current: Option<ToolApproval> = None;
            let mut answer: Option<oneshot::Receiver<bool>> = None;
            let mut outgoing = None;
            let mut deadline = None;
            loop {
                tokio::select! { biased;
                    () = cancel.cancelled() => break,
                    () = finished.cancelled() => break,
                    () = outbound.closed() => break,
                    () = changed.notified() => {
                        if state.lock().expect("approval state").closed { break; }
                    }
                    allow = async { answer.as_mut().expect("answer").await }, if answer.is_some() => {
                        if let Some(approval) = current.take() {
                            approval.decide(allow.unwrap_or(false));
                        }
                        answer = None;
                        outgoing = None;
                        deadline = None;
                    }
                    () = async { current.as_mut().expect("approval").abandoned().await }, if current.is_some() => {
                        withdraw(&state, &outbound, withdrawals);
                        current = None;
                        answer = None;
                        outgoing = None;
                        deadline = None;
                    }
                    () = async { tokio::time::sleep_until(deadline.expect("pending expiry")).await }, if deadline.is_some() => {
                        withdraw(&state, &outbound, withdrawals);
                        current = None;
                        answer = None;
                        outgoing = None;
                        deadline = None;
                    }
                    approval = inbox.next() => {
                        let Some(approval) = approval else { break };
                        if current.is_some() {
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
                            started = started_at(approval.intent.clone()) => {
                                started.and_then(|started| request(thread_id, &approval, started, &next_id))
                            },
                        };
                        let Some(message) = message else { approval.decide(false); continue; };
                        let (decide, decision) = oneshot::channel();
                        let mut state = state.lock().expect("approval state");
                        if state.closed { approval.decide(false); break; }
                        state.pending = Some(Pending {
                            id: message.id.clone(),
                            deadline: expires,
                            expires_ms: approval.intent.expires_at_ms,
                            sent: false,
                            decide,
                        });
                        current = Some(approval);
                        answer = Some(decision);
                        outgoing = Some(Message::Request(message));
                        deadline = Some(expires);
                    }
                    permit = outbound.reserve(), if outgoing.is_some() => {
                        let Ok(permit) = permit else { break };
                        let mut state = state.lock().expect("approval state");
                        match (&mut state.pending, outgoing.take()) {
                            (Some(pending), Some(message)) if Instant::now() < pending.deadline => {
                                pending.sent = true;
                                permit.send(message);
                            }
                            _ => {
                                state.pending = None;
                                current = None;
                                answer = None;
                                deadline = None;
                            }
                        }
                    }
                }
            }
            let mut state = state.lock().expect("approval state");
            state.closed = true;
            let pending = state.pending.take();
            drop(state);
            notify_withdrawal(pending, &outbound, withdrawals);
        }));
        owner
    }

    pub fn respond(&self, id: RequestId, result: Option<Value>) {
        let mut state = self.state.lock().expect("approval state");
        let closed = state.closed;
        let Some(pending) = state.pending.take_if(|pending| {
            !closed
                && pending.id == id
                && pending.sent
                && Instant::now() < pending.deadline
                && pending.expires_ms > now_ms()
        }) else {
            return;
        };
        let decision = result
            .and_then(|value| serde_json::from_value::<ApprovalResult>(value).ok())
            .map(|result| result.decision);
        let _ = pending
            .decide
            .send(matches!(decision, Some(ApprovalDecision::Accept)));
        if matches!(decision, Some(ApprovalDecision::Cancel)) {
            self.cancel.cancel();
        }
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

fn withdraw(state: &Mutex<State>, output: &mpsc::Sender<Message>, enabled: bool) {
    let pending = state.lock().expect("approval state").pending.take();
    notify_withdrawal(pending, output, enabled);
}

fn notify_withdrawal(pending: Option<Pending>, output: &mpsc::Sender<Message>, enabled: bool) {
    if enabled && let Some(pending) = pending.filter(|pending| pending.sent) {
        let _ = output.try_send(Message::Notification(super::items::notification(
            "$/cancel_request",
            serde_json::json!({"requestId": pending.id}),
        )));
    }
}

pub(super) async fn started_at(
    state_dir: PathBuf,
    thread_id: SessionId,
    intent: EffectIntent,
) -> Option<i64> {
    let store = Store::open_read_only(StateRoot::open_existing(&state_dir).ok()?).ok()?;
    let events = store.load_session(thread_id).await;
    store.close().await.ok()?;
    events
        .ok()?
        .into_iter()
        .find(|event| matches!(&event.event, Event::ToolStarted { intent: started, .. } if *started == intent))
        .map(|event| event.created_at_ms)
}

fn method(call: &ToolCall) -> &'static str {
    match call {
        ToolCall::Command { .. } | ToolCall::CommandUnguarded { .. } => {
            "item/commandExecution/requestApproval"
        }
        ToolCall::Write { .. }
        | ToolCall::Edit { .. }
        | ToolCall::MultiEdit { .. }
        | ToolCall::Mkdir { .. }
        | ToolCall::ApplyChangeset { .. } => "item/fileChange/requestApproval",
        _ => "item/tool/requestApproval",
    }
}

fn lines(intent: &EffectIntent, unguarded: Option<&UnguardedPrompt>) -> Vec<String> {
    match unguarded {
        Some(prompt) => crate::presentation::unguarded_approval_lines(intent, prompt),
        None => crate::presentation::tool_approval_lines(intent),
    }
}

// Keep fetch_update for the Rust 1.88 minimum.
// `fetch_update` is deprecated on newer toolchains and `try_update` is above the MSRV.
fn next_request_id(next_id: &AtomicI64) -> Option<i64> {
    let mut current = next_id.load(Ordering::Relaxed);
    loop {
        let next = current.checked_add(1)?;
        match next_id.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Some(current),
            Err(actual) => current = actual,
        }
    }
}

fn request(
    thread_id: SessionId,
    approval: &ToolApproval,
    started_at_ms: i64,
    next_id: &AtomicI64,
) -> Option<Request> {
    let intent = &approval.intent;
    let request = Request {
        jsonrpc: Default::default(),
        id: RequestId::Integer(next_request_id(next_id)?),
        method: method(&intent.call).into(),
        params: Some(
            serde_json::to_value(ApprovalParams {
                thread_id,
                turn_id: intent.run_id,
                item_id: intent.id.to_string(),
                started_at_ms,
                expires_at_ms: intent.expires_at_ms.try_into().ok()?,
                intent: serde_json::to_value(intent).ok()?,
                lines: lines(intent, approval.unguarded.as_ref()),
            })
            .ok()?,
        ),
    };
    encode_line(&Message::Request(request.clone())).ok()?;
    Some(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arany::{AgentRunId, ApprovalMode, RunId, ToolApprovals, ToolLimits};
    use std::time::Duration;

    fn intent(call: ToolCall) -> EffectIntent {
        EffectIntent {
            host: None,
            target: arany::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: RunId::new(),
            agent_run_id: AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call,
            limits: ToolLimits::default(),
            expires_at_ms: now_ms() + 60_000,
            use_count: 1,
        }
    }

    fn read() -> ToolCall {
        ToolCall::Read {
            path: "a".into(),
            offset: None,
            limit: Some(1),
        }
    }

    fn unguarded() -> ToolCall {
        ToolCall::CommandUnguarded {
            program: "sh".into(),
            args: vec![],
            cwd: String::new(),
            justification: "reason".into(),
        }
    }

    #[test]
    fn methods_follow_the_call_family() {
        for (call, expected) in [
            (read(), "item/tool/requestApproval"),
            (unguarded(), "item/commandExecution/requestApproval"),
            (
                ToolCall::Mkdir { path: "d".into() },
                "item/fileChange/requestApproval",
            ),
        ] {
            assert_eq!(method(&call), expected);
        }
    }

    #[test]
    fn unguarded_commands_use_the_full_unguarded_warning() {
        let intent = intent(unguarded());
        let prompt = UnguardedPrompt {
            executable: "/bin/sh".into(),
            attribution: None,
            remember: true,
        };
        let warning = lines(&intent, Some(&prompt));
        assert_eq!(warning[0], "Run outside the Guard? (unguarded)");
        assert_ne!(warning, lines(&intent, None));
    }

    async fn next_request(output: &mut mpsc::Receiver<Message>) -> Request {
        match tokio::time::timeout(Duration::from_secs(5), output.recv())
            .await
            .expect("request deadline")
            .expect("request")
        {
            Message::Request(request) => request,
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_oversized_request_denies_without_being_sent() {
        let _deadline = crate::cli::serve::tests::deadline();
        let (approvals, inbox) = ToolApprovals::new(ApprovalMode::Request);
        let (outbound, mut output) = mpsc::channel(8);
        let _owner = Approvals::start(
            inbox,
            RunCancellation::new(),
            SessionId::new(),
            Channel {
                outbound,
                next_id: Arc::new(AtomicI64::new(1)),
                withdrawals: false,
            },
            RunCancellation::new(),
            |_| async { Some(1) },
        );
        let big = ToolCall::CommandUnguarded {
            program: "sh".into(),
            args: vec![],
            cwd: String::new(),
            justification: "x".repeat(MAX_LINE_BYTES),
        };
        assert!(!arany::test_support::approval::ask(&approvals, &intent(big)).await);
        assert!(output.try_recv().is_err());
    }

    #[tokio::test]
    async fn one_approval_is_pending_and_an_abandoned_ask_frees_the_slot() {
        let _deadline = crate::cli::serve::tests::deadline();
        for withdrawals in [false, true] {
            let (approvals, inbox) = ToolApprovals::new(ApprovalMode::Request);
            let (outbound, mut output) = mpsc::channel(8);
            let owner = Approvals::start(
                inbox,
                RunCancellation::new(),
                SessionId::new(),
                Channel {
                    outbound,
                    next_id: Arc::new(AtomicI64::new(1)),
                    withdrawals,
                },
                RunCancellation::new(),
                |_| async { Some(1) },
            );
            let ask = |call| {
                let approvals = approvals.clone();
                tokio::spawn(async move {
                    arany::test_support::approval::ask(&approvals, &intent(call)).await
                })
            };
            let first = ask(read());
            let request = next_request(&mut output).await;
            assert_eq!(request.id, RequestId::Integer(1));
            assert!(!ask(read()).await.expect("second ask"));
            assert!(output.try_recv().is_err());
            owner.respond(request.id, Some(serde_json::json!({"decision":"accept"})));
            assert!(first.await.expect("first ask"));

            let abandoned = ask(read());
            let request = next_request(&mut output).await;
            assert_eq!(request.id, RequestId::Integer(2));
            abandoned.abort();
            let _ = abandoned.await;
            let third = ask(read());
            if withdrawals {
                let Some(Message::Notification(note)) = output.recv().await else {
                    panic!("approval withdrawal")
                };
                assert_eq!(note.method, "$/cancel_request");
                assert_eq!(note.params.unwrap()["requestId"], 2);
            }
            assert_eq!(next_request(&mut output).await.id, RequestId::Integer(3));
            owner.respond(
                RequestId::Integer(2),
                Some(serde_json::json!({"decision":"accept"})),
            );
            owner.close();
            assert!(!third.await.expect("third ask"));
        }
    }
}

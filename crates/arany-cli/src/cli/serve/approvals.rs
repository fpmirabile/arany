use arany::{
    ApprovalInbox, ApprovalReply, EffectIntent, Event, RunCancellation, SessionId, StateRoot,
    Store, ToolApproval, ToolCall, UnguardedPrompt,
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
};
use tokio::sync::{Notify, mpsc, oneshot};

struct Pending {
    id: RequestId,
    decide: oneshot::Sender<ApprovalReply>,
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
            let mut answer: Option<oneshot::Receiver<ApprovalReply>> = None;
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
                            approval.decide(allow.unwrap_or(ApprovalReply::Unavailable));
                        }
                        answer = None;
                    }
                    () = async { current.as_mut().expect("approval").abandoned().await }, if current.is_some() => {
                        answer = None;
                        let pending = state.lock().expect("approval state").pending.take();
                        let committed = current.as_mut().expect("approval").withdrawal_committed().await;
                        if committed != Some(false) {
                            notify_withdrawal(pending, &outbound, withdrawals);
                        }
                        current = None;
                    }
                    approval = inbox.next() => {
                        let Some(mut approval) = approval else { break };
                        if current.is_some() {
                            approval.decide(ApprovalReply::Unavailable);
                            continue;
                        }
                        let intent = approval.intent.clone();
                        let message = tokio::select! { biased;
                            () = cancel.cancelled() => break,
                            () = finished.cancelled() => break,
                            () = outbound.closed() => break,
                            () = approval.abandoned() => None,
                            started = started_at(intent) => {
                                started.and_then(|started| request(thread_id, &approval, started, &next_id))
                            },
                        };
                        let Some(message) = message else { approval.decide(ApprovalReply::Unavailable); continue; };
                        let (decide, decision) = oneshot::channel();
                        let mut state = state.lock().expect("approval state");
                        if state.closed { approval.decide(ApprovalReply::Unavailable); break; }
                        let Ok(permit) = outbound.try_reserve() else {
                            approval.decide(ApprovalReply::Unavailable);
                            continue;
                        };
                        state.pending = Some(Pending {
                            id: message.id.clone(),
                            decide,
                        });
                        current = Some(approval);
                        answer = Some(decision);
                        permit.send(Message::Request(message));
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
        let Some(pending) = state.pending.take_if(|pending| !closed && pending.id == id) else {
            return;
        };
        let decision = result
            .and_then(|value| serde_json::from_value::<ApprovalResult>(value).ok())
            .map(|result| result.decision);
        if matches!(decision, Some(ApprovalDecision::Cancel)) {
            self.cancel.cancel();
        }
        let _ = pending.decide.send(match decision {
            Some(ApprovalDecision::Accept) => ApprovalReply::AllowOnce,
            Some(ApprovalDecision::Decline) => ApprovalReply::Deny,
            Some(ApprovalDecision::Cancel) | None => ApprovalReply::Unavailable,
        });
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

fn notify_withdrawal(pending: Option<Pending>, output: &mpsc::Sender<Message>, enabled: bool) {
    if enabled && let Some(pending) = pending {
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

    const WITHDRAWAL_COMMIT_WAIT: Duration = Duration::from_secs(5);

    fn intent(call: ToolCall) -> EffectIntent {
        EffectIntent {
            host: None,
            host_command: None,
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
            sensitive: false,
            inactive_rules: false,
            executable: "/bin/sh".into(),
            attribution: None,
            remember: true,
        };
        let warning = lines(&intent, Some(&prompt));
        assert_eq!(warning[0], "Run outside the Guard? (unguarded)");
        assert!(
            warning
                .iter()
                .any(|line| line.contains("opaque beyond a lexical privilege-tool check"))
        );
        assert!(
            warning
                .iter()
                .any(|line| line.contains("no controlling terminal"))
        );
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
            justification: "x".repeat(MAX_RPC_LINE_BYTES),
        };
        assert!(!arany::test_support::approval::ask(&approvals, &intent(big)).await);
        assert!(output.try_recv().is_err());
    }

    #[tokio::test]
    async fn full_or_closed_output_refuses_an_unsent_request_and_forgets_its_id() {
        let _deadline = crate::cli::serve::tests::deadline();
        for closed in [false, true] {
            let (approvals, inbox) = ToolApprovals::new(ApprovalMode::Request);
            let (outbound, mut output) = mpsc::channel(1);
            if closed {
                output.close();
            } else {
                outbound
                    .try_send(super::super::server::response(
                        RequestId::Integer(99),
                        serde_json::json!({}),
                    ))
                    .unwrap();
            }
            let owner = Approvals::start(
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
            assert!(
                !tokio::time::timeout(
                    Duration::from_secs(5),
                    arany::test_support::approval::ask(&approvals, &intent(read()))
                )
                .await
                .unwrap()
            );
            owner.respond(
                RequestId::Integer(1),
                Some(serde_json::json!({"decision":"accept"})),
            );
            if !closed {
                assert!(matches!(output.try_recv().unwrap(), Message::Response(_)));
            }
            assert!(output.try_recv().is_err());
        }
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
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(3600)).await;
            tokio::time::resume();
            assert!(!first.is_finished(), "live approval must not expire");
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

    #[tokio::test]
    async fn lifecycle_withdrawal_rejects_late_answers_before_notifying_the_client() {
        let _deadline = crate::cli::serve::tests::deadline();
        let (approvals, inbox) = ToolApprovals::new(ApprovalMode::Request);
        let (outbound, mut output) = mpsc::channel(8);
        let owner = Approvals::start(
            inbox,
            RunCancellation::new(),
            SessionId::new(),
            Channel {
                outbound,
                next_id: Arc::new(AtomicI64::new(1)),
                withdrawals: true,
            },
            RunCancellation::new(),
            |_| async { Some(1) },
        );
        let (reject, rejected) = tokio::sync::oneshot::channel();
        let (rejected_ack, rejected_acknowledged) = tokio::sync::oneshot::channel();
        let (commit, committed) = tokio::sync::oneshot::channel();
        let ask = tokio::spawn(arany::test_support::approval::delay_withdrawal(
            approvals,
            intent(read()),
            rejected,
            rejected_ack,
            committed,
        ));
        let request = next_request(&mut output).await;
        reject.send(()).unwrap();
        tokio::time::timeout(WITHDRAWAL_COMMIT_WAIT, rejected_acknowledged)
            .await
            .expect("Engine rejects late replies immediately")
            .unwrap();
        tokio::time::timeout(WITHDRAWAL_COMMIT_WAIT, async {
            loop {
                if owner
                    .state
                    .lock()
                    .expect("approval state")
                    .pending
                    .is_none()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("protocol request retires before the lifecycle commit");
        owner.respond(
            request.id.clone(),
            Some(serde_json::json!({"decision":"accept"})),
        );
        assert!(output.try_recv().is_err());
        commit.send(()).unwrap();
        let Some(Message::Notification(notification)) =
            tokio::time::timeout(WITHDRAWAL_COMMIT_WAIT, output.recv())
                .await
                .expect("committed withdrawal is sent")
        else {
            panic!("expected cancellation notification")
        };
        assert_eq!(notification.method, "$/cancel_request");
        assert_eq!(
            notification.params.unwrap()["requestId"],
            serde_json::to_value(&request.id).unwrap()
        );
        ask.await.unwrap();
    }
}

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
    decisions: Vec<ApprovalDecision>,
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
    pub approval_channel: super::server::ApprovalChannel,
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
            approval_channel,
        } = channel;
        let withdrawals = approval_channel != super::server::ApprovalChannel::Human;
        let terminal = approval_channel == super::server::ApprovalChannel::Terminal;
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
                        let _ = current.as_mut().expect("approval").withdrawal_committed().await;
                        notify_withdrawal(pending, &outbound, withdrawals, terminal).await;
                        current = None;
                    }
                    approval = inbox.next(), if !terminal || current.is_none() => {
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
                            () = changed.notified(), if terminal => break,
                            () = approval.abandoned() => None,
                            started = started_at(intent) => {
                                started.and_then(|started| request(thread_id, &approval, started, &next_id, terminal))
                            },
                        };
                        let Some(message) = message else { approval.decide(ApprovalReply::Unavailable); continue; };
                        let permit = if terminal {
                            tokio::select! { biased;
                                () = cancel.cancelled() => break,
                                () = finished.cancelled() => break,
                                () = outbound.closed() => break,
                                () = changed.notified() => break,
                                () = approval.abandoned() => continue,
                                permit = outbound.reserve() => match permit {
                                    Ok(permit) => permit,
                                    Err(_) => break,
                                },
                            }
                        } else {
                            let Ok(permit) = outbound.try_reserve() else {
                                approval.decide(ApprovalReply::Unavailable);
                                continue;
                            };
                            permit
                        };
                        let (decide, decision) = oneshot::channel();
                        let mut state = state.lock().expect("approval state");
                        if state.closed { approval.decide(ApprovalReply::Unavailable); break; }
                        state.pending = Some(Pending {
                            id: message.id.clone(),
                            decisions: decisions(&approval, terminal),
                            decide,
                        });
                        current = Some(approval);
                        answer = Some(decision);
                        permit.send(Message::Request(message));
                    }
                }
            }
            let pending = {
                let mut state = state.lock().expect("approval state");
                state.closed = true;
                state.pending.take()
            };
            notify_withdrawal(pending, &outbound, withdrawals, terminal).await;
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
            .map(|result| result.decision)
            .filter(|decision| pending.decisions.contains(decision));
        if matches!(decision, Some(ApprovalDecision::Cancel)) {
            self.cancel.cancel();
        }
        let _ = pending.decide.send(match decision {
            Some(ApprovalDecision::Accept) => ApprovalReply::AllowOnce,
            Some(ApprovalDecision::AcceptForSession) => ApprovalReply::AllowForSession,
            Some(ApprovalDecision::AllowPrefixInFolder) => ApprovalReply::AllowPrefixInFolder,
            Some(ApprovalDecision::Decline) => ApprovalReply::Deny,
            Some(ApprovalDecision::Cancel | ApprovalDecision::Unavailable) | None => {
                ApprovalReply::Unavailable
            }
        });
    }

    pub fn close(&self) {
        let mut state = self.state.lock().expect("approval state");
        state.closed = true;
        state.pending.take();
        self.changed.notify_one();
    }

    pub fn closed() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                pending: None,
                closed: true,
            })),
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

async fn notify_withdrawal(
    pending: Option<Pending>,
    output: &mpsc::Sender<Message>,
    enabled: bool,
    reliable: bool,
) {
    if enabled && let Some(pending) = pending {
        let message = Message::Notification(super::items::notification(
            CANCEL_REQUEST_METHOD,
            CancelRequestParams {
                request_id: pending.id,
            },
        ));
        if reliable {
            let _ = output.send(message).await;
        } else {
            let _ = output.try_send(message);
        }
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
    let intent = arany::ToolIntentRecord::from(intent);
    events
        .ok()?
        .into_iter()
        .find(|event| matches!(&event.event, Event::ToolProposed { intent: started, .. } if *started == intent))
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
        | ToolCall::Delete { .. }
        | ToolCall::Move { .. }
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

fn decisions(approval: &ToolApproval, terminal: bool) -> Vec<ApprovalDecision> {
    let mut decisions = crate::presentation::tool_approval_choices(approval)
        .into_iter()
        .filter_map(|choice| match choice.reply() {
            ApprovalReply::AllowOnce => Some(ApprovalDecision::Accept),
            ApprovalReply::AllowForSession => Some(ApprovalDecision::AcceptForSession),
            ApprovalReply::AllowPrefixInFolder if terminal => {
                Some(ApprovalDecision::AllowPrefixInFolder)
            }
            ApprovalReply::Deny => Some(ApprovalDecision::Decline),
            ApprovalReply::AllowPrefixInFolder | ApprovalReply::Unavailable => None,
        })
        .collect::<Vec<_>>();
    decisions.dedup();
    decisions.extend([ApprovalDecision::Cancel, ApprovalDecision::Unavailable]);
    decisions
}

fn request(
    thread_id: SessionId,
    approval: &ToolApproval,
    started_at_ms: i64,
    next_id: &AtomicI64,
    terminal: bool,
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
                allow_auto_edits: terminal && approval.allow_auto_edits,
                available_decisions: {
                    let offered = decisions(approval, terminal);
                    (offered.contains(&ApprovalDecision::AcceptForSession)
                        || offered.contains(&ApprovalDecision::AllowPrefixInFolder))
                    .then(|| {
                        offered
                            .into_iter()
                            .filter(|decision| *decision != ApprovalDecision::Unavailable)
                            .collect()
                    })
                },
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
            wrap: None,
            rewrite: Vec::new(),
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
            prefix_rule: None,
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
            prefix: false,
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
    async fn typed_prefix_candidates_are_offered_only_to_the_attached_human_channel() {
        let _deadline = crate::cli::serve::tests::deadline();
        for terminal in [false, true] {
            let (approvals, mut inbox) = ToolApprovals::new(ApprovalMode::Request);
            let intent = intent(read());
            let (allowed, ()) = tokio::join!(
                arany::test_support::approval::ask(&approvals, &intent),
                async {
                    let mut approval = inbox.next().await.unwrap();
                    approval.remember_prefix = Some(arany::permissions::TypedGitPrefix::Commit);
                    let request =
                        request(SessionId::new(), &approval, 1, &AtomicI64::new(1), terminal)
                            .unwrap();
                    let params: ApprovalParams =
                        serde_json::from_value(request.params.unwrap()).unwrap();
                    assert_eq!(
                        params
                            .available_decisions
                            .unwrap_or_default()
                            .contains(&ApprovalDecision::AllowPrefixInFolder),
                        terminal
                    );
                    assert_eq!(
                        decisions(&approval, terminal)
                            .contains(&ApprovalDecision::AllowPrefixInFolder),
                        terminal
                    );
                    approval.decide(ApprovalReply::Deny);
                }
            );
            assert!(!allowed);
        }
    }

    #[tokio::test]
    async fn terminal_client_retains_the_rendered_choices_and_unguarded_focus() {
        for (call, prefix, host) in [
            (
                ToolCall::Mkdir {
                    path: "directory".into(),
                },
                None,
                false,
            ),
            (
                ToolCall::GitCommit {
                    message: "Update files".into(),
                    paths: vec!["file.txt".into()],
                    staged: false,
                },
                Some(arany::permissions::TypedGitPrefix::Commit),
                false,
            ),
            (
                ToolCall::CommandUnguarded {
                    program: "make".into(),
                    args: Vec::new(),
                    cwd: String::new(),
                    justification: "Build".into(),
                    prefix_rule: Some(vec!["make".into()]),
                },
                None,
                true,
            ),
        ] {
            let (approvals, mut inbox) = ToolApprovals::new(ApprovalMode::Request);
            let intent = intent(call);
            let (_, ()) = tokio::join!(
                arany::test_support::approval::ask(&approvals, &intent),
                async {
                    let mut approval = inbox.next().await.unwrap();
                    approval.remember_prefix = prefix;
                    approval.allow_auto_edits = !host;
                    if host {
                        approval.unguarded = Some(UnguardedPrompt {
                            executable: "/bin/make".into(),
                            attribution: None,
                            remember: true,
                            prefix: true,
                            sensitive: false,
                            inactive_rules: false,
                        });
                    }
                    let request =
                        request(SessionId::new(), &approval, 1, &AtomicI64::new(1), true).unwrap();
                    let received =
                        crate::cli::serve::embedded::Approval::from_request(request).unwrap();
                    assert_eq!(received.prompt.intent_id, approval.intent.id);
                    assert_eq!(
                        received.prompt.lines,
                        lines(&approval.intent, approval.unguarded.as_ref())
                    );
                    assert_eq!(
                        received.prompt.choices,
                        crate::presentation::tool_approval_choices(&approval)
                    );
                    assert_eq!(received.prompt.unguarded, host);
                    approval.decide(ApprovalReply::Deny);
                }
            );
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
                approval_channel: super::super::server::ApprovalChannel::Human,
            },
            RunCancellation::new(),
            |_| async { Some(1) },
        );
        let big = ToolCall::CommandUnguarded {
            program: "sh".into(),
            args: vec![],
            cwd: String::new(),
            justification: "x".repeat(MAX_RPC_LINE_BYTES),
            prefix_rule: None,
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
                    approval_channel: super::super::server::ApprovalChannel::Human,
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

    #[derive(Clone, Copy)]
    enum CapacityEnding {
        Human,
        Cancel,
        Abandon,
        Shutdown,
        Finished,
        OutputClosed,
    }

    struct CapacityAsk {
        approvals: ToolApprovals,
        owner: Approvals,
        output: mpsc::Receiver<Message>,
        asked: tokio::task::JoinHandle<bool>,
        cancel: RunCancellation,
        finished: RunCancellation,
    }

    impl CapacityAsk {
        const OUTPUT_CAPACITY: usize = 1;

        fn start() -> Self {
            let (approvals, inbox) = ToolApprovals::new(ApprovalMode::Request);
            let (outbound, output) = mpsc::channel(Self::OUTPUT_CAPACITY);
            outbound
                .try_send(super::super::server::response(
                    RequestId::Integer(99),
                    serde_json::json!({}),
                ))
                .unwrap();
            let cancel = RunCancellation::new();
            let finished = RunCancellation::new();
            let owner = Approvals::start(
                inbox,
                cancel.clone(),
                SessionId::new(),
                Channel {
                    outbound,
                    next_id: Arc::new(AtomicI64::new(1)),
                    approval_channel: super::super::server::ApprovalChannel::Terminal,
                },
                finished.clone(),
                |_| async { Some(1) },
            );
            let asking = approvals.clone();
            let asked = tokio::spawn(async move {
                arany::test_support::approval::ask(&asking, &intent(read())).await
            });
            Self {
                approvals,
                owner,
                output,
                asked,
                cancel,
                finished,
            }
        }

        async fn finish(self, ending: CapacityEnding) {
            match ending {
                CapacityEnding::Human => self.answer().await,
                CapacityEnding::Abandon => self.abandon().await,
                stop => self.stop(stop).await,
            }
        }

        async fn answer(mut self) {
            assert!(matches!(
                self.output.recv().await,
                Some(Message::Response(_))
            ));
            let request = next_request(&mut self.output).await;
            assert!(
                tokio::time::timeout(WITHDRAWAL_COMMIT_WAIT, &mut self.asked)
                    .await
                    .is_err(),
                "delivery alone is not a human verdict"
            );
            self.owner
                .respond(request.id, Some(serde_json::json!({"decision":"accept"})));
            assert!(self.asked.await.unwrap());
        }

        async fn abandon(mut self) {
            self.asked.abort();
            let _ = self.asked.await;
            assert!(matches!(
                self.output.recv().await,
                Some(Message::Response(_))
            ));
            let asking = self.approvals.clone();
            let fresh = tokio::spawn(async move {
                arany::test_support::approval::ask(&asking, &intent(read())).await
            });
            let request = next_request(&mut self.output).await;
            assert_eq!(
                request.id,
                RequestId::Integer(2),
                "the abandoned unsent Ask must not be delivered"
            );
            self.owner
                .respond(request.id, Some(serde_json::json!({"decision":"accept"})));
            assert!(fresh.await.unwrap());
        }

        async fn stop(mut self, stop: CapacityEnding) {
            match stop {
                CapacityEnding::Cancel => {
                    self.cancel.cancel();
                }
                CapacityEnding::Shutdown => self.owner.close(),
                CapacityEnding::Finished => {
                    self.finished.cancel();
                }
                CapacityEnding::OutputClosed => self.output.close(),
                CapacityEnding::Human | CapacityEnding::Abandon => {
                    unreachable!("separate delivery paths")
                }
            }
            assert!(!self.asked.await.unwrap());
            assert!(matches!(
                self.output.recv().await,
                Some(Message::Response(_))
            ));
            assert!(
                self.output.try_recv().is_err(),
                "no request is delivered after its lifecycle ends"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_full_output_waits_for_delivery_or_lifecycle_without_inventing_a_verdict() {
        let _deadline = crate::cli::serve::tests::deadline();
        for ending in [
            CapacityEnding::Human,
            CapacityEnding::Cancel,
            CapacityEnding::Abandon,
            CapacityEnding::Shutdown,
            CapacityEnding::Finished,
            CapacityEnding::OutputClosed,
        ] {
            let mut pending = CapacityAsk::start();
            assert!(
                tokio::time::timeout(WITHDRAWAL_COMMIT_WAIT, &mut pending.asked)
                    .await
                    .is_err(),
                "a full terminal queue must not resolve the live Ask"
            );
            assert!(
                pending.owner.state.lock().unwrap().pending.is_none(),
                "an unsent Ask has no published decision slot"
            );
            pending.finish(ending).await;
        }
    }

    #[tokio::test]
    async fn one_approval_is_pending_and_an_abandoned_ask_frees_the_slot() {
        let _deadline = crate::cli::serve::tests::deadline();
        use super::super::server::ApprovalChannel;
        for approval_channel in [
            ApprovalChannel::Human,
            ApprovalChannel::Restricted,
            ApprovalChannel::Terminal,
        ] {
            let withdrawals = approval_channel != ApprovalChannel::Human;
            let (approvals, inbox) = ToolApprovals::new(ApprovalMode::Request);
            let (outbound, mut output) = mpsc::channel(8);
            let owner = Approvals::start(
                inbox,
                RunCancellation::new(),
                SessionId::new(),
                Channel {
                    outbound,
                    next_id: Arc::new(AtomicI64::new(1)),
                    approval_channel,
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
            let abandoned_id = if approval_channel == ApprovalChannel::Terminal {
                let mut second = ask(read());
                tokio::time::pause();
                assert!(
                    tokio::time::timeout(Duration::from_secs(1), &mut second)
                        .await
                        .is_err(),
                    "a second terminal ask waits for the open dialog"
                );
                tokio::time::resume();
                owner.respond(request.id, Some(serde_json::json!({"decision":"accept"})));
                assert!(first.await.expect("first ask"));
                let queued = next_request(&mut output).await;
                assert_eq!(queued.id, RequestId::Integer(2));
                owner.respond(queued.id, Some(serde_json::json!({"decision":"decline"})));
                assert!(!second.await.expect("second ask"));
                3
            } else {
                assert!(!ask(read()).await.expect("second ask"));
                assert!(output.try_recv().is_err());
                owner.respond(request.id, Some(serde_json::json!({"decision":"accept"})));
                assert!(first.await.expect("first ask"));
                2
            };

            let abandoned = ask(read());
            let request = next_request(&mut output).await;
            assert_eq!(request.id, RequestId::Integer(abandoned_id));
            abandoned.abort();
            let _ = abandoned.await;
            let third = ask(read());
            if withdrawals {
                let Some(Message::Notification(note)) = output.recv().await else {
                    panic!("approval withdrawal")
                };
                assert_eq!(note.method, "$/cancel_request");
                assert_eq!(note.params.unwrap()["requestId"], abandoned_id);
            }
            assert_eq!(
                next_request(&mut output).await.id,
                RequestId::Integer(abandoned_id + 1)
            );
            owner.respond(
                RequestId::Integer(abandoned_id),
                Some(serde_json::json!({"decision":"accept"})),
            );
            owner.close();
            assert!(!third.await.expect("third ask"));
        }
    }

    #[tokio::test]
    async fn lifecycle_withdrawal_rejects_late_answers_before_notifying_the_client() {
        let _deadline = crate::cli::serve::tests::deadline();
        for commit in [false, true] {
            lifecycle_withdrawal_case(commit).await;
        }
    }

    async fn lifecycle_withdrawal_case(commit: bool) {
        let (approvals, inbox) = ToolApprovals::new(ApprovalMode::Request);
        let (outbound, mut output) = mpsc::channel(8);
        let owner = Approvals::start(
            inbox,
            RunCancellation::new(),
            SessionId::new(),
            Channel {
                outbound,
                next_id: Arc::new(AtomicI64::new(1)),
                approval_channel: super::super::server::ApprovalChannel::Restricted,
            },
            RunCancellation::new(),
            |_| async { Some(1) },
        );
        let (reject, rejected) = tokio::sync::oneshot::channel();
        let (rejected_ack, rejected_acknowledged) = tokio::sync::oneshot::channel();
        let (commit_signal, committed) = tokio::sync::oneshot::channel();
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
        if commit {
            commit_signal.send(()).unwrap();
        } else {
            drop(commit_signal);
        }
        let Some(Message::Notification(notification)) =
            tokio::time::timeout(WITHDRAWAL_COMMIT_WAIT, output.recv())
                .await
                .expect("ended withdrawal is sent")
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

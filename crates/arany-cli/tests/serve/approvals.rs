use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Executor {
    Tool,
    Host,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ApprovalCase {
    Accept,
    Decline,
    Cancel,
    Interrupt,
    Eof,
    LateAccept,
    Duplicate,
    Invalid,
    ClientError,
    Unavailable,
    Backpressure,
    BackpressureInterrupt,
    Undelivered,
    Session,
    SessionBusyResume,
    Remember,
}

#[cfg(unix)]
#[tokio::test]
async fn approval_accept_dispatches_the_exact_journaled_intent_once() {
    let _deadline = deadline();
    approval_scenario(ApprovalCase::Accept, Executor::Tool).await;
}

#[cfg(unix)]
#[tokio::test]
async fn approval_decline_invalid_and_client_error_deny() {
    let _deadline = deadline();
    for case in [
        ApprovalCase::Decline,
        ApprovalCase::Invalid,
        ApprovalCase::ClientError,
        ApprovalCase::Unavailable,
    ] {
        approval_scenario(case, Executor::Tool).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn approval_cancel_interrupt_and_eof_deny() {
    let _deadline = deadline();
    for case in [
        ApprovalCase::Cancel,
        ApprovalCase::Interrupt,
        ApprovalCase::Eof,
    ] {
        approval_scenario(case, Executor::Tool).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn approval_remains_answerable_after_the_former_deadline() {
    let _deadline = deadline();
    approval_scenario(ApprovalCase::LateAccept, Executor::Tool).await;
}

#[cfg(unix)]
#[tokio::test]
async fn approval_duplicate_and_unknown_responses_cannot_resolve_another_intent() {
    let _deadline = deadline();
    approval_scenario(ApprovalCase::Duplicate, Executor::Tool).await;
}

#[cfg(unix)]
#[tokio::test]
async fn approval_cancel_journals_while_outbound_is_full() {
    let _deadline = deadline();
    for case in [
        ApprovalCase::Backpressure,
        ApprovalCase::BackpressureInterrupt,
    ] {
        approval_scenario(case, Executor::Tool).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn approval_never_delivered_is_unavailable_and_rejects_late_acceptance() {
    let _deadline = deadline();
    approval_scenario(ApprovalCase::Undelivered, Executor::Tool).await;
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native serve host commands; requires the arany executable"
)]
fn host_approval_once_refusal_and_transport_failures_replay() {
    let _gate = crate::test_process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for case in [
                ApprovalCase::Accept,
                ApprovalCase::Decline,
                ApprovalCase::Cancel,
                ApprovalCase::Interrupt,
                ApprovalCase::Eof,
                ApprovalCase::LateAccept,
                ApprovalCase::Duplicate,
                ApprovalCase::Invalid,
                ApprovalCase::ClientError,
                ApprovalCase::Backpressure,
                ApprovalCase::BackpressureInterrupt,
                ApprovalCase::Undelivered,
                ApprovalCase::Session,
                ApprovalCase::SessionBusyResume,
                ApprovalCase::Remember,
            ] {
                let _deadline = deadline();
                approval_scenario(case, Executor::Host).await;
            }
            let _deadline = deadline();
            approval_scenario_channel(
                ApprovalCase::Remember,
                Executor::Host,
                server::ApprovalChannel::Terminal,
            )
            .await;
        });
}

fn decision(id: Value, result: Value) -> ClientMessage {
    ClientMessage::Message(Message::Response(Response {
        jsonrpc: Default::default(),
        id: serde_json::from_value(id).unwrap(),
        result,
    }))
}

const BUSY_RESUME_OBJECTIVE: &str = "Hold a tools-disabled turn";

struct ResumeProvider {
    scripted: Arc<ScriptedProvider>,
    waiting: WaitingProvider,
}

#[async_trait::async_trait]
impl Provider for ResumeProvider {
    fn identity(&self) -> &arany::ProviderIdentity {
        self.scripted.identity()
    }

    async fn invoke(
        &self,
        request: arany::ProviderRequest,
    ) -> Result<arany::ProviderResponse, arany::ProviderError> {
        if request.input.objective == BUSY_RESUME_OBJECTIVE {
            self.waiting.invoke(request).await
        } else {
            self.scripted.invoke(request).await
        }
    }
}

async fn resume_host_thread(
    input: &mpsc::Sender<ClientMessage>,
    output: &mut mpsc::Receiver<Message>,
    thread: &Value,
    workspace: &std::path::Path,
) {
    input.send(request(6, "thread/resume", json!({"threadId":thread,"cwd":workspace,"provider":"openai","model":"gpt-5.4","tools":true,"permissionMode":"default","collaboration":"single"}))).await.unwrap();
    loop {
        let message = next(output).await;
        assert!(message.get("error").is_none(), "{message}");
        if message["id"] == 6 {
            break;
        }
    }
}

async fn resume_during_tools_disabled_turn(
    input: &mpsc::Sender<ClientMessage>,
    output: &mut mpsc::Receiver<Message>,
    thread: &Value,
    workspace: &std::path::Path,
    started: &mut mpsc::UnboundedReceiver<()>,
) {
    input
        .send(request(
            8,
            "thread/start",
            json!({"cwd":workspace,"provider":"openai","model":"gpt-5.4","collaboration":"single"}),
        ))
        .await
        .unwrap();
    let other = next(output).await["result"]["thread"]["id"].clone();
    assert!(!other.is_null());
    assert_eq!(next(output).await["method"], "thread/started");
    input
        .send(request(
            3,
            "turn/start",
            json!({"threadId":other,"input":[{"type":"text","text":BUSY_RESUME_OBJECTIVE}]}),
        ))
        .await
        .unwrap();
    let run = loop {
        let message = next(output).await;
        assert!(message.get("error").is_none(), "{message}");
        if message["id"] == 3 {
            break message["result"]["turn"]["id"].clone();
        }
    };
    started.recv().await.unwrap();
    resume_host_thread(input, output, thread, workspace).await;
    input
        .send(request(
            9,
            "turn/interrupt",
            json!({"threadId":other,"turnId":run}),
        ))
        .await
        .unwrap();
    let mut acknowledged = false;
    let mut completed = false;
    while !acknowledged || !completed {
        let message = next(output).await;
        assert!(message.get("error").is_none(), "{message}");
        if message["id"] == 9 {
            assert_eq!(message["result"], json!({}));
            acknowledged = true;
        }
        if message["method"] == "turn/completed" {
            assert_eq!(message["params"]["threadId"], other);
            assert_eq!(message["params"]["turn"]["status"], "interrupted");
            completed = true;
        }
    }
}

async fn session_host_turn(
    input: &mpsc::Sender<ClientMessage>,
    output: &mut mpsc::Receiver<Message>,
    thread: &Value,
    expected_prompts: usize,
) {
    input
        .send(request(
            3,
            "turn/start",
            json!({"threadId":thread,"input":[{"type":"text","text":"Inspect host value"}]}),
        ))
        .await
        .unwrap();
    let mut prompts = 0;
    loop {
        let message = next(output).await;
        assert!(message.get("error").is_none(), "{message}");
        if message["method"] == "item/commandExecution/requestApproval" {
            prompts += 1;
            assert!(prompts <= expected_prompts);
            input
                .send(decision(
                    message["id"].clone(),
                    json!({"decision":"accept"}),
                ))
                .await
                .unwrap();
        }
        if message["method"] == "turn/completed" {
            assert_eq!(message["params"]["turn"]["status"], "completed");
            break;
        }
    }
    assert_eq!(prompts, expected_prompts);
}

#[cfg(unix)]
async fn approval_scenario(case: ApprovalCase, executor: Executor) {
    approval_scenario_channel(case, executor, server::ApprovalChannel::Human).await;
}

#[cfg(unix)]
async fn approval_scenario_channel(
    case: ApprovalCase,
    executor: Executor,
    channel: server::ApprovalChannel,
) {
    let terminal_prefix =
        channel == server::ApprovalChannel::Terminal && case == ApprovalCase::Remember;
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("facts.txt"), "private fact").unwrap();
    let state = root.path().join("state");
    let state_root = StateRoot::admit(&state).unwrap();
    private_json(
        &state.join("tools.json"),
        json!({"version":1,"workspace_paths":["facts.txt"],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    private_json(
        &state.join("permissions.json"),
        json!({"permissions":{"ask":["Read"]}}),
    );
    let engine = arany::test_support::engine::open(
        state_root,
        EngineConfig {
            guard_executable: Some(if executor == Executor::Host {
                std::env::current_exe()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join("arany")
            } else {
                std::env::current_exe().unwrap()
            }),
            ..Default::default()
        },
    )
    .unwrap();
    let mut events = engine.subscribe();
    let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let stalled = outbound.clone();
    let fill_output = outbound.clone();
    let read = arany::test_support::scenario::tool_outcome(if executor == Executor::Host {
        arany::ToolCall::CommandUnguarded {
            program: "printf".into(),
            args: vec!["host value".into()],
            cwd: String::new(),
            justification: "Inspect a synthetic host value".into(),
            prefix_rule: Some(vec!["printf".into(), "host value".into()]),
        }
    } else {
        arany::ToolCall::Read {
            path: "facts.txt".into(),
            offset: None,
            limit: Some(32),
        }
    });
    let finish = arany::ProviderOutcome::Answer(arany::Answer {
        text: "Finished".into(),
    });
    let outcomes = if matches!(
        case,
        ApprovalCase::Session | ApprovalCase::SessionBusyResume
    ) {
        vec![
            read.clone(),
            read.clone(),
            finish.clone(),
            read.clone(),
            finish.clone(),
            read,
            finish,
        ]
    } else if case == ApprovalCase::Duplicate {
        vec![read.clone(), read, finish]
    } else {
        vec![read, finish]
    };
    let provider = Arc::new(
        ScriptedProvider::new(outcomes)
            .with_profile("openai", "gpt-5.4")
            .with_before_call(0, move || {
                if case == ApprovalCase::Undelivered {
                    for _ in 0..fill_output.capacity() {
                        fill_output
                            .try_send(server::response(RequestId::Integer(99), json!({})))
                            .unwrap();
                    }
                }
            }),
    );
    let requests = provider.requests.clone();
    let (busy_started, mut busy_start) = mpsc::unbounded_channel();
    let busy_requests = Arc::new(Mutex::new(Vec::new()));
    let busy_dropped = Arc::new(AtomicUsize::new(0));
    let selected: Arc<dyn Provider> = if case == ApprovalCase::SessionBusyResume {
        Arc::new(ResumeProvider {
            scripted: provider.clone(),
            waiting: WaitingProvider {
                started: busy_started,
                dropped: busy_dropped.clone(),
                requests: busy_requests.clone(),
                identity: provider.identity().clone(),
            },
        })
    } else {
        provider.clone()
    };
    let threads = threads::Threads {
        provider: Some(selected),
        approval_channel: channel,
        ..Default::default()
    };
    let approval_method = if executor == Executor::Host {
        "item/commandExecution/requestApproval"
    } else {
        "item/tool/requestApproval"
    };
    let client = async {
        input.send(if matches!(case, ApprovalCase::Session | ApprovalCase::SessionBusyResume | ApprovalCase::Remember) {
            request(1, "initialize", json!({"clientInfo":{"name":"test","version":"1"},"capabilities":{"hostCommandSessionApproval":true}}))
        } else { initialize() }).await.unwrap();
        assert!(
            next(&mut output).await["result"]["capabilities"]
                .as_array()
                .unwrap()
                .contains(&json!("approvals_v2"))
        );
        let start = json!({"cwd":workspace,"provider":"openai","model":"gpt-5.4","tools":true,"permissionMode":if matches!(case, ApprovalCase::Session | ApprovalCase::SessionBusyResume | ApprovalCase::Remember) {"default"} else {"auto"},"collaboration":"single"});
        input.send(request(2, "thread/start", start)).await.unwrap();
        let thread = next(&mut output).await["result"]["thread"]["id"].clone();
        next(&mut output).await;
        input
            .send(request(
                3,
                "turn/start",
                json!({"threadId":thread,"input":[{"type":"text","text":"Read the fact"}]}),
            ))
            .await
            .unwrap();
        let mut wire_approval = None;
        let mut approval_count = 0;
        if case == ApprovalCase::Undelivered {
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut unavailable = false;
                loop {
                    if let Some(arany::Tapped::Committed(event)) = events.next().await {
                        if let Event::ToolFinished { observation, .. } = &event.event {
                            assert_eq!(
                                observation.approval_cause,
                                Some(arany::ApprovalCause::ChannelUnavailable)
                            );
                            assert!(observation.attestation.is_none());
                            unavailable = true;
                        }
                        if matches!(event.event, Event::RunFinished { .. }) {
                            assert!(unavailable);
                            break;
                        }
                    }
                }
            })
            .await
            .expect("undelivered approval must settle without draining output");
            assert_eq!(stalled.capacity(), 0);
            input
                .send(decision(json!(1), json!({"decision":"accept"})))
                .await
                .unwrap();
            for _ in 0..OUTBOUND_CAPACITY {
                let message = serde_json::to_value(output.try_recv().unwrap()).unwrap();
                assert_ne!(message["method"], approval_method);
                assert_ne!(message["method"], "turn/completed");
            }
        } else {
            let mut approval = None;
            let mut started = false;
            while approval.is_none() || !started {
                let message = next(&mut output).await;
                if message["id"] == 3 && message.get("result").is_some() {
                    started = true;
                }
                if message["method"] == approval_method {
                    approval = Some(message);
                }
            }
            let approval = approval.unwrap();
            approval_count += 1;
            assert_eq!(approval["id"], 1);
            let params = &approval["params"];
            assert_eq!(params["threadId"], thread);
            assert_eq!(params["turnId"], params["intent"]["run_id"]);
            assert_eq!(params["itemId"], params["intent"]["id"]);
            assert!(params.get("expiresAtMs").is_none());
            assert!(params["intent"].get("expires_at_ms").is_none());
            let intent: arany::EffectIntent =
                serde_json::from_value(params["intent"].clone()).unwrap();
            if executor == Executor::Host {
                assert!(
                    params["lines"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|line| line.as_str().unwrap().contains("Provider"))
                );
                assert_eq!(
                    params.get("availableDecisions").is_some(),
                    matches!(
                        case,
                        ApprovalCase::Session
                            | ApprovalCase::SessionBusyResume
                            | ApprovalCase::Remember
                    )
                );
            } else {
                assert_eq!(
                    params["lines"],
                    json!(crate::presentation::tool_approval_lines(&intent))
                );
            }
            wire_approval = Some(approval.clone());
            match case {
                ApprovalCase::Session
                | ApprovalCase::SessionBusyResume
                | ApprovalCase::Remember => {
                    input
                        .send(decision(json!(999), json!({"decision":"acceptForSession"})))
                        .await
                        .unwrap();
                    let reply = if matches!(
                        case,
                        ApprovalCase::Session | ApprovalCase::SessionBusyResume
                    ) {
                        "acceptForSession"
                    } else {
                        "allowPrefixInFolder"
                    };
                    input
                        .send(decision(approval["id"].clone(), json!({"decision":reply})))
                        .await
                        .unwrap();
                    input
                        .send(decision(
                            approval["id"].clone(),
                            json!({"decision":"acceptForSession"}),
                        ))
                        .await
                        .unwrap();
                }
                ApprovalCase::Accept => input
                    .send(decision(
                        approval["id"].clone(),
                        json!({"decision":"accept"}),
                    ))
                    .await
                    .unwrap(),
                ApprovalCase::Decline => input
                    .send(decision(
                        approval["id"].clone(),
                        json!({"decision":"decline"}),
                    ))
                    .await
                    .unwrap(),
                ApprovalCase::Cancel => {
                    input
                        .send(decision(
                            approval["id"].clone(),
                            json!({"decision":"cancel"}),
                        ))
                        .await
                        .unwrap();
                    input
                        .send(decision(
                            approval["id"].clone(),
                            json!({"decision":"accept"}),
                        ))
                        .await
                        .unwrap();
                }
                ApprovalCase::Interrupt => input
                    .send(request(
                        5,
                        "turn/interrupt",
                        json!({"threadId":thread,"turnId":params["turnId"]}),
                    ))
                    .await
                    .unwrap(),
                ApprovalCase::Eof => input.send(ClientMessage::Eof).await.unwrap(),
                ApprovalCase::LateAccept => {
                    tokio::time::pause();
                    tokio::time::advance(Duration::from_secs(3600)).await;
                    tokio::time::resume();
                    input
                        .send(decision(
                            approval["id"].clone(),
                            json!({"decision":"accept"}),
                        ))
                        .await
                        .unwrap();
                }
                ApprovalCase::Duplicate => {
                    input
                        .send(decision(json!(999), json!({"decision":"accept"})))
                        .await
                        .unwrap();
                    input
                        .send(decision(
                            approval["id"].clone(),
                            json!({"decision":"accept"}),
                        ))
                        .await
                        .unwrap();
                    input
                        .send(decision(
                            approval["id"].clone(),
                            json!({"decision":"accept"}),
                        ))
                        .await
                        .unwrap();
                }
                ApprovalCase::Invalid => {
                    input
                        .send(decision(
                            approval["id"].clone(),
                            json!({"decision":"acceptForSession"}),
                        ))
                        .await
                        .unwrap();
                }
                ApprovalCase::Unavailable => input
                    .send(decision(
                        approval["id"].clone(),
                        json!({"decision":"unavailable"}),
                    ))
                    .await
                    .unwrap(),
                ApprovalCase::ClientError => input
                    .send(ClientMessage::Message(Message::Error(ErrorResponse {
                        jsonrpc: Default::default(),
                        id: Some(serde_json::from_value(approval["id"].clone()).unwrap()),
                        error: ErrorObject {
                            code: -32603,
                            message: "Human channel unavailable".into(),
                            data: None,
                        },
                    })))
                    .await
                    .unwrap(),
                ApprovalCase::Backpressure | ApprovalCase::BackpressureInterrupt => {
                    for _ in 0..stalled.capacity() {
                        stalled
                            .try_send(server::response(RequestId::Integer(99), json!({})))
                            .unwrap();
                    }
                    input
                        .send(request(8, "thread/list", json!({})))
                        .await
                        .unwrap();
                    drop(input.reserve_many(INBOUND_CAPACITY).await.unwrap());
                    if case == ApprovalCase::Backpressure {
                        input.send(ClientMessage::Eof).await.unwrap();
                    } else {
                        input
                            .send(request(
                                9,
                                "turn/interrupt",
                                json!({"threadId":thread,"turnId":params["turnId"]}),
                            ))
                            .await
                            .unwrap();
                    }
                    tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            if let Some(arany::Tapped::Committed(event)) = events.next().await
                                && let Event::RunFinished { disposition, .. } = event.event
                            {
                                assert_eq!(disposition, arany::RunDisposition::Cancelled);
                                break;
                            }
                        }
                    })
                    .await
                    .expect("cancellation must commit while output is full");
                    assert_eq!(stalled.capacity(), 0);
                    return (serde_json::from_value(thread).unwrap(), wire_approval, None);
                }
                ApprovalCase::Undelivered => unreachable!(),
            }
        }
        let mut completed = false;
        let mut completed_goal = Value::Null;
        while !completed {
            let message = next(&mut output).await;
            assert!(message.get("error").is_none(), "{case:?}: {message}");
            if message
                .get("method")
                .and_then(Value::as_str)
                .is_some_and(|m| m.ends_with("requestApproval"))
            {
                approval_count += 1;
                assert_eq!(case, ApprovalCase::Duplicate);
                assert_eq!(message["id"], 2);
                input
                    .send(decision(
                        message["id"].clone(),
                        json!({"decision":"accept"}),
                    ))
                    .await
                    .unwrap();
            }
            if message["method"] == "turn/completed" {
                let interrupted = matches!(
                    case,
                    ApprovalCase::Cancel
                        | ApprovalCase::Interrupt
                        | ApprovalCase::Eof
                        | ApprovalCase::Backpressure
                );
                assert_eq!(
                    message["params"]["turn"]["status"] == "interrupted",
                    interrupted,
                    "{case:?}: {message}"
                );
                completed_goal = message["params"]["turn"]["goalId"].clone();
                completed = true;
            }
        }
        assert_eq!(
            approval_count,
            if case == ApprovalCase::Duplicate {
                2
            } else {
                usize::from(case != ApprovalCase::Undelivered)
            }
        );
        if case == ApprovalCase::Duplicate {
            input
                .send(decision(json!(2), json!({"decision":"accept"})))
                .await
                .unwrap();
        }
        let mut receipt = None;
        if matches!(
            case,
            ApprovalCase::Session | ApprovalCase::SessionBusyResume
        ) {
            session_host_turn(&input, &mut output, &thread, 0).await;
            if case == ApprovalCase::SessionBusyResume {
                resume_during_tools_disabled_turn(
                    &input,
                    &mut output,
                    &thread,
                    &workspace,
                    &mut busy_start,
                )
                .await;
            } else {
                resume_host_thread(&input, &mut output, &thread, &workspace).await;
            }
            session_host_turn(&input, &mut output, &thread, 1).await;
            input
                .send(request(
                    7,
                    "receipt/get",
                    json!({"threadId":thread,"goalId":completed_goal}),
                ))
                .await
                .unwrap();
            loop {
                let message = next(&mut output).await;
                assert!(message.get("error").is_none(), "{message}");
                if message["id"] == 7 {
                    receipt = Some(message["result"]["goal"].clone());
                    break;
                }
            }
        }
        if !matches!(case, ApprovalCase::Eof | ApprovalCase::Backpressure) {
            input.send(ClientMessage::Eof).await.unwrap();
        }
        (
            serde_json::from_value(thread).unwrap(),
            wire_approval,
            receipt,
        )
    };
    let (result, (id, wire_approval, wire_receipt)) = tokio::join!(
        server::run_with_threads(engine, state.clone(), inbound, outbound, threads),
        client
    );
    if matches!(
        case,
        ApprovalCase::Backpressure | ApprovalCase::BackpressureInterrupt
    ) {
        assert!(result.unwrap_err().contains("output blocked"));
    } else {
        result.unwrap();
    }
    while let Ok(message) = output.try_recv() {
        assert!(
            serde_json::to_value(&message)
                .unwrap()
                .get("error")
                .is_none(),
            "{case:?}: {message:?}"
        );
    }
    if case == ApprovalCase::SessionBusyResume {
        assert_eq!(busy_dropped.load(Ordering::SeqCst), 1);
        let busy_session = {
            let busy = busy_requests.lock().unwrap();
            assert_eq!(busy.len(), 1);
            busy[0].session_id
        };
        let (events, view) = closed_view(&state, busy_session).await;
        assert!(view.runs[0].config.as_ref().unwrap().tool_policy.is_none());
        assert_eq!(view.runs[0].status, arany::RunStatus::Cancelled);
        for prefix in 1..=events.len() {
            arany::SessionView::replay(view.id, &events[..prefix]).unwrap();
        }
    }
    let (events, view) = closed_view(&state, id).await;
    if executor == Executor::Host {
        if terminal_prefix {
            let saved: Value =
                serde_json::from_slice(&std::fs::read(state.join("permissions.json")).unwrap())
                    .unwrap();
            assert_eq!(
                saved["permissions"]["hostPrefixes"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        } else {
            assert_eq!(
                std::fs::read(state.join("permissions.json")).unwrap(),
                serde_json::to_vec(&json!({"permissions":{"ask":["Read"]}})).unwrap()
            );
        }
        for prefix in 1..=events.len() {
            arany::SessionView::replay(id, &events[..prefix]).unwrap();
        }
    }
    if let Some(receipt) = wire_receipt {
        let goal = view
            .goals
            .iter()
            .find(|goal| json!(goal.id) == receipt["id"])
            .unwrap();
        let report = goal.report.as_ref().unwrap();
        assert_eq!(
            receipt["report"],
            json!({
                "summary":report.summary,"changes":report.changes,"evidence":report.evidence,
                "unresolved":report.unresolved,"cachedInputTokens":report.cached_input_tokens,
            })
        );
        assert!(receipt["report"].to_string().contains("unguarded host run"));
    }
    if matches!(
        case,
        ApprovalCase::Session | ApprovalCase::SessionBusyResume
    ) {
        assert_eq!(view.runs.len(), 3);
        let second = &view.runs[1].tools[0];
        let third = &view.runs[2].tools[0];
        assert_eq!(
            second.permission_verdict.as_ref().unwrap().source,
            arany::permissions::PermissionSource::SessionApproval
        );
        assert_eq!(
            third.permission_verdict.as_ref().unwrap().source,
            arany::permissions::PermissionSource::Human
        );
        for tool in [second, third] {
            assert_eq!(
                tool.observation.as_ref().unwrap().disposition,
                arany::ToolDisposition::Succeeded
            );
        }
        let epoch = |run: &arany::RunView| {
            run.config
                .as_ref()
                .unwrap()
                .tool_policy
                .as_ref()
                .unwrap()
                .host_epoch
        };
        assert_eq!(epoch(&view.runs[0]), epoch(&view.runs[1]));
        assert_ne!(epoch(&view.runs[1]), epoch(&view.runs[2]));
    }
    let tools: Vec<_> = view.runs[0]
        .tools
        .iter()
        .chain(
            view.runs[0]
                .agents
                .iter()
                .flat_map(|agent| agent.tools.iter()),
        )
        .collect();
    let accepted = terminal_prefix
        || matches!(
            case,
            ApprovalCase::Accept
                | ApprovalCase::Duplicate
                | ApprovalCase::LateAccept
                | ApprovalCase::Session
                | ApprovalCase::SessionBusyResume
        );
    let continued = accepted
        || matches!(
            case,
            ApprovalCase::Decline
                | ApprovalCase::Invalid
                | ApprovalCase::ClientError
                | ApprovalCase::Unavailable
                | ApprovalCase::Undelivered
                | ApprovalCase::Remember
        );
    assert_eq!(
        tools.len(),
        if matches!(
            case,
            ApprovalCase::Duplicate | ApprovalCase::Session | ApprovalCase::SessionBusyResume
        ) {
            2
        } else {
            1
        }
    );
    assert_eq!(
        tools
            .iter()
            .filter(|tool| tool.observation.as_ref().unwrap().disposition
                == arany::ToolDisposition::Succeeded)
            .count(),
        if matches!(
            case,
            ApprovalCase::Duplicate | ApprovalCase::Session | ApprovalCase::SessionBusyResume
        ) {
            2
        } else {
            usize::from(accepted)
        }
    );
    for (index, tool) in tools.iter().enumerate() {
        assert_eq!(
            tool.observation.as_ref().unwrap().approval_cause,
            match case {
                ApprovalCase::Remember if terminal_prefix => None,
                ApprovalCase::Decline => Some(arany::ApprovalCause::Declined),
                ApprovalCase::Cancel
                | ApprovalCase::Interrupt
                | ApprovalCase::Eof
                | ApprovalCase::Backpressure
                | ApprovalCase::BackpressureInterrupt => Some(arany::ApprovalCause::Cancelled),
                ApprovalCase::Invalid
                | ApprovalCase::ClientError
                | ApprovalCase::Unavailable
                | ApprovalCase::Undelivered
                | ApprovalCase::Remember => Some(arany::ApprovalCause::ChannelUnavailable),
                ApprovalCase::Accept
                | ApprovalCase::LateAccept
                | ApprovalCase::Duplicate
                | ApprovalCase::Session
                | ApprovalCase::SessionBusyResume => None,
            }
        );
        let verdict = tool.permission_verdict.as_ref().unwrap();
        assert_eq!(
            verdict.source,
            if matches!(
                case,
                ApprovalCase::Session | ApprovalCase::SessionBusyResume
            ) && index == 1
            {
                arany::permissions::PermissionSource::SessionApproval
            } else if terminal_prefix
                || matches!(
                    case,
                    ApprovalCase::Accept
                        | ApprovalCase::LateAccept
                        | ApprovalCase::Decline
                        | ApprovalCase::Duplicate
                        | ApprovalCase::Session
                        | ApprovalCase::SessionBusyResume
                )
            {
                arany::permissions::PermissionSource::Human
            } else {
                arany::permissions::PermissionSource::UnavailableApproval
            }
        );
        assert_eq!(
            verdict.approved,
            tool.observation.as_ref().unwrap().disposition == arany::ToolDisposition::Succeeded
        );
    }
    if let Some(approval) = wire_approval {
        let event = events.iter().find(|event| matches!(&event.event, Event::ToolProposed { intent, .. } if json!(intent.id) == approval["params"]["itemId"])).unwrap();
        let Event::ToolProposed { intent, .. } = &event.event else {
            unreachable!()
        };
        assert_eq!(approval["params"]["intent"], json!(intent));
        assert_eq!(
            approval["params"]["startedAtMs"],
            json!(event.created_at_ms)
        );
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::ToolFinished { .. }))
            .count(),
        tools.len()
            + if matches!(
                case,
                ApprovalCase::Session | ApprovalCase::SessionBusyResume
            ) {
                2
            } else {
                0
            }
    );
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.phase != arany::AgentPhase::ToolReview)
    );
    if !accepted {
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| !format!("{request:?}").contains("private fact"))
        );
    }
    if continued && !accepted {
        let requests = requests.lock().unwrap();
        let result = requests[1]
            .input
            .tools
            .as_ref()
            .unwrap()
            .steps
            .last()
            .unwrap()
            .observations
            .last()
            .unwrap();
        let reason = if case == ApprovalCase::Decline {
            "user_declined"
        } else {
            "channel_unavailable"
        };
        assert!(
            result.output.contains(reason),
            "{case:?} {executor:?} {channel:?}: missing correlated denial cause {reason}"
        );
        assert!(!format!("{:?}", requests).contains("Human channel unavailable"));
    }
    assert_eq!(provider.responses.lock().unwrap().is_empty(), continued);
}

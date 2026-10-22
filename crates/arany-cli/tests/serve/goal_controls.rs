use super::*;
use std::future::Future;
use tokio::sync::oneshot;

struct Gated {
    inner: ScriptedProvider,
    started: mpsc::UnboundedSender<()>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait::async_trait]
impl Provider for Gated {
    fn identity(&self) -> &arany::ProviderIdentity {
        &self.inner.identity
    }

    async fn invoke(
        &self,
        request: arany::ProviderRequest,
    ) -> Result<arany::ProviderResponse, arany::ProviderError> {
        let gate = self.release.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = self.started.send(());
            let _ = gate.await;
        }
        self.inner.invoke(request).await
    }
}

fn finish(result: &str) -> arany::ProviderOutcome {
    arany::ProviderOutcome::Answer(arany::Answer {
        text: result.into(),
    })
}

fn delegate() -> arany::ProviderOutcome {
    arany::ProviderOutcome::Delegate(arany::Delegate {
        children: vec![arany::ChildAssignment {
            editor: None,
            objective: "Inspect".into(),
            profile: None,
        }],
        agent: None,
    })
}

struct Gate {
    started: mpsc::UnboundedReceiver<()>,
    release: oneshot::Sender<()>,
}

fn gated(script: Vec<arany::ProviderOutcome>) -> (Arc<dyn Provider>, Gate) {
    let (started, started_rx) = mpsc::unbounded_channel();
    let (release, release_rx) = oneshot::channel();
    let provider = Gated {
        inner: ScriptedProvider::new(script).with_profile("openai", "gpt-5.4"),
        started,
        release: Mutex::new(Some(release_rx)),
    };
    (
        Arc::new(provider),
        Gate {
            started: started_rx,
            release,
        },
    )
}

struct Wire {
    input: mpsc::Sender<ClientMessage>,
    output: mpsc::Receiver<Message>,
    seen: Vec<Value>,
    timeout: Duration,
}

impl Wire {
    async fn call(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.input.send(request(id, method, params)).await.unwrap();
        loop {
            let message = next_with_timeout(&mut self.output, self.timeout).await;
            if message["id"] == id {
                return message;
            }
            self.seen.push(message);
        }
    }

    async fn notification(&mut self, method: &str) -> Value {
        loop {
            if let Some(index) = self.seen.iter().position(|m| m["method"] == method) {
                return self.seen.remove(index);
            }
            let message = next_with_timeout(&mut self.output, self.timeout).await;
            self.seen.push(message);
        }
    }

    async fn start_turn(&mut self, thread: &Value, text: &str) -> Value {
        let started = self
            .call(
                3,
                "turn/start",
                json!({"threadId":thread,"input":[{"type":"text","text":text}]}),
            )
            .await;
        started["result"]["turn"].clone()
    }
}

async fn serve<F, Fut>(
    provider: Arc<dyn Provider>,
    script: F,
) -> (tempfile::TempDir, std::path::PathBuf, arany::SessionId)
where
    F: FnOnce(Wire, Value) -> Fut,
    Fut: Future<Output = Value>,
{
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("workspace")).unwrap();
    let thread = serve_in(root.path(), provider, None, script).await;
    let state = root.path().join("state");
    (root, state, thread)
}

async fn serve_in<F, Fut>(
    root: &std::path::Path,
    provider: Arc<dyn Provider>,
    resume: Option<arany::SessionId>,
    script: F,
) -> arany::SessionId
where
    F: FnOnce(Wire, Value) -> Fut,
    Fut: Future<Output = Value>,
{
    let workspace = root.join("workspace");
    let state = root.join("state");
    let threads = threads::Threads {
        provider: Some(provider),
        ..Default::default()
    };
    let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, output) = mpsc::channel(OUTBOUND_CAPACITY);
    let client = async {
        let mut wire = Wire {
            input,
            output,
            seen: Vec::new(),
            timeout: Duration::from_secs(10),
        };
        wire.call(
            1,
            "initialize",
            json!({"clientInfo":{"name":"test","version":"1"}}),
        )
        .await;
        let thread = match resume {
            Some(id) => {
                let resumed = wire
                    .call(
                        6,
                        "thread/resume",
                        json!({"threadId":id,"cwd":workspace,"provider":"openai","model":"gpt-5.4"}),
                    )
                    .await;
                resumed["result"]["thread"]["id"].clone()
            }
            None => wire
                .call(
                    2,
                    "thread/start",
                    json!({"cwd":workspace,"provider":"openai","model":"gpt-5.4","collaboration":"auto","maxActiveChildren":1}),
                )
                .await["result"]["thread"]["id"]
                .clone(),
        };
        let input = wire.input.clone();
        let thread = script(wire, thread).await;
        input.send(ClientMessage::Eof).await.unwrap();
        thread
    };
    let (result, thread) = tokio::join!(
        server::run_with_threads(engine(&state), state.clone(), inbound, outbound, threads),
        client
    );
    result.unwrap();
    serde_json::from_value(thread).unwrap()
}

fn reason(response: &Value) -> (&Value, &Value) {
    (
        &response["error"]["code"],
        &response["error"]["data"]["reason"],
    )
}

#[tokio::test]
async fn serve_defaults_complete_past_64_calls_and_30_active_minutes() {
    use arany::test_support::goals::{assert_long_turn, facts_workspace, long_turn_provider};
    use std::os::unix::fs::PermissionsExt;
    let _deadline = deadline();
    for finite in [false, true] {
        let root = tempfile::tempdir().unwrap();
        facts_workspace(root.path(), "Fact");
        let state = root.path().join("state");
        let admitted = StateRoot::admit(&state).unwrap();
        let config = state.join("tools.json");
        std::fs::write(&config, br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        if finite {
            let preferences = json!({"version":4,"preferences":{
                "completion":"tabs","view":"fullscreen",
                "goal_budget":arany::GoalBudget { max_provider_calls:Some(1), ..Default::default() },
                "resources":arany::ToolResourceSettings::default(),"mouse":"wheel_and_clicks","copy_on_select":true
            }});
            admitted
                .record(arany::RecordKind::UiPreferences)
                .replace(&serde_json::to_vec(&preferences).unwrap())
                .unwrap();
        }
        drop(admitted);
        let provider = Arc::new(long_turn_provider());
        let observed = provider.clone();
        let threads = threads::Threads::with_provider(provider);
        let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
        let (outbound, output) = mpsc::channel(OUTBOUND_CAPACITY);
        let client = async {
            let mut wire = Wire {
                input,
                output,
                seen: Vec::new(),
                timeout: Duration::from_secs(2_000),
            };
            wire.call(
                1,
                "initialize",
                json!({"clientInfo":{"name":"test","version":"1"}}),
            )
            .await;
            let thread = wire.call(2, "thread/start", json!({"cwd":root.path().join("workspace"),"provider":"openai","model":"gpt-5.4","collaboration":"single","tools":true})).await["result"]["thread"]["id"].clone();
            let turn = wire.start_turn(&thread, "Inspect the folder").await;
            let completed = wire.notification("turn/completed").await;
            assert_eq!(
                completed["params"]["turn"]["status"],
                if finite { "failed" } else { "completed" }
            );
            if finite {
                assert_eq!(
                    completed["params"]["turn"]["goal"]["stopped"],
                    json!({"reason":"goalExhausted","limit":"calls"})
                );
            }
            let receipt = wire
                .call(
                    10,
                    "receipt/get",
                    json!({"threadId":thread,"goalId":turn["goalId"]}),
                )
                .await;
            let goal = &receipt["result"]["goal"];
            assert_eq!(
                goal["budget"],
                json!({"maxProviderCalls":if finite { Some(1) } else { None },"maxTokens":null,"maxActiveMs":null,"maxRuns":16})
            );
            assert_eq!(goal["usage"]["providerCalls"], if finite { 1 } else { 65 });
            assert_eq!(goal["status"], if finite { "failed" } else { "completed" });
            wire.input.send(ClientMessage::Eof).await.unwrap();
            serde_json::from_value::<arany::SessionId>(thread).unwrap()
        };
        let (result, id) = tokio::join!(
            server::run_with_threads(
                Engine::open(
                    StateRoot::admit(&state).unwrap(),
                    EngineConfig {
                        guard_executable: Some(std::env::current_exe().unwrap()),
                        ..EngineConfig::default()
                    }
                )
                .unwrap(),
                state.clone(),
                inbound,
                outbound,
                threads
            ),
            client
        );
        result.unwrap();
        let (events, view) = closed_view(&state, id).await;
        if finite {
            assert_eq!(view.goals[0].budget.max_provider_calls, Some(1));
            assert_eq!(view.goals[0].provider_calls, 1);
            assert!(matches!(
                events.last().unwrap().event,
                Event::GoalFinished {
                    exhausted: true,
                    ..
                }
            ));
            assert!(view.runs[0].tools.is_empty());
        } else {
            assert_long_turn(&events, &view);
            assert!(observed.responses.lock().unwrap().is_empty());
        }
        assert_eq!(
            observed.requests.lock().unwrap().len(),
            if finite { 1 } else { 65 }
        );
    }
}

#[tokio::test]
async fn malformed_and_unsafe_settings_refuse_thread_admission_before_dispatch() {
    use std::os::unix::fs::PermissionsExt;
    let _deadline = deadline();
    for unsafe_record in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state = root.path().join("state");
        let admitted = StateRoot::admit(&state).unwrap();
        admitted
            .record(arany::RecordKind::UiPreferences)
            .replace(b"malformed")
            .unwrap();
        if unsafe_record {
            std::fs::set_permissions(
                state.join("ui-preferences.json"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        drop(admitted);
        let provider =
            Arc::new(ScriptedProvider::new(Vec::new()).with_profile("openai", "gpt-5.4"));
        let observed = provider.requests.clone();
        let threads = threads::Threads::with_provider(provider);
        let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
        let (outbound, output) = mpsc::channel(OUTBOUND_CAPACITY);
        let client = async {
            let mut wire = Wire {
                input,
                output,
                seen: Vec::new(),
                timeout: Duration::from_secs(10),
            };
            wire.call(
                1,
                "initialize",
                json!({"clientInfo":{"name":"test","version":"1"}}),
            )
            .await;
            let refused = wire
                .call(
                    2,
                    "thread/start",
                    json!({"cwd":workspace,"provider":"openai","model":"gpt-5.4"}),
                )
                .await;
            assert_eq!(reason(&refused), (&json!(-32000), &json!("invalidRequest")));
            wire.input.send(ClientMessage::Eof).await.unwrap();
        };
        let (result, ()) = tokio::join!(
            server::run_with_threads(engine(&state), state.clone(), inbound, outbound, threads),
            client
        );
        result.unwrap();
        assert!(observed.lock().unwrap().is_empty());
        assert!(
            arany::list_sessions(StateRoot::open_existing(&state).unwrap(), workspace)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            std::fs::read(state.join("ui-preferences.json")).unwrap(),
            b"malformed"
        );
    }
}

#[tokio::test]
async fn steer_note_is_journaled_once_and_refused_when_empty_oversized_or_full() {
    let _deadline = deadline();
    let (provider, mut gate) = gated(vec![delegate(), finish("child"), finish("synthesis")]);
    let (_root, state, id) = serve(provider, |mut wire, thread| async move {
        let turn = wire.start_turn(&thread, "Steer me").await;
        gate.started.recv().await.unwrap();
        let ok = wire
            .call(
                10,
                "turn/steer",
                json!({"threadId":thread,"turnId":turn["id"],"input":[{"type":"text","text":"a".repeat(5000)}]}),
            )
            .await;
        assert_eq!(ok["result"], json!({"turnId":turn["id"]}));
        for (id, text, expected) in [
            (11, "b".repeat(5000), "runFull"),
            (12, "  ".into(), "empty"),
            (13, "c".repeat(9000), "tooLarge"),
        ] {
            let refused = wire
                .call(
                    id,
                    "turn/steer",
                    json!({"threadId":thread,"turnId":turn["id"],"input":[{"type":"text","text":text}]}),
                )
                .await;
            assert_eq!(reason(&refused), (&json!(-32602), &json!(expected)));
        }
        let huge = wire
            .call(
                14,
                "turn/steer",
                json!({"threadId":thread,"turnId":turn["id"],"input":[{"type":"text","text":"d".repeat(70 * 1024)}]}),
            )
            .await;
        assert_eq!(reason(&huge), (&json!(-32602), &json!("invalidParams")));
        gate.release.send(()).unwrap();
        let completed = wire.notification("turn/completed").await;
        assert_eq!(completed["params"]["turn"]["status"], "completed");
        thread
    })
    .await;
    let (events, view) = closed_view(&state, id).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.event, Event::GoalSteeringNoteRecorded { .. }))
            .count(),
        1
    );
    assert_eq!(view.runs[0].steering_notes, ["a".repeat(5000)]);
}

#[tokio::test]
async fn steer_pause_and_resume_without_a_running_turn_or_goal_are_refused() {
    let _deadline = deadline();
    let provider =
        Arc::new(ScriptedProvider::new(vec![finish("Done")]).with_profile("openai", "gpt-5.4"));
    let (_root, state, id) = serve(provider, |mut wire, thread| async move {
        let run = "019a0011-2233-7444-8555-667788990001";
        let goal = "019a0011-2233-7444-8555-667788990002";
        for (id, method, params) in [
            (10, "turn/steer", json!({"threadId":thread,"turnId":run,"input":[{"type":"text","text":"x"}]})),
            (11, "goal/pause", json!({"threadId":thread,"goalId":goal})),
        ] {
            let refused = wire.call(id, method, params).await;
            assert_eq!(reason(&refused), (&json!(-32600), &json!("turnNotActive")));
        }
        let resume = wire
            .call(12, "goal/resume", json!({"threadId":thread,"goalId":goal}))
            .await;
        assert_eq!(reason(&resume), (&json!(-32000), &json!("goalUnavailable")));
        let unloaded = wire
            .call(
                13,
                "goal/resume",
                json!({"threadId":run,"goalId":goal}),
            )
            .await;
        assert_eq!(reason(&unloaded), (&json!(-32600), &json!("threadNotLoaded")));

        let turn = wire.start_turn(&thread, "Answer").await;
        wire.notification("turn/completed").await;
        for (id, method, params, expected) in [
            (20, "turn/steer", json!({"threadId":thread,"turnId":turn["id"],"input":[{"type":"text","text":"late"}]}), "turnNotActive"),
            (21, "goal/pause", json!({"threadId":thread,"goalId":turn["goalId"]}), "turnNotActive"),
            (22, "goal/resume", json!({"threadId":thread,"goalId":turn["goalId"]}), "goalUnavailable"),
        ] {
            let refused = wire.call(id, method, params).await;
            assert_eq!(reason(&refused).1, &json!(expected), "{method}");
        }
        for (id, method, params) in [
            (30, "turn/steer", json!({"threadId":thread,"turnId":run,"input":[]})),
            (31, "turn/steer", json!({"threadId":thread,"turnId":run,"input":[{"type":"text","text":"a"},{"type":"text","text":"b"}]})),
            (32, "turn/steer", json!({"threadId":thread,"turnId":run,"input":[{"type":"image","mediaType":"image/png","data":"aW1hZ2U="}]})),
            (33, "goal/resume", json!({"threadId":thread,"goalId":goal,"input":[{"type":"image","mediaType":"image/png","data":"aW1hZ2U="}]})),
            (34, "goal/pause", json!({"threadId":thread})),
        ] {
            let refused = wire.call(id, method, params).await;
            assert_eq!(reason(&refused), (&json!(-32602), &json!("invalidParams")), "{method}");
        }
        thread
    })
    .await;
    let (_, view) = closed_view(&state, id).await;
    assert_eq!(view.runs.len(), 1);
    assert_eq!(view.goals[0].status, arany::GoalStatus::Completed);
}

#[tokio::test]
async fn pause_ends_the_turn_interrupted_and_goal_resume_continues_the_pinned_objective() {
    let _deadline = deadline();
    let (provider, mut gate) = gated(vec![delegate(), finish("Resumed")]);
    let (_root, state, id) = serve(provider, |mut wire, thread| async move {
        let turn = wire.start_turn(&thread, "Pinned objective").await;
        gate.started.recv().await.unwrap();
        let pause = json!({"threadId":thread,"goalId":turn["goalId"]});
        assert_eq!(
            wire.call(10, "goal/pause", pause.clone()).await["result"],
            json!({})
        );
        assert_eq!(
            wire.call(11, "goal/pause", pause).await["result"],
            json!({})
        );
        let other = wire
            .call(
                12,
                "goal/pause",
                json!({"threadId":thread,"goalId":turn["id"]}),
            )
            .await;
        assert_eq!(reason(&other).1, &json!("goalUnavailable"));
        gate.release.send(()).unwrap();
        let completed = wire.notification("turn/completed").await;
        assert_eq!(completed["params"]["turn"]["status"], "interrupted");
        assert_eq!(completed["params"]["turn"]["goal"]["status"], "interrupted");

        let resumed = wire
            .call(
                21,
                "goal/resume",
                json!({"threadId":thread,"goalId":turn["goalId"],"input":[{"type":"text","text":"Carry on"}]}),
            )
            .await;
        assert_eq!(resumed["result"]["turn"]["status"], "inProgress");
        assert_eq!(resumed["result"]["turn"]["goalId"], turn["goalId"]);
        assert_ne!(resumed["result"]["turn"]["id"], turn["id"]);
        let completed = wire.notification("turn/completed").await;
        assert_eq!(completed["params"]["turn"]["status"], "completed");
        assert_eq!(
            completed["params"]["turn"]["id"],
            resumed["result"]["turn"]["id"]
        );
        let message = wire
            .seen
            .iter()
            .find(|m| {
                m["method"] == "item/started" && m["params"]["turnId"] == resumed["result"]["turn"]["id"]
            })
            .expect("resumed user message");
        assert_eq!(message["params"]["item"]["text"], "Carry on");
        let again = wire
            .call(
                22,
                "goal/resume",
                json!({"threadId":thread,"goalId":turn["goalId"]}),
            )
            .await;
        assert_eq!(reason(&again).1, &json!("goalUnavailable"));
        thread
    })
    .await;
    let (events, view) = closed_view(&state, id).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.event, Event::GoalResumed { .. }))
            .count(),
        1
    );
    assert_eq!(view.runs.len(), 2);
    assert_eq!(view.runs[0].status, arany::RunStatus::Cancelled);
    assert_eq!(view.goals.len(), 1);
    assert_eq!(view.goals[0].objective, "Pinned objective");
    assert_eq!(view.goals[0].status, arany::GoalStatus::Completed);
}

#[tokio::test]
async fn oversized_steer_line_is_rejected_by_the_line_cap_and_the_next_line_is_served() {
    let _deadline = deadline();
    let (mut client, transport) = tokio::io::duplex(4096);
    let (input, mut inbound) = mpsc::channel(INBOUND_CAPACITY);
    let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
    let reader = read_messages(transport, input, outbound);
    let peer = async {
        let note = "a".repeat(MAX_LINE_BYTES);
        let line = json!({"id":1,"method":"turn/steer","params":{"threadId":"t","turnId":"r","input":[{"type":"text","text":note}]}});
        client.write_all(line.to_string().as_bytes()).await.unwrap();
        client
            .write_all(b"\n{\"id\":2,\"method\":\"goal/pause\",\"params\":{}}\n")
            .await
            .unwrap();
        client.shutdown().await.unwrap();
        assert_eq!(
            next(&mut output).await["error"]["data"]["reason"],
            "messageTooLarge"
        );
        assert!(matches!(
            inbound.recv().await,
            Some(ClientMessage::Message(Message::Request(_)))
        ));
    };
    let (result, ()) = tokio::join!(reader, peer);
    result.unwrap();
}

#[tokio::test]
async fn resume_of_an_exhausted_goal_closes_it_failed_in_a_second_server() {
    let _deadline = deadline();
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("workspace")).unwrap();
    let state = root.path().join("state");
    let preferences = json!({"version":4,"preferences":{
        "completion":"tabs",
        "view":"classic",
        "goal_budget":arany::GoalBudget{max_provider_calls:Some(1),..Default::default()},
        "resources":arany::ToolResourceSettings::default(),
        "mouse":"wheel_and_clicks",
        "copy_on_select":true,
    }});
    arany::StateRoot::admit(&state)
        .unwrap()
        .record(arany::RecordKind::UiPreferences)
        .replace(&serde_json::to_vec(&preferences).unwrap())
        .unwrap();

    let (provider, mut gate) = gated(vec![delegate()]);
    let goal = Arc::new(Mutex::new(Value::Null));
    let seen_goal = goal.clone();
    let id = serve_in(root.path(), provider, None, |mut wire, thread| async move {
        let turn = wire.start_turn(&thread, "Spend the only call").await;
        gate.started.recv().await.unwrap();
        let pause = json!({"threadId":thread,"goalId":turn["goalId"]});
        assert_eq!(
            wire.call(10, "goal/pause", pause).await["result"],
            json!({})
        );
        gate.release.send(()).unwrap();
        let completed = wire.notification("turn/completed").await;
        assert_eq!(completed["params"]["turn"]["status"], "interrupted");
        *seen_goal.lock().unwrap() = turn["goalId"].clone();
        thread
    })
    .await;
    let goal = goal.lock().unwrap().clone();
    let mut preferences = preferences;
    preferences["preferences"]["goal_budget"] =
        serde_json::to_value(arany::GoalBudget::default()).unwrap();
    StateRoot::open_existing(&state)
        .unwrap()
        .record(arany::RecordKind::UiPreferences)
        .replace(&serde_json::to_vec(&preferences).unwrap())
        .unwrap();

    let provider = Arc::new(ScriptedProvider::new(Vec::new()).with_profile("openai", "gpt-5.4"));
    let requests = provider.requests.clone();
    serve_in(
        root.path(),
        provider,
        Some(id),
        |mut wire, thread| async move {
            let params = json!({"threadId":thread,"goalId":goal});
            let exhausted = wire.call(40, "goal/resume", params.clone()).await;
            assert_eq!(
                reason(&exhausted),
                (&json!(-32000), &json!("goalBudgetExhausted"))
            );
            assert!(
                exhausted["error"]["message"]
                    .as_str()
                    .unwrap()
                    .starts_with("Arany status: Goal budget refused resume:")
            );
            let again = wire.call(41, "goal/resume", params).await;
            assert_eq!(reason(&again), (&json!(-32000), &json!("goalUnavailable")));
            assert!(wire.seen.iter().all(|m| m["method"] != "turn/started"));
            thread
        },
    )
    .await;
    assert!(requests.lock().unwrap().is_empty());
    let (events, view) = closed_view(&state, id).await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.event, Event::GoalResumed { .. }))
    );
    assert_eq!(view.runs.len(), 1);
    assert_eq!(view.goals[0].status, arany::GoalStatus::Failed);
    assert_eq!(view.interrupted_goal_id(), None);
}

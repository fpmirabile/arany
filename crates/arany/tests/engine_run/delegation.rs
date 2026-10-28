use super::*;

#[test]
fn invalid_delegation_fails_the_run_without_committing_an_answer() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let provider = ScriptedProvider::new(
            (0..3)
                .map(|_| delegate(&[("unauthorized child", None)]))
                .collect(),
        );
        let requests = provider.requests.clone();
        let state_path = temp.path().join("state");
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let outcome = run_to_end(&engine, &provider, request(&workspace, "Question"))
            .await
            .expect("durable failed Run");
        assert_eq!(outcome.run.status, RunStatus::Failed);
        assert!(outcome.run.assistant_message.is_none());
        assert_eq!(outcome.run.agents.len(), 1);
        {
            let requests = requests.lock().unwrap();
            assert_eq!(
                requests.len(),
                3,
                "an invalid outcome is retried to the refusal limit without fake exhaustion"
            );
            assert_test_call(
                &requests[0],
                AgentPhase::RootPlan,
                "Question",
                &["Fact"],
                &[],
                None,
                &[],
            );
            assert_eq!(requests[0].collaboration, CollaborationPolicy::Single);
        }
        assert_eq!(outcome.run.failure, Some(arany::RunFailure::RefusalLimit));
        assert_eq!(outcome.run.agents[0].provider_calls.len(), 3);
        assert!(
            outcome.run.agents[0]
                .provider_calls
                .iter()
                .all(|call| { call.disposition == ProviderCallDisposition::Rejected })
        );
        engine.close().await.expect("engine shutdown");
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        assert_eq!(
            store
                .load_view(outcome.session_id)
                .await
                .unwrap()
                .unwrap()
                .runs[0],
            outcome.run
        );
        store.close().await.unwrap();
    });
}

#[test]
fn team_policy_rejects_a_direct_primary_finish() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Answer(Answer {
                text: "unearned result".into()
            });
            3
        ]);
        let requests = provider.requests.clone();
        let state_path = temp.path().join("state");
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut run_request = request(&workspace, "Question");
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 1,
        };
        let outcome = run_to_end(&engine, &provider, run_request)
            .await
            .expect("durable failed Run");
        assert_eq!(outcome.run.status, RunStatus::Failed);
        assert!(outcome.run.assistant_message.is_none());
        assert_eq!(outcome.run.failure, Some(arany::RunFailure::RefusalLimit));
        {
            let requests = requests.lock().unwrap();
            assert_eq!(
                requests.len(),
                3,
                "three identical phase refusals must stop the Run"
            );
            assert_test_call(
                &requests[0],
                AgentPhase::RootPlan,
                "Question",
                &["Fact"],
                &[],
                None,
                &[],
            );
            assert_eq!(
                requests[0].collaboration,
                CollaborationPolicy::Team {
                    max_active_children: 1
                }
            );
        }
        assert_eq!(outcome.run.agents[0].provider_calls.len(), 3);
        assert_eq!(
            outcome.run.agents[0].provider_calls[0].disposition,
            ProviderCallDisposition::Rejected
        );
        engine.close().await.expect("engine shutdown");
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        assert_eq!(
            store
                .load_view(outcome.session_id)
                .await
                .unwrap()
                .unwrap()
                .runs[0],
            outcome.run
        );
        store.close().await.unwrap();
    });
}

#[tokio::test]
async fn team_planning_refusal_can_be_repaired_with_delegate() {
    let temp = tempfile::tempdir().expect("fixture");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state = temp.path().join("state");
    let finish = ProviderOutcome::Answer(Answer {
        text: "answer".into(),
    });
    let provider = Arc::new(ScriptedProvider::new(vec![
        finish.clone(),
        delegate(&[("Inspect", None)]),
        finish.clone(),
        finish.clone(),
        finish,
    ]));
    let engine = engine_support::open(
        StateRoot::admit(&state).expect("state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let mut input = request(&workspace, "Question");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Team {
        max_active_children: 1,
    };
    let result = run_to_end(&engine, &provider, input)
        .await
        .expect("team repaired");
    let mut follow_up = request(&workspace, "What happened?");
    follow_up.include_paths.clear();
    follow_up.session_id = Some(result.session_id);
    follow_up.title = None;
    run_to_end(&engine, &provider, follow_up)
        .await
        .expect("follow-up");
    engine.close().await.expect("close engine");
    {
        let requests = provider.requests.lock().expect("requests");
        assert_eq!(requests.len(), 5);
        assert_eq!(requests[4].input.history[0].assistant, "answer");
        assert_eq!(
            requests[1].input.last_answer_refused,
            Some(arany::ProposalRefusal {
                operation: "answer".into(),
                cause: arany::ProposalRefusalCause::Phase
            })
        );
        assert!(
            requests[2..]
                .iter()
                .all(|request| request.input.last_answer_refused.is_none())
        );
    }
    assert!(provider.responses.lock().expect("script").is_empty());
    let store =
        Store::open_read_only(StateRoot::open_existing(&state).expect("state")).expect("store");
    let view = store
        .load_view(result.session_id)
        .await
        .expect("reopen")
        .expect("Session");
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert!(view.runs[0].tools.is_empty());
    store.close().await.expect("store close");
}

#[test]
fn one_child_team_finishes_only_after_child_result_is_committed() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let provider = ScriptedProvider::new(vec![
            delegate(&[("Check fact", None)]),
            ProviderOutcome::Answer(Answer {
                text: "Fact is true".into(),
            }),
            ProviderOutcome::Answer(Answer {
                text: "Final answer".into(),
            }),
        ]);
        let recorded_requests = Arc::clone(&provider.requests);
        let state_path = temp.path().join("state");
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut run_request = request(&workspace, "Question");
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 1,
        };
        let outcome = run_to_end(&engine, &provider, run_request)
            .await
            .expect("successful team Run");
        assert_eq!(outcome.run.status, RunStatus::Finished);
        assert_eq!(outcome.run.agents.len(), 2);
        assert_eq!(
            outcome
                .run
                .config
                .as_ref()
                .expect("team config")
                .context_usage,
            Some(ContextUsage {
                used_bytes: ("Question".len() + include_data("notes/facts.txt", "Fact").len())
                    as u32,
                budget_bytes: 376 * 1024 - (20 * 1024 + 64) - unplanned_goal_metadata_bytes(),
                compactable_bytes: 0,
                tool_history_bytes: 0,
            })
        );
        assert_eq!(
            outcome.run.assistant_message.as_deref(),
            Some("Final answer")
        );
        {
            let requests = recorded_requests.lock().expect("requests lock");
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[0].phase, AgentPhase::RootPlan);
            assert_eq!(requests[1].phase, AgentPhase::ChildWork);
            assert_eq!(requests[1].input.objective, "Check fact");
            assert!(requests[1].input.history.is_empty());
            assert_eq!(requests[2].phase, AgentPhase::RootSynthesis);
            assert_eq!(requests[2].input.child_results.len(), 1);
            assert_eq!(requests[2].input.child_results[0].result, "Fact is true");
        }
        engine.close().await.expect("engine shutdown");
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let events = store
            .load_session(outcome.session_id)
            .await
            .expect("durable Events");
        // Primary, child wave and synthesis each journal their call; the Run settles active time once.
        assert_eq!(events.len(), 18);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalProviderCallReserved { .. }))
                .count(),
            3
        );
        let delegated_call = events
            .iter()
            .position(|event| {
                matches!(
                    &event.event,
                    Event::ProviderCallRecorded { record, .. }
                        if record.phase == AgentPhase::RootPlan
                            && record.disposition == ProviderCallDisposition::Delegated
                )
            })
            .expect("committed delegation");
        let interrupted = SessionView::replay(outcome.session_id, &events[..=delegated_call])
            .expect("valid post-call prefix")
            .expect("Session");
        assert_eq!(interrupted.runs[0].status, RunStatus::Interrupted);
        // A crash after any committed Event before the first effect is an interruption, never an
        // unknown effect; with no Tool started, every such prefix projects the same stop.
        let accepted = events
            .iter()
            .position(|event| matches!(event.event, Event::MessageAccepted { .. }))
            .expect("accepted message");
        let finished = events
            .iter()
            .position(|event| matches!(event.event, Event::RunFinished { .. }))
            .expect("finished Run");
        assert!(
            events[accepted..finished]
                .iter()
                .all(|event| !matches!(event.event, Event::ToolStarted { .. }))
        );
        for end in accepted..finished {
            let crashed = SessionView::replay(outcome.session_id, &events[..=end])
                .expect("valid prefix")
                .expect("Session");
            assert_eq!(
                crashed.run_stop(&crashed.runs[0]),
                Some(arany::RunStop::Interrupted),
                "crash after {:?}",
                events[end].event.kind()
            );
        }
        assert_eq!(interrupted.runs[0].agents[0].provider_calls.len(), 1);
        assert_eq!(
            interrupted.runs[0].agents[0].provider_calls[0].disposition,
            ProviderCallDisposition::Delegated
        );
        let child_id = outcome.run.agents[1].id;
        let primary_id = outcome.run.agents[0].id;
        let child_call = events
            .iter()
            .position(|event| {
                matches!(
                    &event.event,
                    Event::ProviderCallRecorded { agent_run_id, record, .. }
                        if *agent_run_id == child_id && record.phase == AgentPhase::ChildWork
                )
            })
            .expect("committed child call");
        let synthesis_call = events
            .iter()
            .position(|event| {
                matches!(
                    &event.event,
                    Event::ProviderCallRecorded { agent_run_id, record, .. }
                        if *agent_run_id == primary_id && record.phase == AgentPhase::RootSynthesis
                )
            })
            .expect("committed primary synthesis call");
        let child_finished = events
            .iter()
            .position(|event| {
                matches!(
                    &event.event,
                    Event::AgentFinished {
                        agent_run_id,
                        disposition: arany::AgentDisposition::Finished,
                        ..
                    } if *agent_run_id == child_id
                )
            })
            .expect("committed child result");
        let primary_finished = events
            .iter()
            .position(|event| {
                matches!(
                    &event.event,
                    Event::AgentFinished {
                        agent_run_id,
                        disposition: arany::AgentDisposition::Finished,
                        ..
                    } if *agent_run_id == primary_id
                )
            })
            .expect("committed primary result");
        assert!(delegated_call < child_call);
        assert!(child_call < child_finished);
        assert!(child_finished < synthesis_call);
        assert!(synthesis_call < primary_finished);
        let calls = events
            .iter()
            .filter_map(|event| match &event.event {
                Event::ProviderCallRecorded { record, .. } => Some(record),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            calls.iter().map(|call| call.phase).collect::<Vec<_>>(),
            [
                AgentPhase::RootPlan,
                AgentPhase::ChildWork,
                AgentPhase::RootSynthesis
            ]
        );
        let mut invalid_phase = events.clone();
        if let Event::ProviderCallRecorded { record, .. } = &mut invalid_phase[delegated_call].event
        {
            record.phase = AgentPhase::RootSynthesis;
        } else {
            panic!("expected committed primary delegation");
        }
        assert!(SessionView::replay(outcome.session_id, &invalid_phase).is_err());
        let mut duplicate_call = events.clone();
        duplicate_call[child_call].event = events[delegated_call].event.clone();
        duplicate_call[child_call].run_id = events[delegated_call].run_id;
        duplicate_call[child_call].agent_run_id = events[delegated_call].agent_run_id;
        assert!(SessionView::replay(outcome.session_id, &duplicate_call).is_err());
        let missing_calls: Vec<_> = events
            .iter()
            .filter(|event| !matches!(event.event, Event::ProviderCallRecorded { .. }))
            .cloned()
            .collect();
        assert!(SessionView::replay(outcome.session_id, &missing_calls).is_err());
        store.close().await.expect("store shutdown");
    });
}

#[test]
fn provider_concurrency_ceiling_serializes_admitted_children() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let (started, mut started_rx) = mpsc::unbounded_channel();
        let provider = GatedProvider {
            started,
            requests: Arc::new(Mutex::new(Vec::new())),
            fail_child: None,
            identity: gated_identity(1),
        };
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&temp.path().join("state")).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut run_request = request(&workspace, "Question");
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 3,
        };
        let mut run_task = tokio::spawn(async move {
            let outcome = run_to_end(&engine, &provider, run_request).await;
            let close = engine.close().await;
            (outcome, close)
        });
        let controlled = tokio::time::timeout(Duration::from_secs(5), async {
            for expected in ["A", "B", "C"] {
                let (objective, release) = started_rx.recv().await.expect("child started");
                assert_eq!(objective, expected);
                assert!(matches!(
                    started_rx.try_recv(),
                    Err(mpsc::error::TryRecvError::Empty)
                ));
                release.send(()).expect("live child");
            }
        })
        .await;
        if controlled.is_err() {
            run_task.abort();
            let _ = run_task.await;
            panic!("serialized team calls exceeded the test deadline");
        }
        let joined = tokio::time::timeout(Duration::from_secs(5), &mut run_task).await;
        let (outcome, close) = match joined {
            Ok(result) => result.expect("Run task"),
            Err(_) => {
                run_task.abort();
                let _ = run_task.await;
                panic!("serialized team Run exceeded the test deadline");
            }
        };
        let outcome = outcome.expect("successful team Run");
        close.expect("engine shutdown");
        assert_eq!(outcome.run.status, RunStatus::Finished);
        assert_eq!(
            outcome
                .run
                .config
                .as_ref()
                .expect("pinned config")
                .provider_concurrency,
            1
        );
    });
}

#[test]
fn team_cancellation_drains_active_and_queued_children() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        for capacity in [1_u8, 3] {
            let temp = tempfile::tempdir().expect("temporary root");
            let workspace = temp.path().join("workspace");
            std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
            std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
            let state_path = temp.path().join("state");
            let (started, mut started_rx) = mpsc::unbounded_channel();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let provider = GatedProvider {
                started,
                requests: Arc::clone(&requests),
                fail_child: None,
                identity: gated_identity(capacity),
            };
            let provider = Arc::new(provider);
            let engine = engine_support::open(
                StateRoot::admit(&state_path).expect("private state"),
                EngineConfig::default(),
            )
            .expect("engine");
            let cancellation = RunCancellation::new();
            let run_cancellation = cancellation.clone();
            let mut run_request = request(&workspace, "Question");
            run_request.policy = CollaborationPolicy::Team {
                max_active_children: 3,
            };
            let mut run_task = tokio::spawn(async move {
                let outcome = run_with(
                    &engine,
                    &provider,
                    run_request,
                    RunControls {
                        cancellation: run_cancellation,
                        ..Default::default()
                    },
                )
                .await;
                let close = engine.close().await;
                (outcome, close)
            });
            let started_calls = tokio::time::timeout(Duration::from_secs(5), async {
                let mut gates = Vec::new();
                for _ in 0..capacity {
                    gates.push(started_rx.recv().await.expect("child started"));
                }
                gates
            })
            .await;
            let gates = match started_calls {
                Ok(gates) => gates,
                Err(_) => {
                    run_task.abort();
                    let _ = run_task.await;
                    panic!(
                        "team calls did not start within the test deadline: capacity {capacity}"
                    );
                }
            };
            assert!(cancellation.cancel());
            let joined = tokio::time::timeout(Duration::from_secs(5), &mut run_task).await;
            let (outcome, close) = match joined {
                Ok(result) => result.expect("Run task"),
                Err(_) => {
                    run_task.abort();
                    let _ = run_task.await;
                    panic!("team cancellation exceeded the test deadline: capacity {capacity}");
                }
            };
            let outcome = outcome.expect("durable cancelled team Run");
            close.expect("engine shutdown");
            assert_eq!(
                outcome.run.status,
                RunStatus::Cancelled,
                "capacity {capacity}"
            );
            assert!(
                outcome
                    .run
                    .agents
                    .iter()
                    .all(|agent| agent.status == AgentStatus::Cancelled),
                "capacity {capacity}"
            );
            assert!(outcome.run.assistant_message.is_none());
            for (_, release) in gates {
                assert!(
                    release.send(()).is_err(),
                    "call still live after cancellation"
                );
            }
            assert!(matches!(
                started_rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected)
            ));
            {
                let requests = requests.lock().expect("requests lock");
                assert_eq!(
                    requests
                        .iter()
                        .filter(|request| request.phase == AgentPhase::ChildWork)
                        .count(),
                    usize::from(capacity)
                );
                assert!(
                    requests
                        .iter()
                        .all(|request| request.phase != AgentPhase::RootSynthesis)
                );
            }
            let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
                .expect("reopened store");
            let events = store
                .load_session(outcome.session_id)
                .await
                .expect("durable Events");
            let replay = SessionView::replay(outcome.session_id, &events)
                .expect("valid replay")
                .expect("Session");
            assert_eq!(replay.runs[0].status, RunStatus::Cancelled);
            assert_eq!(
                events
                    .iter()
                    .filter(|envelope| matches!(envelope.event, Event::AgentFinished { .. }))
                    .count(),
                4
            );
            assert!(
                !events
                    .iter()
                    .any(|envelope| matches!(envelope.event, Event::MessageCommitted { .. }))
            );
            store.close().await.expect("store shutdown");
        }
    });
}

fn output_cap_provider(
    bound: OutputTokenBound,
    cap: u32,
    responses: Vec<ProviderOutcome>,
) -> ScriptedProvider {
    let provider = ScriptedProvider::new(responses).with_max_output_tokens(cap);
    match bound {
        OutputTokenBound::ProviderEnforced => provider.with_profile("openai", "gpt-5.4"),
        OutputTokenBound::LocalAcceptanceOnly => {
            provider.with_chatgpt_account(Uuid::now_v7(), [1; 32])
        }
    }
}

async fn assert_team_output_caps(workspace: &Path, state: &Path, provider: ScriptedProvider) {
    let cap = provider.identity().limits.max_output_tokens;
    let bound = provider.identity().limits.output_token_bound;
    let requests = Arc::clone(&provider.requests);
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::admit(state).expect("private state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let mut team = request(workspace, "Question");
    team.policy = CollaborationPolicy::Team {
        max_active_children: 1,
    };
    let outcome = run_to_end(&engine, &provider, team)
        .await
        .expect("team Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    let config = outcome.run.config.as_ref().expect("Run config");
    assert_eq!(config.output_token_cap, cap);
    assert_eq!(config.output_token_bound, bound);
    let caps: Vec<_> = requests
        .lock()
        .expect("requests lock")
        .iter()
        .map(|call| (call.phase, call.max_output_tokens))
        .collect();
    assert_eq!(
        caps,
        [
            (AgentPhase::RootPlan, cap),
            (AgentPhase::ChildWork, 4096),
            (AgentPhase::RootSynthesis, cap)
        ]
    );
    engine.close().await.expect("engine shutdown");
}

async fn assert_output_cap_refused(workspace: &Path, state: &Path, provider: ScriptedProvider) {
    let requests = Arc::clone(&provider.requests);
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::admit(state).expect("private state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let result = run_to_end(&engine, &provider, request(workspace, "Question")).await;
    engine.close().await.expect("engine shutdown");
    assert!(matches!(result, Err(EngineError::InvalidRequest)));
    assert!(requests.lock().expect("requests lock").is_empty());
}

#[test]
fn provider_declared_output_cap_reaches_the_primary_and_caps_children() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let state = temp.path().join("state");
        let cap = arany::OpenAiProvider::from_api_key_with_effort(
            "gpt-5.4".into(),
            None,
            "synthetic-openai-key".into(),
        )
        .expect("synthetic native identity")
        .identity()
        .limits
        .max_output_tokens;
        assert_eq!(cap, 32_768);
        for bound in [
            OutputTokenBound::ProviderEnforced,
            OutputTokenBound::LocalAcceptanceOnly,
        ] {
            let team = output_cap_provider(
                bound,
                cap,
                vec![
                    delegate(&[("Inspect", None)]),
                    ProviderOutcome::Answer(Answer {
                        text: "Child answer".into(),
                    }),
                    ProviderOutcome::Answer(Answer {
                        text: "Answer".into(),
                    }),
                ],
            );
            assert_team_output_caps(&workspace, &state, team).await;
            let oversized = output_cap_provider(
                bound,
                cap + 1,
                vec![ProviderOutcome::Answer(Answer {
                    text: "Unexpected dispatch".into(),
                })],
            );
            assert_output_cap_refused(&workspace, &state, oversized).await;
        }
    });
}

#[test]
fn eight_children_use_the_same_ordered_loop() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let objectives: Vec<_> = (1..=8).map(|index| format!("Child {index}")).collect();
        let mut responses = vec![ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: objectives
                .iter()
                .map(|objective| arany::ChildAssignment {
                    editor: None,
                    objective: objective.clone(),
                    profile: None,
                })
                .collect(),
        })];
        responses.extend((0..8).map(|_| {
            ProviderOutcome::Answer(Answer {
                text: "result".into(),
            })
        }));
        responses.push(ProviderOutcome::Answer(Answer {
            text: "final".into(),
        }));
        let provider = ScriptedProvider::new(responses);
        let requests = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&temp.path().join("state")).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut run_request = request(&workspace, "Question");
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 8,
        };
        let outcome = run_to_end(&engine, &provider, run_request)
            .await
            .expect("eight-child Run");
        assert_eq!(outcome.run.status, RunStatus::Finished);
        assert_eq!(outcome.run.agents.len(), 9);
        {
            let requests = requests.lock().expect("requests lock");
            assert_eq!(requests.len(), 10);
            let synthesis = requests.last().expect("synthesis call");
            assert_eq!(synthesis.phase, AgentPhase::RootSynthesis);
            assert_eq!(
                synthesis
                    .input
                    .child_results
                    .iter()
                    .map(|child| child.objective.as_str())
                    .collect::<Vec<_>>(),
                objectives.iter().map(String::as_str).collect::<Vec<_>>()
            );
        }
        engine.close().await.expect("engine shutdown");
    });
}

#[test]
fn collaboration_capacity_corpus_rejects_invalid_delegation() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let cases = [
            (
                CollaborationPolicy::Auto {
                    max_active_children: 0,
                },
                1,
            ),
            (
                CollaborationPolicy::Team {
                    max_active_children: 1,
                },
                0,
            ),
            (
                CollaborationPolicy::Team {
                    max_active_children: 1,
                },
                2,
            ),
            (
                CollaborationPolicy::Team {
                    max_active_children: 8,
                },
                9,
            ),
        ];
        for (index, (policy, count)) in cases.into_iter().enumerate() {
            let children: Vec<_> = (0..count)
                .map(|child| arany::ChildAssignment {
                    editor: None,
                    objective: format!("Child {child}"),
                    profile: None,
                })
                .collect();
            let provider = ScriptedProvider::new(
                (0..3)
                    .map(|_| {
                        ProviderOutcome::Delegate(Delegate {
                            children: children.clone(),
                            agent: None,
                        })
                    })
                    .collect(),
            );
            let requests = provider.requests.clone();
            let provider = Arc::new(provider);
            let engine = engine_support::open(
                StateRoot::admit(&temp.path().join(format!("state-{index}")))
                    .expect("private state"),
                EngineConfig::default(),
            )
            .expect("engine");
            let mut run_request = request(&workspace, "Question");
            run_request.policy = policy;
            let outcome = run_to_end(&engine, &provider, run_request)
                .await
                .expect("durable failed Run");
            assert_eq!(outcome.run.status, RunStatus::Failed, "case {index}");
            assert_eq!(outcome.run.agents.len(), 1, "case {index}");
            assert!(outcome.run.assistant_message.is_none(), "case {index}");
            {
                let requests = requests.lock().unwrap();
                assert_eq!(
                    requests.len(),
                    3,
                    "case {index}: no fake exhaustion or child call"
                );
                assert_test_call(
                    &requests[0],
                    AgentPhase::RootPlan,
                    "Question",
                    &["Fact"],
                    &[],
                    None,
                    &[],
                );
                assert_eq!(requests[0].collaboration, policy);
            }
            assert_eq!(outcome.run.failure, Some(arany::RunFailure::RefusalLimit));
            assert_eq!(outcome.run.agents[0].provider_calls.len(), 3);
            assert!(
                outcome.run.agents[0]
                    .provider_calls
                    .iter()
                    .all(|call| call.disposition == ProviderCallDisposition::Rejected)
            );
            engine.close().await.expect("engine shutdown");
        }
        for (index, policy) in [
            CollaborationPolicy::Team {
                max_active_children: 0,
            },
            CollaborationPolicy::Auto {
                max_active_children: 9,
            },
        ]
        .into_iter()
        .enumerate()
        {
            let provider = ScriptedProvider::new(vec![]);
            let requests = Arc::clone(&provider.requests);
            let provider = Arc::new(provider);
            let engine = engine_support::open(
                StateRoot::admit(&temp.path().join(format!("invalid-{index}")))
                    .expect("private state"),
                EngineConfig::default(),
            )
            .expect("engine");
            let mut run_request = request(&workspace, "Question");
            run_request.policy = policy;
            assert!(
                matches!(
                    run_to_end(&engine, &provider, run_request).await,
                    Err(EngineError::InvalidRequest)
                ),
                "invalid policy case {index}"
            );
            assert!(requests.lock().expect("requests lock").is_empty());
            engine.close().await.expect("engine shutdown");
        }
    });
}

#[test]
fn delegation_during_synthesis_is_invalid_and_fails_the_run() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let provider = ScriptedProvider::new(
            vec![
                delegate(&[("Check fact", None)]),
                ProviderOutcome::Answer(Answer {
                    text: "Fact is true".into(),
                }),
            ]
            .into_iter()
            .chain((0..3).map(|_| delegate(&[("Second round", None)])))
            .collect::<Vec<_>>(),
        );
        let requests = provider.requests.clone();
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&temp.path().join("state")).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut run_request = request(&workspace, "Question");
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 1,
        };
        let outcome = run_to_end(&engine, &provider, run_request)
            .await
            .expect("durable failed Run");
        assert_eq!(outcome.run.status, RunStatus::Failed);
        assert!(outcome.run.assistant_message.is_none());
        assert_eq!(outcome.run.agents.len(), 2);
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 5, "no second delegation round starts");
            assert!(
                requests[2..]
                    .iter()
                    .all(|r| r.phase == AgentPhase::RootSynthesis)
            );
        }
        let calls = &outcome.run.agents[0].provider_calls;
        assert_eq!(calls.len(), 4);
        assert!(calls[1..].iter().all(|call| {
            call.disposition == ProviderCallDisposition::Rejected
                && call.phase == AgentPhase::RootSynthesis
        }));
        assert_eq!(outcome.run.failure, Some(arany::RunFailure::RefusalLimit));
        engine.close().await.expect("engine shutdown");
    });
}

#[test]
fn failed_child_cancels_and_drains_siblings_without_a_final_message() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").expect("include");
        let state_path = temp.path().join("state");
        let (started, mut started_rx) = mpsc::unbounded_channel();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = GatedProvider {
            started,
            requests: Arc::clone(&requests),
            fail_child: Some("B"),
            identity: gated_identity(3),
        };
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("private state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut run_request = request(&workspace, "Question");
        run_request.policy = CollaborationPolicy::Team {
            max_active_children: 3,
        };
        let mut run_task = tokio::spawn(async move {
            let outcome = run_to_end(&engine, &provider, run_request).await;
            let close = engine.close().await;
            (outcome, close)
        });
        let gates = tokio::time::timeout(Duration::from_secs(5), async {
            let mut gates = HashMap::new();
            for _ in 0..3 {
                let (objective, release) = started_rx.recv().await.expect("child started");
                gates.insert(objective, release);
            }
            gates
        })
        .await;
        let mut gates = match gates {
            Ok(gates) => gates,
            Err(_) => {
                run_task.abort();
                let _ = run_task.await;
                panic!("children did not start within the test deadline");
            }
        };
        gates
            .remove("B")
            .expect("B gate")
            .send(())
            .expect("live B call");
        let joined = tokio::time::timeout(Duration::from_secs(5), &mut run_task).await;
        let (outcome, close) = match joined {
            Ok(result) => result.expect("Run task"),
            Err(_) => {
                run_task.abort();
                let _ = run_task.await;
                panic!("failed team Run exceeded its test deadline");
            }
        };
        let outcome = outcome.expect("durable failed Run");
        close.expect("engine shutdown");
        assert_eq!(outcome.run.status, RunStatus::Failed);
        assert!(outcome.run.assistant_message.is_none());
        assert_eq!(
            outcome
                .run
                .agents
                .iter()
                .map(|agent| agent.status)
                .collect::<Vec<_>>(),
            [
                AgentStatus::Failed,
                AgentStatus::Cancelled,
                AgentStatus::Failed,
                AgentStatus::Cancelled
            ]
        );
        assert!(gates.remove("A").expect("A gate").send(()).is_err());
        assert!(gates.remove("C").expect("C gate").send(()).is_err());
        assert!(
            requests
                .lock()
                .expect("requests lock")
                .iter()
                .all(|request| request.phase != AgentPhase::RootSynthesis)
        );
        let store = Store::open(StateRoot::admit(&state_path).expect("state readmission"))
            .expect("reopened store");
        let events = store
            .load_session(outcome.session_id)
            .await
            .expect("durable Events");
        assert!(events.iter().any(|envelope| matches!(
            &envelope.event,
            Event::ProviderCallRecorded { record, .. }
                if record.disposition == ProviderCallDisposition::Rejected
        )));
        let rejected = events
            .iter()
            .find_map(|envelope| match &envelope.event {
                Event::ProviderCallRecorded { record, .. }
                    if record.disposition == ProviderCallDisposition::Rejected =>
                {
                    Some(record)
                }
                _ => None,
            })
            .expect("failed child's observed call");
        assert_eq!(
            serde_json::to_value(rejected).expect("bounded call metadata")["failure_reason"],
            "account_access",
            "typed HTTP 403 must retain safe guidance through the journal"
        );
        let replay = SessionView::replay(outcome.session_id, &events)
            .expect("strict failure replay")
            .expect("Session");
        assert_eq!(replay.runs[0], outcome.run);

        assert!(
            replay.runs[0]
                .agents
                .iter()
                .filter(|agent| agent.status == AgentStatus::Cancelled)
                .all(|agent| agent
                    .provider_calls
                    .iter()
                    .all(|call| call.failure_reason.is_none()))
        );
        assert!(
            !events
                .iter()
                .any(|envelope| matches!(envelope.event, Event::MessageCommitted { .. }))
        );
        store.close().await.expect("store shutdown");
    });
}

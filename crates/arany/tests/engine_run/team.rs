use super::goals::*;
use super::*;
use arany::test_support::goals::*;

struct PendingChildren {
    in_flight: Arc<AtomicUsize>,
}

struct InFlightCall(Arc<AtomicUsize>);

impl Drop for InFlightCall {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[async_trait::async_trait]
impl Provider for PendingChildren {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                let mut identity = arany::ProviderIdentity::new(
                    "openai",
                    "pending-children-model",
                    arany::Provenance::native(),
                );
                identity.limits = arany::CallLimits {
                    concurrency: 2,
                    ..Default::default()
                };
                identity
            });
        &IDENTITY
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        match request.phase {
            AgentPhase::RootPlan => Ok(ProviderResponse {
                turn: arany::ProviderTurn::scripted(),
                outcome: delegate(&[("First child", None), ("Second child", None)]),
                response_id: None,
                usage: arany::Usage {
                    input_tokens: Some(0),
                    output_tokens: Some(1),
                    ..Default::default()
                },
                wire_provenance: None,
            }),
            AgentPhase::ChildWork => {
                self.in_flight.fetch_add(1, Ordering::AcqRel);
                let _call = InFlightCall(Arc::clone(&self.in_flight));
                std::future::pending::<()>().await;
                Err(ProviderError::InvalidOutcome)
            }
            _ => Err(ProviderError::InvalidOutcome),
        }
    }

    async fn compact(&self, _: CompactionRequest) -> Result<CompactionResponse, ProviderError> {
        Err(ProviderError::InvalidOutcome)
    }
}

#[tokio::test]
async fn store_failure_while_admitting_a_child_drains_active_calls_before_returning() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let in_flight = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(PendingChildren {
        in_flight: Arc::clone(&in_flight),
    });
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
    )
    .expect("engine");
    {
        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
            .expect("fault injection connection");
        connection
            .execute_batch(
                "CREATE TRIGGER fail_second_child_reservation BEFORE INSERT ON events
                 WHEN NEW.kind = 'GoalProviderCallReserved'
                   AND (SELECT COUNT(*) FROM events WHERE kind = 'GoalProviderCallReserved') >= 2
                 BEGIN SELECT RAISE(ABORT, 'injected reservation failure'); END;",
            )
            .expect("reservation failure trigger");
    }
    let mut input = request(&workspace, "Delegate two calls");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Team {
        max_active_children: 2,
    };
    let result = run_to_end(&engine, &provider, input).await;
    assert!(matches!(result, Err(EngineError::Store(_))));
    assert_eq!(
        in_flight.load(Ordering::Acquire),
        0,
        "the admitted child call is drained before the store error returns"
    );
    engine.close().await.expect("engine shutdown");
}

fn child_catalog(request: &arany::ProviderRequest) -> serde_json::Value {
    let tools = request.input.tools.as_ref().expect("research catalog");
    serde_json::from_str(tools.catalog()).expect("catalog JSON")
}

pub(super) fn child_tool_starts(
    events: &[arany::EventEnvelope],
    child: arany::AgentRunId,
) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(
                &event.event,
                Event::ToolStarted { agent_run_id, .. } if *agent_run_id == child
            )
        })
        .count()
}

#[tokio::test]
async fn read_only_child_reads_the_workspace_and_its_report_reaches_synthesis() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        delegate(&[("Inspect facts", None)]),
        crate::tool_outcome(facts_read()),
        finish_with("facts say v1"),
        finish_with("synthesized"),
    ]);
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    input.goal.budget = Some(arany::GoalBudget {
        max_provider_calls: Some(8),
        ..arany::GoalBudget::default()
    });
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .expect("Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[1].phase, AgentPhase::ChildWork);
    assert_eq!(calls[1].collaboration, CollaborationPolicy::Single);
    assert!(call_steering_notes(&calls[1]).is_empty());
    assert!(call_facts(&calls[1]).is_none());
    let first = child_catalog(&calls[1]);
    assert_eq!(first["write"], false);
    assert_eq!(first["commands"], serde_json::json!([]));
    assert_eq!(first["tool_calls_remaining"], 2);
    assert_eq!(
        child_catalog(&calls[2]),
        first,
        "the child catalog is frozen at spawn"
    );
    let remaining =
        |call: &arany::ProviderRequest| call.input.tools.as_ref().unwrap().calls_remaining();
    assert_eq!(remaining(&calls[1]), Some(2));
    assert_eq!(remaining(&calls[2]), Some(1));
    let observations = &calls[2]
        .input
        .tools
        .as_ref()
        .expect("child observations")
        .observations();
    assert_eq!(observations.len(), 1);
    assert_eq!(
        observations[0].disposition,
        arany::ToolDisposition::Succeeded
    );
    assert_eq!(calls[3].phase, AgentPhase::RootSynthesis);
    assert_eq!(calls[3].input.child_results[0].result, "facts say v1");
    assert!(
        calls[3]
            .input
            .tools
            .as_ref()
            .is_none_or(|tools| tools.observations().is_empty())
    );
    assert!(!format!("{:?}", calls[3]).contains(&facts_digest("v1")));

    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert_action_bounds(&events, false, false);
    let child = view.runs[0].agents[1].id;
    assert_eq!(
        view.runs[0].agents[1].grant,
        Some(arany::ChildGrant::ReadOnly {
            unguarded: arany::UnguardedGrant::Denied
        })
    );
    assert_eq!(child_tool_starts(&events, child), 1);
    assert!(view.runs[0].tools.is_empty());
    assert_eq!(view.runs[0].agents[1].tools.len(), 1);
}

#[tokio::test]
async fn child_mcp_write_or_unguarded_request_is_rejected_before_dispatch() {
    let outcomes = [
        arany::ToolCall::McpCall {
            server: "local".into(),
            tool: "probe".into(),
            schema_digest: "a".repeat(64),
            arguments: "{}".into(),
        },
        arany::ToolCall::Write {
            path: "notes/facts.txt".into(),
            expected_digest: None,
            content: "v2".into(),
        },
        arany::ToolCall::CommandUnguarded {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 0".into()],
            cwd: String::new(),
            justification: "Request forbidden host authority".into(),
            prefix_rule: None,
        },
        arany::ToolCall::GitStatus { path: ".".into() },
    ];
    for call in outcomes {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![
            delegate(&[("Inspect facts", None)]),
            crate::tool_outcome(call),
            finish_with("never reached"),
        ]);
        let requests = provider.requests.clone();
        let provider = Arc::new(provider);
        let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
        let mut input = request(&workspace, "Check the facts");
        input.include_paths.clear();
        input.policy = CollaborationPolicy::Auto {
            max_active_children: 1,
        };
        let outcome = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await
        .expect("Run");
        assert_eq!(outcome.run.status, RunStatus::Failed);
        engine.close().await.expect("engine shutdown");
        assert_eq!(requests.lock().expect("requests").len(), 2);

        let (events, view) = replayed(&state_path, outcome.session_id).await;
        let child = view.runs[0].agents[1].id;
        assert_eq!(child_tool_starts(&events, child), 0);
        assert!(events.iter().any(|event| matches!(
            &event.event,
            Event::ProviderCallRecorded { agent_run_id, record, .. }
                if *agent_run_id == child
                    && record.disposition == ProviderCallDisposition::InvalidResponse
        )));
        assert!(events.iter().any(|event| matches!(
            &event.event,
            Event::AgentFinished { agent_run_id, disposition, .. }
                if *agent_run_id == child && *disposition == arany::AgentDisposition::Failed
        )));
        assert_eq!(
            std::fs::read_to_string(workspace.join("notes/facts.txt")).expect("facts"),
            "v1"
        );
    }
}

#[tokio::test]
async fn child_tool_bound_offers_only_finish_and_rejects_a_further_tool() {
    for further_tool in [false, true] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let mut outcomes = vec![delegate(&[("Inspect facts", None)])];
        outcomes.extend((0..16).map(|_| crate::tool_outcome(facts_read())));
        if further_tool {
            outcomes.push(crate::tool_outcome(facts_read()));
        } else {
            outcomes.push(finish_with("report"));
        }
        outcomes.push(finish_with("done"));
        let provider = ScriptedProvider::new(outcomes);
        let requests = provider.requests.clone();
        let provider = Arc::new(provider);
        let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
        let mut input = request(&workspace, "Check the facts");
        input.include_paths.clear();
        input.policy = CollaborationPolicy::Auto {
            max_active_children: 1,
        };
        let outcome = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await
        .expect("Run");
        engine.close().await.expect("engine shutdown");

        let calls = requests.lock().expect("requests").clone();
        assert!(
            calls
                .iter()
                .all(|call| call.session_id == outcome.session_id)
        );
        let (events, view) = replayed(&state_path, outcome.session_id).await;
        let child = view.runs[0].agents[1].id;
        assert_eq!(child_tool_starts(&events, child), 16);
        if further_tool {
            assert_eq!(outcome.run.status, RunStatus::Failed);
            assert!(events.iter().any(|event| matches!(
                &event.event,
                Event::ProviderCallRecorded { agent_run_id, record, .. }
                    if *agent_run_id == child
                        && record.disposition == ProviderCallDisposition::InvalidResponse
            )));
        } else {
            assert_eq!(outcome.run.status, RunStatus::Finished);
            assert_eq!(
                calls[17].input.tools.as_ref().unwrap().calls_remaining(),
                Some(0)
            );
            assert_eq!(calls[17].phase, AgentPhase::ChildWork);
        }
    }
}

#[tokio::test]
async fn cancellation_after_a_child_tool_request_dispatches_no_child_tool() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let cancellation = RunCancellation::new();
    let mut provider = ScriptedProvider::new(vec![
        delegate(&[("Inspect facts", None)]),
        crate::tool_outcome(facts_read()),
        finish_with("never reached"),
    ]);
    provider.cancel_at_completion = Some((AgentPhase::ChildWork, cancellation.clone()));
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            cancellation,
            ..Default::default()
        },
    )
    .await
    .expect("cancelled Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");
    assert_eq!(requests.lock().expect("requests").len(), 2);
    assert_paused_child_stopped(&state_path, outcome.session_id, outcome.goal_id).await;
}

pub(super) async fn assert_paused_child_stopped(
    state_path: &Path,
    session_id: arany::SessionId,
    goal_id: arany::GoalId,
) {
    let (events, view) = replayed(state_path, session_id).await;
    let child = view.runs[0].agents[1].id;
    assert!(events.iter().any(|event| matches!(
        &event.event,
        Event::ProviderCallRecorded { agent_run_id, record, .. }
            if *agent_run_id == child
                && record.disposition == ProviderCallDisposition::ToolRequested
    )));
    assert_eq!(child_tool_starts(&events, child), 0);
    assert!(events.iter().any(|event| matches!(
        &event.event,
        Event::AgentFinished { agent_run_id, disposition, .. }
            if *agent_run_id == child && *disposition == arany::AgentDisposition::Cancelled
    )));
    assert_eq!(
        view.goal(goal_id).expect("Goal").status,
        arany::GoalStatus::Interrupted
    );
}

#[tokio::test]
async fn delegation_turn_is_replayed_before_synthesis() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        delegate(&[("Inspect facts", None)]),
        finish_with("facts say v1"),
        finish_with("synthesized"),
    ]);
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .expect("Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert_eq!(calls.len(), 3);
    assert!(
        calls[0]
            .input
            .tools
            .as_ref()
            .is_some_and(|tools| tools.steps.is_empty())
    );
    let steps = &calls[2].input.tools.as_ref().expect("Tool context").steps;
    assert_eq!(steps.len(), 1, "the delegating turn precedes synthesis");
    assert!(steps[0].requests.is_empty() && steps[0].observations.is_empty());
    assert_eq!(steps[0].turn, arany::ProviderTurn::scripted());
    assert_eq!(calls[2].input.child_results[0].result, "facts say v1");
    assert!(
        calls[1]
            .input
            .tools
            .as_ref()
            .is_some_and(|tools| tools.steps.is_empty()),
        "children never inherit the primary's turns"
    );
}

#[tokio::test]
async fn foreground_worker_goal_refuses_editor_authority_without_a_write_capable_parent() {
    use arany::worker::{EditorAssignment, WorkerMode};
    for (case, calls, refused_count) in [
        ("foreground", 3, 3),
        ("background", 3, 3),
        ("mixed", 3, 3),
        ("wave", 3, 3),
        ("append_failed", 1, 0),
        ("append_unknown", 1, 1),
        ("cancelled", 1, 1),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = facts_workspace(temp.path(), "Fact");
        let state_path = temp.path().join("state");
        let mut assignments = vec![arany::ChildAssignment {
            objective: "Edit notes".into(),
            profile: None,
            editor: Some(Box::new(EditorAssignment {
                commands: Vec::new(),
                definition: "editor".into(),
                definition_digest: [1; 32],
                mission: "Edit notes".into(),
                direction: String::new(),
                lease: vec!["notes".into()],
                criterion: arany::GoalSuccessCriterion::WorkspaceFileContent {
                    path: "notes/a".into(),
                    content: "done".into(),
                },
                mode: if case == "background" {
                    WorkerMode::Background
                } else {
                    WorkerMode::Foreground
                },
                max_turns: 17,
                wave: if case == "wave" { 2 } else { 1 },
                resumes: None,
                supersedes: None,
            })),
        }];
        if case == "mixed" {
            assignments.push(arany::ChildAssignment {
                editor: None,
                objective: "Research".into(),
                profile: None,
            });
        }
        let provider = Arc::new(ScriptedProvider::new(
            (0..3)
                .map(|_| {
                    ProviderOutcome::Delegate(Delegate {
                        agent: None,
                        children: assignments.clone(),
                    })
                })
                .collect(),
        ));
        let engine = engine_support::open(
            StateRoot::admit(&state_path).unwrap(),
            EngineConfig::default(),
        )
        .unwrap();
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap();
        let refused = |event: &Event| matches!(event, Event::ProviderCallRecorded { record, .. } if record.refusal.is_some());
        let gate = if case == "append_unknown" || case == "cancelled" {
            Some(arany::FaultPlan::gate_goal_append_acknowledgment(
                session, refused,
            ))
        } else {
            if case == "append_failed" {
                arany::FaultPlan::fail_goal_append(session, refused);
            }
            None
        };
        let mut input = request(&workspace, "Edit notes");
        input.session_id = Some(session);
        input.title = None;
        input.include_paths.clear();
        input.policy = CollaborationPolicy::Auto {
            max_active_children: 3,
        };
        let cancellation = arany::RunCancellation::new();
        let result = {
            let run = run_with(
                &engine,
                &provider,
                input,
                RunControls {
                    cancellation: cancellation.clone(),
                    ..Default::default()
                },
            );
            tokio::pin!(run);
            if let Some((entered, release)) = gate {
                tokio::select! {
                    result = &mut run => panic!("Run ended before refusal acknowledgment: {result:?}"),
                    entered = entered => entered.unwrap(),
                }
                if case == "cancelled" {
                    cancellation.cancel();
                    release.send(()).unwrap();
                } else {
                    drop(release);
                }
            }
            run.await
        };
        if case.starts_with("append_") {
            assert!(
                matches!(result, Err(EngineError::Store(arany::StoreError::Closed))),
                "{case}: failed or unknown append must stop"
            );
        } else {
            let outcome = result.unwrap();
            assert_eq!(
                outcome.run.status,
                if case == "cancelled" {
                    RunStatus::Cancelled
                } else {
                    RunStatus::Failed
                },
                "{case}"
            );
        }
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            calls,
            "{case}: no child egress"
        );
        assert_eq!(provider.responses.lock().unwrap().len(), 3 - calls);
        engine.close().await.unwrap();
        let (events, view) = replayed(&state_path, session).await;
        assert_eq!(view.runs[0].agents.len(), 1, "{case}: no child spawn");
        assert!(view.runs[0].tools.is_empty(), "{case}: no effect");
        let refusals = view.runs[0].agents[0]
            .provider_calls
            .iter()
            .filter_map(|r| r.refusal.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(refusals.len(), refused_count);
        if calls == 3 {
            assert_eq!(view.goals[0].status, arany::GoalStatus::Failed);
        }
        assert!(
            refusals
                .iter()
                .all(|r| r.cause == arany::ProposalRefusalCause::EditorGrant)
        );
        assert!(
            view.runs[0]
                .agents
                .iter()
                .all(|agent| agent.assignment.is_none())
        );
        assert_action_bounds(&events, false, false);
    }
}

#[tokio::test]
async fn researcher_multi_call_steps_charge_each_call_and_keep_model_order() {
    for exhausted in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = facts_workspace(temp.path(), "bounded facts");
        let state = temp.path().join("state");
        let engine = facts_engine(StateRoot::admit(&state).unwrap(), &state);
        let batch = |count| {
            ProviderOutcome::Tools(
                (0..count)
                    .map(|index| arany::ToolRequest {
                        id: arany::ToolUseId::new(format!("call_{index}")).unwrap(),
                        call: facts_read(),
                    })
                    .collect(),
            )
        };
        let mut script = vec![delegate(&[("Read the facts", None)]), batch(4)];
        if exhausted {
            script.extend([batch(4), batch(4), batch(2), batch(3)]);
        } else {
            script.extend([
                finish_with("Research complete"),
                finish_with("Facts collected"),
            ]);
        }
        let provider = Arc::new(ScriptedProvider::new(script));
        let mut input = request(&workspace, "Delegate the reading");
        input.include_paths.clear();
        input.policy = CollaborationPolicy::Auto {
            max_active_children: 1,
        };
        let outcome = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        engine.close().await.unwrap();
        let (_, view) = replayed(&state, outcome.session_id).await;
        assert_eq!(view.runs[0], outcome.run);
        assert_eq!(
            outcome.run.status,
            if exhausted {
                RunStatus::Failed
            } else {
                RunStatus::Finished
            }
        );
        let child = &outcome.run.agents[1];
        assert_eq!(child.tools.len(), if exhausted { 14 } else { 4 });
        assert_eq!(child.provider_calls[0].requested_tools, 4);
        let calls = provider.requests.lock().unwrap();
        assert_eq!(calls.len(), if exhausted { 6 } else { 4 });
        let step = &calls[2].input.tools.as_ref().unwrap().steps[0];
        assert_eq!(step.calls_remaining, Some(12));
        assert_eq!(step.observations.len(), 4);
        if exhausted {
            assert_eq!(
                calls[5]
                    .input
                    .tools
                    .as_ref()
                    .unwrap()
                    .steps
                    .last()
                    .unwrap()
                    .calls_remaining,
                Some(2)
            );
            assert_eq!(
                child.provider_calls.last().unwrap().disposition,
                ProviderCallDisposition::InvalidResponse
            );
        }
    }
}

#[tokio::test]
async fn researcher_deadline_records_every_batch_intent_before_non_dispatched_results() {
    const TEST_TIMEOUT: Duration = Duration::from_secs(180);
    const DELEGATION_ALLOWANCE: Duration = Duration::from_secs(20 * 60);
    const REMAINING_TOOL_TIME: Duration = Duration::from_secs(5);
    let _deadline = engine_support::deadline("researcher batch deadline", TEST_TIMEOUT);
    for count in [1, 4] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = facts_workspace(temp.path(), "bounded facts");
        let state = temp.path().join("state");
        let engine = facts_engine(StateRoot::admit(&state).unwrap(), &state);
        let mut provider = ScriptedProvider::new(vec![
            delegate(&[("Read the facts", None)]),
            ProviderOutcome::Tools(
                (0..count)
                    .map(|index| arany::ToolRequest {
                        id: arany::ToolUseId::new(format!("call_{index}")).unwrap(),
                        call: facts_read(),
                    })
                    .collect(),
            ),
            finish_with("Unexpected child continuation"),
            finish_with("Unexpected primary continuation"),
        ])
        .with_active_time_per_call(DELEGATION_ALLOWANCE - REMAINING_TOOL_TIME);
        provider.identity.limits.deadline = DELEGATION_ALLOWANCE;
        let provider = Arc::new(provider);
        let mut input = request(&workspace, "Delegate the reading");
        input.include_paths.clear();
        input.policy = CollaborationPolicy::Auto {
            max_active_children: 1,
        };
        let outcome = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
        engine.close().await.unwrap();
        let (events, view) = replayed(&state, outcome.session_id).await;
        assert_eq!(view.runs[0], outcome.run);
        assert_researcher_deadline_batch(&events, &view, count);
    }
}

fn assert_researcher_deadline_batch(
    events: &[arany::EventEnvelope],
    view: &SessionView,
    count: usize,
) {
    let run = &view.runs[0];
    assert_eq!(run.failure, Some(arany::RunFailure::ToolStopped));
    assert!(!view.goals[0].budget_closed);
    assert!(view.goals[0].budget_refusal.is_none());
    assert!(run.tools.is_empty());
    let child = &run.agents[1];
    assert_eq!(child.tools.len(), count);
    assert_eq!(child.provider_calls[0].requested_tools as usize, count);
    let starts: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::ToolStarted {
                agent_run_id,
                intent,
                ..
            } if *agent_run_id == child.id => Some((event.sequence, intent.id)),
            _ => None,
        })
        .collect();
    let finishes: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::ToolFinished {
                agent_run_id,
                observation,
                ..
            } if *agent_run_id == child.id => Some((event.sequence, observation.intent.id)),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), count);
    assert_eq!(finishes.len(), count);
    let declarations: Vec<_> = if count > 1 {
        events
            .iter()
            .filter_map(|event| match &event.event {
                Event::ToolProposed {
                    agent_run_id,
                    intent,
                    ..
                } if *agent_run_id == child.id => Some((event.sequence, intent.id)),
                _ => None,
            })
            .collect()
    } else {
        starts.clone()
    };
    assert_eq!(declarations.len(), count);
    assert!(declarations.last().unwrap().0 < finishes[0].0);
    for (index, tool) in child.tools.iter().enumerate() {
        assert_eq!(starts[index].1, finishes[index].1);
        let observation = tool.observation.as_ref().unwrap();
        assert_eq!(observation.disposition, arany::ToolDisposition::Limit);
        assert_eq!(observation.limit_cause, Some(arany::LimitCause::Timeout));
        assert_eq!(
            observation.output,
            "Delegation deadline reached; no operation was dispatched"
        );
        assert!(observation.attestation.is_none());
        assert!(tool.hooks.is_empty());
    }
    assert_action_bounds(events, false, false);
}

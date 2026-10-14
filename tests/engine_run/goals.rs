use super::*;

#[tokio::test]
async fn goal_lifecycle_bounds_report_and_requires_goal_replay_linkage() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    // Control characters escape to six JSON bytes each, so the serialized report budget shortens this summary.
    let provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
        summary: "\u{1}".repeat(2000),
        result: "accepted result".into(),
    })]);
    let mut engine =
        Engine::open(StateRoot::admit(&state_path).expect("state"), provider).expect("engine");
    let mut input = request(&workspace, "Complete a Goal");
    input.include_paths.clear();
    let outcome = engine.run(input).await.expect("Goal Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let events = store
        .load_session(outcome.session_id)
        .await
        .expect("Events");
    let view = SessionView::replay(outcome.session_id, &events)
        .expect("strict replay")
        .expect("Session");
    let [goal] = view.goals.as_slice() else {
        panic!("one Goal is linked to the accepted task");
    };
    assert_eq!(goal.id, outcome.goal_id);
    assert_eq!(goal.run_ids, [outcome.run.id]);
    assert_eq!(goal.status, arany::GoalStatus::Completed);
    let report = goal.report.as_ref().expect("durable report");
    assert!(report.summary.len() <= 2048);
    assert!(
        report
            .unresolved
            .iter()
            .any(|item| item.contains("summary was shortened"))
    );
    assert!(
        report
            .unresolved
            .iter()
            .any(|item| item.contains("token usage is incomplete"))
    );

    let without_goal: Vec<_> = events
        .iter()
        .filter(|event| {
            !matches!(
                &event.event,
                Event::GoalStarted { .. }
                    | Event::GoalRunLinked { .. }
                    | Event::GoalActiveTimeReserved { .. }
                    | Event::GoalActiveTimeSettled { .. }
                    | Event::GoalProviderCallReserved { .. }
                    | Event::GoalFinished { .. }
            )
        })
        .cloned()
        .collect();
    assert!(
        SessionView::replay(outcome.session_id, &without_goal).is_err(),
        "current-format replay rejects accepted tasks without Goal linkage"
    );
    store.close().await.expect("Store shutdown");
}

#[tokio::test]
async fn summary_within_the_serialized_report_budget_is_kept_whole() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let summary = "ࠀ".repeat(620);
    let provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
        summary: summary.clone(),
        result: "accepted result".into(),
    })]);
    let mut engine =
        Engine::open(StateRoot::admit(&state_path).expect("state"), provider).expect("engine");
    let mut input = request(&workspace, "Complete a Goal");
    input.include_paths.clear();
    let outcome = engine.run(input).await.expect("Goal Run");
    engine.close().await.expect("engine shutdown");

    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let view = store
        .load_view(outcome.session_id)
        .await
        .expect("replayed Session")
        .expect("Session");
    let report = view.goals[0].report.as_ref().expect("durable report");
    assert_eq!(report.summary, summary);
    assert!(
        !report
            .unresolved
            .iter()
            .any(|item| item.contains("summary was shortened"))
    );
    store.close().await.expect("Store shutdown");
}

#[tokio::test]
async fn goal_failure_and_cancellation_have_distinct_terminal_receipts() {
    for cancel in [false, true] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state_path = temp.path().join("state");
        let cancellation = RunCancellation::new();
        let mut provider = if cancel {
            ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
                summary: "done".into(),
                result: "answer".into(),
            })])
        } else {
            ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
                summary: String::new(),
                result: "invalid finish".into(),
            })])
        };
        if cancel {
            provider.cancel_at_completion = Some((AgentPhase::RootPlan, cancellation.clone()));
        }
        let mut engine =
            Engine::open(StateRoot::admit(&state_path).expect("state"), provider).expect("engine");
        let mut input = request(&workspace, "Finish or cancel");
        input.include_paths.clear();
        let outcome = engine
            .run_with_cancel(input, cancellation)
            .await
            .expect("terminal Run");
        assert_eq!(
            outcome.run.status,
            if cancel {
                RunStatus::Cancelled
            } else {
                RunStatus::Failed
            }
        );
        engine.close().await.expect("engine shutdown");

        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only Store");
        let view = store
            .load_view(outcome.session_id)
            .await
            .expect("replayed Session")
            .expect("Session");
        assert_eq!(
            view.goals[0].status,
            if cancel {
                arany::GoalStatus::Cancelled
            } else {
                arany::GoalStatus::Failed
            }
        );
        assert!(view.goals[0].report.is_some());
        store.close().await.expect("Store shutdown");
    }
}

#[tokio::test]
async fn goal_call_budget_blocks_a_later_run_before_provider_dispatch() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
        summary: "first Run complete".into(),
        result: "continue later".into(),
    })]);
    let requests = Arc::clone(&provider.requests);
    let mut engine =
        Engine::open(StateRoot::admit(&state_path).expect("state"), provider).expect("engine");
    let mut first = request(&workspace, "Use one Provider call");
    first.include_paths.clear();
    first.goal = arany::GoalRequest::new(
        arany::GoalBudget {
            max_provider_calls: 1,
            ..arany::GoalBudget::default()
        },
        arany::GoalPhase::Implementation,
    );
    first.goal.finish_after_run = false;
    let initial = engine.run(first).await.expect("first Goal Run");

    let mut second = request(&workspace, "Try to use another Provider call");
    second.session_id = Some(initial.session_id);
    second.title = None;
    second.include_paths.clear();
    second.goal = arany::GoalRequest::resume(initial.goal_id, true);
    let result = engine.run(second).await;
    assert!(
        matches!(&result, Err(EngineError::GoalBudgetExhausted)),
        "unexpected second-Run result: {}",
        result
            .as_ref()
            .err()
            .map_or_else(|| "Run succeeded".into(), |error| format!("{error:?}"))
    );
    assert_eq!(requests.lock().expect("Provider requests").len(), 1);
    engine.close().await.expect("engine shutdown");
}

struct FiniteTokenTeam {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

impl Provider for FiniteTokenTeam {
    fn profile_name(&self) -> &str {
        "scripted"
    }

    fn model_name(&self) -> &str {
        "goal-token-team-model"
    }

    fn max_concurrent_calls(&self) -> u8 {
        3
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let phase = request.phase;
        self.requests
            .lock()
            .expect("Provider requests")
            .push(request);
        let (outcome, input_tokens, output_tokens) = match phase {
            AgentPhase::RootPlan => (
                ProviderOutcome::Delegate(Delegate {
                    children: vec![
                        "Review one".into(),
                        "Review two".into(),
                        "Review three".into(),
                    ],
                }),
                0,
                1,
            ),
            AgentPhase::ChildWork => (
                ProviderOutcome::Finish(Finish {
                    summary: "Review complete".into(),
                    result: "No issues found".into(),
                }),
                1,
                2,
            ),
            _ => return Err(ProviderError::InvalidOutcome),
        };
        Ok(ProviderResponse {
            outcome,
            response_id: None,
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            wire_provenance: None,
        })
    }

    async fn compact(&self, _: CompactionRequest) -> Result<CompactionResponse, ProviderError> {
        Err(ProviderError::InvalidOutcome)
    }
}

#[tokio::test]
async fn finite_goal_token_budget_observes_a_child_before_filling_another_slot() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let provider = FiniteTokenTeam {
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let requests = Arc::clone(&provider.requests);
    let mut engine =
        Engine::open(StateRoot::admit(&state_path).expect("state"), provider).expect("engine");
    let mut input = request(&workspace, "Review the task with a finite token budget");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Team {
        max_active_children: 3,
    };
    input.goal = arany::GoalRequest::new(
        arany::GoalBudget {
            max_tokens: Some(4),
            ..arany::GoalBudget::default()
        },
        arany::GoalPhase::Implementation,
    );
    let outcome = engine.run(input).await.expect("bounded team Run");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    {
        let requests = requests.lock().expect("Provider requests");
        assert_eq!(requests.len(), 2, "the queued children have not dispatched");
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.phase == AgentPhase::ChildWork)
                .count(),
            1,
            "the exact-cap child is observed before another queued child is admitted"
        );
    }
    engine.close().await.expect("engine shutdown");
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let events = store
        .load_session(outcome.session_id)
        .await
        .expect("Events");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(&event.event, Event::GoalProviderCallReserved { .. }))
            .count(),
        2,
        "only dispatched calls consume durable call reservations"
    );
    store.close().await.expect("read-only Store shutdown");
}

#[tokio::test]
async fn interrupted_goal_resumes_only_by_exact_id_without_replaying_the_old_call() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let first_provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
        summary: "first Run complete".into(),
        result: "continue after review".into(),
    })]);
    let first_requests = Arc::clone(&first_provider.requests);
    let mut first_engine = Engine::open(
        StateRoot::admit(&state_path).expect("state"),
        first_provider,
    )
    .expect("first engine");
    let mut first = request(&workspace, "Inspect current state");
    first.include_paths.clear();
    first.goal = arany::GoalRequest::new(
        arany::GoalBudget {
            max_runs: 2,
            ..arany::GoalBudget::default()
        },
        arany::GoalPhase::Research,
    );
    first.goal.finish_after_run = false;
    let initial = first_engine.run(first).await.expect("first Goal Run");
    assert_eq!(first_requests.lock().expect("Provider requests").len(), 1);
    first_engine.close().await.expect("first engine shutdown");

    let second_provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
        summary: "resumed Run complete".into(),
        result: "the Goal is complete".into(),
    })]);
    let second_requests = Arc::clone(&second_provider.requests);
    let mut second_engine = Engine::open(
        StateRoot::open_existing(&state_path).expect("reopened state"),
        second_provider,
    )
    .expect("second engine");
    let mut mismatched = request(&workspace, "Must not select a different Goal");
    mismatched.session_id = Some(initial.session_id);
    mismatched.title = None;
    mismatched.include_paths.clear();
    mismatched.goal = arany::GoalRequest::resume(arany::GoalId::new(), true);
    let result = second_engine.run(mismatched).await;
    assert!(
        matches!(&result, Err(EngineError::GoalUnavailable)),
        "unexpected mismatched-resume result: {}",
        result
            .as_ref()
            .err()
            .map_or_else(|| "Run succeeded".into(), |error| format!("{error:?}"))
    );
    assert!(
        second_requests
            .lock()
            .expect("Provider requests")
            .is_empty()
    );

    let mut resumed = request(&workspace, "Complete the reviewed Goal");
    resumed.session_id = Some(initial.session_id);
    resumed.title = None;
    resumed.include_paths.clear();
    resumed.goal = arany::GoalRequest::resume(initial.goal_id, true);
    let final_run = second_engine.run(resumed).await.expect("exact Goal resume");
    assert_eq!(second_requests.lock().expect("Provider requests").len(), 1);
    second_engine.close().await.expect("second engine shutdown");

    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let events = store
        .load_session(initial.session_id)
        .await
        .expect("Events");
    let view = SessionView::replay(initial.session_id, &events)
        .expect("strict replay")
        .expect("Session");
    assert_eq!(view.goals[0].id, initial.goal_id);
    assert_eq!(view.goals[0].status, arany::GoalStatus::Completed);
    assert_eq!(view.goals[0].run_ids, [initial.run.id, final_run.run.id]);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(&event.event, Event::GoalResumed { .. }))
            .count(),
        1
    );
    assert_eq!(
        view.runs.len(),
        2,
        "old Run remains history; only one new Run executes"
    );
    store.close().await.expect("Store shutdown");
}

#[tokio::test]
async fn interrupted_preaccept_goal_prefixes_resume_without_spending_runs_or_replaying_work() {
    use std::os::unix::fs::MetadataExt;

    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let metadata = std::fs::metadata(&workspace).expect("Workspace identity");
    for prefix in 0..3 {
        let state_path = temp.path().join(format!("state-{prefix}"));
        let session_id = SessionId::new();
        let goal_id = arany::GoalId::new();
        let orphan_run_id = RunId::new();
        let store = Store::open(StateRoot::admit(&state_path).expect("state")).expect("Store");
        store
            .append(
                session_id,
                Event::SessionStarted {
                    title: "Interrupted before acceptance".into(),
                    workspace_identity: (metadata.dev(), metadata.ino()),
                },
            )
            .await
            .expect("Session start");
        store
            .append(
                session_id,
                Event::GoalStarted {
                    goal_id,
                    phase: arany::GoalPhase::Implementation,
                    autonomous: false,
                    objective: "Resume an unaccepted task".into(),
                    workspace_identity: (metadata.dev(), metadata.ino()),
                    budget: arany::GoalBudget {
                        max_runs: 1,
                        ..arany::GoalBudget::default()
                    },
                },
            )
            .await
            .expect("Goal start");
        if prefix >= 1 {
            store
                .append(
                    session_id,
                    Event::GoalRunLinked {
                        goal_id,
                        run_id: orphan_run_id,
                        phase: arany::GoalPhase::Implementation,
                    },
                )
                .await
                .expect("preaccept Run link");
        }
        if prefix >= 2 {
            store
                .append(session_id, Event::GoalResumed { goal_id })
                .await
                .expect("preaccept resume acknowledgment");
        }
        store.close().await.expect("seed Store shutdown");

        let provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
            summary: "Completed after explicit recovery".into(),
            result: "Only the new Run was dispatched".into(),
        })]);
        let requests = Arc::clone(&provider.requests);
        let mut engine = Engine::open(
            StateRoot::open_existing(&state_path).expect("reopened state"),
            provider,
        )
        .expect("Engine");
        let mut mismatched = request(&workspace, "A different initial objective");
        mismatched.session_id = Some(session_id);
        mismatched.title = None;
        mismatched.include_paths.clear();
        mismatched.goal = arany::GoalRequest::resume(goal_id, true);
        assert!(matches!(
            engine.run(mismatched).await,
            Err(EngineError::GoalUnavailable)
        ));
        assert!(requests.lock().expect("Provider requests").is_empty());

        let mut resumed = request(&workspace, "Resume an unaccepted task");
        resumed.session_id = Some(session_id);
        resumed.title = None;
        resumed.include_paths.clear();
        resumed.goal = arany::GoalRequest::resume(goal_id, true);
        let outcome = engine.run(resumed).await.expect("explicit recovery");
        assert_ne!(outcome.run.id, orphan_run_id);
        assert_eq!(outcome.run.status, RunStatus::Finished);
        assert_eq!(requests.lock().expect("Provider requests").len(), 1);
        engine.close().await.expect("Engine shutdown");

        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only Store");
        let view = store
            .load_view(session_id)
            .await
            .expect("replayed Session")
            .expect("Session");
        assert_eq!(view.goals[0].status, arany::GoalStatus::Completed);
        assert_eq!(view.goals[0].run_ids, [outcome.run.id]);
        assert_eq!(view.runs.len(), 1);
        store.close().await.expect("read-only Store shutdown");
    }
}

struct ExactCapToolReview;

impl Provider for ExactCapToolReview {
    fn profile_name(&self) -> &str {
        "scripted"
    }

    fn model_name(&self) -> &str {
        "exact-cap-model"
    }

    fn max_concurrent_calls(&self) -> u8 {
        1
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let (outcome, input_tokens, output_tokens) = match request.phase {
            AgentPhase::RootPlan => (
                ProviderOutcome::Tool {
                    call: arany::ToolCall::Write {
                        path: "notes/a.txt".into(),
                        expected_digest: None,
                        content: "x".into(),
                    },
                    plan_contract: Some(Box::new(arany::GoalPlanContract {
                        mission: "Write one note".into(),
                        paths: vec!["notes".into()],
                        commands: Vec::new(),
                        skills: Vec::new(),
                        mcp_tools: Vec::new(),
                        success_criterion: arany::GoalSuccessCriterion::WorkspaceFileContent {
                            path: "notes/a.txt".into(),
                            content: "x".into(),
                        },
                        budget: arany::GoalBudget {
                            max_tokens: Some(10),
                            ..arany::GoalBudget::default()
                        },
                    })),
                },
                3,
                2,
            ),
            AgentPhase::ToolReview => (
                ProviderOutcome::Finish(Finish {
                    summary: "Approve the note".into(),
                    result: "approve".into(),
                }),
                3,
                2,
            ),
            _ => return Err(ProviderError::InvalidOutcome),
        };
        Ok(ProviderResponse {
            outcome,
            response_id: None,
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            wire_provenance: None,
        })
    }

    async fn compact(&self, _: CompactionRequest) -> Result<CompactionResponse, ProviderError> {
        Err(ProviderError::InvalidOutcome)
    }
}

#[tokio::test]
async fn exact_token_cap_after_tool_review_settles_the_goal_as_failed() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
    let state_path = temp.path().join("state");
    let mut engine = Engine::open(
        StateRoot::admit(&state_path).expect("state"),
        ExactCapToolReview,
    )
    .expect("engine");
    {
        use std::os::unix::fs::PermissionsExt;
        let config = state_path.join("tools.json");
        std::fs::write(
            &config,
            br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#,
        )
        .expect("tool config");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
            .expect("tool config mode");
    }
    engine
        .enable_tools(env!("CARGO_BIN_EXE_arany").into())
        .expect("tools");
    let (approvals, _inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Auto);
    engine.set_tool_approvals(approvals);
    let mut input = request(&workspace, "Write one note at the exact token cap");
    input.include_paths.clear();
    input.goal = arany::GoalRequest::new(
        arany::GoalBudget {
            max_tokens: Some(10),
            ..arany::GoalBudget::default()
        },
        arany::GoalPhase::Implementation,
    );
    let outcome = engine
        .run(input)
        .await
        .expect("exhaustion ends the Run as a durable failure");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    assert!(
        !workspace.join("notes/a.txt").exists(),
        "the exact-cap Tool was not dispatched"
    );
    engine.close().await.expect("engine shutdown");

    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let view = store
        .load_view(outcome.session_id)
        .await
        .expect("replayed Session")
        .expect("Session");
    assert_eq!(view.goals[0].status, arany::GoalStatus::Failed);
    assert!(view.goals[0].report.is_some());
    store.close().await.expect("Store shutdown");
}

struct PendingChildren {
    in_flight: Arc<AtomicUsize>,
}

struct InFlightCall(Arc<AtomicUsize>);

impl Drop for InFlightCall {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Provider for PendingChildren {
    fn profile_name(&self) -> &str {
        "scripted"
    }

    fn model_name(&self) -> &str {
        "pending-children-model"
    }

    fn max_concurrent_calls(&self) -> u8 {
        2
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        match request.phase {
            AgentPhase::RootPlan => Ok(ProviderResponse {
                outcome: ProviderOutcome::Delegate(Delegate {
                    children: vec!["First child".into(), "Second child".into()],
                }),
                response_id: None,
                input_tokens: Some(0),
                output_tokens: Some(1),
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
    let mut engine = Engine::open(
        StateRoot::admit(&state_path).expect("state"),
        PendingChildren {
            in_flight: Arc::clone(&in_flight),
        },
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
    let result = engine.run(input).await;
    assert!(matches!(result, Err(EngineError::Store(_))));
    assert_eq!(
        in_flight.load(Ordering::Acquire),
        0,
        "the admitted child call is drained before the store error returns"
    );
    engine.close().await.expect("engine shutdown");
}

#[tokio::test]
async fn resumed_goal_keeps_its_stored_phase() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let first_provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
        summary: "first Run complete".into(),
        result: "continue after review".into(),
    })]);
    let mut first_engine = Engine::open(
        StateRoot::admit(&state_path).expect("state"),
        first_provider,
    )
    .expect("first engine");
    let mut first = request(&workspace, "Inspect current state");
    first.include_paths.clear();
    first.goal = arany::GoalRequest::new(
        arany::GoalBudget {
            max_runs: 2,
            ..arany::GoalBudget::default()
        },
        arany::GoalPhase::Research,
    );
    first.goal.finish_after_run = false;
    let initial = first_engine.run(first).await.expect("first Goal Run");
    first_engine.close().await.expect("first engine shutdown");

    let second_provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
        summary: "resumed Run complete".into(),
        result: "the Goal is complete".into(),
    })]);
    let mut second_engine = Engine::open(
        StateRoot::open_existing(&state_path).expect("reopened state"),
        second_provider,
    )
    .expect("second engine");
    let mut resumed = request(&workspace, "Complete the reviewed Goal");
    resumed.session_id = Some(initial.session_id);
    resumed.title = None;
    resumed.include_paths.clear();
    resumed.goal = arany::GoalRequest::resume(initial.goal_id, true);
    let final_run = second_engine.run(resumed).await.expect("exact Goal resume");
    second_engine.close().await.expect("second engine shutdown");

    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let events = store
        .load_session(initial.session_id)
        .await
        .expect("Events");
    let linked_phases: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::GoalRunLinked { run_id, phase, .. } if *run_id == final_run.run.id => {
                Some(*phase)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        linked_phases,
        [arany::GoalPhase::Research],
        "a resumed Goal must keep the phase it was started with"
    );
    store.close().await.expect("Store shutdown");
}

const AUTONOMOUS_OBJECTIVE: &str = "Verify the facts file";

fn finish_with(text: &str) -> ProviderOutcome {
    ProviderOutcome::Finish(Finish {
        summary: text.into(),
        result: text.into(),
    })
}

fn facts_digest(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn facts_read() -> arany::ToolCall {
    arany::ToolCall::Read {
        path: "notes/facts.txt".into(),
        offset: 0,
        limit: 4096,
    }
}

/// A plan whose only success evidence is a verified read of `notes/facts.txt` with `sha256`.
fn facts_plan(sha256: String) -> arany::GoalPlanContract {
    arany::GoalPlanContract {
        mission: AUTONOMOUS_OBJECTIVE.into(),
        paths: vec!["notes/facts.txt".into()],
        commands: Vec::new(),
        skills: Vec::new(),
        mcp_tools: Vec::new(),
        success_criterion: arany::GoalSuccessCriterion::WorkspaceFileDigest {
            path: "notes/facts.txt".into(),
            sha256,
        },
        budget: arany::GoalBudget::default(),
    }
}

fn facts_workspace(root: &Path, text: &str) -> std::path::PathBuf {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
    std::fs::write(workspace.join("notes/facts.txt"), text).expect("facts file");
    workspace
}

fn write_facts(workspace: &Path, text: &'static str) -> impl Fn() + Send + Sync + 'static {
    let path = workspace.join("notes/facts.txt");
    move || std::fs::write(&path, text).expect("updated facts file")
}

/// Opens the Engine with Guard-backed Read access to `notes/facts.txt`.
fn facts_engine<P: Provider + 'static>(
    state: StateRoot,
    state_path: &Path,
    provider: P,
) -> Engine<P> {
    facts_engine_with_telemetry(state, state_path, provider, arany::Telemetry::disabled())
}

fn facts_engine_with_telemetry<P: Provider + 'static>(
    state: StateRoot,
    state_path: &Path,
    provider: P,
    telemetry: arany::Telemetry,
) -> Engine<P> {
    use std::os::unix::fs::PermissionsExt;
    let mut engine = Engine::open_with_telemetry(state, provider, telemetry).expect("engine");
    let config = state_path.join("tools.json");
    std::fs::write(
        &config,
        br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#,
    )
    .expect("tool configuration");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
        .expect("tool configuration mode");
    engine
        .enable_tools(env!("CARGO_BIN_EXE_arany").into())
        .expect("Guard enabled");
    engine
}

fn autonomous_input(workspace: &Path) -> RunRequest {
    let mut input = request(workspace, AUTONOMOUS_OBJECTIVE);
    input.include_paths.clear();
    input.goal = arany::GoalRequest::autonomous(
        arany::GoalBudget::default(),
        arany::GoalPhase::Implementation,
    );
    input
}

async fn replayed(
    state_path: &Path,
    session_id: arany::SessionId,
) -> (Vec<arany::EventEnvelope>, SessionView) {
    let store = Store::open_read_only(StateRoot::open_existing(state_path).expect("state"))
        .expect("read-only Store");
    let events = store.load_session(session_id).await.expect("Events");
    let view = SessionView::replay(session_id, &events)
        .expect("strict replay")
        .expect("Session");
    store.close().await.expect("Store shutdown");
    (events, view)
}

/// Interrupts an autonomous Goal after its second Run starts. Returns the Session and Goal IDs.
async fn interrupted_autonomous_goal(
    state_path: &Path,
    workspace: &Path,
) -> (arany::SessionId, arany::GoalId) {
    let cancellation = RunCancellation::new();
    let cancel = cancellation.clone();
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Tool {
            call: facts_read(),
            plan_contract: Some(Box::new(facts_plan(facts_digest("v2")))),
        },
        finish_with("first pass"),
        finish_with("interrupted pass"),
    ])
    .with_before_call(2, move || assert!(cancel.cancel(), "first cancellation"));
    let mut engine = facts_engine(
        StateRoot::admit(state_path).expect("state"),
        state_path,
        provider,
    );
    let outcome = engine
        .run_with_cancel(autonomous_input(workspace), cancellation)
        .await
        .expect("cancelled Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");
    (outcome.session_id, outcome.goal_id)
}

#[tokio::test]
async fn autonomous_goal_iterates_until_its_success_criterion_is_satisfied() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Tool {
            call: facts_read(),
            plan_contract: Some(Box::new(facts_plan(facts_digest("v2")))),
        },
        finish_with("first pass"),
        ProviderOutcome::Tool {
            call: facts_read(),
            plan_contract: None,
        },
        finish_with("verified"),
    ])
    .with_before_call(2, write_facts(&workspace, "v2"));
    let requests = Arc::clone(&provider.requests);
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let outcome = engine
        .run(autonomous_input(&workspace))
        .await
        .expect("autonomous Goal");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    {
        let calls = requests.lock().expect("Provider requests");
        assert_eq!(calls.len(), 4, "two Runs, each a Read then a Finish");
        assert_eq!(
            calls[2].objective, AUTONOMOUS_OBJECTIVE,
            "the next Run takes the pinned objective and no new text"
        );
    }
    engine.close().await.expect("engine shutdown");

    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert_eq!(goal.status, arany::GoalStatus::Completed);
    assert_eq!(goal.run_ids.len(), 2);
    assert!(
        !events
            .iter()
            .any(|event| matches!(&event.event, Event::GoalResumed { .. })),
        "autonomous continuations are not explicit resumes"
    );
}

#[tokio::test]
async fn autonomous_goal_stops_at_its_run_budget_and_fails_with_a_report() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget.max_runs = 2;
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Tool {
            call: facts_read(),
            plan_contract: Some(Box::new(plan)),
        },
        finish_with("first pass"),
        finish_with("second pass"),
        finish_with("never requested"),
    ]);
    let requests = Arc::clone(&provider.requests);
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let mut input = autonomous_input(&workspace);
    input.goal = arany::GoalRequest::autonomous(
        arany::GoalBudget {
            max_runs: 2,
            ..arany::GoalBudget::default()
        },
        arany::GoalPhase::Implementation,
    );
    let outcome = engine.run(input).await.expect("terminal Run");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    assert_eq!(
        requests.lock().expect("Provider requests").len(),
        3,
        "the second Run's Finish is refused, and no third Run dispatches"
    );
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, outcome.session_id).await;
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert_eq!(goal.status, arany::GoalStatus::Failed);
    assert_eq!(goal.run_ids.len(), 2);
    assert!(goal.report.is_some(), "exhausted Goal keeps its report");
}

#[tokio::test]
async fn autonomous_goal_call_cap_at_its_boundary_fails_the_goal_with_a_report() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let budget = arany::GoalBudget {
        max_provider_calls: 3,
        ..arany::GoalBudget::default()
    };
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget = budget.clone();
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Tool {
            call: facts_read(),
            plan_contract: Some(Box::new(plan)),
        },
        finish_with("first pass"),
        finish_with("second pass"),
        finish_with("never requested"),
    ]);
    let requests = Arc::clone(&provider.requests);
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let mut input = autonomous_input(&workspace);
    input.goal = arany::GoalRequest::autonomous(budget, arany::GoalPhase::Implementation);
    // The second Run's Finish is the third and last admissible call.
    let outcome = engine.run(input).await.expect("terminal Run");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    assert_eq!(
        requests.lock().expect("Provider requests").len(),
        3,
        "the call cap admits no fourth Provider call"
    );
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, outcome.session_id).await;
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert_eq!(goal.status, arany::GoalStatus::Failed);
    assert_eq!(goal.provider_calls, 3);
    assert!(goal.report.is_some(), "exhausted Goal keeps its report");
}

/// Reports the same input-token count for every call, so a token cap can land on an exact boundary.
struct MeteredProvider {
    tokens: u32,
    responses: std::sync::Mutex<VecDeque<ProviderOutcome>>,
}

impl Provider for MeteredProvider {
    fn profile_name(&self) -> &str {
        "scripted"
    }

    fn model_name(&self) -> &str {
        "metered-model"
    }

    fn max_concurrent_calls(&self) -> u8 {
        1
    }

    async fn invoke(&self, _request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let outcome = self
            .responses
            .lock()
            .expect("responses lock")
            .pop_front()
            .ok_or(ProviderError::InvalidOutcome)?;
        Ok(ProviderResponse {
            outcome,
            response_id: None,
            input_tokens: Some(self.tokens),
            output_tokens: Some(0),
            wire_provenance: None,
        })
    }

    async fn compact(&self, _: CompactionRequest) -> Result<CompactionResponse, ProviderError> {
        Err(ProviderError::InvalidOutcome)
    }
}

#[tokio::test]
async fn autonomous_goal_token_cap_at_its_boundary_fails_the_goal_with_a_report() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let budget = arany::GoalBudget {
        max_tokens: Some(10),
        ..arany::GoalBudget::default()
    };
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget = budget.clone();
    // Five input tokens per call: the Read and the Finish reach the 10-token cap exactly.
    let provider = MeteredProvider {
        tokens: 5,
        responses: std::sync::Mutex::new(VecDeque::from([
            ProviderOutcome::Tool {
                call: facts_read(),
                plan_contract: Some(Box::new(plan)),
            },
            finish_with("first pass"),
            finish_with("never requested"),
        ])),
    };
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let mut input = autonomous_input(&workspace);
    input.goal = arany::GoalRequest::autonomous(budget, arany::GoalPhase::Implementation);
    let outcome = engine.run(input).await.expect("terminal Run");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, outcome.session_id).await;
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert_eq!(goal.status, arany::GoalStatus::Failed);
    assert_eq!(goal.provider_calls, 2, "no third Provider call is reserved");
    assert_eq!(goal.known_tokens, 10);
    assert!(goal.report.is_some(), "exhausted Goal keeps its report");
}

#[tokio::test]
async fn autonomous_goal_cancellation_stops_the_loop_and_leaves_the_goal_resumable() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let (session_id, goal_id) = interrupted_autonomous_goal(&state_path, &workspace).await;

    let (events, view) = replayed(&state_path, session_id).await;
    let goal = view.goal(goal_id).expect("Goal");
    assert_eq!(goal.status, arany::GoalStatus::Interrupted);
    assert_eq!(
        goal.run_ids.len(),
        2,
        "the loop starts no Run after cancellation"
    );
    assert!(goal.report.is_none());
    assert!(
        !events
            .iter()
            .any(|event| matches!(&event.event, Event::GoalFinished { .. })),
        "cancellation leaves the Goal open for explicit resume"
    );
}

#[tokio::test]
async fn resumed_autonomous_goal_continues_with_its_pinned_objective_and_no_new_text() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let (session_id, goal_id) = interrupted_autonomous_goal(&state_path, &workspace).await;

    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Tool {
            call: facts_read(),
            plan_contract: None,
        },
        finish_with("resumed, still unverified"),
        ProviderOutcome::Tool {
            call: facts_read(),
            plan_contract: None,
        },
        finish_with("verified"),
    ])
    .with_before_call(2, write_facts(&workspace, "v2"));
    let requests = Arc::clone(&provider.requests);
    let mut engine = facts_engine(
        StateRoot::open_existing(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let mut input = request(&workspace, "");
    input.session_id = Some(session_id);
    input.title = None;
    input.include_paths.clear();
    input.goal = arany::GoalRequest::resume(goal_id, true);
    let outcome = engine.run(input).await.expect("explicit resume");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    {
        let calls = requests.lock().expect("Provider requests");
        assert_eq!(
            calls.len(),
            4,
            "the resumed loop runs two Runs and no replayed call"
        );
        assert_eq!(calls[0].objective, AUTONOMOUS_OBJECTIVE);
        assert_eq!(calls[2].objective, AUTONOMOUS_OBJECTIVE);
    }
    engine.close().await.expect("engine shutdown");

    let (events, view) = replayed(&state_path, session_id).await;
    let goal = view.goal(goal_id).expect("Goal");
    assert_eq!(goal.status, arany::GoalStatus::Completed);
    assert_eq!(goal.run_ids.len(), 4);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(&event.event, Event::GoalResumed { .. }))
            .count(),
        1
    );
    let observations: usize = view.runs.iter().map(|run| run.tools.len()).sum();
    assert_eq!(
        observations, 3,
        "earlier Read effects are history; only the new Reads dispatch"
    );
}

#[tokio::test]
async fn plain_prompt_is_one_run_even_when_its_criterion_is_unsatisfied() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Tool {
            call: facts_read(),
            plan_contract: Some(Box::new(facts_plan(facts_digest("v2")))),
        },
        finish_with("first pass"),
        finish_with("never requested"),
    ]);
    let requests = Arc::clone(&provider.requests);
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let mut input = request(&workspace, AUTONOMOUS_OBJECTIVE);
    input.include_paths.clear();
    let outcome = engine.run(input).await.expect("terminal Run");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    assert_eq!(requests.lock().expect("Provider requests").len(), 2);
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, outcome.session_id).await;
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert_eq!(goal.status, arany::GoalStatus::Failed);
    assert_eq!(goal.run_ids, [outcome.run.id]);
}

/// Action taken once a Run's Finish is committed and observed through progress.
enum AfterFinish {
    Cancel(RunCancellation),
    AdvanceClock(Duration),
}

/// Drives an admitted Run and applies `after` as soon as its Finished Run becomes visible.
async fn run_observing_finish(
    engine: &mut Engine<ScriptedProvider>,
    input: RunRequest,
    after: AfterFinish,
) -> Result<arany::RunOutcome, EngineError> {
    let cancellation = match &after {
        AfterFinish::Cancel(cancellation) => cancellation.clone(),
        AfterFinish::AdvanceClock(_) => RunCancellation::new(),
    };
    let progress = RunProgress::new();
    let mut observer = progress.clone();
    let run = engine.run_with_progress(input, cancellation, progress);
    tokio::pin!(run);
    let mut after = Some(after);
    loop {
        tokio::select! {
            result = &mut run => return result,
            update = observer.changed() => {
                if update.run.status == RunStatus::Finished && let Some(action) = after.take() {
                    match action {
                        AfterFinish::Cancel(cancellation) => {
                            cancellation.cancel();
                        }
                        AfterFinish::AdvanceClock(duration) => tokio::time::advance(duration).await,
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn autonomous_goal_cancelled_after_a_finished_run_keeps_that_outcome_and_stays_open() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Tool {
            call: facts_read(),
            plan_contract: Some(Box::new(facts_plan(facts_digest("v2")))),
        },
        finish_with("first pass"),
        finish_with("never requested"),
    ]);
    let requests = Arc::clone(&provider.requests);
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let outcome = run_observing_finish(
        &mut engine,
        autonomous_input(&workspace),
        AfterFinish::Cancel(RunCancellation::new()),
    )
    .await
    .expect("the committed Finished Run is returned");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    assert_eq!(
        requests.lock().expect("Provider requests").len(),
        2,
        "cancellation after the first Finish starts no second Run"
    );
    engine.close().await.expect("engine shutdown");

    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert_eq!(goal.status, arany::GoalStatus::Interrupted);
    assert_eq!(goal.run_ids.len(), 1);
    assert!(goal.report.is_none());
    assert!(
        !events
            .iter()
            .any(|event| matches!(&event.event, Event::GoalFinished { .. })),
        "the Goal stays open for explicit resume"
    );
}

/// The Finished Run stays committed and the stop names the exhausted Goal budget.
fn stopped_by_budget(result: &Result<arany::RunOutcome, EngineError>) -> bool {
    matches!(
        result,
        Err(EngineError::StoppedAfterRun { reason, .. })
            if matches!(**reason, EngineError::GoalBudgetExhausted)
    )
}

#[tokio::test(start_paused = true)]
async fn autonomous_goal_active_time_spent_after_a_finish_fails_the_goal_with_its_report() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let budget = arany::GoalBudget {
        max_active_ms: 60_000,
        ..arany::GoalBudget::default()
    };
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget = budget.clone();
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Tool {
            call: facts_read(),
            plan_contract: Some(Box::new(plan)),
        },
        finish_with("first pass"),
        finish_with("never requested"),
    ]);
    let requests = Arc::clone(&provider.requests);
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let mut input = autonomous_input(&workspace);
    input.goal = arany::GoalRequest::autonomous(budget, arany::GoalPhase::Implementation);
    // Terminal-fact recording after the Finish check spends the whole active budget.
    let result = run_observing_finish(
        &mut engine,
        input,
        AfterFinish::AdvanceClock(Duration::from_secs(60)),
    )
    .await;
    assert!(
        stopped_by_budget(&result),
        "the refused continuation reports the Goal failure: {:?}",
        result.as_ref().map(|outcome| outcome.run.status)
    );
    assert_eq!(
        requests.lock().expect("Provider requests").len(),
        2,
        "no third Provider call is dispatched"
    );
    engine.close().await.expect("engine shutdown");

    let session_id = arany::list_sessions(
        StateRoot::open_existing(&state_path).expect("state"),
        workspace.clone(),
    )
    .await
    .expect("Sessions")
    .first()
    .expect("Session")
    .id;
    let (events, view) = replayed(&state_path, session_id).await;
    let goal = view.goals.last().expect("Goal");
    assert_eq!(goal.status, arany::GoalStatus::Failed);
    assert!(goal.report.is_some());
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(&event.event, Event::GoalFinished { .. }))
            .count(),
        1
    );
}

/// Accepts OTLP trace POSTs on a loopback listener until `done` is set after the final flush.
fn collect_trace_bodies(
    listener: std::net::TcpListener,
    done: Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<Vec<Vec<u8>>> {
    std::thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .expect("nonblocking Collector");
        let mut bodies = Vec::new();
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => bodies.push(read_trace_body(&mut stream)),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if done.load(std::sync::atomic::Ordering::Acquire) {
                        return bodies;
                    }
                    std::thread::yield_now();
                }
                Err(error) => panic!("Collector accept failed: {error}"),
            }
        }
    })
}

fn read_trace_body(stream: &mut std::net::TcpStream) -> Vec<u8> {
    use std::io::{Read, Write};
    stream
        .set_nonblocking(false)
        .expect("blocking Collector stream");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("Collector deadline");
    let mut request = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).expect("trace request bytes");
        assert!(count > 0, "trace request ended before its body");
        request.extend_from_slice(&chunk[..count]);
        let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
            continue;
        };
        let header_end = end + 4;
        let header = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
        let body_len = header
            .lines()
            .find_map(|line| {
                line.strip_prefix("content-length: ")?
                    .trim()
                    .parse::<usize>()
                    .ok()
            })
            .expect("trace Content-Length");
        if request.len() >= header_end + body_len {
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .expect("Collector acknowledgment");
            return request[header_end..header_end + body_len].to_vec();
        }
    }
}

/// The string value of `key` on every exported Goal segment span, in export order.
fn goal_segment_attribute(bodies: &[Vec<u8>], key: &str) -> Vec<String> {
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    use prost::Message;
    bodies
        .iter()
        .flat_map(|body| {
            ExportTraceServiceRequest::decode(body.as_slice())
                .expect("OTLP protobuf")
                .resource_spans
        })
        .flat_map(|resource| resource.scope_spans)
        .flat_map(|scope| scope.spans)
        .filter(|span| span.name == "invoke_workflow arany.goal")
        .filter_map(|span| {
            span.attributes
                .into_iter()
                .find(|attribute| attribute.key == key)
                .and_then(|attribute| attribute.value)
                .and_then(|value| match value.value {
                    Some(Value::StringValue(status)) => Some(status),
                    _ => None,
                })
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn exhausted_autonomous_segment_exports_its_failed_status() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let budget = arany::GoalBudget {
        max_active_ms: 60_000,
        ..arany::GoalBudget::default()
    };
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget = budget.clone();
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Tool {
            call: facts_read(),
            plan_contract: Some(Box::new(plan)),
        },
        finish_with("first pass"),
        finish_with("never requested"),
    ]);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("Collector listener");
    let endpoint = format!(
        "http://{}",
        listener.local_addr().expect("Collector address")
    );
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let collector = collect_trace_bodies(listener, Arc::clone(&done));
    let telemetry = arany::Telemetry::from_process(Some(&endpoint)).expect("loopback telemetry");
    let mut engine = facts_engine_with_telemetry(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
        telemetry.clone(),
    );
    let mut input = autonomous_input(&workspace);
    input.goal = arany::GoalRequest::autonomous(budget, arany::GoalPhase::Implementation);
    // Active time spent after the Finish exhausts the Goal before its segment span closes.
    let result = run_observing_finish(
        &mut engine,
        input,
        AfterFinish::AdvanceClock(Duration::from_secs(60)),
    )
    .await;
    assert!(
        stopped_by_budget(&result),
        "the exhausted Goal reports its refusal: {:?}",
        result.as_ref().map(|outcome| outcome.run.status)
    );
    engine.close().await.expect("engine shutdown");
    telemetry.shutdown();
    done.store(true, std::sync::atomic::Ordering::Release);
    let bodies = collector.join().expect("Collector thread");
    assert_eq!(
        goal_segment_attribute(&bodies, "arany.goal.status"),
        vec!["failed".to_owned()],
        "the segment that decided exhaustion carries the Failed terminal status"
    );
}

#[tokio::test]
async fn progress_snapshots_carry_the_goal_through_each_iteration() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Tool {
            call: facts_read(),
            plan_contract: Some(Box::new(facts_plan(facts_digest("v2")))),
        },
        finish_with("first pass"),
        ProviderOutcome::Tool {
            call: facts_read(),
            plan_contract: None,
        },
        finish_with("verified"),
    ])
    .with_before_call(2, write_facts(&workspace, "v2"));
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let progress = RunProgress::new();
    let mut observer = progress.clone();
    let mut observed_runs = Vec::new();
    let outcome = {
        let run = engine.run_with_progress(
            autonomous_input(&workspace),
            RunCancellation::new(),
            progress,
        );
        tokio::pin!(run);
        loop {
            tokio::select! {
                update = observer.changed() => {
                    observed_runs.push(update.goal.as_ref().map(|goal| goal.run_ids.len()));
                }
                result = &mut run => break result.expect("autonomous Goal"),
            }
        }
    };
    assert_eq!(outcome.run.status, RunStatus::Finished);
    assert!(
        observed_runs.contains(&Some(1)),
        "the first iteration's snapshots name one Run: {observed_runs:?}"
    );
    assert_eq!(
        observed_runs.last(),
        Some(&Some(2)),
        "the final snapshot counts both iterations, not the first: {observed_runs:?}"
    );
    engine.close().await.expect("engine shutdown");
}

#[tokio::test]
async fn plan_with_fewer_runs_than_linked_is_refused_as_an_invalid_plan() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    // The first Run is cancelled before any plan, so the Goal stays open with one linked Run.
    let cancellation = RunCancellation::new();
    let cancel = cancellation.clone();
    let provider = ScriptedProvider::new(vec![finish_with("never requested")])
        .with_before_call(0, move || assert!(cancel.cancel(), "first cancellation"));
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let cancelled = engine
        .run_with_cancel(autonomous_input(&workspace), cancellation)
        .await
        .expect("cancelled Run");
    assert_eq!(cancelled.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");

    // Resuming links a second Run, so a plan allowing one Run is invalid for this Goal.
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget.max_runs = 1;
    let provider = ScriptedProvider::new(vec![ProviderOutcome::Tool {
        call: facts_read(),
        plan_contract: Some(Box::new(plan)),
    }]);
    let mut engine = facts_engine(
        StateRoot::open_existing(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let mut input = request(&workspace, "");
    input.session_id = Some(cancelled.session_id);
    input.title = None;
    input.include_paths.clear();
    input.goal = arany::GoalRequest::resume(cancelled.goal_id, true);
    let outcome = engine
        .run(input)
        .await
        .expect("an invalid plan is a model-visible Run failure, not a Store error");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, cancelled.session_id).await;
    let goal = view.goal(cancelled.goal_id).expect("Goal");
    assert_eq!(goal.run_ids.len(), 2);
    assert!(
        goal.plan_contract.is_none(),
        "the refused plan is not recorded"
    );
}

#[tokio::test]
async fn cancelled_autonomous_segment_exports_its_run_outcome() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let cancellation = RunCancellation::new();
    let cancel = cancellation.clone();
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Tool {
            call: facts_read(),
            plan_contract: Some(Box::new(facts_plan(facts_digest("v2")))),
        },
        finish_with("first pass"),
        finish_with("cancelled pass"),
    ])
    .with_before_call(2, move || assert!(cancel.cancel(), "first cancellation"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("Collector listener");
    let endpoint = format!(
        "http://{}",
        listener.local_addr().expect("Collector address")
    );
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let collector = collect_trace_bodies(listener, Arc::clone(&done));
    let telemetry = arany::Telemetry::from_process(Some(&endpoint)).expect("loopback telemetry");
    let mut engine = facts_engine_with_telemetry(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
        telemetry.clone(),
    );
    let outcome = engine
        .run_with_cancel(autonomous_input(&workspace), cancellation)
        .await
        .expect("cancelled Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");
    telemetry.shutdown();
    done.store(true, std::sync::atomic::Ordering::Release);
    let bodies = collector.join().expect("Collector thread");
    assert_eq!(
        goal_segment_attribute(&bodies, "arany.outcome"),
        vec!["finished".to_owned(), "cancelled".to_owned()],
        "a cancelled segment of an open Goal exports the Run's cancelled outcome"
    );
    assert_eq!(
        goal_segment_attribute(&bodies, "arany.goal.status"),
        vec!["running".to_owned(), "running".to_owned()],
        "the open Goal keeps its Running status"
    );
}

#[tokio::test]
async fn autonomous_goal_stopped_by_a_failed_admission_returns_the_committed_run() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    // Removing the Workspace after the first Run finishes makes the next admission fail.
    let removed = workspace.clone();
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Tool {
            call: facts_read(),
            plan_contract: Some(Box::new(facts_plan(facts_digest("v2")))),
        },
        finish_with("first pass"),
    ])
    .with_before_call(1, move || {
        std::fs::remove_dir_all(&removed).expect("Workspace removed")
    });
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let (committed, reason) = match engine.run(autonomous_input(&workspace)).await {
        Err(EngineError::StoppedAfterRun { run, reason }) => (run, reason),
        Err(error) => panic!("the committed Run must be returned with its stop reason: {error}"),
        Ok(_) => panic!("a failed continuation admission must stop the loop"),
    };
    assert_eq!(committed.run.status, RunStatus::Finished);
    assert_eq!(
        committed.run.assistant_message.as_deref(),
        Some("first pass"),
        "the committed answer is the first Run's"
    );
    assert!(!matches!(*reason, EngineError::GoalBudgetExhausted));
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, committed.session_id).await;
    assert_eq!(view.runs.last().map(|run| run.id), Some(committed.run.id));
    let goal = view.goal(committed.goal_id).expect("Goal");
    assert_eq!(
        goal.status,
        arany::GoalStatus::Interrupted,
        "a non-budget stop leaves the Goal open for explicit resume"
    );
}

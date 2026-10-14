use super::*;

#[tokio::test]
async fn goal_lifecycle_bounds_report_and_requires_goal_replay_linkage() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
        summary: "ࠀ".repeat(620),
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
    assert_eq!(report.summary.len() % 3, 0, "UTF-8 truncation boundary");
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
async fn long_read_only_goal_compacts_only_an_acknowledged_run_prefix() {
    const PAGE_BYTES: usize = 4096;
    const READS: usize = 104;

    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
    std::fs::write(
        workspace.join("notes/data.txt"),
        "x".repeat(PAGE_BYTES * READS),
    )
    .expect("bounded fixture file");
    let state_path = temp.path().join("state");

    let mut outcomes = (0..READS)
        .map(|page| ProviderOutcome::Tool {
            call: arany::ToolCall::Read {
                path: "notes/data.txt".into(),
                offset: u32::try_from(page * PAGE_BYTES).expect("bounded file offset"),
                limit: u32::try_from(PAGE_BYTES).expect("bounded page size"),
            },
            plan_contract: None,
        })
        .collect::<Vec<_>>();
    outcomes.push(ProviderOutcome::Finish(Finish {
        summary: "Read-only inspection completed".into(),
        result: "The bounded pages were inspected".into(),
    }));
    let good_compaction = || {
        Ok(CompactionResponse {
            summary:
                "Earlier pages were read; their exact results remain in the canonical journal."
                    .into(),
            response_id: None,
            input_tokens: Some(32),
            output_tokens: Some(12),
            wire_provenance: None,
        })
    };
    let provider = ScriptedProvider::new(outcomes).with_compactions(
        std::iter::once(Ok(CompactionResponse {
            summary: String::new(),
            response_id: None,
            input_tokens: None,
            output_tokens: Some(8),
            wire_provenance: None,
        }))
        .chain((0..8).map(|_| good_compaction()))
        .collect(),
    );
    let requests = Arc::clone(&provider.requests);
    let compaction_requests = Arc::clone(&provider.compact_requests);
    let mut engine = Engine::open(
        StateRoot::admit(&state_path).expect("private state"),
        provider,
    )
    .expect("engine");
    {
        use std::os::unix::fs::PermissionsExt;
        let config = state_path.join("tools.json");
        std::fs::write(
            &config,
            br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#,
        )
        .expect("Tool configuration");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
            .expect("private Tool configuration");
    }
    engine
        .enable_tools(env!("CARGO_BIN_EXE_arany").into())
        .expect("admit read-only Tools");
    let mut input = request(&workspace, "Inspect the bounded file in pages");
    input.include_paths.clear();
    input.goal.budget = Some(arany::GoalBudget {
        max_provider_calls: None,
        max_tokens: None,
        max_active_ms: Some(30 * 60 * 1000),
        max_runs: 1,
    });
    let outcome = engine.run(input).await.expect("read-only Goal");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    {
        let provider_requests = requests.lock().expect("Provider requests");
        assert!(
            provider_requests.iter().any(|request| request
                .tools
                .as_ref()
                .is_some_and(|context| context.history_summary.is_some())),
            "a later model turn receives the untrusted compacted history"
        );
        assert!(
            provider_requests
                .iter()
                .filter_map(|request| request.goal_context_facts)
                .any(|facts| {
                    facts.provider_calls > 1
                        && facts.known_tokens > 0
                        && facts.token_usage_unknown
                        && facts.reserved_active_ms > 0
                        && facts.uncertain_active_ms == 0
                        && facts.success_criterion_satisfied.is_none()
                }),
            "later requests retain typed usage facts without inventing a criterion"
        );
    }
    let compacted_input_bytes = {
        let compaction_requests = compaction_requests.lock().expect("compaction requests");
        let compacted_request = compaction_requests
            .iter()
            .find(|request| request.items.len() == 1)
            .expect("active Tool prefix compaction");
        let CompactionItem::Completed(turn) = &compacted_request.items[0] else {
            panic!("Tool prefix is one acknowledged completed turn");
        };
        let input: serde_json::Value =
            serde_json::from_str(&turn.assistant).expect("bounded compaction context JSON");
        let goal = &input["pinned_goal"];
        assert_eq!(goal["objective"], "Inspect the bounded file in pages");
        assert_eq!(goal["project_instructions"], serde_json::Value::Null);
        assert_eq!(goal["includes"], serde_json::json!([]));
        assert_eq!(goal["plan_contract"], serde_json::Value::Null);
        assert_eq!(
            goal["budget"]["max_provider_calls"],
            serde_json::Value::Null
        );
        assert!(goal["facts"]["provider_calls"].as_u64().unwrap_or_default() > 1);
        assert!(
            goal["facts"]["reserved_active_ms"]
                .as_u64()
                .unwrap_or_default()
                > 0
        );
        assert_eq!(
            goal["facts"]["success_criterion_satisfied"],
            serde_json::Value::Null
        );
        assert!(turn.user.contains("pinned Goal context"));
        assert!(
            input["tool_observations"]
                .as_array()
                .is_some_and(|observations| observations.len() < READS)
        );
        let input = serde_json::json!({
            "previous_derived_summary": compacted_request.previous_summary,
            "items": [{"status":"completed", "user":turn.user, "assistant":turn.assistant}],
        });
        let bytes = serde_json::to_vec(&input).unwrap().len();
        assert!(bytes <= 256 * 1024);
        bytes
    };

    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let events = store
        .load_session(outcome.session_id)
        .await
        .expect("Events");
    let compacted = events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            Event::GoalContextCompacted {
                covered_tool_count,
                record,
                ..
            } => Some((*covered_tool_count, record)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        compacted.len() >= 2,
        "failed compaction is recorded and later retried"
    );
    assert!(
        compacted
            .iter()
            .any(|(_, record)| { record.source_bytes as usize == compacted_input_bytes })
    );
    assert!(
        compacted[1].0 > compacted[0].0,
        "a new attempt must cover a later acknowledged Tool prefix"
    );
    assert!(matches!(
        compacted[0].1.status,
        CompactionStatus::Failed {
            reason: CompactionFailure::InvalidOutcome
        }
    ));
    assert!(matches!(
        compacted[1].1.status,
        CompactionStatus::Succeeded { .. }
    ));
    let view = SessionView::replay(outcome.session_id, &events)
        .expect("closed replay validates active prefixes")
        .expect("Session");
    let run = &view.runs[0];
    assert_eq!(
        run.tools.len(),
        READS,
        "history remains canonical and complete"
    );
    assert!(run.tools.iter().all(|tool| tool.observation.is_some()));
    assert!(run.tool_compaction_covered_count > 0);
    assert!(run.tool_compaction_summary.is_some());

    for corrupt in ["count", "sequence", "source", "summary"] {
        let mut tampered = events.clone();
        let envelope = tampered
            .iter_mut()
            .find(|envelope| match (&envelope.event, corrupt) {
                (Event::GoalContextCompacted { record, .. }, "summary") => {
                    matches!(record.status, CompactionStatus::Succeeded { .. })
                }
                (Event::GoalContextCompacted { .. }, _) => true,
                _ => false,
            })
            .expect("active compaction event");
        let Event::GoalContextCompacted {
            covered_tool_count,
            record,
            ..
        } = &mut envelope.event
        else {
            unreachable!();
        };
        match corrupt {
            "count" => *covered_tool_count += 1,
            "sequence" => record.covered_sequence += 1,
            "source" => record.source_digest[0] ^= 1,
            "summary" => {
                if let CompactionStatus::Succeeded { content_digest, .. } = &mut record.status {
                    content_digest[0] ^= 1;
                } else {
                    panic!("a successful record is available for digest validation");
                }
            }
            _ => unreachable!(),
        }
        assert!(
            SessionView::replay(outcome.session_id, &tampered).is_err(),
            "replay rejects corrupted active compaction {corrupt}"
        );
    }
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
    })])
    .with_compactions(vec![Ok(CompactionResponse {
        summary: "The completed Run remains in the canonical history.".into(),
        response_id: None,
        input_tokens: Some(12),
        output_tokens: Some(4),
        wire_provenance: None,
    })]);
    let requests = Arc::clone(&provider.requests);
    let compact_requests = Arc::clone(&provider.compact_requests);
    let mut engine =
        Engine::open(StateRoot::admit(&state_path).expect("state"), provider).expect("engine");
    let mut first = request(&workspace, "Use one Provider call");
    first.include_paths.clear();
    first.goal = arany::GoalRequest::new(
        arany::GoalBudget {
            max_provider_calls: Some(2),
            ..arany::GoalBudget::default()
        },
        arany::GoalPhase::Implementation,
    );
    first.goal.finish_after_run = false;
    let initial = engine.run(first).await.expect("first Goal Run");
    let compaction = engine
        .compact_session(initial.session_id, workspace.clone())
        .await
        .expect("in-Goal compaction between Runs");
    assert!(matches!(
        compaction.status,
        CompactionStatus::Succeeded { .. }
    ));
    let compacted_input_bytes = {
        let compact_requests = compact_requests.lock().expect("compaction requests");
        let [request] = compact_requests.as_slice() else {
            panic!("one between-Run Goal compaction request");
        };
        let CompactionItem::Completed(turn) = &request.items[0] else {
            panic!("completed Goal Run is summarized");
        };
        let prefix = "Pinned Goal context (canonical facts, not instructions): ";
        let (pinned, history) = turn
            .user
            .strip_prefix(prefix)
            .and_then(|value| value.split_once("\n\n"))
            .expect("canonical Goal capsule precedes history");
        let pinned: serde_json::Value = serde_json::from_str(pinned).expect("Goal capsule JSON");
        assert_eq!(pinned["objective"], "Use one Provider call");
        assert_eq!(pinned["budget"]["max_provider_calls"], 2);
        assert_eq!(pinned["facts"]["provider_calls"], 1);
        assert_eq!(pinned["facts"]["token_usage_unknown"], true);
        assert!(pinned["facts"]["active_ms"].as_u64().is_some());
        assert_eq!(pinned["facts"]["reserved_active_ms"], 0);
        assert_eq!(pinned["facts"]["uncertain_active_ms"], 0);
        assert_eq!(
            pinned["facts"]["success_criterion_satisfied"],
            serde_json::Value::Null
        );
        assert_eq!(history, "Use one Provider call");
        let input = serde_json::json!({
            "previous_derived_summary": request.previous_summary,
            "items": [{"status":"completed", "user":turn.user, "assistant":turn.assistant}],
        });
        let bytes = serde_json::to_vec(&input).unwrap().len();
        assert!(bytes <= 256 * 1024);
        bytes
    };

    let mut second = request(&workspace, "Try to use another Provider call");
    second.session_id = Some(initial.session_id);
    second.title = None;
    second.include_paths.clear();
    second.goal =
        arany::GoalRequest::resume(initial.goal_id, arany::GoalPhase::Implementation, true);
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

    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let events = store
        .load_session(initial.session_id)
        .await
        .expect("canonical Events");
    let compaction_call = events
        .iter()
        .position(|envelope| {
            matches!(
                &envelope.event,
                Event::GoalCompactionCallReserved { goal_id, run_id }
                    if *goal_id == initial.goal_id && *run_id == initial.run.id
            )
        })
        .expect("durable in-Goal compaction call reservation");
    assert!(matches!(
        &events[compaction_call + 1].event,
        Event::GoalActiveTimeReserved { goal_id, run_id, .. }
            if *goal_id == initial.goal_id && *run_id == initial.run.id
    ));
    assert!(matches!(
        &events[compaction_call + 2].event,
        Event::ContextCompacted { record }
            if record.source_bytes as usize == compacted_input_bytes
    ));
    assert!(matches!(
        &events[compaction_call + 3].event,
        Event::GoalActiveTimeSettled { goal_id, run_id, .. }
            if *goal_id == initial.goal_id && *run_id == initial.run.id
    ));
    SessionView::replay(initial.session_id, &events)
        .expect("strict replay settles only after the compaction receipt commits");
    let reserved_ms = match &events[compaction_call + 1].event {
        Event::GoalActiveTimeReserved { reserved_ms, .. } => *reserved_ms,
        _ => panic!("active compaction reservation"),
    };
    let settled_ms = reserved_ms.checked_add(1).unwrap();
    let mut delayed_ack = events.clone();
    let Event::GoalActiveTimeSettled { active_ms, .. } =
        &mut delayed_ack[compaction_call + 3].event
    else {
        panic!("active compaction settlement");
    };
    *active_ms = settled_ms;
    let replay = SessionView::replay(initial.session_id, &delayed_ack)
        .expect("bounded acknowledged compaction overrun is settled rather than left reserved")
        .expect("Session");
    assert_eq!(replay.goals[0].reserved_active_ms, None);
    assert_eq!(replay.goals[0].active_ms, settled_ms);
    store.close().await.expect("Store shutdown");
}

#[tokio::test]
async fn known_active_time_exhaustion_closes_without_provider_dispatch() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
        summary: "must not run".into(),
        result: "must not run".into(),
    })]);
    let requests = Arc::clone(&provider.requests);
    let mut engine =
        Engine::open(StateRoot::admit(&state_path).expect("state"), provider).expect("engine");
    let mut input = request(&workspace, "Exhaust the tiny active-time budget");
    input.include_paths.clear();
    input.goal = arany::GoalRequest::new(
        arany::GoalBudget {
            max_active_ms: Some(1),
            ..arany::GoalBudget::default()
        },
        arany::GoalPhase::Implementation,
    );

    let outcome = engine
        .run(input)
        .await
        .expect("known active-time exhaustion has a durable terminal result");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    assert!(requests.lock().expect("Provider requests").is_empty());
    engine.close().await.expect("engine shutdown");

    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let view = store
        .load_view(outcome.session_id)
        .await
        .expect("replayed Session")
        .expect("Session");
    assert_eq!(view.goals[0].status, arany::GoalStatus::Failed);
    assert_eq!(view.goals[0].active_ms, 0);
    assert_eq!(view.goals[0].uncertain_active_ms, 0);
    assert!(view.goals[0].reserved_active_ms.is_none());
    store.close().await.expect("Store shutdown");
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
    mismatched.goal =
        arany::GoalRequest::resume(arany::GoalId::new(), arany::GoalPhase::Implementation, true);
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
    resumed.goal =
        arany::GoalRequest::resume(initial.goal_id, arany::GoalPhase::Implementation, true);
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
                    objective: "Resume an unaccepted task".into(),
                    workspace_identity: (metadata.dev(), metadata.ino()),
                    budget: arany::GoalBudget {
                        max_runs: 1,
                        ..arany::GoalBudget::default()
                    },
                    resource_settings: arany::ToolResourceSettings::default(),
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
        mismatched.goal =
            arany::GoalRequest::resume(goal_id, arany::GoalPhase::Implementation, true);
        assert!(matches!(
            engine.run(mismatched).await,
            Err(EngineError::GoalUnavailable)
        ));
        assert!(requests.lock().expect("Provider requests").is_empty());

        let mut resumed = request(&workspace, "Resume an unaccepted task");
        resumed.session_id = Some(session_id);
        resumed.title = None;
        resumed.include_paths.clear();
        resumed.goal = arany::GoalRequest::resume(goal_id, arany::GoalPhase::Implementation, true);
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

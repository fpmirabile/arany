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
async fn goal_report_counts_cached_input_tokens() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
        summary: "done".into(),
        result: "accepted result".into(),
    })])
    .with_cached_input_tokens(7);
    let mut engine =
        Engine::open(StateRoot::admit(&state_path).expect("state"), provider).expect("engine");
    let mut input = request(&workspace, "Complete a Goal");
    input.include_paths.clear();
    let outcome = engine.run(input).await.expect("Goal Run");
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
    let report = view.goals[0].report.as_ref().expect("durable report");
    assert_eq!(report.cached_input_tokens, 7);
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
async fn long_read_only_goal_compacts_only_an_acknowledged_run_prefix() {
    compact_read_only_goal_prefix("Inspect the bounded file in pages").await;
}

#[tokio::test]
async fn prose_objective_at_the_limit_compacts_its_read_only_goal_prefix() {
    // Prose escapes about 1x; the pinned objective and the first turn each carry it once.
    let prose = "Review the module and keep its contract. ".repeat(1_600);
    compact_read_only_goal_prefix(&prose[..64 * 1024]).await;
}

async fn compact_read_only_goal_prefix(objective: &str) {
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
        .map(|page| {
            crate::tool_outcome(
                arany::ToolCall::Read {
                    path: "notes/data.txt".into(),
                    offset: u32::try_from(page * PAGE_BYTES).expect("bounded file offset"),
                    limit: u32::try_from(PAGE_BYTES).expect("bounded page size"),
                },
                None,
            )
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
            usage: arany::Usage {
                input_tokens: Some(32),
                output_tokens: Some(12),
                ..Default::default()
            },
            wire_provenance: None,
        })
    };
    let provider = ScriptedProvider::new(outcomes).with_compactions(
        std::iter::once(Ok(CompactionResponse {
            summary: String::new(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: Some(8),
                ..Default::default()
            },
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
    let mut input = request(&workspace, objective);
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
                .input
                .tools
                .as_ref()
                .is_some_and(|context| context.history_summary.is_some())),
            "a later model turn receives the untrusted compacted history"
        );
        assert!(
            provider_requests
                .iter()
                .filter_map(call_facts)
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
        assert_eq!(goal["objective"], objective);
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
        // Cancellation pauses the Goal, which stays resumable; failure closes it with a report.
        assert_eq!(
            view.goals[0].status,
            if cancel {
                arany::GoalStatus::Interrupted
            } else {
                arany::GoalStatus::Failed
            }
        );
        assert_eq!(view.goals[0].report.is_some(), !cancel);
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
        usage: arany::Usage {
            input_tokens: Some(12),
            output_tokens: Some(4),
            ..Default::default()
        },
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
    let Event::GoalActiveTimeSettled {
        active_ms: original_ms,
        ..
    } = &events[compaction_call + 3].event
    else {
        panic!("active compaction settlement");
    };
    let original_ms = *original_ms;
    let mut delayed_ack = events.clone();
    let Event::GoalActiveTimeSettled { active_ms, .. } =
        &mut delayed_ack[compaction_call + 3].event
    else {
        panic!("active compaction settlement");
    };
    *active_ms = settled_ms;
    // The terminal receipt records the Goal total, which the overrun shifts by the same delta.
    let total_ms = match &mut delayed_ack.last_mut().expect("terminal Goal receipt").event {
        Event::GoalFinished { active_ms, .. } => {
            *active_ms = active_ms
                .checked_sub(original_ms)
                .and_then(|prior| prior.checked_add(settled_ms))
                .unwrap();
            *active_ms
        }
        _ => panic!("terminal Goal receipt"),
    };
    let replay = SessionView::replay(initial.session_id, &delayed_ack)
        .expect("bounded acknowledged compaction overrun is settled rather than left reserved")
        .expect("Session");
    assert_eq!(replay.goals[0].reserved_active_ms, None);
    assert_eq!(replay.goals[0].active_ms, total_ms);
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

#[tokio::test]
async fn goal_paused_at_its_call_budget_closes_as_failed_on_explicit_resume() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let first_provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
        summary: "first Run complete".into(),
        result: "continue later".into(),
    })]);
    let mut first_engine = Engine::open(
        StateRoot::admit(&state_path).expect("state"),
        first_provider,
    )
    .expect("first engine");
    let mut first = request(&workspace, "Use the only Provider call");
    first.include_paths.clear();
    first.goal = arany::GoalRequest::new(
        arany::GoalBudget {
            max_provider_calls: Some(1),
            ..arany::GoalBudget::default()
        },
        arany::GoalPhase::Implementation,
    );
    first.goal.finish_after_run = false;
    let initial = first_engine.run(first).await.expect("first Goal Run");
    first_engine.close().await.expect("first engine shutdown");

    let second_provider = ScriptedProvider::new(Vec::new());
    let second_requests = Arc::clone(&second_provider.requests);
    let mut second_engine = Engine::open(
        StateRoot::open_existing(&state_path).expect("reopened state"),
        second_provider,
    )
    .expect("second engine");
    let mut resumed = request(&workspace, "Try the exhausted Goal");
    resumed.session_id = Some(initial.session_id);
    resumed.title = None;
    resumed.include_paths.clear();
    resumed.goal = arany::GoalRequest::resume(initial.goal_id, true);
    let result = second_engine.run(resumed).await;
    assert!(
        matches!(&result, Err(EngineError::GoalBudgetExhausted)),
        "unexpected exhausted-resume result: {}",
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

    let mut again = request(&workspace, "Resume the closed Goal");
    again.session_id = Some(initial.session_id);
    again.title = None;
    again.include_paths.clear();
    again.goal = arany::GoalRequest::resume(initial.goal_id, true);
    assert!(
        matches!(
            second_engine.run(again).await,
            Err(EngineError::GoalUnavailable)
        ),
        "a closed Goal is not resumable"
    );
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
    let goal = &view.goals[0];
    assert_eq!(goal.status, arany::GoalStatus::Failed);
    assert!(
        goal.report
            .as_ref()
            .is_some_and(|report| !report.summary.is_empty())
    );
    assert_eq!(view.interrupted_goal_id(), None);
    assert_eq!(
        view.runs.len(),
        1,
        "no Run is linked for the exhausted resume"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(&event.event, Event::GoalResumed { .. })),
        "the exhausted resume records no GoalResumed"
    );
    store.close().await.expect("Store shutdown");
}

struct FiniteTokenTeam {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

#[async_trait::async_trait]
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
            turn: arany::ProviderTurn::scripted(),
            outcome,
            response_id: None,
            usage: arany::Usage {
                input_tokens: Some(input_tokens),
                output_tokens: Some(output_tokens),
                ..Default::default()
            },
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

#[tokio::test]
async fn exhausted_goal_crashed_inside_its_only_provider_call_closes_on_resume() {
    use std::os::unix::fs::MetadataExt;

    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let metadata = std::fs::metadata(&workspace).expect("Workspace identity");
    let identity = (metadata.dev(), metadata.ino());
    let state_path = temp.path().join("state");
    let session_id = SessionId::new();
    let (goal_id, run_id, agent_run_id) =
        (arany::GoalId::new(), RunId::new(), arany::AgentRunId::new());
    let store = Store::open(StateRoot::admit(&state_path).expect("state")).expect("Store");
    // The process died inside the only admitted Provider call: the Run is journaled Active.
    let crashed = [
        Event::SessionStarted {
            title: "Crashed Goal".into(),
            workspace_identity: identity,
        },
        Event::GoalStarted {
            goal_id,
            phase: arany::GoalPhase::Implementation,
            autonomous: false,
            resource_settings: arany::ToolResourceSettings::default(),
            objective: "Use the only Provider call".into(),
            workspace_identity: identity,
            budget: arany::GoalBudget {
                max_provider_calls: Some(1),
                ..arany::GoalBudget::default()
            },
        },
        Event::GoalRunLinked {
            goal_id,
            run_id,
            phase: arany::GoalPhase::Implementation,
        },
        Event::MessageAccepted {
            run_id,
            text: "Use the only Provider call".into(),
            images: Vec::new(),
        },
        Event::RunStarted {
            run_id,
            config: arany::RunConfig {
                provider: "scripted".into(),
                model: "scripted-model".into(),
                effort: None,
                custom_profile_provenance: None,
                saved_api_account_id: None,
                chatgpt_provenance: None,
                output_token_bound: arany::OutputTokenBound::ProviderEnforced,
                policy: CollaborationPolicy::Single,
                output_token_cap: 4096,
                provider_concurrency: 1,
                workspace_device: identity.0,
                workspace_inode: identity.1,
                instruction_digest: None,
                include_digests: Vec::new(),
                history_run_ids: Vec::new(),
                excluded_history_runs: 0,
                context_usage: None,
                compaction_event_sequence: None,
                compaction_content_digest: None,
                tool_policy: None,
            },
        },
        Event::AgentSpawned {
            run_id,
            agent_run_id,
            role: arany::AgentRole::Primary,
            ordinal: 0,
            objective: None,
            grant: None,
        },
        Event::GoalProviderCallReserved {
            goal_id,
            run_id,
            agent_run_id,
        },
    ];
    for event in crashed {
        store
            .append(session_id, event)
            .await
            .expect("crash-left journal prefix");
    }
    store.close().await.expect("seed Store shutdown");

    let provider = ScriptedProvider::new(Vec::new());
    let requests = Arc::clone(&provider.requests);
    let mut engine = Engine::open(
        StateRoot::open_existing(&state_path).expect("reopened state"),
        provider,
    )
    .expect("Engine");
    let mut resumed = request(&workspace, "Resume the crashed Goal");
    resumed.session_id = Some(session_id);
    resumed.title = None;
    resumed.include_paths.clear();
    resumed.goal = arany::GoalRequest::resume(goal_id, true);
    assert!(
        matches!(
            engine.run(resumed).await,
            Err(EngineError::GoalBudgetExhausted)
        ),
        "the exhausted crash-left Goal reports its budget, not a store or replay error"
    );
    assert!(requests.lock().expect("Provider requests").is_empty());

    let mut again = request(&workspace, "Resume the closed Goal");
    again.session_id = Some(session_id);
    again.title = None;
    again.include_paths.clear();
    again.goal = arany::GoalRequest::resume(goal_id, true);
    assert!(
        matches!(engine.run(again).await, Err(EngineError::GoalUnavailable)),
        "a closed Goal is not resumable"
    );
    engine.close().await.expect("Engine shutdown");

    let (_, view) = replayed(&state_path, session_id).await;
    let goal = view.goal(goal_id).expect("Goal");
    assert_eq!(goal.status, arany::GoalStatus::Failed);
    assert!(goal.report.is_some(), "closed Goal keeps its report");
    assert_eq!(view.runs.len(), 1);
    assert_eq!(view.runs[0].status, RunStatus::Interrupted);
    assert_eq!(view.interrupted_goal_id(), None);
}

struct ExactCapToolReview;

#[async_trait::async_trait]
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
                crate::tool_outcome(
                    arany::ToolCall::Write {
                        path: "notes/a.txt".into(),
                        expected_digest: None,
                        content: "x".into(),
                    },
                    Some(Box::new(arany::GoalPlanContract {
                        mission: "Write one note".into(),
                        resource_class: arany::ToolResourceClass::Default,
                        command_timeout_ms: None,
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
                ),
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
            turn: arany::ProviderTurn::scripted(),
            outcome,
            response_id: None,
            usage: arany::Usage {
                input_tokens: Some(input_tokens),
                output_tokens: Some(output_tokens),
                ..Default::default()
            },
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

#[async_trait::async_trait]
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
                turn: arany::ProviderTurn::scripted(),
                outcome: ProviderOutcome::Delegate(Delegate {
                    children: vec!["First child".into(), "Second child".into()],
                }),
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

/// The Goal facts one call carries: on its newest step, or in the request head when it has none.
fn call_facts(request: &arany::ProviderRequest) -> Option<arany::GoalContextFacts> {
    request
        .input
        .tools
        .as_ref()
        .and_then(|tools| tools.steps.last())
        .and_then(|step| step.facts)
        .or(request.input.goal_context_facts)
}

/// Every steering note one call carries, in the order they were recorded.
fn call_steering_notes(request: &arany::ProviderRequest) -> Vec<String> {
    request
        .input
        .steering_notes
        .iter()
        .cloned()
        .chain(
            request
                .input
                .tools
                .iter()
                .flat_map(|tools| &tools.steps)
                .flat_map(|step| step.steering_notes.iter().cloned()),
        )
        .collect()
}

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
        resource_class: arany::ToolResourceClass::Default,
        command_timeout_ms: None,
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
fn facts_engine<P: Provider + 'static>(state: StateRoot, state_path: &Path, provider: P) -> Engine {
    facts_engine_with_telemetry(state, state_path, provider, arany::Telemetry::disabled())
}

fn facts_engine_with_telemetry<P: Provider + 'static>(
    state: StateRoot,
    state_path: &Path,
    provider: P,
    telemetry: arany::Telemetry,
) -> Engine {
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
        crate::tool_outcome(facts_read(), Some(Box::new(facts_plan(facts_digest("v2"))))),
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
        crate::tool_outcome(facts_read(), Some(Box::new(facts_plan(facts_digest("v2"))))),
        finish_with("first pass"),
        crate::tool_outcome(facts_read(), None),
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
            calls[2].input.objective, AUTONOMOUS_OBJECTIVE,
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
        crate::tool_outcome(facts_read(), Some(Box::new(plan))),
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
        max_provider_calls: Some(3),
        ..arany::GoalBudget::default()
    };
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget = budget;
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read(), Some(Box::new(plan))),
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

#[async_trait::async_trait]
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
            turn: arany::ProviderTurn::scripted(),
            outcome,
            response_id: None,
            usage: arany::Usage {
                input_tokens: Some(self.tokens),
                output_tokens: Some(0),
                ..Default::default()
            },
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
    plan.budget = budget;
    // Five input tokens per call: the Read and the Finish reach the 10-token cap exactly.
    let provider = MeteredProvider {
        tokens: 5,
        responses: std::sync::Mutex::new(VecDeque::from([
            crate::tool_outcome(facts_read(), Some(Box::new(plan))),
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
        crate::tool_outcome(facts_read(), None),
        finish_with("resumed, still unverified"),
        crate::tool_outcome(facts_read(), None),
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
        assert_eq!(calls[0].input.objective, AUTONOMOUS_OBJECTIVE);
        assert_eq!(calls[2].input.objective, AUTONOMOUS_OBJECTIVE);
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
        crate::tool_outcome(facts_read(), Some(Box::new(facts_plan(facts_digest("v2"))))),
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
}

/// Drives an admitted Run and applies `after` as soon as its Finished Run becomes visible.
async fn run_observing_finish(
    engine: &mut Engine,
    input: RunRequest,
    after: AfterFinish,
) -> Result<arany::RunOutcome, EngineError> {
    let cancellation = match &after {
        AfterFinish::Cancel(cancellation) => cancellation.clone(),
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
        crate::tool_outcome(facts_read(), Some(Box::new(facts_plan(facts_digest("v2"))))),
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

/// A Finish whose call spends the Goal's active budget is refused; its Run fails and the Goal closes with a report.
#[tokio::test]
async fn autonomous_goal_active_time_spent_by_a_finish_fails_the_goal_with_its_report() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let budget = arany::GoalBudget {
        max_active_ms: Some(60_000),
        ..arany::GoalBudget::default()
    };
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget = budget;
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read(), Some(Box::new(plan))),
        finish_with("first pass"),
        finish_with("never requested"),
    ])
    // Advanced in the Finish call, after the Read Tool's Guard I/O (a paused clock would auto-advance that I/O's timeouts).
    .with_clock_advance_before_call(1, Duration::from_secs(60));
    let requests = Arc::clone(&provider.requests);
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let mut input = autonomous_input(&workspace);
    input.goal = arany::GoalRequest::autonomous(budget, arany::GoalPhase::Implementation);
    let result = engine.run_with_cancel(input, RunCancellation::new()).await;
    assert!(
        matches!(&result, Ok(outcome) if outcome.run.status == RunStatus::Failed),
        "the exhausted Finish fails its Run: {:?}",
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

#[tokio::test]
async fn exhausted_autonomous_segment_exports_its_failed_status() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let budget = arany::GoalBudget {
        max_active_ms: Some(60_000),
        ..arany::GoalBudget::default()
    };
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget = budget;
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read(), Some(Box::new(plan))),
        finish_with("first pass"),
        finish_with("never requested"),
    ])
    // Advanced in the Finish call, after the Read Tool's Guard I/O (a paused clock would auto-advance that I/O's timeouts).
    .with_clock_advance_before_call(1, Duration::from_secs(60));
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
    let result = engine.run_with_cancel(input, RunCancellation::new()).await;
    assert!(
        matches!(&result, Ok(outcome) if outcome.run.status == RunStatus::Failed),
        "the exhausted Finish fails its Run: {:?}",
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
        crate::tool_outcome(facts_read(), Some(Box::new(facts_plan(facts_digest("v2"))))),
        finish_with("first pass"),
        crate::tool_outcome(facts_read(), None),
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
    let provider = ScriptedProvider::new(vec![crate::tool_outcome(
        facts_read(),
        Some(Box::new(plan)),
    )]);
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
async fn plan_active_limit_below_elapsed_call_time_fails_the_run() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget.max_active_ms = Some(1);
    // The plan-bearing call runs longer than the plan's active-time limit, so the plan cannot narrow the Goal.
    let provider = ScriptedProvider::new(vec![crate::tool_outcome(
        facts_read(),
        Some(Box::new(plan)),
    )])
    .with_before_call(0, || {
        std::thread::sleep(std::time::Duration::from_millis(20))
    });
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let outcome = engine
        .run(autonomous_input(&workspace))
        .await
        .expect("an unnarrowable plan is a Failed Run, not a Store error");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    engine.close().await.expect("engine shutdown");

    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert!(
        goal.plan_contract.is_none(),
        "the refused plan is not recorded"
    );
    let root_plan_dispositions: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::ProviderCallRecorded { record, .. } if record.phase == AgentPhase::RootPlan => {
                Some(record.disposition)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        root_plan_dispositions,
        [ProviderCallDisposition::ToolRequested],
        "the plan-bearing call is refused at the plan contract, after its Tool was requested"
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
        crate::tool_outcome(facts_read(), Some(Box::new(facts_plan(facts_digest("v2"))))),
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
        crate::tool_outcome(facts_read(), Some(Box::new(facts_plan(facts_digest("v2"))))),
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

#[tokio::test]
async fn paused_goal_coexists_with_a_new_goal_and_resumes_with_its_pinned_objective() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let cancellation = RunCancellation::new();
    let mut paused_provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
        summary: "done".into(),
        result: "cancelled answer".into(),
    })]);
    paused_provider.cancel_at_completion = Some((AgentPhase::RootPlan, cancellation.clone()));
    let mut engine = Engine::open(
        StateRoot::admit(&state_path).expect("state"),
        paused_provider,
    )
    .expect("engine");
    // A cancelled plain Run pauses its one-Run Goal like an autonomous one: no terminal receipt, resumable.
    let mut paused = request(&workspace, "Pinned objective of the paused Goal");
    paused.include_paths.clear();
    let cancelled = engine
        .run_with_cancel(paused, cancellation)
        .await
        .expect("cancelled Run");
    assert_eq!(cancelled.run.status, RunStatus::Cancelled);
    let session_id = cancelled.session_id;
    engine.close().await.expect("first engine shutdown");
    {
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only Store");
        let view = store
            .load_view(session_id)
            .await
            .expect("replayed Session")
            .expect("Session");
        let paused_goal = view.goal(cancelled.goal_id).expect("paused Goal");
        assert_eq!(paused_goal.status, arany::GoalStatus::Interrupted);
        assert!(paused_goal.report.is_none());
        store.close().await.expect("Store shutdown");
    }

    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Finish(Finish {
            summary: "second".into(),
            result: "second answer".into(),
        }),
        ProviderOutcome::Finish(Finish {
            summary: "resumed".into(),
            result: "resumed answer".into(),
        }),
    ]);
    let requests = Arc::clone(&provider.requests);
    let mut engine = Engine::open(
        StateRoot::open_existing(&state_path).expect("reopened state"),
        provider,
    )
    .expect("second engine");

    let mut second = request(&workspace, "A new plain prompt");
    second.session_id = Some(session_id);
    second.title = None;
    second.include_paths.clear();
    let second_run = engine
        .run(second)
        .await
        .expect("a new Goal starts while the earlier Goal is paused");
    assert_eq!(second_run.run.status, RunStatus::Finished);
    assert_ne!(second_run.goal_id, cancelled.goal_id);

    let mut resumed = request(&workspace, "");
    resumed.session_id = Some(session_id);
    resumed.title = None;
    resumed.include_paths.clear();
    resumed.objective = String::new();
    resumed.goal = arany::GoalRequest::resume(cancelled.goal_id, true);
    let resumed_run = engine
        .run(resumed)
        .await
        .expect("the most recent paused Goal resumes after a newer Goal finished");
    assert_eq!(resumed_run.goal_id, cancelled.goal_id);
    assert_eq!(resumed_run.run.status, RunStatus::Finished);
    assert_eq!(
        resumed_run.run.objective, "Pinned objective of the paused Goal",
        "resume runs the pinned objective"
    );
    engine.close().await.expect("second engine shutdown");
    assert_eq!(requests.lock().expect("Provider requests").len(), 2);

    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let view = store
        .load_view(session_id)
        .await
        .expect("replayed Session")
        .expect("Session");
    let paused_goal = view.goal(cancelled.goal_id).expect("paused Goal");
    assert_eq!(paused_goal.status, arany::GoalStatus::Completed);
    assert_eq!(paused_goal.run_ids, [cancelled.run.id, resumed_run.run.id]);
    assert_eq!(
        view.goal(second_run.goal_id).expect("new Goal").status,
        arany::GoalStatus::Completed
    );
    store.close().await.expect("Store shutdown");
}

#[tokio::test]
async fn older_paused_goal_is_refused_once_a_newer_goal_is_paused() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let autonomous = || {
        arany::GoalRequest::autonomous(
            arany::GoalBudget::default(),
            arany::GoalPhase::Implementation,
        )
    };
    let mut session_id = None;
    let mut paused_goals = Vec::new();
    for objective in ["First paused objective", "Second paused objective"] {
        let cancellation = RunCancellation::new();
        let mut provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
            summary: "done".into(),
            result: "cancelled answer".into(),
        })]);
        provider.cancel_at_completion = Some((AgentPhase::RootPlan, cancellation.clone()));
        let mut engine =
            Engine::open(StateRoot::admit(&state_path).expect("state"), provider).expect("engine");
        let mut input = request(&workspace, objective);
        input.include_paths.clear();
        input.session_id = session_id;
        input.title = None;
        input.goal = autonomous();
        let cancelled = engine
            .run_with_cancel(input, cancellation)
            .await
            .expect("cancelled Run");
        assert_eq!(cancelled.run.status, RunStatus::Cancelled);
        session_id = Some(cancelled.session_id);
        paused_goals.push(cancelled.goal_id);
        engine.close().await.expect("engine shutdown");
    }
    let (first, second) = (paused_goals[0], paused_goals[1]);
    let session_id = session_id.expect("Session");

    let provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
        summary: "resumed".into(),
        result: "resumed answer".into(),
    })]);
    let requests = Arc::clone(&provider.requests);
    let mut engine = Engine::open(
        StateRoot::open_existing(&state_path).expect("reopened state"),
        provider,
    )
    .expect("engine");
    let mut stale = request(&workspace, "");
    stale.session_id = Some(session_id);
    stale.title = None;
    stale.include_paths.clear();
    stale.objective = String::new();
    stale.goal = arany::GoalRequest::resume(first, true);
    assert!(matches!(
        engine.run(stale).await,
        Err(EngineError::GoalUnavailable)
    ));
    assert!(requests.lock().expect("Provider requests").is_empty());

    let mut resumed = request(&workspace, "");
    resumed.session_id = Some(session_id);
    resumed.title = None;
    resumed.include_paths.clear();
    resumed.objective = String::new();
    resumed.goal = arany::GoalRequest::resume(second, true);
    let outcome = engine
        .run(resumed)
        .await
        .expect("the most recent paused Goal resumes");
    assert_eq!(outcome.goal_id, second);
    assert_eq!(outcome.run.objective, "Second paused objective");
    engine.close().await.expect("engine shutdown");
}

fn steering_note_at(steering: &RunSteering, text: &str) {
    steering
        .offer_note(text.into())
        .expect("steering note offered while its Run is open");
}

#[tokio::test]
async fn steering_note_reaches_the_next_primary_call_and_is_journaled_once() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let offer = steering.clone();
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read(), None),
        finish_with("noted"),
    ])
    .with_before_call(0, move || {
        steering_note_at(&offer, "use the smaller fixture")
    });
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    engine.set_steering(steering.clone());
    let mut input = request(&workspace, "Read the facts");
    input.include_paths.clear();
    let outcome = engine.run(input).await.expect("Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert_eq!(calls.len(), 2);
    assert!(call_steering_notes(&calls[0]).is_empty());
    assert_eq!(call_steering_notes(&calls[1]), ["use the smaller fixture"]);
    assert_eq!(steering.delivered_notes(), 1);
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(view.runs[0].steering_notes, ["use the smaller fixture"]);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::GoalSteeringNoteRecorded { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn steering_notes_offered_before_delivery_reach_one_primary_call_as_one_note() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let offer = steering.clone();
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read(), None),
        finish_with("noted"),
    ])
    .with_before_call(0, move || {
        steering_note_at(&offer, "use the smaller fixture");
        steering_note_at(&offer, "and keep the old name");
    });
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    engine.set_steering(steering.clone());
    let mut input = request(&workspace, "Read the facts");
    input.include_paths.clear();
    let outcome = engine.run(input).await.expect("Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert_eq!(
        call_steering_notes(&calls[1]),
        ["use the smaller fixture\n\nand keep the old name"]
    );
    assert_eq!(steering.delivered_notes(), 1);
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(
        view.runs[0].steering_notes,
        ["use the smaller fixture\n\nand keep the old name"]
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::GoalSteeringNoteRecorded { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn steering_note_never_reaches_children_and_is_kept_for_synthesis() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let offer = steering.clone();
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            children: vec!["Inspect facts".into()],
        }),
        finish_with("child result"),
        finish_with("synthesized"),
    ])
    .with_before_call(0, move || {
        steering_note_at(&offer, "keep this for synthesis")
    });
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    engine.set_steering(steering);
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    let outcome = engine.run(input).await.expect("Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[1].phase, AgentPhase::ChildWork);
    assert!(call_steering_notes(&calls[1]).is_empty());
    assert_eq!(calls[2].phase, AgentPhase::RootSynthesis);
    assert_eq!(call_steering_notes(&calls[2]), ["keep this for synthesis"]);
}

#[tokio::test]
async fn undelivered_steering_note_stays_with_the_caller_when_the_run_finishes() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let offer = steering.clone();
    let provider = ScriptedProvider::new(vec![finish_with("done")])
        .with_before_call(0, move || steering_note_at(&offer, "too late"));
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    engine.set_steering(steering.clone());
    let mut input = request(&workspace, "Answer quickly");
    input.include_paths.clear();
    let outcome = engine.run(input).await.expect("Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    assert_eq!(steering.delivered_notes(), 0);
    assert_eq!(
        steering.withdraw_note().as_deref(),
        Some("too late"),
        "the caller still holds the note"
    );
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert!(view.runs[0].steering_notes.is_empty());
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, Event::GoalSteeringNoteRecorded { .. }))
    );
}

#[tokio::test]
async fn steering_note_is_history_for_a_later_run_and_never_redelivered() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let offer = steering.clone();
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read(), None),
        finish_with("first answer"),
        finish_with("second answer"),
    ])
    .with_before_call(0, move || {
        steering_note_at(&offer, "use the smaller fixture")
    });
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    engine.set_steering(steering.clone());
    let mut first = request(&workspace, "Read the facts");
    first.include_paths.clear();
    let first_outcome = engine.run(first).await.expect("first Run");

    let mut second = request(&workspace, "Second question");
    second.session_id = Some(first_outcome.session_id);
    second.title = None;
    second.include_paths.clear();
    let second_outcome = engine.run(second).await.expect("second Run");
    assert_eq!(second_outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls[2].input.history[0].user,
        "Read the facts\n\nSteering notes the user sent during this Run:\n[1] use the smaller fixture"
    );
    assert!(call_steering_notes(&calls[2]).is_empty());
    assert_eq!(steering.delivered_notes(), 1);
    let (events, _) = replayed(&state_path, second_outcome.session_id).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::GoalSteeringNoteRecorded { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn pause_waits_for_the_in_flight_call_and_pauses_before_its_tool() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let pause = steering.clone();
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read(), None),
        finish_with("never reached"),
    ])
    .with_before_call(0, move || assert!(pause.pause(), "first pause"));
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    engine.set_steering(steering);
    let mut input = request(&workspace, "Read the facts");
    input.include_paths.clear();
    let outcome = engine.run(input).await.expect("paused Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");

    assert_eq!(requests.lock().expect("requests").len(), 1);
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, Event::ToolStarted { .. })),
        "the paused Run starts no requested Tool"
    );
    let dispositions: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::ProviderCallRecorded { record, .. } => Some(record.disposition),
            _ => None,
        })
        .collect();
    assert_eq!(
        dispositions,
        [ProviderCallDisposition::ToolRequested],
        "the in-flight call completes as the observed Tool request"
    );
    assert_eq!(
        view.goal(outcome.goal_id).expect("Goal").status,
        arany::GoalStatus::Interrupted
    );
}

#[tokio::test]
async fn pause_keeps_a_finish_returned_by_the_in_flight_call() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let pause = steering.clone();
    let provider = ScriptedProvider::new(vec![finish_with("finished before pause")])
        .with_before_call(0, move || assert!(pause.pause(), "first pause"));
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    engine.set_steering(steering);
    let mut input = request(&workspace, "Answer quickly");
    input.include_paths.clear();
    let outcome = engine.run(input).await.expect("Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");
}

#[tokio::test]
async fn pause_between_autonomous_runs_starts_no_next_run() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let pause = steering.clone();
    // The first Run's Finish is unsatisfied against the planned digest, so the Goal would continue.
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read(), Some(Box::new(facts_plan(facts_digest("v2"))))),
        finish_with("first pass"),
        finish_with("never started"),
    ])
    .with_before_call(1, move || assert!(pause.pause(), "first pause"));
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    engine.set_steering(steering);
    let outcome = engine
        .run(autonomous_input(&workspace))
        .await
        .expect("first pass returns without a next Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    assert_eq!(requests.lock().expect("requests").len(), 2);
    let (_, view) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(view.runs.len(), 1);
    // Like Ctrl+C between Runs, the pause leaves the Goal open for explicit resume.
    assert_eq!(
        view.goal(outcome.goal_id).expect("Goal").status,
        arany::GoalStatus::Interrupted
    );
}

#[tokio::test]
async fn pause_during_approval_wait_dispatches_no_tool() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let pause = steering.clone();
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(
            arany::ToolCall::Write {
                path: "notes/facts.txt".into(),
                expected_digest: None,
                content: "changed".into(),
            },
            Some(Box::new(arany::GoalPlanContract {
                mission: "Change the facts".into(),
                resource_class: arany::ToolResourceClass::Default,
                command_timeout_ms: None,
                paths: vec!["notes".into()],
                commands: Vec::new(),
                skills: Vec::new(),
                mcp_tools: Vec::new(),
                success_criterion: arany::GoalSuccessCriterion::WorkspaceFileContent {
                    path: "notes/facts.txt".into(),
                    content: "changed".into(),
                },
                budget: arany::GoalBudget::default(),
            })),
        ),
        finish_with("never reached"),
    ]);
    let requests = provider.requests.clone();
    let mut engine =
        Engine::open(StateRoot::admit(&state_path).expect("state"), provider).expect("engine");
    let config = state_path.join("tools.json");
    std::fs::write(
        &config,
        br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#,
    )
    .expect("tool configuration");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
        .expect("tool configuration mode");
    engine
        .enable_tools(env!("CARGO_BIN_EXE_arany").into())
        .expect("Guard enabled");
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    engine.set_tool_approvals(approvals);
    engine.set_steering(steering);
    let mut input = request(&workspace, "Change the facts");
    input.include_paths.clear();

    // The approval wait is the only point where the caller can pause before dispatch.
    let decide = async {
        let approval = inbox.next().await.expect("approval requested");
        assert!(pause.pause(), "first pause");
        approval.decide(true);
    };
    let (run, ()) = tokio::join!(engine.run(input), decide);
    let outcome = run.expect("paused Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");

    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/facts.txt")).expect("facts"),
        "v1",
        "a Tool approved during a pause is not dispatched"
    );
    assert_eq!(requests.lock().expect("requests").len(), 1);
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let observed: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::ToolFinished { observation, .. } => Some(observation.disposition),
            _ => None,
        })
        .collect();
    assert_eq!(observed, [arany::ToolDisposition::Denied]);
    assert_eq!(
        view.goal(outcome.goal_id).expect("Goal").status,
        arany::GoalStatus::Interrupted
    );
}

#[tokio::test]
async fn pause_during_tool_review_skips_the_prompt_and_dispatches_no_tool() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let pause = steering.clone();
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(
            arany::ToolCall::Write {
                path: "notes/facts.txt".into(),
                expected_digest: None,
                content: "changed".into(),
            },
            Some(Box::new(arany::GoalPlanContract {
                mission: "Change the facts".into(),
                resource_class: arany::ToolResourceClass::Default,
                command_timeout_ms: None,
                paths: vec!["notes".into()],
                commands: Vec::new(),
                skills: Vec::new(),
                mcp_tools: Vec::new(),
                success_criterion: arany::GoalSuccessCriterion::WorkspaceFileContent {
                    path: "notes/facts.txt".into(),
                    content: "changed".into(),
                },
                budget: arany::GoalBudget::default(),
            })),
        ),
        ProviderOutcome::Finish(Finish {
            summary: "Ask the user".into(),
            result: "ask".into(),
        }),
        finish_with("never reached"),
    ])
    // Zero-based call 1 is the Tool review that follows the proposed Tool.
    .with_before_call(1, move || assert!(pause.pause(), "first pause"));
    let requests = provider.requests.clone();
    let mut engine =
        Engine::open(StateRoot::admit(&state_path).expect("state"), provider).expect("engine");
    let config = state_path.join("tools.json");
    std::fs::write(
        &config,
        br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#,
    )
    .expect("tool configuration");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
        .expect("tool configuration mode");
    engine
        .enable_tools(env!("CARGO_BIN_EXE_arany").into())
        .expect("Guard enabled");
    let (approvals, _inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Auto);
    engine.set_tool_approvals(approvals);
    engine.set_steering(steering);
    let mut input = request(&workspace, "Change the facts");
    input.include_paths.clear();
    let outcome = engine.run(input).await.expect("paused Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");

    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/facts.txt")).expect("facts"),
        "v1",
        "a Tool reviewed during a pause is not dispatched"
    );
    assert_eq!(requests.lock().expect("requests").len(), 2);
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let observed: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::ToolFinished { observation, .. } => Some(observation.disposition),
            _ => None,
        })
        .collect();
    assert_eq!(observed, [arany::ToolDisposition::Denied]);
    assert_eq!(
        view.goal(outcome.goal_id).expect("Goal").status,
        arany::GoalStatus::Interrupted
    );
}

#[tokio::test]
async fn pause_at_the_delegate_boundary_spawns_no_child() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let pause = steering.clone();
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            children: vec!["Inspect facts".into()],
        }),
        finish_with("never reached"),
    ])
    .with_before_call(0, move || assert!(pause.pause(), "first pause"));
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    engine.set_steering(steering);
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    let outcome = engine.run(input).await.expect("paused Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");

    assert_eq!(requests.lock().expect("requests").len(), 1);
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert!(
        !events.iter().any(|event| matches!(
            event.event,
            Event::AgentSpawned {
                role: arany::AgentRole::Child,
                ..
            }
        )),
        "a paused Delegate spawns no child"
    );
    assert_eq!(
        view.goal(outcome.goal_id).expect("Goal").status,
        arany::GoalStatus::Interrupted
    );
}

#[tokio::test]
async fn pause_before_synthesis_starts_no_synthesis_call() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let pause = steering.clone();
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            children: vec!["Inspect facts".into()],
        }),
        finish_with("child result"),
        finish_with("never synthesized"),
    ])
    .with_before_call(1, move || assert!(pause.pause(), "first pause"));
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    engine.set_steering(steering);
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    let outcome = engine.run(input).await.expect("paused Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert_eq!(
        calls.len(),
        2,
        "the in-flight child call completes, synthesis does not start"
    );
    assert_eq!(calls[1].phase, AgentPhase::ChildWork);
    let (_, view) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(
        view.goal(outcome.goal_id).expect("Goal").status,
        arany::GoalStatus::Interrupted
    );
}

#[tokio::test]
async fn steering_note_waits_for_the_next_autonomous_run_primary_call() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let offer = steering.clone();
    // The first Run's Finish is unsatisfied against the planned digest, so the Goal starts a second Run.
    // The note is offered while the first Run's Finish is in flight: it has no primary call left in
    // that Run, so it waits for the second Run's first primary call.
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget.max_runs = 2;
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read(), Some(Box::new(plan))),
        finish_with("first pass"),
        finish_with("second pass"),
    ])
    .with_before_call(1, move || {
        steering_note_at(&offer, "carry this into the next Run")
    });
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    engine.set_steering(steering.clone());
    let budget = arany::GoalBudget {
        max_runs: 2,
        ..arany::GoalBudget::default()
    };
    let mut input = request(&workspace, AUTONOMOUS_OBJECTIVE);
    input.include_paths.clear();
    input.goal = arany::GoalRequest::autonomous(budget, arany::GoalPhase::Implementation);
    let outcome = engine.run(input).await.expect("terminal Run");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert_eq!(calls.len(), 3);
    assert!(call_steering_notes(&calls[1]).is_empty());
    assert_eq!(
        call_steering_notes(&calls[2]),
        ["carry this into the next Run"]
    );
    assert_eq!(steering.delivered_notes(), 1);
    let (_, view) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(view.runs.len(), 2);
    assert!(view.runs[0].steering_notes.is_empty());
    assert_eq!(
        view.runs[1].steering_notes,
        ["carry this into the next Run"]
    );
    assert_eq!(view.runs[1].id, outcome.run.id);
}

/// Parses the catalog a child Tool request carried; the catalog is pinned JSON data.
fn child_catalog(request: &arany::ProviderRequest) -> serde_json::Value {
    let tools = request.input.tools.as_ref().expect("research catalog");
    serde_json::from_str(&tools.catalog).expect("catalog JSON")
}

fn child_tool_starts(events: &[arany::EventEnvelope], child: arany::AgentRunId) -> usize {
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
        ProviderOutcome::Delegate(Delegate {
            children: vec!["Inspect facts".into()],
        }),
        crate::tool_outcome(facts_read(), None),
        finish_with("facts say v1"),
        finish_with("synthesized"),
    ]);
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    input.goal.budget = Some(arany::GoalBudget {
        max_provider_calls: Some(8),
        ..arany::GoalBudget::default()
    });
    let outcome = engine.run(input).await.expect("Run");
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
    let child = view.runs[0].agents[1].id;
    assert_eq!(
        view.runs[0].agents[1].grant,
        Some(arany::ChildGrant::ReadOnly)
    );
    assert_eq!(child_tool_starts(&events, child), 1);
    assert!(view.runs[0].tools.is_empty());
    assert_eq!(view.runs[0].agents[1].tools.len(), 1);
}

#[tokio::test]
async fn child_mcp_call_or_write_is_rejected_before_dispatch() {
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
        arany::ToolCall::GitStatus { path: ".".into() },
    ];
    for call in outcomes {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Delegate(Delegate {
                children: vec!["Inspect facts".into()],
            }),
            crate::tool_outcome(call, None),
            finish_with("never reached"),
        ]);
        let requests = provider.requests.clone();
        let mut engine = facts_engine(
            StateRoot::admit(&state_path).expect("state"),
            &state_path,
            provider,
        );
        let mut input = request(&workspace, "Check the facts");
        input.include_paths.clear();
        input.policy = CollaborationPolicy::Auto {
            max_active_children: 1,
        };
        let outcome = engine.run(input).await.expect("Run");
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
async fn child_reads_stay_inside_the_plan_and_never_satisfy_the_goal_criterion() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    std::fs::write(workspace.join("notes/other.txt"), "OTHER-CANARY").expect("other file");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(
            arany::ToolCall::Search {
                path: "notes/facts.txt".into(),
                query: "v".into(),
            },
            Some(Box::new(facts_plan(facts_digest("v1")))),
        ),
        ProviderOutcome::Delegate(Delegate {
            children: vec!["Inspect notes".into()],
        }),
        crate::tool_outcome(facts_read(), None),
        crate::tool_outcome(
            arany::ToolCall::Read {
                path: "notes/other.txt".into(),
                offset: 0,
                limit: 4096,
            },
            None,
        ),
        finish_with("report"),
        finish_with("done"),
    ]);
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    let outcome = engine.run(input).await.expect("Run");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    engine.close().await.expect("engine shutdown");

    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let child = view.runs[0].agents[1].id;
    let dispositions: Vec<_> = view.runs[0].agents[1]
        .tools
        .iter()
        .filter_map(|tool| {
            tool.observation
                .as_ref()
                .map(|observation| observation.disposition)
        })
        .collect();
    assert_eq!(
        dispositions,
        [
            arany::ToolDisposition::Succeeded,
            arany::ToolDisposition::Denied
        ]
    );
    let denied = view.runs[0].agents[1].tools[1]
        .observation
        .as_ref()
        .expect("Denied observation");
    assert_eq!(
        denied.output,
        "The effect is outside the active Goal plan; no operation was dispatched"
    );
    assert!(denied.guard.is_none());
    assert!(!format!("{:?}", requests.lock().expect("requests")).contains("OTHER-CANARY"));
    assert!(!format!("{events:?}").contains("OTHER-CANARY"));
    assert!(!view.goals[0].success_criterion_satisfied);
    assert_eq!(child_tool_starts(&events, child), 2);
}

#[tokio::test]
async fn child_tool_bound_offers_only_finish_and_rejects_a_further_tool() {
    for further_tool in [false, true] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let mut outcomes = vec![ProviderOutcome::Delegate(Delegate {
            children: vec!["Inspect facts".into()],
        })];
        outcomes.extend((0..16).map(|_| crate::tool_outcome(facts_read(), None)));
        if further_tool {
            outcomes.push(crate::tool_outcome(facts_read(), None));
        } else {
            outcomes.push(finish_with("report"));
        }
        outcomes.push(finish_with("done"));
        let provider = ScriptedProvider::new(outcomes);
        let requests = provider.requests.clone();
        let mut engine = facts_engine(
            StateRoot::admit(&state_path).expect("state"),
            &state_path,
            provider,
        );
        let mut input = request(&workspace, "Check the facts");
        input.include_paths.clear();
        input.policy = CollaborationPolicy::Auto {
            max_active_children: 1,
        };
        let outcome = engine.run(input).await.expect("Run");
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
async fn pause_during_research_finishes_the_child_call_and_dispatches_no_child_tool() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let pause = steering.clone();
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            children: vec!["Inspect facts".into()],
        }),
        crate::tool_outcome(facts_read(), None),
        finish_with("never reached"),
    ])
    .with_before_call(1, move || assert!(pause.pause(), "first pause"));
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    engine.set_steering(steering);
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    let outcome = engine.run(input).await.expect("paused Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");
    assert_eq!(requests.lock().expect("requests").len(), 2);
    assert_paused_child_stopped(&state_path, outcome.session_id, outcome.goal_id).await;
}

#[tokio::test]
async fn cancellation_after_a_child_tool_request_dispatches_no_child_tool() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let cancellation = RunCancellation::new();
    let mut provider = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            children: vec!["Inspect facts".into()],
        }),
        crate::tool_outcome(facts_read(), None),
        finish_with("never reached"),
    ]);
    provider.cancel_at_completion = Some((AgentPhase::ChildWork, cancellation.clone()));
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    let outcome = engine
        .run_with_cancel(input, cancellation)
        .await
        .expect("cancelled Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");
    assert_eq!(requests.lock().expect("requests").len(), 2);
    assert_paused_child_stopped(&state_path, outcome.session_id, outcome.goal_id).await;
}

#[tokio::test]
async fn child_read_request_at_an_exhausted_goal_budget_stops_as_budget_not_failure() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            children: vec!["Inspect facts".into()],
        }),
        crate::tool_outcome(facts_read(), None),
        finish_with("never reached"),
    ])
    .with_known_input_tokens();
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    // The primary call spends 8 tokens; the child's call spends the last 8 and exhausts the budget.
    input.goal.budget = Some(arany::GoalBudget {
        max_tokens: Some(16),
        ..arany::GoalBudget::default()
    });
    let outcome = engine.run(input).await.expect("budget-exhausted Run");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    engine.close().await.expect("engine shutdown");
    assert_eq!(requests.lock().expect("requests").len(), 2);

    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let child = view.runs[0].agents[1].id;
    assert_eq!(child_tool_starts(&events, child), 0);
    assert!(events.iter().any(|event| matches!(
        &event.event,
        Event::AgentFinished { agent_run_id, disposition, .. }
            if *agent_run_id == child && *disposition == arany::AgentDisposition::Cancelled
    )));
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert_eq!(goal.status, arany::GoalStatus::Failed);
    assert!(goal.report.is_some());
}

/// The child's last call is its observed Tool request; no Tool starts and the Goal is interrupted.
async fn assert_paused_child_stopped(
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
async fn near_deadline_child_tool_stops_before_tool_started() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            children: vec!["Inspect facts".into()],
        }),
        crate::tool_outcome(facts_read(), None),
        finish_with("never reached"),
    ]);
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    input.goal.budget = Some(arany::GoalBudget {
        max_active_ms: Some(15_000),
        ..arany::GoalBudget::default()
    });
    let outcome = engine.run(input).await.expect("Run");
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
                && record.disposition == ProviderCallDisposition::ToolRequested
    )));
    assert!(events.iter().any(|event| matches!(
        &event.event,
        Event::AgentFinished { agent_run_id, disposition, .. }
            if *agent_run_id == child && *disposition == arany::AgentDisposition::Failed
    )));
}

#[tokio::test]
async fn pause_before_queued_children_start_cancels_them_without_failing_the_run() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let pause = steering.clone();
    let mut provider = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            children: vec!["Inspect facts".into(), "Inspect facts again".into()],
        }),
        crate::tool_outcome(facts_read(), None),
        finish_with("never reached"),
    ])
    .with_before_call(1, move || assert!(pause.pause(), "first pause"));
    // One call at a time: the second child stays queued when the pause lands.
    provider.capacity = 1;
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    engine.set_steering(steering);
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 2,
    };
    let outcome = engine.run(input).await.expect("paused Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");
    assert_eq!(requests.lock().expect("requests").len(), 2);

    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let children: Vec<_> = view.runs[0]
        .agents
        .iter()
        .skip(1)
        .map(|agent| agent.id)
        .collect();
    assert_eq!(children.len(), 2);
    for child in children {
        assert_eq!(child_tool_starts(&events, child), 0);
        assert!(events.iter().any(|event| matches!(
            &event.event,
            Event::AgentFinished { agent_run_id, disposition, .. }
                if *agent_run_id == child && *disposition == arany::AgentDisposition::Cancelled
        )));
    }
    assert_eq!(
        view.goal(outcome.goal_id).expect("Goal").status,
        arany::GoalStatus::Interrupted
    );
}

#[tokio::test]
async fn tool_step_journals_requested_tools_one_and_replays() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read(), None),
        crate::tool_outcome(facts_read(), None),
        finish_with("done"),
    ]);
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let mut input = request(&workspace, "Read twice");
    input.include_paths.clear();
    let outcome = engine.run(input).await.expect("Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let requested: Vec<u8> = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::ProviderCallRecorded { record, .. } => Some(record.requested_tools),
            _ => None,
        })
        .collect();
    assert_eq!(requested, [1, 1, 0]);
    assert_eq!(
        view.runs[0], outcome.run,
        "cold replay equals the live view"
    );
    let calls = requests.lock().expect("requests").clone();
    assert_eq!(calls.len(), 3);
    for (index, call) in calls.iter().enumerate() {
        let steps = &call.input.tools.as_ref().expect("Tool context").steps;
        assert_eq!(steps.len(), index);
        assert!(steps.iter().all(|step| step.requests.len() == 1
            && step.observations.len() == 1
            && step.observations[0].intent.call == step.requests[0].call));
    }
}

#[tokio::test]
async fn delegation_turn_is_replayed_before_synthesis() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            children: vec!["Inspect facts".into()],
        }),
        finish_with("facts say v1"),
        finish_with("synthesized"),
    ]);
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    let outcome = engine.run(input).await.expect("Run");
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
async fn interrupted_prefix_replays_and_a_new_run_starts_without_steps() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read(), None),
        finish_with("first"),
        finish_with("retry"),
    ]);
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let mut input = request(&workspace, "Read once");
    input.include_paths.clear();
    let first = engine.run(input).await.expect("Run");

    let (events, _) = replayed(&state_path, first.session_id).await;
    let started = events
        .iter()
        .position(|event| matches!(event.event, Event::ToolStarted { .. }))
        .expect("started Tool");
    let crashed = SessionView::replay(first.session_id, &events[..=started])
        .expect("valid prefix")
        .expect("Session");
    assert_eq!(
        crashed.runs[0].status,
        RunStatus::Interrupted,
        "no replayed turn can resurrect a lost Run"
    );

    let mut retry = request(&workspace, "Read again");
    retry.include_paths.clear();
    retry.session_id = Some(first.session_id);
    retry.title = None;
    engine.run(retry).await.expect("retry Run");
    engine.close().await.expect("engine shutdown");
    let calls = requests.lock().expect("requests").clone();
    let steps = &calls[2].input.tools.as_ref().expect("Tool context").steps;
    assert!(steps.is_empty(), "a retry is a new Run with no steps");
}

#[tokio::test]
async fn step_facts_and_notes_are_written_before_render() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let offer = steering.clone();
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read(), None),
        crate::tool_outcome(facts_read(), None),
        finish_with("noted"),
    ])
    .with_before_call(1, move || steering_note_at(&offer, "mid-run note"));
    let requests = provider.requests.clone();
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    engine.set_steering(steering);
    let mut input = request(&workspace, "Read twice");
    input.include_paths.clear();
    engine.run(input).await.expect("Run");
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert!(call_facts(&calls[0]).is_some());
    for call in &calls[1..] {
        let steps = &call.input.tools.as_ref().expect("Tool context").steps;
        let last = steps.last().expect("a step");
        assert!(
            last.facts.is_some(),
            "facts exist before the request is built"
        );
        assert!(
            steps[..steps.len() - 1]
                .iter()
                .all(|step| step.facts.is_some()),
            "earlier steps keep the facts they were rendered with"
        );
    }
    let provider_calls: Vec<_> = calls
        .iter()
        .map(|call| call_facts(call).unwrap().provider_calls)
        .collect();
    assert!(provider_calls.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(call_steering_notes(&calls[1]), Vec::<String>::new());
    assert_eq!(call_steering_notes(&calls[2]), ["mid-run note"]);
    let late = &calls[2].input.tools.as_ref().expect("Tool context").steps;
    assert_eq!(
        late[1].steering_notes,
        ["mid-run note"],
        "the note rides the newest step"
    );
}

#[tokio::test]
async fn restating_the_recorded_plan_contract_is_a_no_op_and_a_different_one_is_refused() {
    for (restated_plan, expected) in [
        (facts_plan(facts_digest("v1")), RunStatus::Finished),
        (
            arany::GoalPlanContract {
                mission: "A different mission".into(),
                ..facts_plan(facts_digest("v1"))
            },
            RunStatus::Failed,
        ),
    ] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![
            crate::tool_outcome(facts_read(), Some(Box::new(facts_plan(facts_digest("v1"))))),
            crate::tool_outcome(facts_read(), Some(Box::new(restated_plan))),
            finish_with("done"),
        ]);
        let mut engine = facts_engine(
            StateRoot::admit(&state_path).expect("state"),
            &state_path,
            provider,
        );
        let mut input = request(&workspace, "Read twice");
        input.include_paths.clear();
        let outcome = engine.run(input).await.expect("Run");
        assert_eq!(outcome.run.status, expected);
        engine.close().await.expect("engine shutdown");
        let (events, _) = replayed(&state_path, outcome.session_id).await;
        let recorded = events
            .iter()
            .filter(|event| matches!(event.event, Event::GoalPlanContractRecorded { .. }))
            .count();
        assert_eq!(recorded, 1, "the contract is journaled once");
    }
}

#[tokio::test]
async fn plan_contract_recorded_mid_run_survives_compaction() {
    const PAGE_BYTES: usize = 4096;
    const READS: usize = 104;

    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
    let content = "x".repeat(PAGE_BYTES * READS);
    std::fs::write(workspace.join("notes/data.txt"), &content).expect("bounded fixture file");
    let state_path = temp.path().join("state");
    let budget = arany::GoalBudget {
        max_provider_calls: None,
        max_tokens: None,
        max_active_ms: Some(30 * 60 * 1000),
        max_runs: 1,
    };
    let plan = arany::GoalPlanContract {
        mission: "Inspect the bounded file".into(),
        resource_class: arany::ToolResourceClass::Default,
        command_timeout_ms: None,
        paths: vec!["notes/data.txt".into()],
        commands: Vec::new(),
        skills: Vec::new(),
        mcp_tools: Vec::new(),
        success_criterion: arany::GoalSuccessCriterion::WorkspaceFileDigest {
            path: "notes/data.txt".into(),
            sha256: facts_digest(&content),
        },
        budget,
    };
    let mut outcomes: Vec<_> = (0..READS)
        .map(|page| {
            crate::tool_outcome(
                arany::ToolCall::Read {
                    path: "notes/data.txt".into(),
                    offset: u32::try_from(page * PAGE_BYTES).expect("bounded file offset"),
                    limit: u32::try_from(PAGE_BYTES).expect("bounded page size"),
                },
                (page == 0).then(|| Box::new(plan.clone())),
            )
        })
        .collect();
    outcomes.push(finish_with("inspected"));
    let compaction = || {
        Ok(CompactionResponse {
            summary: "Earlier pages were read.".into(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: Some(32),
                output_tokens: Some(12),
                ..Default::default()
            },
            wire_provenance: None,
        })
    };
    let provider =
        ScriptedProvider::new(outcomes).with_compactions((0..16).map(|_| compaction()).collect());
    let requests = Arc::clone(&provider.requests);
    let mut engine = facts_engine(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        provider,
    );
    let mut input = request(&workspace, "Inspect the bounded file in pages");
    input.include_paths.clear();
    input.goal.budget = Some(budget);
    let outcome = engine.run(input).await.expect("Goal");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert!(calls[0].input.goal_plan_contract.is_none());
    let compacted: Vec<_> = calls
        .iter()
        .filter(|call| {
            call.input
                .tools
                .as_ref()
                .is_some_and(|tools| tools.history_summary.is_some())
        })
        .collect();
    assert!(!compacted.is_empty(), "the Run compacts");
    assert!(
        compacted
            .iter()
            .all(|call| call.input.goal_plan_contract.as_ref() == Some(&plan)),
        "the recorded contract is re-introduced when its step is compacted away"
    );
}

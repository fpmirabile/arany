use super::*;
use arany::WorkspaceIdentity;
use arany::test_support::goals::*;

#[tokio::test]
async fn goal_lifecycle_bounds_report_and_requires_goal_replay_linkage() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    // Control characters escape to six JSON bytes each, so the serialized report budget shortens this summary.
    let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
        text: "\u{1}".repeat(2000),
    })]);
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let mut input = request(&workspace, "Complete a Goal");
    input.include_paths.clear();
    let outcome = run_to_end(&engine, &provider, input)
        .await
        .expect("Goal Run");
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
    let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
        text: "accepted result".into(),
    })])
    .with_cached_input_tokens(7);
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let mut input = request(&workspace, "Complete a Goal");
    input.include_paths.clear();
    let outcome = run_to_end(&engine, &provider, input)
        .await
        .expect("Goal Run");
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
async fn answer_summary_uses_the_first_nonempty_line_and_a_character_boundary() {
    for (summary, expected) in [
        ("ࠀ".repeat(620), "ࠀ".repeat(620)),
        ("🚀".repeat(600), "🚀".repeat(512)),
    ] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
            text: format!("\n \n{summary}\naccepted result"),
        })]);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut input = request(&workspace, "Complete a Goal");
        input.include_paths.clear();
        let outcome = run_to_end(&engine, &provider, input)
            .await
            .expect("Goal Run");
        engine.close().await.expect("engine shutdown");

        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only Store");
        let view = store
            .load_view(outcome.session_id)
            .await
            .expect("replayed Session")
            .expect("Session");
        let report = view.goals[0].report.as_ref().expect("durable report");
        assert_eq!(report.summary, expected);
        assert!(
            !report
                .unresolved
                .iter()
                .any(|item| item.contains("summary was shortened"))
        );
        store.close().await.expect("Store shutdown");
    }
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

const READS: usize = 104;

fn paged_read_workspace(root: &Path) -> std::path::PathBuf {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
    std::fs::write(
        workspace.join("notes/data.txt"),
        format!("{}\n", "x".repeat(1023)).repeat(2 * READS),
    )
    .expect("bounded fixture file");
    workspace
}

fn paged_read_outcomes() -> Vec<ProviderOutcome> {
    let mut outcomes = (0..READS)
        .map(|page| {
            crate::tool_outcome(arany::ToolCall::Read {
                path: "notes/data.txt".into(),
                offset: Some(u32::try_from(page * 2 + 1).expect("bounded line offset")),
                limit: Some(2),
            })
        })
        .collect::<Vec<_>>();
    outcomes.push(ProviderOutcome::Answer(Answer {
        text: "The bounded pages were inspected".into(),
    }));
    outcomes
}

async fn compact_read_only_goal_prefix(objective: &str) {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = paged_read_workspace(temp.path());
    let state_path = temp.path().join("state");
    let outcomes = paged_read_outcomes();
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
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("private state"),
        EngineConfig {
            guard_executable: Some(std::env::current_exe().expect("test executable")),
            ..Default::default()
        },
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
    let mut input = request(&workspace, objective);
    input.include_paths.clear();
    input.goal.budget = Some(arany::GoalBudget {
        max_provider_calls: None,
        max_tokens: None,
        max_active_ms: Some(30 * 60 * 1000),
        max_runs: 1,
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
    .expect("read-only Goal");
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
    assert_action_bounds(&events, false, false);
    // One Tool step journals its call reservation and record, then the Tool's start and finish.
    let reservations: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, envelope)| matches!(envelope.event, Event::GoalProviderCallReserved { .. }))
        .map(|(index, _)| index)
        .collect();
    let tool_steps: Vec<usize> = reservations
        .windows(2)
        .map(|pair| &events[pair[0]..pair[1]])
        .filter(|step| {
            step.iter()
                .any(|envelope| matches!(envelope.event, Event::ToolStarted { .. }))
        })
        .map(|step| {
            step.iter()
                .filter(|envelope| {
                    !matches!(
                        envelope.event,
                        Event::GoalCompactionCallReserved { .. }
                            | Event::GoalContextCompacted { .. }
                    )
                })
                .count()
        })
        .collect();
    assert!(tool_steps.len() >= READS - 1, "{tool_steps:?}");
    assert!(
        tool_steps.iter().all(|&events| events <= 4),
        "{tool_steps:?}"
    );
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
            ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
                text: "answer".into(),
            })])
        } else {
            ScriptedProvider::new(
                (0..3)
                    .map(|_| {
                        ProviderOutcome::Answer(Answer {
                            text: " \n ".into(),
                        })
                    })
                    .collect(),
            )
        };
        if cancel {
            provider.cancel_at_completion = Some((AgentPhase::RootPlan, cancellation.clone()));
        }
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut input = request(&workspace, "Answer or cancel");
        input.include_paths.clear();
        let outcome = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                cancellation,
                ..Default::default()
            },
        )
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
        assert!(!view.goals[0].budget_closed);
        assert!(view.continuable_goal().is_none());
        store.close().await.expect("Store shutdown");
    }
}

#[tokio::test]
async fn goal_call_budget_blocks_a_later_run_before_provider_dispatch() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
        text: "continue later".into(),
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
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
    )
    .expect("engine");
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
    let initial = run_to_end(&engine, &provider, first)
        .await
        .expect("first Goal Run");
    let compaction = engine
        .compact_session(
            &AdmittedProviders::single(provider.clone()),
            initial.session_id,
            workspace.clone(),
        )
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
    let result = run_to_end(&engine, &provider, second).await;
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
                Event::GoalCompactionCallReserved { goal_id, run_id, .. }
                    if *goal_id == initial.goal_id && *run_id == initial.run.id
            )
        })
        .expect("durable in-Goal compaction call reservation");
    let Event::ContextCompacted {
        record,
        active_ms: Some(compaction_ms),
    } = &events[compaction_call + 1].event
    else {
        panic!("the Goal compaction records its settled active time");
    };
    assert_eq!(record.source_bytes as usize, compacted_input_bytes);
    let run_active_ms = events
        .iter()
        .find_map(|envelope| match &envelope.event {
            Event::RunFinished { active_ms, .. } => Some(*active_ms),
            _ => None,
        })
        .expect("the first Run settles its active time");
    let replay = SessionView::replay(initial.session_id, &events)
        .expect("strict replay of the between-Run compaction")
        .expect("Session");
    assert_eq!(replay.goals[0].active_ms, run_active_ms + compaction_ms);

    // A longer compaction is charged to the Goal, and the terminal receipt must include it.
    let mut slower = events.clone();
    if let Event::ContextCompacted { active_ms, .. } = &mut slower[compaction_call + 1].event {
        *active_ms = Some(compaction_ms + 1_000);
    }
    assert!(SessionView::replay(initial.session_id, &slower).is_err());
    let Event::GoalFinished { active_ms, .. } =
        &mut slower.last_mut().expect("terminal Goal receipt").event
    else {
        panic!("terminal Goal receipt");
    };
    *active_ms += 1_000;
    let replay = SessionView::replay(initial.session_id, &slower)
        .expect("the receipt includes the compaction's time")
        .expect("Session");
    assert_eq!(
        replay.goals[0].active_ms,
        run_active_ms + compaction_ms + 1_000
    );
    store.close().await.expect("Store shutdown");
}

#[tokio::test]
async fn known_active_time_exhaustion_closes_without_provider_dispatch() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
        text: "must not run".into(),
    })]);
    let requests = Arc::clone(&provider.requests);
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let mut input = request(&workspace, "Exhaust the tiny active-time budget");
    input.include_paths.clear();
    input.goal = arany::GoalRequest::new(
        arany::GoalBudget {
            max_active_ms: Some(1),
            ..arany::GoalBudget::default()
        },
        arany::GoalPhase::Implementation,
    );

    let outcome = run_to_end(&engine, &provider, input)
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
    assert!(view.goals[0].active_deadline.is_none());
    store.close().await.expect("Store shutdown");
}

#[tokio::test]
async fn goal_paused_at_its_call_budget_closes_as_failed_on_explicit_resume() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let first_provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
        text: "continue later".into(),
    })]);
    let first_engine_provider = Arc::new(first_provider);
    let first_engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
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
    let initial = run_to_end(&first_engine, &first_engine_provider, first)
        .await
        .expect("first Goal Run");
    first_engine.close().await.expect("first engine shutdown");

    let second_provider = ScriptedProvider::new(Vec::new());
    let second_requests = Arc::clone(&second_provider.requests);
    let second_engine_provider = Arc::new(second_provider);
    let second_engine = engine_support::open(
        StateRoot::open_existing(&state_path).expect("reopened state"),
        EngineConfig::default(),
    )
    .expect("second engine");
    let mut resumed = request(&workspace, "Try the exhausted Goal");
    resumed.session_id = Some(initial.session_id);
    resumed.title = None;
    resumed.include_paths.clear();
    resumed.goal = arany::GoalRequest::resume(initial.goal_id, true);
    let result = run_to_end(&second_engine, &second_engine_provider, resumed).await;
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
            run_to_end(&second_engine, &second_engine_provider, again).await,
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

#[tokio::test]
async fn exhausted_goal_continues_as_a_new_linked_goal_with_a_fresh_budget_after_restart() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let first_provider = Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Answer(
        Answer {
            text: "continue later".into(),
        },
    )]));
    let first_engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
    )
    .expect("first engine");
    let mut first = request(&workspace, "Ship the migration");
    first.include_paths.clear();
    first.goal = arany::GoalRequest::new(
        arany::GoalBudget {
            max_provider_calls: Some(1),
            ..arany::GoalBudget::default()
        },
        arany::GoalPhase::Implementation,
    );
    first.goal.finish_after_run = false;
    let initial = run_to_end(&first_engine, &first_provider, first)
        .await
        .expect("first Goal Run");
    first_engine.close().await.expect("first engine shutdown");

    let engine = engine_support::open(
        StateRoot::open_existing(&state_path).expect("reopened state"),
        EngineConfig::default(),
    )
    .expect("second engine");
    let provider = Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Answer(
        Answer {
            text: "done".into(),
        },
    )]));
    let continue_request = |source| {
        let mut next = request(&workspace, "");
        next.session_id = Some(initial.session_id);
        next.title = None;
        next.include_paths.clear();
        next.goal = arany::GoalRequest::continue_from(
            source,
            arany::GoalBudget {
                max_provider_calls: Some(4),
                ..arany::GoalBudget::default()
            },
            arany::ToolResourceSettings::default(),
        );
        next
    };

    // The budget-paused Goal is not closed yet, so it is not an exhausted source.
    assert!(matches!(
        run_to_end(&engine, &provider, continue_request(initial.goal_id)).await,
        Err(EngineError::GoalNotContinuable)
    ));

    let mut closing = request(&workspace, "");
    closing.session_id = Some(initial.session_id);
    closing.title = None;
    closing.include_paths.clear();
    closing.goal = arany::GoalRequest::resume(initial.goal_id, true);
    assert!(matches!(
        run_to_end(&engine, &provider, closing).await,
        Err(EngineError::GoalBudgetExhausted)
    ));

    let unknown = arany::GoalId::new();
    assert!(matches!(
        run_to_end(&engine, &provider, continue_request(unknown)).await,
        Err(EngineError::GoalNotContinuable)
    ));
    let mut with_text = continue_request(initial.goal_id);
    with_text.objective = "another objective".into();
    assert!(matches!(
        run_to_end(&engine, &provider, with_text).await,
        Err(EngineError::GoalNotContinuable)
    ));

    let continued = run_to_end(&engine, &provider, continue_request(initial.goal_id))
        .await
        .expect("continued Goal Run");
    assert_ne!(continued.goal_id, initial.goal_id);
    assert!(matches!(
        run_to_end(&engine, &provider, continue_request(initial.goal_id)).await,
        Err(EngineError::GoalNotContinuable)
    ));
    engine.close().await.expect("engine shutdown");

    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let events = store
        .load_session(initial.session_id)
        .await
        .expect("Events");
    let view = SessionView::replay(initial.session_id, &events)
        .expect("strict replay")
        .expect("Session");
    let old = view.goal(initial.goal_id).expect("source Goal");
    let new = view.goal(continued.goal_id).expect("continued Goal");
    assert_eq!(old.status, arany::GoalStatus::Failed);
    assert_eq!(old.run_ids.len(), 1);
    assert_eq!(new.continues, Some(initial.goal_id));
    assert_eq!(new.objective, "Ship the migration");
    assert_eq!(new.budget.max_provider_calls, Some(4));
    assert_eq!(new.status, arany::GoalStatus::Completed);
    assert_eq!(new.run_ids.len(), 1);
    assert_eq!(new.provider_calls, 1);
    assert!(!events.iter().any(|event| matches!(
        &event.event,
        Event::GoalResumed { goal_id } if *goal_id == initial.goal_id
    )));
    store.close().await.expect("Store shutdown");
}

#[tokio::test]
async fn refused_answer_on_the_last_allowed_call_is_a_budget_close() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let provider = Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Answer(
        Answer {
            text: " \n ".into(),
        },
    )]));
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let mut input = request(&workspace, "Fail on the last call");
    input.include_paths.clear();
    input.goal = arany::GoalRequest::new(
        arany::GoalBudget {
            max_provider_calls: Some(1),
            ..arany::GoalBudget::default()
        },
        arany::GoalPhase::Implementation,
    );
    let outcome = run_to_end(&engine, &provider, input)
        .await
        .expect("failed Run");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, outcome.session_id).await;
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert_eq!(goal.status, arany::GoalStatus::Failed);
    assert_eq!(goal.provider_calls, 1);
    assert!(goal.budget_closed);
    assert_eq!(
        view.continuable_goal().map(|source| source.id),
        Some(goal.id)
    );
}

#[tokio::test]
async fn tool_request_refused_on_the_last_allowed_call_is_a_budget_close() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let budget = arany::GoalBudget {
        max_provider_calls: Some(1),
        ..arany::GoalBudget::default()
    };
    let provider = Arc::new(ScriptedProvider::new(vec![crate::tool_outcome(
        arany::ToolCall::Search {
            path: "notes/facts.txt".into(),
            query: "v".into(),
        },
    )]));
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Read after the budget");
    input.include_paths.clear();
    input.goal = arany::GoalRequest::new(budget, arany::GoalPhase::Implementation);
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
    .expect("budget-stopped Run");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, outcome.session_id).await;
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert_eq!(goal.status, arany::GoalStatus::Failed);
    assert!(goal.budget_closed);
    assert_eq!(
        view.continuable_goal().map(|source| source.id),
        Some(goal.id)
    );
}

#[tokio::test]
async fn single_run_goal_with_an_unmet_plan_criterion_is_not_a_budget_close() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let budget = arany::GoalBudget {
        max_provider_calls: Some(8),
        ..arany::GoalBudget::default()
    };
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget = budget;
    let provider = Arc::new(ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(plan),
        crate::tool_outcome(facts_read()),
        finish_with("criterion unmet"),
    ]));
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Reach the digest");
    input.include_paths.clear();
    input.goal = arany::GoalRequest::new(budget, arany::GoalPhase::Implementation);
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
    .expect("failed Run");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    assert_eq!(outcome.run.failure, Some(arany::RunFailure::CriterionUnmet));
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, outcome.session_id).await;
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert_eq!(goal.status, arany::GoalStatus::Failed);
    assert!(!goal.budget_closed);
    assert!(view.continuable_goal().is_none());
}

struct FiniteTokenTeam {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

#[async_trait::async_trait]
impl Provider for FiniteTokenTeam {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                let mut identity = arany::ProviderIdentity::new(
                    "scripted",
                    "goal-token-team-model",
                    arany::Provenance::native(),
                );
                identity.limits = arany::CallLimits {
                    concurrency: 3,
                    ..Default::default()
                };
                identity
            });
        &IDENTITY
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
                    agent: None,
                    children: vec![
                        arany::ChildAssignment {
                            editor: None,
                            objective: "Review one".into(),
                            profile: None,
                        },
                        arany::ChildAssignment {
                            editor: None,
                            objective: "Review two".into(),
                            profile: None,
                        },
                        arany::ChildAssignment {
                            editor: None,
                            objective: "Review three".into(),
                            profile: None,
                        },
                    ],
                }),
                0,
                1,
            ),
            AgentPhase::ChildWork => (
                ProviderOutcome::Answer(Answer {
                    text: "No issues found".into(),
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
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
    )
    .expect("engine");
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
    let outcome = run_to_end(&engine, &provider, input)
        .await
        .expect("bounded team Run");
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
    let first_provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
        text: "continue after review".into(),
    })]);
    let first_requests = Arc::clone(&first_provider.requests);
    let first_engine_provider = Arc::new(first_provider);
    let first_engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
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
    let initial = run_to_end(&first_engine, &first_engine_provider, first)
        .await
        .expect("first Goal Run");
    assert_eq!(first_requests.lock().expect("Provider requests").len(), 1);
    first_engine.close().await.expect("first engine shutdown");

    let second_provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
        text: "the Goal is complete".into(),
    })]);
    let second_requests = Arc::clone(&second_provider.requests);
    let second_engine_provider = Arc::new(second_provider);
    let second_engine = engine_support::open(
        StateRoot::open_existing(&state_path).expect("reopened state"),
        EngineConfig::default(),
    )
    .expect("second engine");
    let mut mismatched = request(&workspace, "Must not select a different Goal");
    mismatched.session_id = Some(initial.session_id);
    mismatched.title = None;
    mismatched.include_paths.clear();
    mismatched.goal = arany::GoalRequest::resume(arany::GoalId::new(), true);
    let result = run_to_end(&second_engine, &second_engine_provider, mismatched).await;
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
    let final_run = run_to_end(&second_engine, &second_engine_provider, resumed)
        .await
        .expect("exact Goal resume");
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
                    workspace_identity: WorkspaceIdentity {
                        device: metadata.dev(),
                        inode: metadata.ino(),
                    },
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
                    workspace_identity: WorkspaceIdentity {
                        device: metadata.dev(),
                        inode: metadata.ino(),
                    },
                    budget: arany::GoalBudget {
                        max_runs: 1,
                        ..arany::GoalBudget::default()
                    },
                    resource_settings: arany::ToolResourceSettings::default(),
                    continues: None,
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

        let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
            text: "Only the new Run was dispatched".into(),
        })]);
        let requests = Arc::clone(&provider.requests);
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::open_existing(&state_path).expect("reopened state"),
            EngineConfig::default(),
        )
        .expect("Engine");
        let mut mismatched = request(&workspace, "A different initial objective");
        mismatched.session_id = Some(session_id);
        mismatched.title = None;
        mismatched.include_paths.clear();
        mismatched.goal = arany::GoalRequest::resume(goal_id, true);
        assert!(matches!(
            run_to_end(&engine, &provider, mismatched).await,
            Err(EngineError::GoalUnavailable)
        ));
        assert!(requests.lock().expect("Provider requests").is_empty());

        let mut resumed = request(&workspace, "Resume an unaccepted task");
        resumed.session_id = Some(session_id);
        resumed.title = None;
        resumed.include_paths.clear();
        resumed.goal = arany::GoalRequest::resume(goal_id, true);
        let outcome = run_to_end(&engine, &provider, resumed)
            .await
            .expect("explicit recovery");
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
    let identity = WorkspaceIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
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
            continues: None,
        },
        Event::GoalRunLinked {
            goal_id,
            run_id,
            phase: arany::GoalPhase::Implementation,
        },
        Event::MessageAccepted {
            skill: None,
            run_id,
            text: "Use the only Provider call".into(),
            images: Vec::new(),
        },
        Event::RunStarted {
            run_id,
            config: arany::RunConfig {
                supervisor: None,
                worker_policy: None,
                agent_definitions: None,
                roster: None,
                providers: None,
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
                workspace_device: identity.device,
                workspace_inode: identity.inode,
                instruction_digest: None,
                include_digests: Vec::new(),
                history_run_ids: Vec::new(),
                excluded_history_runs: 0,
                context_usage: None,
                compaction_event_sequence: None,
                compaction_content_digest: None,
                tool_policy: None,
            },
            active_deadline_ms: Some(arany::MAX_GOAL_ACTIVE_MS),
        },
        Event::AgentSpawned {
            profile: None,
            assignment: None,
            definition: None,
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
            active_ceiling_ms: 0,
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
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::open_existing(&state_path).expect("reopened state"),
        EngineConfig::default(),
    )
    .expect("Engine");
    let mut resumed = request(&workspace, "Resume the crashed Goal");
    resumed.session_id = Some(session_id);
    resumed.title = None;
    resumed.include_paths.clear();
    resumed.goal = arany::GoalRequest::resume(goal_id, true);
    assert!(
        matches!(
            run_to_end(&engine, &provider, resumed).await,
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
        matches!(
            run_to_end(&engine, &provider, again).await,
            Err(EngineError::GoalUnavailable)
        ),
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
                    "scripted",
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
                outcome: ProviderOutcome::Delegate(Delegate {
                    agent: None,
                    children: vec![
                        arany::ChildAssignment {
                            editor: None,
                            objective: "First child".into(),
                            profile: None,
                        },
                        arany::ChildAssignment {
                            editor: None,
                            objective: "Second child".into(),
                            profile: None,
                        },
                    ],
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

#[tokio::test]
async fn resumed_goal_keeps_its_stored_phase() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let first_provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
        text: "continue after review".into(),
    })]);
    let first_engine_provider = Arc::new(first_provider);
    let first_engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
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
    let initial = run_to_end(&first_engine, &first_engine_provider, first)
        .await
        .expect("first Goal Run");
    first_engine.close().await.expect("first engine shutdown");

    let second_provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
        text: "the Goal is complete".into(),
    })]);
    let second_engine_provider = Arc::new(second_provider);
    let second_engine = engine_support::open(
        StateRoot::open_existing(&state_path).expect("reopened state"),
        EngineConfig::default(),
    )
    .expect("second engine");
    let mut resumed = request(&workspace, "Complete the reviewed Goal");
    resumed.session_id = Some(initial.session_id);
    resumed.title = None;
    resumed.include_paths.clear();
    resumed.goal = arany::GoalRequest::resume(initial.goal_id, true);
    let final_run = run_to_end(&second_engine, &second_engine_provider, resumed)
        .await
        .expect("exact Goal resume");
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

fn call_facts(request: &arany::ProviderRequest) -> Option<arany::GoalContextFacts> {
    request
        .input
        .tools
        .as_ref()
        .and_then(|tools| tools.steps.last())
        .and_then(|step| step.facts)
        .or(request.input.goal_context_facts)
}

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

fn facts_digest(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn facts_read() -> arany::ToolCall {
    arany::ToolCall::Read {
        path: "notes/facts.txt".into(),
        offset: None,
        limit: None,
    }
}

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

fn write_facts(workspace: &Path, text: &'static str) -> impl Fn() + Send + Sync + 'static {
    let path = workspace.join("notes/facts.txt");
    move || std::fs::write(&path, text).expect("updated facts file")
}

pub(super) fn facts_engine(state: StateRoot, state_path: &Path) -> Engine {
    facts_engine_with_telemetry(state, state_path, arany::Telemetry::disabled())
}

fn facts_engine_with_telemetry(
    state: StateRoot,
    state_path: &Path,
    telemetry: arany::Telemetry,
) -> Engine {
    use std::os::unix::fs::PermissionsExt;
    let engine = engine_support::open(
        state,
        EngineConfig {
            worker_concurrency: None,
            telemetry,
            guard_executable: Some(std::env::current_exe().expect("test executable")),
        },
    )
    .expect("engine");
    let config = state_path.join("tools.json");
    std::fs::write(
        &config,
        br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#,
    )
    .expect("tool configuration");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
        .expect("tool configuration mode");
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

async fn interrupted_autonomous_goal(
    state_path: &Path,
    workspace: &Path,
) -> (arany::SessionId, arany::GoalId) {
    let cancellation = RunCancellation::new();
    let cancel = cancellation.clone();
    let provider = ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        finish_with("interrupted pass"),
    ])
    .with_before_call(3, move || assert!(cancel.cancel(), "first cancellation"));
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(state_path).expect("state"), state_path);
    let outcome = run_with(
        &engine,
        &provider,
        autonomous_input(workspace),
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
    (outcome.session_id, outcome.goal_id)
}

#[tokio::test]
async fn autonomous_goal_iterates_until_its_success_criterion_is_satisfied() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        crate::tool_outcome(facts_read()),
        finish_with("verified"),
    ])
    .with_before_call(3, write_facts(&workspace, "v2"));
    let requests = Arc::clone(&provider.requests);
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let outcome = run_with(
        &engine,
        &provider,
        autonomous_input(&workspace),
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .expect("autonomous Goal");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    {
        let calls = requests.lock().expect("Provider requests");
        assert_eq!(
            calls.len(),
            5,
            "one plan call and two Runs with a Read then Answer"
        );
        assert_eq!(
            calls[3].input.objective, AUTONOMOUS_OBJECTIVE,
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
async fn autonomous_goal_pins_permissions_settings_at_submission() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let settings = state_path.join("permissions.json");
    let write_settings = |bytes: &'static [u8]| {
        let path = settings.clone();
        move || {
            std::fs::write(&path, bytes).expect("permissions settings");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("permissions settings mode");
        }
    };
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    write_settings(br#"{"permissions":{"allow":["Read"]}}"#)();
    let provider = ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        crate::tool_outcome(facts_read()),
        finish_with("verified"),
    ])
    .with_before_call(2, {
        let facts = write_facts(&workspace, "v2");
        let settings = write_settings(br#"{ "permissions": { "allow": ["Read"] } }"#);
        move || {
            facts();
            settings();
        }
    });
    let provider = Arc::new(provider);
    let outcome = run_with(
        &engine,
        &provider,
        autonomous_input(&workspace),
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .expect("autonomous Goal");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, outcome.session_id).await;
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert_eq!(goal.run_ids.len(), 2);
    let digests: Vec<_> = view
        .runs
        .iter()
        .filter(|run| goal.run_ids.contains(&run.id))
        .map(|run| {
            run.config
                .as_ref()
                .and_then(|config| config.tool_policy.as_ref())
                .and_then(|policy| policy.permissions_digest)
        })
        .collect();
    assert_eq!(digests.len(), 2);
    assert!(digests[0].is_some());
    assert_eq!(
        digests[0], digests[1],
        "the Goal loop pins one settings snapshot"
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
        arany::test_support::scenario::plan_outcome(plan),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        finish_with("second pass"),
        finish_with("never requested"),
    ]);
    let requests = Arc::clone(&provider.requests);
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = autonomous_input(&workspace);
    input.goal = arany::GoalRequest::autonomous(
        arany::GoalBudget {
            max_runs: 2,
            ..arany::GoalBudget::default()
        },
        arany::GoalPhase::Implementation,
    );
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
    .expect("terminal Run");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    assert_eq!(
        requests.lock().expect("Provider requests").len(),
        4,
        "the second Run's Answer is refused, and no third Run dispatches"
    );
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, outcome.session_id).await;
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert!(
        goal.budget_closed,
        "a budget close is journaled as exhausted"
    );
    assert_eq!(
        view.continuable_goal().map(|source| source.id),
        Some(goal.id)
    );
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
        max_provider_calls: Some(4),
        ..arany::GoalBudget::default()
    };
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget = budget;
    let provider = ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(plan),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        finish_with("second pass"),
        finish_with("never requested"),
    ]);
    let requests = Arc::clone(&provider.requests);
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = autonomous_input(&workspace);
    input.goal = arany::GoalRequest::autonomous(budget, arany::GoalPhase::Implementation);
    // The second Run's Answer is the fourth and last admissible call.
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
    .expect("terminal Run");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    assert_eq!(
        requests.lock().expect("Provider requests").len(),
        4,
        "the call cap admits no fifth Provider call"
    );
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, outcome.session_id).await;
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert!(
        goal.budget_closed,
        "a budget close is journaled as exhausted"
    );
    assert_eq!(
        view.continuable_goal().map(|source| source.id),
        Some(goal.id)
    );
    assert_eq!(goal.status, arany::GoalStatus::Failed);
    assert_eq!(goal.provider_calls, 4);
    assert!(goal.report.is_some(), "exhausted Goal keeps its report");
}

struct MeteredProvider {
    tokens: u32,
    responses: std::sync::Mutex<VecDeque<ProviderOutcome>>,
}

#[async_trait::async_trait]
impl Provider for MeteredProvider {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                arany::ProviderIdentity::new(
                    "scripted",
                    "metered-model",
                    arany::Provenance::native(),
                )
            });
        &IDENTITY
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
        max_tokens: Some(15),
        ..arany::GoalBudget::default()
    };
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget = budget;
    // Five input tokens per call: Plan, Read and Answer reach the 15-token cap exactly.
    let provider = MeteredProvider {
        tokens: 5,
        responses: std::sync::Mutex::new(VecDeque::from([
            arany::test_support::scenario::plan_outcome(plan),
            crate::tool_outcome(facts_read()),
            finish_with("first pass"),
            finish_with("never requested"),
        ])),
    };
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = autonomous_input(&workspace);
    input.goal = arany::GoalRequest::autonomous(budget, arany::GoalPhase::Implementation);
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
    .expect("terminal Run");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, outcome.session_id).await;
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert!(
        goal.budget_closed,
        "a budget close is journaled as exhausted"
    );
    assert_eq!(
        view.continuable_goal().map(|source| source.id),
        Some(goal.id)
    );
    assert_eq!(goal.status, arany::GoalStatus::Failed);
    assert_eq!(
        goal.provider_calls, 3,
        "no fourth Provider call is reserved"
    );
    assert_eq!(goal.known_tokens, 15);
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
        crate::tool_outcome(facts_read()),
        finish_with("resumed, still unverified"),
        crate::tool_outcome(facts_read()),
        finish_with("verified"),
    ])
    .with_before_call(2, write_facts(&workspace, "v2"));
    let requests = Arc::clone(&provider.requests);
    let provider = Arc::new(provider);
    let engine = facts_engine(
        StateRoot::open_existing(&state_path).expect("state"),
        &state_path,
    );
    let mut input = request(&workspace, "");
    input.session_id = Some(session_id);
    input.title = None;
    input.include_paths.clear();
    input.goal = arany::GoalRequest::resume(goal_id, true);
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
    .expect("explicit resume");
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
        arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        finish_with("never requested"),
    ]);
    let requests = Arc::clone(&provider.requests);
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, AUTONOMOUS_OBJECTIVE);
    input.include_paths.clear();
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
    .expect("terminal Run");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    assert_eq!(requests.lock().expect("Provider requests").len(), 3);
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, outcome.session_id).await;
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert_eq!(goal.status, arany::GoalStatus::Failed);
    assert_eq!(goal.run_ids, [outcome.run.id]);
}

enum AfterFinish {
    Cancel(RunCancellation),
}

async fn run_observing_finish(
    engine: &Engine,
    provider: &Arc<ScriptedProvider>,
    input: RunRequest,
    after: AfterFinish,
) -> Result<arany::RunOutcome, EngineError> {
    let cancellation = match &after {
        AfterFinish::Cancel(cancellation) => cancellation.clone(),
    };
    let mut observer = engine.subscribe();
    let run = run_with(
        engine,
        provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            cancellation,
            ..Default::default()
        },
    );
    tokio::pin!(run);
    let mut after = Some(after);
    loop {
        tokio::select! {
            result = &mut run => return result,
            event = observer.next() => {
                if matches!(event, Some(arany::Tapped::Committed(event)) if matches!(event.event, Event::RunFinished { disposition: arany::RunDisposition::Finished, .. })) && let Some(action) = after.take() {
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
        arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        finish_with("never requested"),
    ]);
    let requests = Arc::clone(&provider.requests);
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let outcome = run_observing_finish(
        &engine,
        &provider,
        autonomous_input(&workspace),
        AfterFinish::Cancel(RunCancellation::new()),
    )
    .await
    .expect("the committed Finished Run is returned");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    assert_eq!(
        requests.lock().expect("Provider requests").len(),
        3,
        "cancellation after the first Answer starts no second Run"
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
        arany::test_support::scenario::plan_outcome(plan),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        finish_with("never requested"),
    ])
    // Advanced in the Answer call, after the Read Tool's Guard I/O (a paused clock would auto-advance that I/O's timeouts).
    .with_clock_advance_before_call(2, Duration::from_secs(60));
    let requests = Arc::clone(&provider.requests);
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = autonomous_input(&workspace);
    input.goal = arany::GoalRequest::autonomous(budget, arany::GoalPhase::Implementation);
    let result = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            cancellation: RunCancellation::new(),
            ..Default::default()
        },
    )
    .await;
    assert!(
        matches!(&result, Ok(outcome) if outcome.run.status == RunStatus::Failed),
        "the exhausted Answer fails its Run: {:?}",
        result.as_ref().map(|outcome| outcome.run.status)
    );
    assert_eq!(
        requests.lock().expect("Provider requests").len(),
        3,
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
    assert!(
        goal.budget_closed,
        "a budget close is journaled as exhausted"
    );
    assert_eq!(
        view.continuable_goal().map(|source| source.id),
        Some(goal.id)
    );
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
        arany::test_support::scenario::plan_outcome(plan),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        finish_with("never requested"),
    ])
    // Advanced in the Answer call, after the Read Tool's Guard I/O (a paused clock would auto-advance that I/O's timeouts).
    .with_clock_advance_before_call(2, Duration::from_secs(60));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("Collector listener");
    let endpoint = format!(
        "http://{}",
        listener.local_addr().expect("Collector address")
    );
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let collector = collect_trace_bodies(listener, Arc::clone(&done));
    let telemetry = arany::Telemetry::from_process(Some(&endpoint)).expect("loopback telemetry");
    let provider = Arc::new(provider);
    let engine = facts_engine_with_telemetry(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        telemetry.clone(),
    );
    let mut input = autonomous_input(&workspace);
    input.goal = arany::GoalRequest::autonomous(budget, arany::GoalPhase::Implementation);
    let result = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            cancellation: RunCancellation::new(),
            ..Default::default()
        },
    )
    .await;
    assert!(
        matches!(&result, Ok(outcome) if outcome.run.status == RunStatus::Failed),
        "the exhausted Answer fails its Run: {:?}",
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
async fn run_hints_carry_only_display_text_for_the_committed_run() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let provider = Arc::new(crate::PreviewProvider {
        text: arany::PreviewText {
            answer: "x".repeat(2300),
            commentary: Vec::new(),
            commentary_limited: false,
            tentative: false,
        },
        completion: None,
    });
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let mut hints = RunHints::with_previews();
    let mut previews = hints.previews();
    let cancellation = RunCancellation::new();
    let mut input = request(&workspace, "question");
    input.include_paths.clear();
    let (mut events, update, outcome) = {
        let arany::RunHandle { events, result, .. } = engine
            .submit(
                AdmittedProviders::single(provider),
                input,
                RunControls {
                    cancellation: cancellation.clone(),
                    hints: Some(hints.clone()),
                    ..Default::default()
                },
            )
            .expect("submission");
        let mut result = std::pin::pin!(tokio::time::timeout(Duration::from_secs(10), result));
        let update = tokio::select! {
            update = previews.changed() => update.expect("preview update"),
            _ = &mut result => panic!("the Run ended before its preview"),
        };
        assert!(cancellation.cancel());
        let outcome = result
            .await
            .expect("cancellation deadline")
            .expect("cancelled Run")
            .run;
        (events, update, outcome)
    };
    engine.close().await.expect("engine shutdown");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    assert_eq!(update.run_id, outcome.run.id);
    assert!(!update.text.answer.is_empty());
    assert!(outcome.run.assistant_message.is_none());
    tokio::select! {
        biased;
        _ = hints.waiting_changed() => panic!("no Tool waited for capacity"),
        () = std::future::ready(()) => {}
    }
    let mut tapped = Vec::new();
    while let Some(event) = events.next().await {
        let arany::Tapped::Committed(event) = event else {
            panic!("bounded Run cannot lag");
        };
        tapped.push((*event).clone());
    }
    let (journal, _) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(tapped, journal);
}

#[tokio::test]
async fn live_reduction_of_an_autonomous_goal_equals_the_reloaded_view() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        finish_with("earlier turn"),
        arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        crate::tool_outcome(facts_read()),
        finish_with("verified"),
    ])
    .with_before_call(4, write_facts(&workspace, "v2"));
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut earlier = request(&workspace, "Earlier");
    earlier.include_paths.clear();
    let earlier = run_to_end(&engine, &provider, earlier)
        .await
        .expect("earlier Run");
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let mut live = store
        .load_view(earlier.session_id)
        .await
        .expect("closed view")
        .expect("Session");
    let mut input = autonomous_input(&workspace);
    input.session_id = Some(earlier.session_id);
    input.title = None;
    let mut events = {
        let arany::RunHandle { events, result, .. } = engine
            .submit(
                AdmittedProviders::single(provider),
                input,
                RunControls {
                    tools: ToolGrant::Configured,
                    ..Default::default()
                },
            )
            .expect("submission");
        let outcome = result.await.expect("autonomous Goal").run;
        assert_eq!(outcome.run.status, RunStatus::Finished);
        events
    };
    engine.close().await.expect("engine shutdown");
    while let Some(event) = events.next().await {
        let arany::Tapped::Committed(event) = event else {
            panic!("bounded Run cannot lag");
        };
        arany::reduce_into(&mut live, &event).expect("incremental committed reduction");
    }
    let reloaded = store
        .load_view(earlier.session_id)
        .await
        .expect("reloaded view")
        .expect("Session");
    store.close().await.expect("Store shutdown");
    assert_eq!(live, reloaded);
    assert_eq!(reloaded.goals.last().expect("Goal").run_ids.len(), 2);
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
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let cancelled = run_with(
        &engine,
        &provider,
        autonomous_input(&workspace),
        RunControls {
            tools: ToolGrant::Configured,
            cancellation,
            ..Default::default()
        },
    )
    .await
    .expect("cancelled Run");
    assert_eq!(cancelled.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");

    // Resuming links a second Run, so a plan allowing one Run is invalid for this Goal.
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget.max_runs = 1;
    let provider = ScriptedProvider::new(
        (0..3)
            .map(|_| arany::test_support::scenario::plan_outcome(plan.clone()))
            .collect(),
    );
    let provider = Arc::new(provider);
    let engine = facts_engine(
        StateRoot::open_existing(&state_path).expect("state"),
        &state_path,
    );
    let mut input = request(&workspace, "");
    input.session_id = Some(cancelled.session_id);
    input.title = None;
    input.include_paths.clear();
    input.goal = arany::GoalRequest::resume(cancelled.goal_id, true);
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
async fn plan_exhausted_by_its_own_call_is_refused_and_the_goal_continues() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget.max_active_ms = Some(1);
    let provider = ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(plan),
        finish_with("done without a plan"),
    ])
    .with_clock_advance_before_call(0, Duration::from_secs(1));
    let requests = Arc::clone(&provider.requests);
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let outcome = run_with(
        &engine,
        &provider,
        autonomous_input(&workspace),
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .expect("refused plan Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    {
        let calls = requests.lock().expect("Provider requests");
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[1].input.last_answer_refused.as_ref().map(|r| r.cause),
            Some(arany::ProposalRefusalCause::PlanBudgetSpent)
        );
    }
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert!(
        view.goal(outcome.goal_id)
            .expect("Goal")
            .plan_contract
            .is_none()
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.event, Event::GoalPlanContractRecorded { .. }))
    );
    let refusals: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::ProviderCallRecorded { record, .. } => record.refusal.as_ref().map(|r| r.cause),
            _ => None,
        })
        .collect();
    assert_eq!(refusals, [arany::ProposalRefusalCause::PlanBudgetSpent]);
}

#[tokio::test]
async fn plan_active_limit_below_elapsed_call_time_fails_the_run() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget.max_active_ms = Some(1);
    // The settled Read call runs longer than the plan's active-time limit, so no later plan can narrow the Goal.
    let provider = ScriptedProvider::new(
        std::iter::once(crate::tool_outcome(facts_read()))
            .chain((0..3).map(|_| arany::test_support::scenario::plan_outcome(plan.clone())))
            .collect(),
    )
    .with_before_call(0, || {
        std::thread::sleep(std::time::Duration::from_millis(20))
    });
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let outcome = run_with(
        &engine,
        &provider,
        autonomous_input(&workspace),
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .expect("an unnarrowable plan is a Failed Run, not a Store error");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    engine.close().await.expect("engine shutdown");
    assert_eq!(outcome.run.failure, Some(arany::RunFailure::RefusalLimit));

    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert!(
        goal.plan_contract.is_none(),
        "the refused plan is not recorded"
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
        [
            ProviderCallDisposition::ToolRequested,
            ProviderCallDisposition::InvalidResponse,
            ProviderCallDisposition::InvalidResponse,
            ProviderCallDisposition::InvalidResponse,
        ],
        "each unnarrowable plan call is refused and never recorded"
    );
}

#[tokio::test]
async fn active_time_left_below_one_second_stops_the_tool_step_without_a_context_cause() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let budget = arany::GoalBudget {
        max_active_ms: Some(6_000),
        ..arany::GoalBudget::default()
    };
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget = budget;
    let provider = ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(plan),
        crate::tool_outcome(facts_read()),
    ]);
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, AUTONOMOUS_OBJECTIVE);
    input.include_paths.clear();
    input.goal = arany::GoalRequest::autonomous(budget, arany::GoalPhase::Implementation);
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
    .expect("budget-stopped Run");
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, outcome.session_id).await;
    let run = &view.runs[0];
    assert_eq!(run.failure, Some(arany::RunFailure::BudgetExhausted));
    assert_eq!(
        provider.requests.lock().expect("requests").len(),
        2,
        "the plan call and the stopped Read call; no third call"
    );
}

// A cancellation or active-time expiry that lands while the call record commits still stops that call.
#[tokio::test]
async fn interruption_while_the_call_record_commits_stops_that_call() {
    // The cancellation case runs first: the active-time case pauses the clock for the rest of the test.
    for cancel in [true, false] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .expect("Session");
        let budget = arany::GoalBudget {
            max_active_ms: Some(120_000),
            ..arany::GoalBudget::default()
        };
        let mut plan = facts_plan(facts_digest("v2"));
        plan.budget = budget;
        let provider = Arc::new(ScriptedProvider::new(vec![if cancel {
            arany::test_support::scenario::plan_outcome(plan)
        } else {
            finish_with("Answered while time ran out")
        }]));
        let mut input = request(&workspace, AUTONOMOUS_OBJECTIVE);
        input.session_id = Some(session);
        input.title = None;
        input.include_paths.clear();
        input.goal = arany::GoalRequest::autonomous(budget, arany::GoalPhase::Implementation);
        let cancellation = RunCancellation::new();
        let (entered, release) =
            arany::FaultPlan::gate_goal_append_acknowledgment(session, |event| {
                matches!(event, Event::ProviderCallRecorded { .. })
            });
        let outcome = {
            let run = run_with(
                &engine,
                &provider,
                input,
                RunControls {
                    tools: ToolGrant::Configured,
                    cancellation: cancellation.clone(),
                    ..Default::default()
                },
            );
            tokio::pin!(run);
            tokio::select! {
                result = &mut run => panic!("cancel={cancel}: Run ended before its call record committed: {:?}", result.map(|outcome| outcome.run.status)),
                entered = entered => entered.expect("call record reached its acknowledgment"),
            }
            if cancel {
                assert!(cancellation.cancel());
            } else {
                tokio::time::pause();
                tokio::time::advance(Duration::from_secs(121)).await;
            }
            release.send(()).expect("release the call record");
            run.await.expect("settled Run")
        };
        engine.close().await.expect("engine shutdown");

        let (events, view) = replayed(&state_path, outcome.session_id).await;
        let run = &view.runs[0];
        assert_eq!(provider.requests.lock().expect("requests").len(), 1);
        assert!(run.assistant_message.is_none(), "cancel={cancel}");
        assert_eq!(
            events
                .iter()
                .filter(|envelope| matches!(envelope.event, Event::GoalProviderCallReserved { .. }))
                .count(),
            1,
            "cancel={cancel}: no further Provider call is reserved"
        );
        assert!(
            events
                .iter()
                .all(|envelope| !matches!(envelope.event, Event::GoalPlanContractRecorded { .. })),
            "cancel={cancel}: the decided plan is not applied"
        );
        if cancel {
            assert_eq!(run.status, RunStatus::Cancelled);
        } else {
            assert_eq!(run.status, RunStatus::Failed);
            assert_eq!(run.failure, Some(arany::RunFailure::BudgetExhausted));
        }
    }
}

#[tokio::test]
async fn tool_context_overflow_fails_with_the_context_cause() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    std::fs::write(workspace.join("AGENTS.md"), "a".repeat(37_768)).expect("instructions");
    std::fs::write(workspace.join("notes/facts.txt"), "f".repeat(40 * 1024)).expect("facts");
    std::fs::write(workspace.join("big1.txt"), "b".repeat(127 * 1024)).expect("include");
    std::fs::write(workspace.join("big2.txt"), "c".repeat(127 * 1024)).expect("include");
    let state_path = temp.path().join("state");
    let budget = arany::GoalBudget {
        max_provider_calls: Some(30),
        ..arany::GoalBudget::default()
    };
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget = budget;
    let mut script = vec![
        arany::test_support::scenario::plan_outcome(plan),
        crate::tool_outcome(facts_read()),
    ];
    script.extend((0..11).map(|_| crate::tool_outcome(facts_read())));
    script.push(finish_with("done"));
    let provider = Arc::new(ScriptedProvider::new(script));
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, &"x".repeat(60 * 1024));
    input.include_paths = vec!["big1.txt".into(), "big2.txt".into()];
    input.goal = arany::GoalRequest::new(budget, arany::GoalPhase::Implementation);
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
    .expect("context-full Run");
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, outcome.session_id).await;
    let run = &view.runs[0];
    assert_eq!(run.failure, Some(arany::RunFailure::ToolContextFull));
}

#[tokio::test]
async fn cancelled_autonomous_segment_exports_its_run_outcome() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let cancellation = RunCancellation::new();
    let cancel = cancellation.clone();
    let provider = ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        finish_with("cancelled pass"),
    ])
    .with_before_call(3, move || assert!(cancel.cancel(), "first cancellation"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("Collector listener");
    let endpoint = format!(
        "http://{}",
        listener.local_addr().expect("Collector address")
    );
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let collector = collect_trace_bodies(listener, Arc::clone(&done));
    let telemetry = arany::Telemetry::from_process(Some(&endpoint)).expect("loopback telemetry");
    let provider = Arc::new(provider);
    let engine = facts_engine_with_telemetry(
        StateRoot::admit(&state_path).expect("state"),
        &state_path,
        telemetry.clone(),
    );
    let outcome = run_with(
        &engine,
        &provider,
        autonomous_input(&workspace),
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
        arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
    ])
    .with_before_call(2, move || {
        std::fs::remove_dir_all(&removed).expect("Workspace removed")
    });
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let result = engine
        .submit(
            AdmittedProviders::single(provider),
            autonomous_input(&workspace),
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .expect("idle Engine")
        .await;
    let (committed, reason) = match result {
        Ok(RunResult {
            run,
            stopped: Some(reason),
        }) => (run, reason),
        Err(error) => panic!("the committed Run must be returned with its stop reason: {error}"),
        Ok(_) => panic!("a failed continuation admission must stop the loop"),
    };
    assert_eq!(committed.run.status, RunStatus::Finished);
    assert_eq!(
        committed.run.assistant_message.as_deref(),
        Some("first pass"),
        "the committed answer is the first Run's"
    );
    assert!(!matches!(reason, EngineError::GoalBudgetExhausted));
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
    let mut paused_provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
        text: "cancelled answer".into(),
    })]);
    paused_provider.cancel_at_completion = Some((AgentPhase::RootPlan, cancellation.clone()));
    let provider = Arc::new(paused_provider);
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
    )
    .expect("engine");
    // A cancelled plain Run pauses its one-Run Goal like an autonomous one: no terminal receipt, resumable.
    let mut paused = request(&workspace, "Pinned objective of the paused Goal");
    paused.include_paths.clear();
    let cancelled = run_with(
        &engine,
        &provider,
        paused,
        RunControls {
            cancellation,
            ..Default::default()
        },
    )
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
        ProviderOutcome::Answer(Answer {
            text: "second answer".into(),
        }),
        ProviderOutcome::Answer(Answer {
            text: "resumed answer".into(),
        }),
    ]);
    let requests = Arc::clone(&provider.requests);
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::open_existing(&state_path).expect("reopened state"),
        EngineConfig::default(),
    )
    .expect("second engine");

    let mut second = request(&workspace, "A new plain prompt");
    second.session_id = Some(session_id);
    second.title = None;
    second.include_paths.clear();
    let second_run = run_to_end(&engine, &provider, second)
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
    let resumed_run = run_to_end(&engine, &provider, resumed)
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
        let mut provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
            text: "cancelled answer".into(),
        })]);
        provider.cancel_at_completion = Some((AgentPhase::RootPlan, cancellation.clone()));
        let provider = Arc::new(provider);
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut input = request(&workspace, objective);
        input.include_paths.clear();
        input.session_id = session_id;
        input.title = None;
        input.goal = autonomous();
        let cancelled = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                cancellation,
                ..Default::default()
            },
        )
        .await
        .expect("cancelled Run");
        assert_eq!(cancelled.run.status, RunStatus::Cancelled);
        session_id = Some(cancelled.session_id);
        paused_goals.push(cancelled.goal_id);
        engine.close().await.expect("engine shutdown");
    }
    let (first, second) = (paused_goals[0], paused_goals[1]);
    let session_id = session_id.expect("Session");

    let provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
        text: "resumed answer".into(),
    })]);
    let requests = Arc::clone(&provider.requests);
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::open_existing(&state_path).expect("reopened state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let mut stale = request(&workspace, "");
    stale.session_id = Some(session_id);
    stale.title = None;
    stale.include_paths.clear();
    stale.objective = String::new();
    stale.goal = arany::GoalRequest::resume(first, true);
    assert!(matches!(
        run_to_end(&engine, &provider, stale).await,
        Err(EngineError::GoalUnavailable)
    ));
    assert!(requests.lock().expect("Provider requests").is_empty());

    let mut resumed = request(&workspace, "");
    resumed.session_id = Some(session_id);
    resumed.title = None;
    resumed.include_paths.clear();
    resumed.objective = String::new();
    resumed.goal = arany::GoalRequest::resume(second, true);
    let outcome = run_to_end(&engine, &provider, resumed)
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
        crate::tool_outcome(facts_read()),
        finish_with("noted"),
    ])
    .with_before_call(0, move || {
        steering_note_at(&offer, "use the smaller fixture")
    });
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Read the facts");
    input.include_paths.clear();
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert_eq!(calls.len(), 2);
    assert!(call_steering_notes(&calls[0]).is_empty());
    assert_eq!(call_steering_notes(&calls[1]), ["use the smaller fixture"]);
    assert_eq!(steering.delivered_notes(), 1);
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert_action_bounds(&events, false, true);
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
        crate::tool_outcome(facts_read()),
        finish_with("noted"),
    ])
    .with_before_call(0, move || {
        steering_note_at(&offer, "use the smaller fixture");
        steering_note_at(&offer, "and keep the old name");
    });
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Read the facts");
    input.include_paths.clear();
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("Run");
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
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "Inspect facts".into(),
                profile: None,
            }],
        }),
        finish_with("child result"),
        finish_with("synthesized"),
    ])
    .with_before_call(0, move || {
        steering_note_at(&offer, "keep this for synthesis")
    });
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
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("Run");
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
async fn two_child_research_delegation_admission_covers_its_spawns() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let assignment = |objective: &str| arany::ChildAssignment {
        editor: None,
        objective: objective.into(),
        profile: None,
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![assignment("Inspect facts"), assignment("Inspect notes")],
        }),
        finish_with("first child"),
        finish_with("second child"),
        finish_with("synthesized"),
    ]));
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 2,
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

    let (events, _) = replayed(&state_path, outcome.session_id).await;
    assert!(
        arany::test_support::admissions::recorded(outcome.session_id)
            .iter()
            .any(|(_, slots, sealed_data)| *slots > 200 && !sealed_data),
        "the research delegation admission exceeds 200 slots"
    );
    assert_action_bounds(&events, false, false);
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
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Answer quickly");
    input.include_paths.clear();
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("Run");
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
        crate::tool_outcome(facts_read()),
        finish_with("first answer"),
        finish_with("second answer"),
    ])
    .with_before_call(0, move || {
        steering_note_at(&offer, "use the smaller fixture")
    });
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut first = request(&workspace, "Read the facts");
    first.include_paths.clear();
    let first_outcome = run_with(
        &engine,
        &provider,
        first,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("first Run");

    let mut second = request(&workspace, "Second question");
    second.session_id = Some(first_outcome.session_id);
    second.title = None;
    second.include_paths.clear();
    let second_outcome = run_with(
        &engine,
        &provider,
        second,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("second Run");
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
        crate::tool_outcome(facts_read()),
        finish_with("never reached"),
    ])
    .with_before_call(0, move || assert!(pause.pause(), "first pause"));
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Read the facts");
    input.include_paths.clear();
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("paused Run");
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
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Answer quickly");
    input.include_paths.clear();
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("Run");
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
    // The first Run's Answer is unsatisfied against the planned digest, so the Goal would continue.
    let provider = ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        finish_with("never started"),
    ])
    .with_before_call(2, move || assert!(pause.pause(), "first pause"));
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let outcome = run_with(
        &engine,
        &provider,
        autonomous_input(&workspace),
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("first pass returns without a next Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    assert_eq!(requests.lock().expect("requests").len(), 3);
    let (_, view) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(view.runs.len(), 1);
    // Like Ctrl+C between Runs, the pause leaves the Goal open for explicit resume.
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
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "Inspect facts".into(),
                profile: None,
            }],
        }),
        finish_with("never reached"),
    ])
    .with_before_call(0, move || assert!(pause.pause(), "first pause"));
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
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("paused Run");
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
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "Inspect facts".into(),
                profile: None,
            }],
        }),
        finish_with("child result"),
        finish_with("never synthesized"),
    ])
    .with_before_call(1, move || assert!(pause.pause(), "first pause"));
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
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("paused Run");
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
    // The first Run's Answer is unsatisfied against the planned digest, so the Goal starts a second Run.
    // The note is offered while the first Run's Answer is in flight: it has no primary call left in
    // that Run, so it waits for the second Run's first primary call.
    let mut plan = facts_plan(facts_digest("v2"));
    plan.budget.max_runs = 2;
    let provider = ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(plan),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
        finish_with("second pass"),
    ])
    .with_before_call(2, move || {
        steering_note_at(&offer, "carry this into the next Run")
    });
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let budget = arany::GoalBudget {
        max_runs: 2,
        ..arany::GoalBudget::default()
    };
    let mut input = request(&workspace, AUTONOMOUS_OBJECTIVE);
    input.include_paths.clear();
    input.goal = arany::GoalRequest::autonomous(budget, arany::GoalPhase::Implementation);
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("terminal Run");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    engine.close().await.expect("engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    assert_eq!(calls.len(), 4);
    assert!(call_steering_notes(&calls[2]).is_empty());
    assert_eq!(
        call_steering_notes(&calls[3]),
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
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "Inspect facts".into(),
                profile: None,
            }],
        }),
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
        },
        arany::ToolCall::GitStatus { path: ".".into() },
    ];
    for call in outcomes {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: vec![arany::ChildAssignment {
                    editor: None,
                    objective: "Inspect facts".into(),
                    profile: None,
                }],
            }),
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
async fn child_reads_stay_inside_the_plan_and_never_satisfy_the_goal_criterion() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    std::fs::write(workspace.join("notes/other.txt"), "OTHER-CANARY").expect("other file");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v1"))),
        crate::tool_outcome(arany::ToolCall::Search {
            path: "notes/facts.txt".into(),
            query: "v".into(),
        }),
        ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "Inspect notes".into(),
                profile: None,
            }],
        }),
        crate::tool_outcome(facts_read()),
        crate::tool_outcome(arany::ToolCall::Read {
            path: "notes/other.txt".into(),
            offset: None,
            limit: None,
        }),
        finish_with("report"),
        finish_with("done"),
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
    assert!(denied.attestation.is_none());
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
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "Inspect facts".into(),
                profile: None,
            }],
        })];
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
async fn pause_during_research_finishes_the_child_call_and_dispatches_no_child_tool() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let steering = RunSteering::new();
    let pause = steering.clone();
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "Inspect facts".into(),
                profile: None,
            }],
        }),
        crate::tool_outcome(facts_read()),
        finish_with("never reached"),
    ])
    .with_before_call(1, move || assert!(pause.pause(), "first pause"));
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
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("paused Run");
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
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "Inspect facts".into(),
                profile: None,
            }],
        }),
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

#[tokio::test]
async fn child_read_request_at_an_exhausted_goal_budget_stops_as_budget_not_failure() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "Inspect facts".into(),
                profile: None,
            }],
        }),
        crate::tool_outcome(facts_read()),
        finish_with("never reached"),
    ])
    .with_known_input_tokens();
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
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
    .expect("budget-exhausted Run");
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
    assert!(goal.budget_closed);
    assert_eq!(view.continuable_goal().map(|goal| goal.id), Some(goal.id));
    assert_action_bounds(&events, false, false);
}

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
async fn near_deadline_child_tool_journals_limit_without_dispatch() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "Inspect facts".into(),
                profile: None,
            }],
        }),
        crate::tool_outcome(facts_read()),
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
    input.goal.budget = Some(arany::GoalBudget {
        max_active_ms: Some(15_000),
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
    assert_eq!(outcome.run.status, RunStatus::Failed);
    engine.close().await.expect("engine shutdown");
    assert_eq!(requests.lock().expect("requests").len(), 2);

    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let child = view.runs[0].agents[1].id;
    assert_eq!(child_tool_starts(&events, child), 1);
    let tool = &view.runs[0].agents[1].tools[0];
    let observation = tool.observation.as_ref().expect("deadline observation");
    assert_eq!(observation.disposition, arany::ToolDisposition::Limit);
    assert_eq!(
        observation.output,
        "Delegation deadline reached; no operation was dispatched"
    );
    assert!(observation.attestation.is_none());
    assert!(tool.hooks.is_empty());
    assert_action_bounds(&events, false, false);
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
            agent: None,
            children: vec![
                arany::ChildAssignment {
                    editor: None,
                    objective: "Inspect facts".into(),
                    profile: None,
                },
                arany::ChildAssignment {
                    editor: None,
                    objective: "Inspect facts again".into(),
                    profile: None,
                },
            ],
        }),
        crate::tool_outcome(facts_read()),
        finish_with("never reached"),
    ])
    .with_before_call(1, move || assert!(pause.pause(), "first pause"));
    // One call at a time: the second child stays queued when the pause lands.
    provider.identity.limits.concurrency = 1;
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Check the facts");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 2,
    };
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("paused Run");
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
        crate::tool_outcome(facts_read()),
        crate::tool_outcome(facts_read()),
        finish_with("done"),
    ]);
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Read twice");
    input.include_paths.clear();
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
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "Inspect facts".into(),
                profile: None,
            }],
        }),
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
async fn interrupted_prefix_replays_and_a_new_run_starts_without_steps() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read()),
        finish_with("first"),
        finish_with("retry"),
    ]);
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Read once");
    input.include_paths.clear();
    let first = run_with(
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
    run_with(
        &engine,
        &provider,
        retry,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .expect("retry Run");
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
        crate::tool_outcome(facts_read()),
        crate::tool_outcome(facts_read()),
        finish_with("noted"),
    ])
    .with_before_call(1, move || steering_note_at(&offer, "mid-run note"));
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Read twice");
    input.include_paths.clear();
    run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await
    .expect("Run");
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
async fn a_recorded_plan_cannot_be_replaced_or_restated() {
    for (restated_plan, expected) in [
        (facts_plan(facts_digest("v1")), RunStatus::Finished),
        (
            arany::GoalPlanContract {
                mission: "A different mission".into(),
                ..facts_plan(facts_digest("v1"))
            },
            RunStatus::Finished,
        ),
    ] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let provider = ScriptedProvider::new(vec![
            arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v1"))),
            crate::tool_outcome(facts_read()),
            arany::test_support::scenario::plan_outcome(restated_plan),
            crate::tool_outcome(facts_read()),
            finish_with("done"),
        ]);
        let provider = Arc::new(provider);
        let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
        let mut input = request(&workspace, "Read twice");
        input.include_paths.clear();
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
    const READS: usize = 104;

    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
    let content = format!("{}\n", "x".repeat(1023)).repeat(4 * READS);
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
            crate::tool_outcome(arany::ToolCall::Read {
                path: "notes/data.txt".into(),
                offset: Some(u32::try_from(page * 4 + 1).expect("bounded line offset")),
                limit: Some(4),
            })
        })
        .collect();
    outcomes.insert(0, arany::test_support::scenario::plan_outcome(plan.clone()));
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
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Inspect the bounded file in pages");
    input.include_paths.clear();
    input.goal.budget = Some(budget);
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
    .expect("Goal");
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

#[tokio::test]
async fn crash_during_a_call_charges_its_ceiling_and_the_resumed_run_continues() {
    use std::os::unix::fs::MetadataExt;

    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let metadata = std::fs::metadata(&workspace).expect("Workspace identity");
    let identity = WorkspaceIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    let state_path = temp.path().join("state");
    let session_id = SessionId::new();
    let (goal_id, run_id, agent_run_id) =
        (arany::GoalId::new(), RunId::new(), arany::AgentRunId::new());
    let store = Store::open(StateRoot::admit(&state_path).expect("state")).expect("Store");
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
            objective: "Spend reserved time".into(),
            workspace_identity: identity,
            budget: arany::GoalBudget {
                max_runs: 2,
                ..arany::GoalBudget::default()
            },
            continues: None,
        },
        Event::GoalRunLinked {
            goal_id,
            run_id,
            phase: arany::GoalPhase::Implementation,
        },
        Event::MessageAccepted {
            skill: None,
            run_id,
            text: "Spend reserved time".into(),
            images: Vec::new(),
        },
        Event::RunStarted {
            run_id,
            config: arany::RunConfig {
                supervisor: None,
                worker_policy: None,
                agent_definitions: None,
                roster: None,
                providers: None,
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
                workspace_device: identity.device,
                workspace_inode: identity.inode,
                instruction_digest: None,
                include_digests: Vec::new(),
                history_run_ids: Vec::new(),
                excluded_history_runs: 0,
                context_usage: None,
                compaction_event_sequence: None,
                compaction_content_digest: None,
                tool_policy: None,
            },
            active_deadline_ms: Some(arany::MAX_GOAL_ACTIVE_MS),
        },
        Event::AgentSpawned {
            profile: None,
            assignment: None,
            definition: None,
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
            active_ceiling_ms: 7_000,
        },
    ];
    for event in crashed {
        store
            .append(session_id, event)
            .await
            .expect("crash-left journal prefix");
    }
    store.close().await.expect("seed Store shutdown");

    let provider = ScriptedProvider::new(vec![finish_with("recovered")]);
    let requests = Arc::clone(&provider.requests);
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::open_existing(&state_path).expect("reopened state"),
        EngineConfig::default(),
    )
    .expect("Engine");
    let mut resumed = request(&workspace, "Resume the crashed Goal");
    resumed.session_id = Some(session_id);
    resumed.title = None;
    resumed.include_paths.clear();
    resumed.goal = arany::GoalRequest::resume(goal_id, true);
    let outcome = run_to_end(&engine, &provider, resumed)
        .await
        .expect("explicit recovery");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("Engine shutdown");

    let calls = requests.lock().expect("requests").clone();
    let facts = call_facts(&calls[0]).expect("goal context facts");
    assert_eq!(
        facts.uncertain_active_ms, 7_000,
        "only the open call's ceiling is charged as uncertain time on the resumed Run's first call"
    );
    assert!(facts.active_ms >= 7_000);
}

#[tokio::test]
async fn previews_chunk_a_finishing_call_within_its_bound() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let answer = "x".repeat(5000);
    let provider = crate::PreviewProvider {
        text: arany::PreviewText {
            answer: answer.clone(),
            commentary: vec!["commentary".into()],
            commentary_limited: false,
            tentative: false,
        },
        completion: Some(ProviderOutcome::Answer(Answer { text: answer })),
    };
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let mut input = request(&workspace, "question");
    input.include_paths.clear();
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            cancellation: RunCancellation::new(),
            hints: Some(RunHints::with_previews()),
            ..Default::default()
        },
    )
    .await
    .expect("Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");
    let (events, _) = replayed(&state_path, outcome.session_id).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event.event, Event::AssistantDisplayRecorded { .. }))
    );
    assert_action_bounds(&events, true, false);
}

#[tokio::test]
async fn between_run_goal_compaction_stays_within_its_bound() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![finish_with("first")]).with_compactions(vec![Ok(
        CompactionResponse {
            summary: "The completed Run remains in the canonical history.".into(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: Some(12),
                output_tokens: Some(4),
                ..Default::default()
            },
            wire_provenance: None,
        },
    )]);
    let provider = Arc::new(provider);
    let engine = engine_support::open(
        StateRoot::admit(&state_path).expect("state"),
        EngineConfig::default(),
    )
    .expect("engine");
    let mut first = request(&workspace, "Use one Provider call");
    first.include_paths.clear();
    first.goal.finish_after_run = false;
    let initial = run_to_end(&engine, &provider, first)
        .await
        .expect("Goal Run");
    engine
        .compact_session(
            &AdmittedProviders::single(provider.clone()),
            initial.session_id,
            workspace.clone(),
        )
        .await
        .expect("in-Goal compaction between Runs");
    engine.close().await.expect("engine shutdown");
    let (events, _) = replayed(&state_path, initial.session_id).await;
    assert_action_bounds(&events, false, false);
}

struct SteeredRun {
    result: Result<arany::RunOutcome, EngineError>,
    events: Vec<arany::EventEnvelope>,
}

async fn steered_run(
    root: &Path,
    tag: &str,
    free_at_second_call: Option<usize>,
    run_offset: usize,
) -> SteeredRun {
    let workspace = facts_workspace(&root.join(tag), "v1");
    let state_path = root.join(tag).join("state");
    let engine_provider = ScriptedProvider::new(vec![finish_with("initial")]);
    let provider = Arc::new(engine_provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut first = request(&workspace, "Initial");
    first.include_paths.clear();
    let session_id = run_with(
        &engine,
        &provider,
        first,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .expect("initial Run")
    .session_id;
    engine.close().await.expect("initial shutdown");
    if let Some(free) = free_at_second_call {
        let prefix = 10_000 - run_offset - free;
        let mut connection =
            rusqlite::Connection::open(state_path.join("events.sqlite3")).expect("events");
        let transaction = connection.transaction().expect("transaction");
        let first_padding: i64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(sequence), 0) + 1 FROM events",
                [],
                |row| row.get(0),
            )
            .expect("next sequence");
        for sequence in first_padding..=i64::try_from(prefix).expect("prefix") {
            transaction
                .execute(
                    "INSERT INTO events (sequence,session_id,kind,event_version,payload,created_at_ms) VALUES (?1,?2,'SessionRenamed',6,'{\"title\":\"x\"}',0)",
                    rusqlite::params![sequence, session_id.to_string()],
                )
                .expect("padding");
        }
        transaction.commit().expect("padding commit");
    }
    let steering = RunSteering::new();
    let offer = steering.clone();
    let provider = ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read()),
        finish_with("noted"),
    ])
    .with_before_call(0, move || {
        steering_note_at(&offer, "use the smaller fixture")
    });
    let provider = Arc::new(provider);
    let engine = facts_engine(
        StateRoot::open_existing(&state_path).expect("state"),
        &state_path,
    );
    let mut second = request(&workspace, "Steered");
    second.session_id = Some(session_id);
    second.title = None;
    second.include_paths.clear();
    let result = run_with(
        &engine,
        &provider,
        second,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    )
    .await;
    engine.close().await.expect("shutdown");
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let events = store.load_session(session_id).await.expect("Events");
    store.close().await.expect("Store shutdown");
    SteeredRun { result, events }
}

#[tokio::test]
async fn steered_run_near_the_event_cap_is_refused_up_front_never_stranded() {
    let temp = tempfile::tempdir().expect("temporary root");
    let dry = steered_run(temp.path(), "dry", None, 0).await;
    assert_eq!(dry.result.expect("dry Run").run.status, RunStatus::Finished);
    assert_action_bounds(&dry.events, false, true);
    let second_call = dry
        .events
        .iter()
        .enumerate()
        .filter(|(_, envelope)| matches!(envelope.event, Event::GoalProviderCallReserved { .. }))
        .nth(2)
        .map(|(index, _)| index)
        .expect("the steered second call of the steered Run");
    let run_start = dry
        .events
        .iter()
        .enumerate()
        .filter(|(_, envelope)| matches!(envelope.event, Event::GoalStarted { .. }))
        .nth(1)
        .map(|(index, _)| index)
        .expect("the steered Run starts its Goal");
    let run_offset = second_call - run_start;
    let steered_bound = arany::Action::ProviderCall {
        previews: false,
        steering: true,
    };
    // The call's bound includes its three same-route retry facts.
    let steered_bound = steered_bound.max_events()
        + usize::from(arany::MAX_PROVIDER_RETRIES)
        + steered_bound.verdict_slots();
    for free in steered_bound - 3..=steered_bound + 2 {
        let case = steered_run(temp.path(), &format!("free-{free}"), Some(free), run_offset).await;
        let count = |matches: fn(&Event) -> bool| {
            case.events
                .iter()
                .filter(|envelope| matches(&envelope.event))
                .count()
        };
        let run_receipts = count(|event| matches!(event, Event::RunFinished { .. }));
        let goal_receipts = count(|event| matches!(event, Event::GoalFinished { .. }));
        assert_eq!(
            run_receipts, goal_receipts,
            "free {free}: a Run is never left without its Goal receipt"
        );
        if free >= steered_bound {
            assert_eq!(
                case.result.expect("admitted Run").run.status,
                RunStatus::Finished,
                "free {free}"
            );
            assert_eq!(run_receipts, 2, "free {free}");
        } else {
            assert!(
                matches!(
                    case.result,
                    Err(EngineError::Store(arany::StoreError::ReplayLimit))
                ),
                "free {free}: {:?}",
                case.result.as_ref().map(|outcome| outcome.run.status)
            );
            assert_eq!(
                run_receipts, 1,
                "free {free}: the refused Run has no receipt"
            );
        }
    }
}

async fn reviewed_write_run(
    root: &Path,
    tag: &str,
    free_at_tool_admission: Option<usize>,
    run_offset: usize,
) -> (Vec<arany::EventEnvelope>, arany::SessionView) {
    use std::os::unix::fs::PermissionsExt;
    let workspace = facts_workspace(&root.join(tag), "v1");
    let state_path = root.join(tag).join("state");
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    for (name, bytes) in [
        (
            "tools.json",
            &br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#[..],
        ),
        (
            "permissions.json",
            &br#"{"permissions":{"defaultMode":"auto"}}"#[..],
        ),
    ] {
        let path = state_path.join(name);
        std::fs::write(&path, bytes).expect("settings");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
    }
    let provider = Arc::new(ScriptedProvider::new(vec![finish_with("initial")]));
    let mut first = request(&workspace, "Initial");
    first.include_paths.clear();
    let session_id = run_with(
        &engine,
        &provider,
        first,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .expect("initial Run")
    .session_id;
    engine.close().await.expect("initial shutdown");
    if let Some(free) = free_at_tool_admission {
        let prefix = 10_000 - run_offset - free;
        let mut connection =
            rusqlite::Connection::open(state_path.join("events.sqlite3")).expect("events");
        let transaction = connection.transaction().expect("transaction");
        let first_padding: i64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(sequence), 0) + 1 FROM events",
                [],
                |row| row.get(0),
            )
            .expect("next sequence");
        for sequence in first_padding..=i64::try_from(prefix).expect("prefix") {
            transaction
                .execute(
                    "INSERT INTO events (sequence,session_id,kind,event_version,payload,created_at_ms) VALUES (?1,?2,'SessionRenamed',6,'{\"title\":\"x\"}',0)",
                    rusqlite::params![sequence, session_id.to_string()],
                )
                .expect("padding");
        }
        transaction.commit().expect("padding commit");
    }
    let provider = Arc::new(ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
        crate::tool_outcome(facts_read()),
        crate::tool_outcome(arany::ToolCall::Write {
            path: "notes/facts.txt".into(),
            expected_digest: None,
            content: "v2".into(),
        }),
        ProviderOutcome::Review(arany::ReviewVerdict::Ask),
        finish_with("done"),
    ]));
    let engine = engine_support::open(
        StateRoot::open_existing(&state_path).expect("state"),
        EngineConfig {
            guard_executable: Some(std::env::current_exe().expect("test executable")),
            ..Default::default()
        },
    )
    .expect("engine");
    let mut second = request(&workspace, "Write after review");
    second.session_id = Some(session_id);
    second.title = None;
    second.include_paths.clear();
    let _ = run_with(
        &engine,
        &provider,
        second,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await;
    engine.close().await.expect("shutdown");
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/facts.txt")).expect("facts"),
        "v1"
    );
    replayed(&state_path, session_id).await
}

#[tokio::test]
async fn reviewed_tool_step_at_the_event_cap_closes_its_tool_and_leaves_no_tool_without_an_observation()
 {
    let temp = tempfile::tempdir().expect("temporary root");
    let (dry, _) = reviewed_write_run(temp.path(), "dry", None, 0).await;
    let index_of = |nth: usize, matches: fn(&Event) -> bool| {
        dry.iter()
            .enumerate()
            .filter(|(_, envelope)| matches(&envelope.event))
            .nth(nth)
            .map(|(index, _)| index)
            .expect("journal landmark")
    };
    let run_start = index_of(1, |event| matches!(event, Event::GoalStarted { .. }));
    let tool_start = index_of(1, |event| matches!(event, Event::ToolStarted { .. }));
    // The Tool step's bound includes its review call's three same-route retry facts.
    let free = arany::Action::Tool.max_events()
        + usize::from(arany::MAX_PROVIDER_RETRIES)
        + arany::Action::Tool.verdict_slots()
        - 1;
    let (events, view) = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        reviewed_write_run(temp.path(), "capped", Some(free), tool_start - run_start),
    )
    .await
    .expect("the capped Run settles");
    let started = events
        .iter()
        .filter(|envelope| matches!(envelope.event, Event::ToolStarted { .. }))
        .count();
    let finished = events
        .iter()
        .filter(|envelope| matches!(envelope.event, Event::ToolFinished { .. }))
        .count();
    assert_eq!(started, 2, "the padded step reaches its Tool");
    assert_eq!(
        started, finished,
        "the Tool is never left without an observation"
    );
    assert!(
        view.runs
            .iter()
            .flat_map(|run| &run.tools)
            .all(|tool| tool.observation.is_some()),
        "no Tool blocks Goal resume"
    );
}

#[tokio::test]
async fn approving_review_cut_short_by_a_pause_is_not_recorded_as_an_ai_approval() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    for (name, bytes) in [
        (
            "tools.json",
            &br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#[..],
        ),
        (
            "permissions.json",
            &br#"{"permissions":{"defaultMode":"auto"}}"#[..],
        ),
    ] {
        let path = state_path.join(name);
        std::fs::write(&path, bytes).expect("settings");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
    }
    let steering = RunSteering::new();
    let pause = steering.clone();
    let provider = Arc::new(
        ScriptedProvider::new(vec![
            arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
            crate::tool_outcome(facts_read()),
            crate::tool_outcome(arany::ToolCall::Write {
                path: "notes/facts.txt".into(),
                expected_digest: None,
                content: "v2".into(),
            }),
            ProviderOutcome::Review(arany::ReviewVerdict::Approve),
            finish_with("never reached"),
        ])
        .with_before_call(3, move || assert!(pause.pause(), "first pause")),
    );
    let outcome = run_with(
        &engine,
        &provider,
        request(&workspace, "Write after review"),
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering),
            ..Default::default()
        },
    )
    .await
    .expect("paused Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/facts.txt")).expect("facts"),
        "v1"
    );
    let (events, _) = replayed(&state_path, outcome.session_id).await;
    let verdicts: Vec<_> = events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            Event::ToolPermissionDecided { verdict, .. } => Some(verdict),
            _ => None,
        })
        .collect();
    let write = verdicts.last().expect("Write verdict");
    assert!(!write.approved);
    assert_eq!(format!("{:?}", write.source), "UnavailableApproval");
}

#[tokio::test]
async fn in_run_compaction_cancelled_in_its_call_starts_no_retry_or_switch() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = paged_read_workspace(temp.path());
    let state_path = temp.path().join("state");
    let cancellation = RunCancellation::new();
    let mut failing = ScriptedProvider::new(vec![]).with_profile("scripted", "compact-primary");
    failing.compact_responses = Mutex::new(vec![Err(ProviderError::RemoteHttp(503))].into());
    failing.cancel_at_compaction = Some(cancellation.clone());
    let failing = Arc::new(failing);
    let fallback =
        Arc::new(ScriptedProvider::new(vec![]).with_profile("scripted", "compact-fallback"));
    let primary =
        Arc::new(ScriptedProvider::new(paged_read_outcomes()).with_profile("scripted", "primary"));
    let mut bindings = AdmittedProviders::single(primary.clone());
    bindings.roles.compaction = vec![failing.clone(), fallback.clone()];
    let engine = facts_engine(
        StateRoot::admit(&state_path).expect("private state"),
        &state_path,
    );
    let mut input = request(&workspace, "Inspect the bounded file in pages");
    input.include_paths.clear();
    input.goal.budget = Some(arany::GoalBudget {
        max_provider_calls: None,
        max_tokens: None,
        max_active_ms: Some(30 * 60 * 1000),
        max_runs: 1,
    });
    let outcome = super::engine_support::run_routed(
        &engine,
        bindings,
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

    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let run = &view.runs[0];
    assert!(
        run.provider_fallbacks.is_empty() && run.provider_retries.is_empty(),
        "a cancelled compaction call starts no further attempt"
    );
    let record = events
        .iter()
        .find_map(|event| match &event.event {
            Event::GoalContextCompacted { record, .. } => Some(record),
            _ => None,
        })
        .expect("compaction record");
    assert_eq!(record.model, "compact-primary");
    assert!(matches!(
        record.status,
        arany::CompactionStatus::Failed {
            reason: arany::CompactionFailure::Cancelled
        }
    ));
    assert!(
        fallback
            .compact_requests
            .lock()
            .expect("requests")
            .is_empty()
    );
    assert_eq!(failing.compact_requests.lock().expect("requests").len(), 1);
}

#[tokio::test]
async fn a_pause_in_an_in_run_compaction_backoff_starts_no_further_compaction_call() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = paged_read_workspace(temp.path());
    let state_path = temp.path().join("state");
    let mut failing = ScriptedProvider::new(vec![]).with_profile("scripted", "compact-primary");
    failing.compact_responses = Mutex::new(vec![Err(ProviderError::RemoteHttp(503))].into());
    let failing = Arc::new(failing);
    let primary =
        Arc::new(ScriptedProvider::new(paged_read_outcomes()).with_profile("scripted", "primary"));
    let mut bindings = AdmittedProviders::single(primary.clone());
    bindings.roles.compaction = vec![failing.clone()];
    let engine = facts_engine(
        StateRoot::admit(&state_path).expect("private state"),
        &state_path,
    );
    let session = engine
        .create_session(workspace.clone(), Some("Pause".into()))
        .await
        .expect("session");
    let (entered, release) = arany::FaultPlan::gate_retry_backoff(session);
    let mut input = request(&workspace, "Inspect the bounded file in pages");
    input.include_paths.clear();
    input.session_id = Some(session);
    input.title = None;
    input.goal.budget = Some(arany::GoalBudget {
        max_provider_calls: None,
        max_tokens: None,
        max_active_ms: Some(30 * 60 * 1000),
        max_runs: 1,
    });
    let steering = RunSteering::new();
    let run = super::engine_support::run_routed(
        &engine,
        bindings,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            steering: Some(steering.clone()),
            ..Default::default()
        },
    );
    let outcome = {
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => panic!("Run ended before the held compaction backoff: {}", result.is_ok()),
            entered = entered => entered.expect("backoff entered"),
        }
        assert!(steering.pause());
        run.await.expect("paused Run")
    };
    // The gate stays held: only the pause can end the wait.
    drop(release);
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("engine shutdown");
    let (events, view) = replayed(&state_path, session).await;
    let run = &view.runs[0];
    assert!(run.provider_fallbacks.is_empty());
    assert_eq!(
        run.provider_retries
            .iter()
            .map(|(_, retry)| retry.role)
            .collect::<Vec<_>>(),
        [arany::ProviderRole::Compaction]
    );
    assert_eq!(failing.compact_requests.lock().expect("requests").len(), 1);
    let record = events
        .iter()
        .find_map(|event| match &event.event {
            Event::GoalContextCompacted { record, .. } => Some(record),
            _ => None,
        })
        .expect("compaction record");
    assert!(matches!(
        record.status,
        arany::CompactionStatus::Failed {
            reason: arany::CompactionFailure::Cancelled
        }
    ));
}

#[tokio::test]
async fn foreground_worker_goal_refuses_editor_authority_without_a_write_capable_parent() {
    use arany::worker::{EditorAssignment, WorkerMode};
    for case in ["foreground", "background", "mixed", "wave"] {
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
        let mut input = request(&workspace, "Edit notes");
        input.include_paths.clear();
        input.policy = CollaborationPolicy::Auto {
            max_active_children: 3,
        };
        let outcome = run_to_end(&engine, &provider, input).await.unwrap();
        assert_eq!(outcome.run.status, RunStatus::Failed, "{case}");
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            3,
            "{case}: no child egress"
        );
        assert!(provider.responses.lock().unwrap().is_empty());
        engine.close().await.unwrap();
        let (events, view) = replayed(&state_path, outcome.session_id).await;
        assert_eq!(view.goals[0].status, arany::GoalStatus::Failed);
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
async fn blocked_stop_reserves_its_shadow_verdict_before_journaling() {
    // The continuation tail includes its call's three same-route retry facts.
    const REFUSED: usize = 10 + arany::MAX_PROVIDER_RETRIES as usize;
    async fn run(root: &Path, free: Option<(usize, usize)>) -> Vec<arany::EventEnvelope> {
        use std::os::unix::fs::PermissionsExt;
        let workspace = facts_workspace(root, "stable");
        let state_path = root.join("state");
        let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap();
        let permissions = state_path.join("permissions.json");
        std::fs::write(
            &permissions,
            br#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"unconfigured","args":[]}]}]}}"#,
        )
        .unwrap();
        std::fs::set_permissions(&permissions, std::fs::Permissions::from_mode(0o600)).unwrap();
        engine.close().await.unwrap();
        if let Some((offset, free)) = free {
            let mut connection =
                rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
            let transaction = connection.transaction().unwrap();
            for sequence in 2..=i64::try_from(10_000 - offset - free).unwrap() {
                transaction.execute(
                    "INSERT INTO events (sequence,session_id,kind,event_version,payload,created_at_ms) VALUES (?1,?2,'SessionRenamed',6,'{\"title\":\"x\"}',0)",
                    rusqlite::params![sequence, session.to_string()],
                ).unwrap();
            }
            transaction.commit().unwrap();
        }
        let engine = facts_engine(StateRoot::open_existing(&state_path).unwrap(), &state_path);
        let provider = Arc::new(ScriptedProvider::new(vec![finish_with("candidate"); 9]));
        let mut input = request(&workspace, "Answer after the Stop continuations");
        input.include_paths.clear();
        input.session_id = Some(session);
        input.title = None;
        let result = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await;
        if free.is_some_and(|(_, free)| free == REFUSED) {
            assert!(matches!(
                result,
                Err(EngineError::Store(arany::StoreError::ReplayLimit))
            ));
        } else {
            assert_eq!(result.unwrap().run.status, RunStatus::Finished);
        }
        engine.close().await.unwrap();
        replayed(&state_path, session).await.0
    }
    let temp = tempfile::tempdir().unwrap();
    let dry = run(&temp.path().join("dry"), None).await;
    let last_hook = dry
        .iter()
        .rposition(|e| matches!(e.event, Event::HookStarted { .. }))
        .unwrap();
    let initial = dry
        .iter()
        .position(|e| matches!(e.event, Event::GoalStarted { .. }))
        .unwrap();
    let offset = last_hook - initial;
    for free in [REFUSED, REFUSED + 1] {
        let events = run(
            &temp.path().join(format!("free-{free}")),
            Some((offset, free)),
        )
        .await;
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e.event, Event::HookStarted { .. }))
                .count(),
            if free == REFUSED { 7 } else { 8 },
            "free {free}: the last blocked hook needs its pair, verdict and complete continuation tail"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e.event, Event::SupervisorVerdictRecorded { .. }))
        );
    }
}

// A paused clock keeps rendered active-time digits equal across both Runs; they set each Read's result cap.
#[tokio::test(start_paused = true)]
async fn supervisor_shadow_journals_loops_without_changing_provider_input_or_outcome() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = facts_workspace(temp.path(), "stable");
    let mut observations = Vec::new();
    let normalized_run = arany::RunId::new();
    let normalized_actor = arany::AgentRunId::new();
    for enabled in [true, false] {
        let state_path = temp.path().join(if enabled { "shadow" } else { "off" });
        let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
        let session = engine
            .create_session(workspace.clone(), Some("Supervisor".into()))
            .await
            .unwrap();
        if !enabled {
            arany::FaultPlan::supervisor_off(session);
        }
        let mut outcomes: Vec<_> = (0..4).map(|_| crate::tool_outcome(facts_read())).collect();
        outcomes.push(finish_with("done"));
        let provider = Arc::new(ScriptedProvider::new(outcomes));
        let mut input = request(&workspace, "Inspect repeatedly");
        input.include_paths.clear();
        input.session_id = Some(session);
        input.title = None;
        let result = run_with(
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
        let calls = provider.requests.lock().unwrap().clone();
        assert_eq!(calls.len(), 5);
        let (events, view) = replayed(&state_path, session).await;
        assert_action_bounds(&events, false, false);
        let verdicts: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.event {
                Event::SupervisorVerdictRecorded { verdict, .. } => Some((e.sequence, verdict)),
                _ => None,
            })
            .collect();
        assert_eq!(verdicts.len(), if enabled { 2 } else { 0 });
        for (sequence, verdict) in verdicts {
            assert!(
                verdict
                    .signals
                    .iter()
                    .any(|signal| signal.signal == arany::supervisor::SignalKind::Loop)
            );
            assert_eq!(sequence, verdict.subject_sequence + 1);
        }
        assert_eq!(view.runs[0].status, RunStatus::Finished);
        let inputs: Vec<_> = calls
            .iter()
            .map(|c| {
                let mut input = c.input.clone();
                // Elapsed and reserved time vary across live Runs; compare every other Goal fact.
                let normalize_facts = |facts: &mut arany::GoalContextFacts| {
                    facts.active_ms = 0;
                    facts.reserved_active_ms = 0;
                };
                if let Some(facts) = &mut input.goal_context_facts {
                    normalize_facts(facts);
                }
                let tools = input.tools.as_mut().unwrap();
                // The catalog embeds the wall clock at each Run's start.
                let mut catalog: serde_json::Value = serde_json::from_str(&tools.catalog).unwrap();
                catalog["host_clock"]["unix_ms"] = 0.into();
                tools.catalog = catalog.to_string();
                for step in &mut tools.steps {
                    if let Some(facts) = &mut step.facts {
                        normalize_facts(facts);
                    }
                    for observation in &mut step.observations {
                        // Normalize the per-run intent identities and expiry, which never enter model input.
                        observation.intent.id = Uuid::nil();
                        observation.intent.run_id = normalized_run;
                        observation.intent.agent_run_id = normalized_actor;
                        observation.intent.expires_at_ms = 0;
                    }
                }
                input
            })
            .collect();
        observations.push((
            result.run.assistant_message.clone(),
            result.run.agents[0].summary.clone(),
            inputs,
        ));
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            5,
            "strict replay never calls the Provider"
        );
    }
    assert_eq!(observations[0], observations[1]);
}

#[tokio::test]
async fn optional_plan_narrows_later_calls_and_keeps_the_success_gate() {
    let temp = tempfile::tempdir().expect("fixture");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let plan = facts_plan(facts_digest("v1"));
    let provider = Arc::new(ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(plan.clone()),
        crate::tool_outcome(arany::ToolCall::Write {
            path: "outside.txt".into(),
            expected_digest: None,
            content: "never published".into(),
        }),
        crate::tool_outcome(facts_read()),
        finish_with("verified"),
        finish_with("history checked"),
    ]));
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Verify the selected file");
    input.include_paths.clear();
    let result = run_with(
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
    engine.close().await.expect("shutdown");
    let engine = engine_support::open(
        StateRoot::open_existing(&state_path).expect("state"),
        EngineConfig::default(),
    )
    .expect("reopened Engine");
    let mut follow_up = request(&workspace, "What happened?");
    follow_up.include_paths.clear();
    follow_up.session_id = Some(result.session_id);
    follow_up.title = None;
    run_to_end(&engine, &provider, follow_up)
        .await
        .expect("follow-up");
    engine.close().await.expect("follow-up shutdown");
    let (events, view) = replayed(&state_path, result.session_id).await;
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(view.goals[0].plan_contract, Some(plan));
    assert!(view.goals[0].success_criterion_satisfied);
    assert!(!workspace.join("outside.txt").exists());
    assert_eq!(view.runs[0].tools.len(), 1);
    let calls = &view.runs[0].agents[0].provider_calls;
    assert_eq!(calls[0].disposition, ProviderCallDisposition::PlanRecorded);
    assert_eq!(calls[0].requested_tools, 0);
    assert_eq!(
        calls[1].refusal.as_ref().expect("scope refusal").cause,
        arany::ProposalRefusalCause::PlanScopeExcludesCall
    );
    let requests = provider.requests.lock().expect("requests");
    assert_eq!(requests.len(), 5);
    let history = &requests[4].input.history[0].assistant;
    assert!(history.contains("verified"));
    assert!(!history.contains("last_answer_refused"));
    assert!(!history.contains("nothing dispatched"));
    assert!(
        requests[1].input.tools.as_ref().expect("context").steps[0]
            .plan
            .is_some()
    );
    assert_eq!(requests[2].input.last_answer_refused, calls[1].refusal);
    let plan_call = events.iter().position(|event| matches!(&event.event, Event::ProviderCallRecorded { record, .. } if record.disposition == ProviderCallDisposition::PlanRecorded)).expect("plan call");
    let contract = events
        .iter()
        .position(|event| matches!(&event.event, Event::GoalPlanContractRecorded { .. }))
        .expect("contract");
    assert!(plan_call < contract);
    assert!(
        SessionView::replay(result.session_id, &events[..=plan_call])
            .expect("interrupted plan prefix")
            .expect("Session")
            .goals[0]
            .plan_contract
            .is_none()
    );
    let mut missing = events.clone();
    missing.remove(contract);
    for (index, event) in missing.iter_mut().enumerate() {
        event.sequence = u64::try_from(index + 1).expect("bounded journal");
    }
    assert!(SessionView::replay(result.session_id, &missing).is_err());
}

#[tokio::test]
async fn resumed_goal_refuses_plan_excluded_invoked_skill_before_tool_started() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("fixture");
    let workspace = facts_workspace(temp.path(), "v1");
    let state = temp.path().join("state");
    let (session_id, goal_id) = interrupted_autonomous_goal(&state, &workspace).await;
    let skill = workspace.join(".agents/skills/ask-matt");
    std::fs::create_dir_all(&skill).expect("Skill directory");
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: ask-matt\ndescription: Guide\n---\nUntrusted guidance",
    )
    .expect("Skill");
    let engine = facts_engine(StateRoot::open_existing(&state).expect("state"), &state);
    let config = state.join("tools.json");
    std::fs::write(
        &config,
        br#"{"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}"#,
    )
    .expect("admit project Skill discovery");
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
        .expect("private config");
    let provider = Arc::new(ScriptedProvider::new(vec![finish_with(
        "Must not be called",
    )]));
    let mut input = request(&workspace, "Resume the reviewed Goal");
    input.session_id = Some(session_id);
    input.title = None;
    input.include_paths.clear();
    input.goal = arany::GoalRequest::resume(goal_id, true);
    input.skill = Some("ask-matt".into());
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
    .expect("closed refusal rather than journal error");
    assert_eq!(outcome.run.status, RunStatus::Failed);
    assert_eq!(outcome.run.failure, Some(arany::RunFailure::ToolStopped));
    assert!(provider.requests.lock().expect("requests").is_empty());
    engine.close().await.expect("shutdown");
    let (events, view) = replayed(&state, session_id).await;
    let run = view.runs.last().expect("resumed Run");
    assert_eq!(run.id, outcome.run.id);
    assert_eq!(run.skill.as_deref(), Some("ask-matt"));
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.tools.is_empty());
    assert!(!events.iter().any(|event| matches!(event.event,
        Event::ToolStarted { run_id, .. } | Event::GoalProviderCallReserved { run_id, .. }
            if run_id == run.id
    )));
}

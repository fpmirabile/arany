use super::goals::{
    AUTONOMOUS_OBJECTIVE, autonomous_input, call_facts, facts_digest, facts_engine, facts_plan,
    facts_read,
};
use super::steering::steering_note_at;
use super::team::child_tool_starts;
use super::*;
use arany::WorkspaceIdentity;
use arany::test_support::MAX_SESSION_EVENTS;
use arany::test_support::goals::*;

#[tokio::test]
async fn budget_incident_path_distinguishes_failed_reads_from_refused_next_actions() {
    for (case, calls, tokens, unknown, expected) in [
        ("calls", Some(3), None, false, Some(arany::GoalLimit::Calls)),
        (
            "tokens",
            None,
            Some(24),
            false,
            Some(arany::GoalLimit::Tokens),
        ),
        ("finite_remaining", Some(6), Some(100), false, None),
        ("default_unlimited", None, None, true, None),
        ("provider_failure", Some(3), None, false, None),
        (
            "unknown_tokens",
            None,
            Some(100),
            true,
            Some(arany::GoalLimit::Tokens),
        ),
    ] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "facts");
        let state_path = temp.path().join("state");
        let missing = || {
            crate::tool_outcome(arany::ToolCall::Read {
                path: "notes/missing.txt".into(),
                offset: None,
                limit: None,
            })
        };
        let mut provider = ScriptedProvider::new(vec![
            missing(),
            missing(),
            missing(),
            finish_with("Recovered"),
        ]);
        if !unknown {
            provider = provider.with_known_input_tokens();
        }
        let failed_call = case == "provider_failure";
        if failed_call {
            provider.responses.lock().unwrap()[2] = Err(arany::ProviderError::Unavailable);
        }
        let provider = Arc::new(provider);
        let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
        let mut input = request(&workspace, "Inspect missing facts and report");
        input.include_paths.clear();
        input.goal = arany::GoalRequest::new(
            arany::GoalBudget {
                max_provider_calls: calls,
                max_tokens: tokens,
                ..Default::default()
            },
            arany::GoalPhase::Implementation,
        );
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
        .expect("classified Run");
        engine.close().await.expect("Engine shutdown");
        let (events, view) = replayed(&state_path, result.session_id).await;
        let run = &view.runs[0];
        let goal = &view.goals[0];
        assert_eq!(goal.budget_closed, expected.is_some(), "{case}");
        assert_eq!(
            goal.budget_refusal.map(|refusal| refusal.dimension),
            expected,
            "{case}"
        );
        if let Some(refusal) = goal.budget_refusal {
            assert_eq!(
                refusal.used,
                if unknown {
                    8
                } else if tokens.is_some() {
                    24
                } else {
                    3
                },
                "{case}"
            );
            assert_eq!(
                refusal.limit,
                if unknown {
                    100
                } else if tokens.is_some() {
                    24
                } else {
                    3
                },
                "{case}"
            );
            assert_eq!(refusal.token_usage_unknown, unknown, "{case}");
            assert_eq!(refusal.action, arany::BudgetAction::Tool, "{case}");
        }
        assert_eq!(
            view.run_stop(run),
            if failed_call {
                Some(arany::RunStop::Failed(arany::RunFailure::ProviderCall))
            } else {
                expected.map(arany::RunStop::GoalExhausted)
            },
            "{case}"
        );
        let count = if failed_call {
            3
        } else if expected.is_none() {
            4
        } else if unknown {
            1
        } else {
            3
        };
        assert_eq!(
            provider.requests.lock().expect("requests").len(),
            count,
            "{case}: no extra call"
        );
        assert_eq!(goal.provider_calls as usize, count, "{case}");
        assert_eq!(
            run.tools.len(),
            if failed_call {
                2
            } else if expected.is_none() {
                3
            } else if unknown {
                0
            } else {
                2
            },
            "{case}"
        );
        assert!(
            run.tools.iter().all(|tool| tool
                .observation
                .as_ref()
                .is_some_and(|o| o.disposition == arany::ToolDisposition::Failed)),
            "{case}"
        );
        assert!(
            matches!(events.last().expect("Goal receipt").event, Event::GoalFinished { exhausted, .. } if exhausted == expected.is_some()),
            "{case}"
        );
        assert_eq!(
            run.failure,
            if failed_call {
                Some(arany::RunFailure::ProviderCall)
            } else {
                expected.map(|_| arany::RunFailure::BudgetExhausted)
            },
            "{case}"
        );
    }
}

#[tokio::test]
async fn budget_refusal_store_failure_and_lost_ack_never_claim_a_live_receipt() {
    for (before_goal, committed) in [(true, false), (false, false), (false, true)] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = facts_workspace(temp.path(), "facts");
        let state_path = temp.path().join("state");
        let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap();
        let gate = if committed {
            Some(arany::FaultPlan::gate_goal_append_acknowledgment(
                session,
                |event| matches!(event, Event::GoalFinished { .. }),
            ))
        } else if before_goal {
            arany::FaultPlan::fail_goal_append(session, |event| {
                matches!(event, Event::GoalStarted { .. })
            });
            None
        } else {
            arany::FaultPlan::fail_goal_append(session, |event| {
                matches!(event, Event::GoalFinished { .. })
            });
            None
        };
        let provider = Arc::new(
            ScriptedProvider::new(vec![
                crate::tool_outcome(facts_read()),
                finish_with("Forbidden extra call"),
            ])
            .with_known_input_tokens(),
        );
        let mut input = request(&workspace, "Read facts");
        input.session_id = Some(session);
        input.title = None;
        input.include_paths.clear();
        input.goal = arany::GoalRequest::new(
            arany::GoalBudget {
                max_provider_calls: Some(1),
                ..Default::default()
            },
            arany::GoalPhase::Implementation,
        );
        let error = {
            let run = run_with(
                &engine,
                &provider,
                input,
                RunControls {
                    tools: ToolGrant::Configured,
                    ..Default::default()
                },
            );
            tokio::pin!(run);
            if let Some((entered, release)) = gate {
                tokio::select! { result = &mut run => panic!("Run ended before terminal acknowledgment: {:?}", result.map(|r| r.run.status)), entered = entered => entered.unwrap() }
                drop(release);
            }
            run.await.expect_err("unacknowledged terminal fact")
        };
        if before_goal {
            assert!(matches!(error, EngineError::Store(_)));
        } else if committed {
            assert!(matches!(error, EngineError::GoalAdmissionUncertain));
            assert!(!error.to_string().contains("uncommitted"));
        } else {
            assert!(matches!(error, EngineError::GoalBudgetUncommitted { .. }));
            assert!(error.to_string().contains("uncommitted"));
        }
        engine.close().await.unwrap();
        let (events, view) = replayed(&state_path, session).await;
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            usize::from(!before_goal)
        );
        if before_goal {
            assert!(view.goals.is_empty());
            assert!(view.runs.is_empty());
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e.event, Event::GoalFinished { .. }))
            );
            continue;
        }
        assert_eq!(view.goals[0].budget_refusal.is_some(), committed);
        assert_eq!(view.goals[0].budget_closed, committed);
        assert!(view.runs[0].tools.is_empty());
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e.event, Event::GoalFinished { .. }))
                .count(),
            usize::from(committed)
        );
    }
}

#[tokio::test]
async fn default_goal_completes_past_64_calls_and_30_active_minutes() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "Fact");
    let state_path = temp.path().join("state");
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let provider = Arc::new(long_turn_provider());
    let outcome = run_with(
        &engine,
        &provider,
        request(&workspace, "Inspect the folder"),
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .expect("long default Goal");
    assert_eq!(
        outcome.run.assistant_message.as_deref(),
        Some("Long turn completed")
    );
    engine.close().await.expect("Engine shutdown");
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert_long_turn(&events, &view);
    assert_eq!(provider.requests.lock().expect("requests").len(), 65);
    assert!(provider.responses.lock().expect("script").is_empty());
    assert!(
        provider
            .compact_requests
            .lock()
            .expect("compactions")
            .is_empty()
    );
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
    let mut provider = provider;
    provider.usage_override = Some(arany::Usage {
        input_tokens: Some(12),
        output_tokens: Some(8),
        cache_read_input_tokens: Some(7),
        cache_creation_input_tokens: Some(3),
    });
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
    let usage = &view.usage.groups[0];
    assert_eq!(usage.cache_read.known, Some(7));
    assert_eq!(usage.cache_creation.known, Some(3));
    assert_eq!(usage.cache_creation.unreported, 0);
    assert_eq!(
        usage.input.known.unwrap() + usage.output.known.unwrap(),
        view.goals[0].known_tokens
    );
    assert!(view.usage.pending.is_empty());
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
    let refusal = view.goals[0].budget_refusal.expect("active refusal");
    assert_eq!(refusal.dimension, arany::GoalLimit::ActiveTime);
    assert_eq!(
        refusal.cause,
        arany::BudgetRefusalCause::InsufficientHeadroom
    );
    assert_eq!((refusal.used, refusal.limit), (0, 1));
    assert_eq!(refusal.required_minimum, Some(5_001));
    assert!(view.goals[0].active_deadline.is_none());
    store.close().await.expect("Store shutdown");
}

#[tokio::test]
async fn goal_paused_at_its_call_budget_closes_as_failed_on_explicit_resume() {
    for lost_ack in [false, true] {
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

        let second_provider = ScriptedProvider::new(vec![finish_with("Forbidden extra call")]);
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
        let result = {
            let submission = run_to_end(&second_engine, &second_engine_provider, resumed);
            tokio::pin!(submission);
            if lost_ack {
                let (entered, release) = arany::FaultPlan::gate_goal_append_acknowledgment(
                    initial.session_id,
                    |event| matches!(event, Event::GoalFinished { .. }),
                );
                tokio::select! {
                    result = &mut submission => panic!("Resume ended before terminal acknowledgment: {:?}", result.map(|outcome| outcome.run.status)),
                    entered = entered => entered.expect("exhausted resume receipt reached acknowledgment"),
                }
                drop(release);
            }
            submission.await
        };
        let expected = if lost_ack {
            matches!(&result, Err(EngineError::GoalAdmissionUncertain))
        } else {
            matches!(&result, Err(EngineError::GoalBudgetExhausted))
        };
        assert!(
            expected,
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
        let refusal = goal.budget_refusal.expect("exhausted resume receipt");
        assert_eq!(refusal.action, arany::BudgetAction::Resume);
        assert_eq!(refusal.dimension, arany::GoalLimit::Calls);
        assert_eq!((refusal.used, refusal.limit), (1, 1));
        assert_eq!((refusal.run_id, refusal.agent_run_id), (None, None));
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
    std::fs::write(workspace.join("admitted.txt"), "Original admitted input").expect("include");
    first.include_paths = vec!["admitted.txt".into()];
    first.objective = "Ship the migration; literal @never-admit.txt".into();
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

    let mut changed = continue_request(initial.goal_id);
    changed.include_paths = vec!["never-admit.txt".into()];
    assert!(matches!(
        run_to_end(&engine, &provider, changed).await,
        Err(EngineError::InvalidRequest)
    ));
    let requests = Arc::clone(&provider.requests);
    let continued = run_to_end(&engine, &provider, continue_request(initial.goal_id))
        .await
        .expect("continued Goal Run");
    assert_ne!(continued.goal_id, initial.goal_id);
    {
        let calls = requests.lock().expect("requests");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input.includes.len(), 1);
        assert!(calls[0].input.includes[0].contains("Original admitted input"));
    }
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
    assert_eq!(
        new.objective,
        "Ship the migration; literal @never-admit.txt"
    );
    assert_eq!(new.include_paths, Some(vec!["admitted.txt".into()]));
    assert_eq!(old.include_paths, new.include_paths);
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
    let refusal = goal.budget_refusal.expect("Provider refusal receipt");
    assert_eq!(refusal.action, arany::BudgetAction::Provider);
    assert_eq!(refusal.run_id, Some(outcome.run.id));
    assert_eq!(refusal.agent_run_id, Some(outcome.run.agents[0].id));
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
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: 100,
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

struct FiniteTokenTeam {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

#[async_trait::async_trait]
impl Provider for FiniteTokenTeam {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                let mut identity = arany::ProviderIdentity::new(
                    "openai",
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
                delegate(&[
                    ("Review one", None),
                    ("Review two", None),
                    ("Review three", None),
                ]),
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
async fn exhausted_crashed_goal_closes_only_when_its_includes_are_known() {
    use std::os::unix::fs::MetadataExt;

    for includes_known in [true, false] {
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
                include_paths: includes_known.then(Vec::new),
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
                    provider: arany::ProviderRecord::parse("openai").expect("Provider record"),
                    model: "scripted-model".into(),
                    effort: None,
                    custom_profile_provenance: None,
                    saved_api_account_id: None,
                    chatgpt_provenance: None,
                    output_token_bound: arany::OutputTokenBound::ProviderEnforced,
                    context_window_tokens: None,
                    policy: CollaborationPolicy::Single,
                    output_token_cap: 4096,
                    provider_concurrency: 1,
                    workspace_device: identity.device,
                    workspace_inode: identity.inode,
                    user_instruction_digest: None,
                    instruction_sources: Vec::new(),
                    launch_directory: String::new(),
                    include_digests: Vec::new(),
                    history_run_ids: Vec::new(),
                    excluded_history_runs: 0,
                    context_usage: None,
                    compaction_event_sequence: None,
                    compaction_content_digest: None,
                    tool_policy: None,
                },
                active_deadline_ms: None,
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
        let before = store.load_session(session_id).await.expect("seed Events");
        store.close().await.expect("seed Store shutdown");

        let provider = ScriptedProvider::new(vec![finish_with("Unexpected resumed call")]);
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
        let result = run_to_end(&engine, &provider, resumed).await;
        assert!(requests.lock().expect("Provider requests").is_empty());
        if !includes_known {
            assert!(
                matches!(result, Err(EngineError::GoalIncludesUnavailable)),
                "unknown includes take precedence over budget closure: {result:?}"
            );
            engine.close().await.expect("Engine shutdown");
            let (after, view) = replayed(&state_path, session_id).await;
            assert_eq!(
                after, before,
                "unknown includes refuse without changing the journal"
            );
            assert_eq!(view.goal(goal_id).expect("Goal").include_paths, None);
            assert_eq!(view.interrupted_goal_id(), Some(goal_id));
            continue;
        }
        assert!(
            matches!(result, Err(EngineError::GoalBudgetExhausted)),
            "the exhausted crash-left Goal reports its budget, not a store or replay error"
        );

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
    let result = engine
        .submit(
            AdmittedProviders::single(provider),
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .expect("idle Engine")
        .await
        .expect("terminal Run");
    let outcome = result.run;
    assert_eq!(outcome.run.status, RunStatus::Failed);
    assert_eq!(
        result.stopped,
        Some(arany::RunStop::GoalExhausted(arany::GoalLimit::Runs))
    );
    assert_eq!(
        requests.lock().expect("Provider requests").len(),
        4,
        "the second Run's Answer is refused, and no third Run dispatches"
    );
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(
        view.runs.last().and_then(|run| view.run_stop(run)),
        result.stopped,
        "the historical turn projects the live stop"
    );
    let goal = view.goal(outcome.goal_id).expect("Goal");
    assert_eq!(
        goal.budget_refusal.map(|refusal| refusal.dimension),
        Some(arany::GoalLimit::Runs)
    );
    let refusal = goal.budget_refusal.expect("next-Run refusal receipt");
    assert_eq!(refusal.action, arany::BudgetAction::Run);
    assert_eq!((refusal.run_id, refusal.agent_run_id), (None, None));
    assert_eq!((refusal.used, refusal.limit), (2, 2));
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
    assert_eq!(
        goal.budget_refusal.map(|refusal| refusal.dimension),
        Some(arany::GoalLimit::Calls)
    );
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
                arany::ProviderIdentity::new("openai", "metered-model", arany::Provenance::native())
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
    assert_eq!(
        goal.budget_refusal.map(|refusal| refusal.dimension),
        Some(arany::GoalLimit::Tokens)
    );
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

#[tokio::test]
async fn active_time_stops_the_tool_step_with_a_tool_refusal() {
    for after_read in [false, true] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let budget = arany::GoalBudget {
            max_active_ms: Some(120_000),
            ..arany::GoalBudget::default()
        };
        let mut plan = facts_plan(facts_digest("v2"));
        plan.budget = budget;
        let provider = ScriptedProvider::new(vec![
            arany::test_support::scenario::plan_outcome(plan),
            crate::tool_outcome(facts_read()),
            finish_with("unexpected continuation"),
        ]);
        let provider = Arc::new(provider);
        let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .expect("Session");
        let (entered, release) = arany::FaultPlan::gate_goal_append_acknowledgment(
            session,
            if after_read {
                |event| matches!(event, Event::ToolFinished { .. })
            } else {
                |event| matches!(event, Event::ProviderCallRecorded { record, .. } if record.disposition == ProviderCallDisposition::ToolRequested)
            },
        );
        let mut input = request(&workspace, AUTONOMOUS_OBJECTIVE);
        input.session_id = Some(session);
        input.title = None;
        input.include_paths.clear();
        input.goal = arany::GoalRequest::autonomous(budget, arany::GoalPhase::Implementation);
        let outcome = {
            let running = run_with(
                &engine,
                &provider,
                input,
                RunControls {
                    tools: ToolGrant::Configured,
                    ..Default::default()
                },
            );
            tokio::pin!(running);
            tokio::select! {
                result = &mut running => panic!("after_read={after_read}: Run ended before the budget gate: {:?}", result.map(|outcome| outcome.run.status)),
                entered = entered => entered.expect("budget gate reached its acknowledgment"),
            }
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(121)).await;
            release
                .send(())
                .expect("release the budget gate acknowledgment");
            tokio::time::resume();
            running.await.expect("budget-stopped Run")
        };
        engine.close().await.expect("engine shutdown");

        let (_, view) = replayed(&state_path, outcome.session_id).await;
        let run = &view.runs[0];
        assert_eq!(run.failure, Some(arany::RunFailure::BudgetExhausted));
        assert!(view.goals[0].budget_closed);
        let refusal = view.goals[0].budget_refusal.expect("Tool refusal receipt");
        assert_eq!(refusal.action, arany::BudgetAction::Tool);
        assert_eq!(refusal.dimension, arany::GoalLimit::ActiveTime);
        assert_eq!(refusal.run_id, Some(run.id));
        assert_eq!(refusal.agent_run_id, Some(run.agents[0].id));
        assert_eq!(refusal.cause, arany::BudgetRefusalCause::Exhausted);
        if after_read {
            assert_eq!(run.tools.len(), 1);
            assert_eq!(
                run.tools[0]
                    .observation
                    .as_ref()
                    .expect("Read outcome")
                    .disposition,
                arany::ToolDisposition::Succeeded
            );
        } else {
            assert!(run.tools.is_empty(), "the refused Read is never dispatched");
        }
        assert_eq!(
            provider.requests.lock().expect("requests").len(),
            2,
            "the plan call and the stopped Read call; no third call"
        );
    }
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
async fn two_child_research_delegation_admission_covers_its_spawns() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = Arc::new(ScriptedProvider::new(vec![
        delegate(&[("Inspect facts", None), ("Inspect notes", None)]),
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
            .any(|(_, slots, _, _)| *slots > 200),
        "the research delegation admission exceeds 200 slots"
    );
    assert_action_bounds(&events, false, false);
}

#[tokio::test]
async fn child_tool_sizing_does_not_exhaust_unspent_goal_time() {
    const GOAL_ACTIVE_MS: u64 = 120_000;
    const ACTIVE_MS_PER_CALL: u64 = 50_000;
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        delegate(&[("Inspect facts", None)]),
        crate::tool_outcome(facts_read()),
        finish_with("unexpected continuation"),
    ])
    .with_active_time_per_call(Duration::from_millis(ACTIVE_MS_PER_CALL));
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = team_request(&workspace, "Check the facts");
    input.goal.budget = Some(arany::GoalBudget {
        max_active_ms: Some(GOAL_ACTIVE_MS),
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
    .expect("bounded child Run");
    assert_eq!(outcome.run.failure, Some(arany::RunFailure::ToolStopped));
    engine.close().await.expect("engine shutdown");
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(requests.lock().expect("requests").len(), 2);
    let child = view.runs[0].agents[1].id;
    assert_eq!(child_tool_starts(&events, child), 1);
    let tool = &view.runs[0].agents[1].tools[0];
    let observation = tool.observation.as_ref().expect("sizing observation");
    assert_eq!(observation.disposition, arany::ToolDisposition::Limit);
    assert_eq!(observation.limit_cause, Some(arany::LimitCause::Timeout));
    assert_eq!(
        observation.output,
        "Delegation deadline reached; no operation was dispatched"
    );
    assert!(observation.attestation.is_none());
    assert!(tool.hooks.is_empty());
    assert!(!view.goal(outcome.goal_id).expect("Goal").budget_closed);
    assert_eq!(view.runs[0].failure, Some(arany::RunFailure::ToolStopped));
}

#[tokio::test]
async fn child_read_request_at_an_exhausted_goal_budget_stops_as_budget_not_failure() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        delegate(&[("Inspect facts", None)]),
        crate::tool_outcome(facts_read()),
        finish_with("never reached"),
    ])
    .with_known_input_tokens();
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = team_request(&workspace, "Check the facts");
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

#[tokio::test]
async fn near_deadline_child_tool_journals_limit_without_dispatch() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![
        delegate(&[("Inspect facts", None)]),
        crate::tool_outcome(facts_read()),
        finish_with("never reached"),
    ]);
    let requests = provider.requests.clone();
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = team_request(&workspace, "Check the facts");
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
    assert_eq!(outcome.run.failure, Some(arany::RunFailure::ToolStopped));
    engine.close().await.expect("engine shutdown");
    assert_eq!(requests.lock().expect("requests").len(), 2);

    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert!(!view.goal(outcome.goal_id).expect("Goal").budget_closed);
    let child = view.runs[0].agents[1].id;
    assert_eq!(child_tool_starts(&events, child), 1);
    let tool = &view.runs[0].agents[1].tools[0];
    let observation = tool.observation.as_ref().expect("deadline observation");
    assert_eq!(observation.disposition, arany::ToolDisposition::Limit);
    assert_eq!(observation.limit_cause, Some(arany::LimitCause::Timeout));
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
            include_paths: Some(Vec::new()),
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
                provider: arany::ProviderRecord::parse("openai").expect("Provider record"),
                model: "scripted-model".into(),
                effort: None,
                custom_profile_provenance: None,
                saved_api_account_id: None,
                chatgpt_provenance: None,
                output_token_bound: arany::OutputTokenBound::ProviderEnforced,
                context_window_tokens: None,
                policy: CollaborationPolicy::Single,
                output_token_cap: 4096,
                provider_concurrency: 1,
                workspace_device: identity.device,
                workspace_inode: identity.inode,
                user_instruction_digest: None,
                instruction_sources: Vec::new(),
                launch_directory: String::new(),
                include_digests: Vec::new(),
                history_run_ids: Vec::new(),
                excluded_history_runs: 0,
                context_usage: None,
                compaction_event_sequence: None,
                compaction_content_digest: None,
                tool_policy: None,
            },
            active_deadline_ms: None,
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
        let prefix = MAX_SESSION_EVENTS - run_offset - free;
        pad_capacity_history(
            &state_path,
            session_id,
            i64::try_from(prefix).expect("padding prefix"),
            false,
        );
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
        let prefix = MAX_SESSION_EVENTS - run_offset - free;
        pad_capacity_history(
            &state_path,
            session_id,
            i64::try_from(prefix).expect("padding prefix"),
            false,
        );
    }
    let mut provider = ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
        crate::tool_outcome(facts_read()),
        crate::tool_outcome(arany::ToolCall::Write {
            path: "notes/facts.txt".into(),
            expected_digest: None,
            content: "v2".into(),
        }),
        ProviderOutcome::Review(arany::ReviewVerdict::Ask),
        finish_with("done"),
    ])
    .with_chatgpt_account(uuid::Uuid::now_v7(), [1; 32]);
    provider.rate_limits.get_mut().unwrap().extend([
        None,
        None,
        None,
        Some(arany::test_support::routing::plan_sample(1234)),
        None,
    ]);
    let provider = Arc::new(provider);
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
    assert!(dry.iter().any(|event| matches!(&event.event, Event::ProviderCallRecorded { record, .. } if record.phase == AgentPhase::ToolReview && record.rate_limits == Some(arany::test_support::routing::plan_sample(1234)))));
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
async fn blocked_stop_reserves_its_shadow_verdict_before_journaling() {
    // The continuation tail includes its call's three same-route retry facts.
    const REFUSED: usize = 9 + arany::MAX_PROVIDER_RETRIES as usize;
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
            let prefix = MAX_SESSION_EVENTS - offset - free;
            pad_capacity_history(
                &state_path,
                session,
                i64::try_from(prefix).expect("padding prefix"),
                false,
            );
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

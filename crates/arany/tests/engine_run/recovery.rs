use super::goals::*;
use super::*;
use arany::WorkspaceIdentity;
use arany::test_support::goals::*;
use arany::test_support::routing::{api_sample, plan_sample};

#[tokio::test]
async fn resumed_goal_uses_recorded_includes_and_replay_rejects_protected_paths() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    std::fs::write(workspace.join("admitted.txt"), "Admitted facts").expect("include");
    std::fs::create_dir(workspace.join("notes")).expect("directory include");
    std::fs::write(workspace.join("notes/entry.txt"), "Directory file body").expect("entry");
    let state_path = temp.path().join("state");
    let engine = engine_support::open(
        StateRoot::admit(&state_path).unwrap(),
        EngineConfig::default(),
    )
    .unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        ProviderOutcome::Answer(Answer {
            text: "first".into(),
        }),
        ProviderOutcome::Answer(Answer {
            text: "second".into(),
        }),
    ]));
    let mut first = request(&workspace, "Read the admitted input");
    first.include_paths = vec!["admitted.txt".into(), "notes/".into()];
    first.goal.finish_after_run = false;
    let first = run_to_end(&engine, &provider, first).await.unwrap();
    let mut next = request(&workspace, "Continue");
    next.session_id = Some(first.session_id);
    next.title = None;
    next.include_paths = vec!["never-admitted.txt".into()];
    next.goal = arany::GoalRequest::resume(first.goal_id, true);
    run_to_end(&engine, &provider, next).await.unwrap();
    {
        let calls = provider.requests.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].input.includes, calls[0].input.includes);
        assert!(calls[1].input.includes[0].contains("Admitted facts"));
        assert!(calls[1].input.includes[1].starts_with("Directory:"));
        assert!(!calls[1].input.includes[1].contains("Directory file body"));
    }
    engine.close().await.unwrap();
    let (events, _) = replayed(&state_path, first.session_id).await;
    for path in [
        ".env",
        "nested/.env.local",
        ".git/config",
        "../outside",
        "line\nfeed",
    ] {
        let mut forged = events.clone();
        let event = forged
            .iter_mut()
            .find(|event| matches!(event.event, Event::GoalStarted { .. }))
            .unwrap();
        if let Event::GoalStarted { include_paths, .. } = &mut event.event {
            *include_paths = Some(vec![path.into()]);
        }
        assert!(
            event.event.payload().is_err(),
            "protected recorded path {path:?}"
        );
        assert!(SessionView::replay(first.session_id, &forged).is_err());
    }
}

#[tokio::test]
async fn long_read_only_goal_compacts_only_an_acknowledged_run_prefix() {
    compact_read_only_goal_prefix(
        "Inspect the bounded file in pages",
        true,
        GOAL_COMPACTION_WINDOW_TOKENS,
    )
    .await;
}

#[tokio::test]
async fn prose_objective_at_the_limit_compacts_its_read_only_goal_prefix() {
    // Prose escapes about 1x; the pinned objective and the first turn each carry it once.
    let prose = "Review the module and keep its contract. ".repeat(1_600);
    compact_read_only_goal_prefix(&prose[..64 * 1024], false, PROSE_COMPACTION_WINDOW_TOKENS).await;
}

#[tokio::test]
async fn user_and_project_guidance_stay_first_in_the_goal_compaction_input() {
    compact_with_guidance(
        "Inspect the bounded file in pages",
        ("User rule.", "Project rule."),
        true,
        GOAL_COMPACTION_WINDOW_TOKENS,
    )
    .await;
}

#[tokio::test]
async fn escape_heavy_guidance_does_not_refuse_goal_compaction() {
    let hostile = "\"\\\u{1}".repeat(21_000);
    compact_with_guidance(
        "Inspect the bounded file in pages",
        (&hostile, &hostile),
        false,
        GOAL_COMPACTION_WINDOW_TOKENS,
    )
    .await;
}

// The Anthropic route that reports the window reports Anthropic rate dimensions.
fn compaction_sample(index: usize) -> arany::RateLimits {
    let mut sample = api_sample(1000 + index as i64);
    sample.api.as_mut().expect("API sample").dialect = arany::ApiLimitDialect::Anthropic;
    sample
}

async fn compact_read_only_goal_prefix(objective: &str, cleared: bool, window_tokens: u32) {
    compact_with_guidance(objective, ("", ""), cleared, window_tokens).await;
}

// `cleared`: whether clearing frees enough to run before the summary.
async fn compact_with_guidance(
    objective: &str,
    (user, project): (&str, &str),
    cleared: bool,
    window_tokens: u32,
) {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = paged_read_workspace(temp.path());
    let state_path = temp.path().join("state");
    let expected_paths: Vec<&str> = [(user, "user AGENTS.md"), (project, "AGENTS.md")]
        .into_iter()
        .filter(|(text, _)| !text.is_empty())
        .map(|(_, path)| path)
        .collect();
    let outcomes = paged_read_outcomes();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(&state_path).expect("state");
        std::fs::set_permissions(&state_path, std::fs::Permissions::from_mode(0o700))
            .expect("private state");
        if !user.is_empty() {
            let file = state_path.join("AGENTS.md");
            std::fs::write(&file, user).expect("user guidance");
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
                .expect("private user guidance");
            std::fs::write(workspace.join("AGENTS.md"), project).expect("project guidance");
        }
    }
    let good_compaction = || {
        Ok(CompactionResponse {
            summary:
                "Earlier pages were read; their exact results remain in the canonical journal."
                    .into(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: Some(32),
                output_tokens: Some(12),
                cache_creation_input_tokens: Some(3),
                ..Default::default()
            },
            wire_provenance: None,
        })
    };
    let mut provider = ScriptedProvider::new(outcomes)
        .with_compactions(
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
            .chain((1..READS).map(|_| good_compaction()))
            .collect(),
        )
        .with_profile("anthropic", "test-model")
        .with_context_window(window_tokens);
    let samples = provider.compact_rate_limits.get_mut().unwrap();
    samples.extend((0..READS).map(|index| Some(compaction_sample(index))));
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
        let observations = input["tool_observations"].as_array().unwrap();
        assert_eq!(
            observations.iter().any(|result| result["cleared"] == true),
            cleared
        );
        assert!(
            observations
                .iter()
                .filter(|result| result["cleared"] == true)
                .all(|result| result.get("output").is_none())
        );
        let goal = &input["pinned_goal"];
        assert_eq!(goal["objective"], objective);
        let listed = goal["project_instructions"].as_array().unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|entry| entry["path"].as_str().unwrap())
                .collect::<Vec<_>>(),
            expected_paths
        );
        if user.len() < 1024 {
            assert_eq!(
                listed
                    .iter()
                    .map(|entry| entry["text"].as_str())
                    .collect::<Vec<_>>(),
                [user, project]
                    .iter()
                    .filter(|t| !t.is_empty())
                    .map(|t| Some(*t))
                    .collect::<Vec<_>>()
            );
        } else {
            assert!(listed.iter().all(|entry| entry.get("text").is_none()));
        }
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
                            | Event::ToolResultsCleared { .. }
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
    for (index, (_, record)) in compacted.iter().enumerate() {
        assert_eq!(record.rate_limits, Some(compaction_sample(index)));
    }
    let group = &view.usage.api_limits[0];
    assert_eq!(group.role, arany::ProviderRole::Compaction);
    let latest = compaction_sample(compacted.len() - 1);
    assert_eq!(group.rows, latest.api.unwrap().rows);
    assert_eq!(
        view.usage
            .groups
            .iter()
            .filter_map(|group| group.cache_creation.known)
            .sum::<u64>(),
        compacted
            .iter()
            .filter_map(|(_, record)| record.cache_creation_input_tokens)
            .map(u64::from)
            .sum::<u64>()
    );
    assert!(
        view.usage
            .groups
            .iter()
            .filter_map(|group| group.cache_creation.known)
            .sum::<u64>()
            >= 3
    );
    assert_eq!(
        view.usage
            .groups
            .iter()
            .map(|group| group.input.known.unwrap_or(0) + group.output.known.unwrap_or(0))
            .sum::<u64>(),
        view.goals[0].known_tokens
    );
    assert!(view.usage.pending.is_empty());
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
                    include_paths: Some(Vec::new()),
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

pub(super) async fn interrupted_autonomous_goal(
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
    let (committed, reason, error) = match result {
        Ok(RunResult {
            run,
            stopped: Some(reason),
            continuation_error: Some(error),
        }) => (run, reason, error),
        Err(error) => panic!("the committed Run must be returned with its stop reason: {error}"),
        Ok(_) => panic!("a failed continuation admission must stop the loop"),
    };
    assert_eq!(committed.run.status, RunStatus::Finished);
    assert_eq!(
        committed.run.assistant_message.as_deref(),
        Some("first pass"),
        "the committed answer is the first Run's"
    );
    assert_eq!(
        reason,
        arany::RunStop::Paused,
        "history keeps the Goal open; it records no admission failure"
    );
    assert!(
        !matches!(error, EngineError::GoalBudgetExhausted),
        "the caller gets the actionable admission failure: {error}"
    );
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, committed.session_id).await;
    assert_eq!(view.runs.last().map(|run| run.id), Some(committed.run.id));
    assert_eq!(
        view.runs.last().and_then(|run| view.run_stop(run)),
        Some(reason),
        "the historical turn projects the live stop"
    );
    let goal = view.goal(committed.goal_id).expect("Goal");
    assert_eq!(
        goal.status,
        arany::GoalStatus::Interrupted,
        "a non-budget stop leaves the Goal open for explicit resume"
    );
}

#[tokio::test]
async fn autonomous_goal_failing_after_the_next_run_is_accepted_returns_the_error() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let session = engine
        .create_session(workspace.clone(), None)
        .await
        .expect("Session");
    // The first Run's last call arms a failure of the next RunStarted, after its MessageAccepted.
    let provider = ScriptedProvider::new(vec![
        arany::test_support::scenario::plan_outcome(facts_plan(facts_digest("v2"))),
        crate::tool_outcome(facts_read()),
        finish_with("first pass"),
    ])
    .with_before_call(2, move || {
        arany::FaultPlan::fail_goal_append(session, |event| {
            matches!(event, Event::RunStarted { .. })
        });
    });
    let mut input = autonomous_input(&workspace);
    input.session_id = Some(session);
    input.title = None;
    let result = engine
        .submit(
            AdmittedProviders::single(Arc::new(provider)),
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .expect("idle Engine")
        .await;
    assert!(
        matches!(result, Err(EngineError::Store(_))),
        "an accepted Run's failure propagates, not a continuation stop: {:?}",
        result.as_ref().map(|result| result.stopped)
    );
    engine.close().await.expect("engine shutdown");

    let (_, view) = replayed(&state_path, session).await;
    assert_eq!(view.runs.len(), 2);
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(view.runs[1].status, RunStatus::Interrupted);
    assert_eq!(
        view.run_stop(&view.runs[1]),
        Some(arany::RunStop::Interrupted),
        "no effect started, so history reports an interruption"
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
async fn cancellation_after_clearing_refuses_compaction_before_reservation() {
    const GATE_TIMEOUT: Duration = Duration::from_secs(180);
    let _deadline = engine_support::deadline("cancelled compaction admission", GATE_TIMEOUT);
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = paged_read_workspace(temp.path());
    let state_path = temp.path().join("state");
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let session = engine
        .create_session(workspace.clone(), None)
        .await
        .unwrap();
    let (entered, release) = arany::FaultPlan::gate_goal_append_acknowledgment(session, |event| {
        matches!(event, Event::ToolResultsCleared { .. })
    });
    let provider = Arc::new(
        ScriptedProvider::new(paged_read_outcomes()).with_compactions(vec![Ok(
            CompactionResponse {
                summary: "Earlier pages remain recorded in the journal.".into(),
                response_id: None,
                usage: arany::Usage::default(),
                wire_provenance: None,
            },
        )]),
    );
    let mut input = request(&workspace, "Inspect the bounded file in pages");
    input.session_id = Some(session);
    input.title = None;
    input.include_paths.clear();
    let cancellation = RunCancellation::new();
    let controls = RunControls {
        tools: ToolGrant::Configured,
        cancellation: cancellation.clone(),
        ..Default::default()
    };
    let watcher = async move {
        let observed = tokio::time::timeout(GATE_TIMEOUT, entered)
            .await
            .is_ok_and(|result| result.is_ok());
        cancellation.cancel();
        let _ = release.send(());
        observed
    };
    let (outcome, reached) = tokio::join!(run_with(&engine, &provider, input, controls), watcher);
    assert!(reached, "clearing reaches its committed acknowledgment");
    let outcome = outcome.expect("cancelled Run");
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    engine.close().await.expect("shutdown");
    let (events, view) = replayed(&state_path, session).await;
    assert_eq!(view.runs[0].status, RunStatus::Cancelled);
    assert_eq!(view.goals[0].status, arany::GoalStatus::Interrupted);
    assert!(view.goals[0].budget_refusal.is_none());
    assert!(provider.compact_requests.lock().unwrap().is_empty());
    let calls = provider.requests.lock().unwrap().len();
    assert!(calls > 0 && calls <= READS);
    assert_eq!(view.goals[0].provider_calls, u32::try_from(calls).unwrap());
    assert_eq!(view.runs[0].tools.len(), calls);
    assert!(!events.iter().any(|envelope| matches!(
        envelope.event,
        Event::GoalCompactionCallReserved { .. }
            | Event::GoalContextCompacted { .. }
            | Event::GoalFinished { .. }
            | Event::MessageCommitted { .. }
    )));
}

#[tokio::test]
async fn in_run_compaction_cancelled_in_its_call_starts_no_retry_or_switch() {
    let mut anthropic = api_sample(1234);
    anthropic.api.as_mut().unwrap().dialect = arany::ApiLimitDialect::Anthropic;
    for (sample, append_failure) in [
        (plan_sample(1234), false),
        (api_sample(1234), false),
        (api_sample(1234), true),
        (anthropic.clone(), false),
        (anthropic, true),
    ] {
        interrupted_compaction_sample(sample, append_failure).await;
    }
}

async fn interrupted_compaction_sample(sample: arany::RateLimits, append_failure: bool) {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = paged_read_workspace(temp.path());
    let state_path = temp.path().join("state");
    let cancellation = RunCancellation::new();
    let mut failing = ScriptedProvider::new(vec![])
        .with_chatgpt_account(uuid::Uuid::now_v7(), [1; 32])
        .with_profile("chatgpt", "compact-primary");
    if let Some(api) = &sample.api {
        let profile = match api.dialect {
            arany::ApiLimitDialect::OpenAi => "openai",
            arany::ApiLimitDialect::Anthropic => "anthropic",
        };
        failing.identity =
            arany::ProviderIdentity::new(profile, "compact-primary", arany::Provenance::native());
    }
    let error = if append_failure {
        ProviderError::InvalidOutcome
    } else {
        ProviderError::RemoteHttp(503)
    };
    failing.compact_responses = Mutex::new(vec![Err(error)].into());
    failing
        .compact_rate_limits
        .get_mut()
        .unwrap()
        .push_back(Some(sample.clone()));
    if !append_failure {
        failing.cancel_at_compaction = Some(cancellation.clone());
    }
    let failing = Arc::new(failing);
    let fallback =
        Arc::new(ScriptedProvider::new(vec![]).with_profile("openai", "compact-fallback"));
    let primary = Arc::new(
        ScriptedProvider::new(paged_read_outcomes())
            .with_profile("anthropic", "primary")
            .with_context_window(SMALL_WINDOW_TOKENS),
    );
    let mut bindings = AdmittedProviders::single(primary.clone());
    bindings.roles.compaction = vec![failing.clone(), fallback.clone()];
    let engine = facts_engine(
        StateRoot::admit(&state_path).expect("private state"),
        &state_path,
    );
    let mut input = request(&workspace, "Inspect the bounded file in pages");
    input.include_paths.clear();
    let session = engine
        .create_session(workspace.clone(), None)
        .await
        .unwrap();
    input.session_id = Some(session);
    input.title = None;
    if append_failure {
        arany::FaultPlan::fail_goal_append(session, |event| {
            matches!(event, Event::GoalContextCompacted { .. })
        });
    }
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
    .await;
    if append_failure {
        assert!(outcome.is_err());
    } else {
        assert_eq!(outcome.unwrap().run.status, RunStatus::Cancelled);
    }
    engine.close().await.expect("engine shutdown");

    let (events, view) = replayed(&state_path, session).await;
    let run = &view.runs[0];
    assert!(
        run.provider_fallbacks.is_empty() && run.provider_retries.is_empty(),
        "a cancelled compaction call starts no further attempt"
    );
    let record = events.iter().find_map(|event| match &event.event {
        Event::GoalContextCompacted { record, .. } => Some(record),
        _ => None,
    });
    if append_failure {
        assert!(view.usage.api_limits.is_empty());
        assert!(record.is_none());
        assert!(fallback.compact_requests.lock().unwrap().is_empty());
        assert_eq!(failing.compact_requests.lock().unwrap().len(), 1);
        return;
    }
    let record = record.expect("compaction record");
    assert_eq!(record.model, "compact-primary");
    assert_eq!(record.rate_limits.as_ref(), Some(&sample));
    if let Some(api) = sample.api {
        assert_eq!(
            view.usage.api_limits[0].role,
            arany::ProviderRole::Compaction
        );
        assert_eq!(view.usage.api_limits[0].rows, api.rows);
    }
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
async fn supervisor_failed_or_unknown_verdict_append_stops_without_another_call() {
    for committed in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = facts_workspace(temp.path(), "stable");
        let state_path = temp.path().join("state");
        let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap();
        let gate = if committed {
            Some(arany::FaultPlan::gate_goal_append_acknowledgment(
                session,
                |event| matches!(event, Event::SupervisorVerdictRecorded { .. }),
            ))
        } else {
            arany::FaultPlan::fail_goal_append(session, |event| {
                matches!(event, Event::SupervisorVerdictRecorded { .. })
            });
            None
        };
        let provider = Arc::new(ScriptedProvider::new(
            (0..4).map(|_| crate::tool_outcome(facts_read())).collect(),
        ));
        let mut input = request(&workspace, "Inspect repeatedly");
        input.include_paths.clear();
        input.session_id = Some(session);
        input.title = None;
        {
            let pending = run_with(
                &engine,
                &provider,
                input,
                RunControls {
                    tools: ToolGrant::Configured,
                    ..Default::default()
                },
            );
            tokio::pin!(pending);
            if let Some((entered, release)) = gate {
                tokio::select! {
                    result = &mut pending => panic!("Run ended before verdict acknowledgment: {:?}", result.map(|r| r.run.status)),
                    entered = entered => entered.expect("verdict committed"),
                }
                assert_eq!(provider.requests.lock().unwrap().len(), 3);
                drop(release);
            }
            assert!(matches!(
                pending.await,
                Err(EngineError::Store(arany::StoreError::Closed))
            ));
        }
        assert_eq!(provider.requests.lock().unwrap().len(), 3);
        engine.close().await.unwrap();
        let (events, view) = replayed(&state_path, session).await;
        assert_action_bounds(&events, false, false);
        assert_eq!(view.runs[0].status, RunStatus::Interrupted);
        let last = events.last().unwrap();
        assert_eq!(
            matches!(last.event, Event::SupervisorVerdictRecorded { .. }),
            committed
        );
        assert_eq!(matches!(last.event, Event::ToolFinished { .. }), !committed);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e.event, Event::SupervisorVerdictRecorded { .. }))
                .count(),
            usize::from(committed)
        );
    }
}

#[tokio::test]
async fn long_run_clears_before_reserving_and_reopens_the_crash_boundary_without_old_bodies() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), &format!("{}\n", "x".repeat(99)).repeat(150));
    let state_path = temp.path().join("state");
    let script = (0..40)
        .map(|_| crate::tool_outcome(facts_read()))
        .chain([finish_with("Inspected forty pages")])
        .collect();
    let provider = Arc::new(ScriptedProvider::new(script));
    let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
    let mut input = request(&workspace, "Inspect forty pages");
    input.include_paths.clear();
    input.goal.budget = Some(arany::GoalBudget {
        max_provider_calls: Some(64),
        max_runs: 2,
        ..Default::default()
    });
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
    .expect("long Run");
    assert_eq!(result.run.status, RunStatus::Finished);
    engine.close().await.unwrap();
    let (events, view) = replayed(&state_path, result.session_id).await;
    assert_eq!(view.runs[0].tools.len(), 40);
    assert!(
        view.runs[0]
            .tools
            .iter()
            .all(|tool| tool.intent.limits.result_bytes == 64 * 1024)
    );
    assert!(
        provider.compact_requests.lock().unwrap().is_empty(),
        "clearing costs no summary call"
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 41);
    let clears: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event.event {
            Event::ToolResultsCleared {
                covered_tool_count,
                cleared_bytes,
                ..
            } => Some((index, covered_tool_count, cleared_bytes)),
            _ => None,
        })
        .collect();
    assert!(clears.len() >= 2, "multiple clearing batches");
    for &(index, count, bytes) in &clears {
        assert!(bytes >= 64 * 1024);
        assert!(matches!(
            events[index + 1].event,
            Event::GoalProviderCallReserved { .. }
        ));
        let observed = events[..index]
            .iter()
            .filter(|event| matches!(event.event, Event::ToolFinished { .. }))
            .count();
        assert_eq!(observed - count as usize, 3);
        let requests = provider.requests.lock().unwrap();
        let context = requests[observed].input.tools.as_ref().unwrap();
        assert_eq!(
            context.steps.iter().filter(|step| step.cleared).count(),
            count as usize
        );
    }
    assert!(clears.windows(2).all(|pair| pair[0].1 < pair[1].1));
    let crash_path = temp.path().join("crash-state");
    let store = Store::open(StateRoot::admit(&crash_path).unwrap()).unwrap();
    let (boundary, covered, _) = clears[0];
    for event in &events[..=boundary] {
        store
            .append(result.session_id, event.event.clone())
            .await
            .unwrap();
    }
    store.close().await.unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&crash_path).unwrap()).unwrap();
    let prefix = store.load_session(result.session_id).await.unwrap();
    let crashed = SessionView::replay(result.session_id, &prefix)
        .unwrap()
        .unwrap();
    assert_eq!(crashed.runs[0].status, RunStatus::Interrupted);
    assert_eq!(crashed.runs[0].tool_results_cleared_count, covered);
    store.close().await.unwrap();
    let resumed_provider = Arc::new(ScriptedProvider::new(vec![finish_with(
        "Checked the saved facts",
    )]));
    let engine = facts_engine(StateRoot::open_existing(&crash_path).unwrap(), &crash_path);
    let mut input = request(&workspace, "Continue the inspection");
    input.session_id = Some(result.session_id);
    input.title = None;
    input.include_paths.clear();
    input.goal = arany::GoalRequest::resume(result.goal_id, true);
    run_with(
        &engine,
        &resumed_provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .expect("fresh Run after clearing crash");
    engine.close().await.unwrap();
    let requests = resumed_provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "no historical Tool or summary retried");
    let history = &requests[0].input.history;
    let records: serde_json::Value =
        serde_json::from_str(&history.last().unwrap().assistant).unwrap();
    let records = records["tool_records"].as_array().unwrap();
    assert_eq!(
        records
            .iter()
            .filter(|record| record["cleared"] == true)
            .count(),
        covered as usize
    );
    assert!(
        records[..covered as usize]
            .iter()
            .all(|record| !record.to_string().contains(&"x".repeat(99)))
    );
    assert!(
        records[covered as usize..]
            .iter()
            .all(|record| record.to_string().contains(&"x".repeat(99)))
    );
}

use super::*;
use arany::WorkspaceIdentity;
use arany::test_support::goals::*;

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
async fn objective_plus_includes_overflow_has_a_typed_pre_admission_error() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    let component = "\"".repeat(170);
    let relative = format!("{component}/{component}/{component}");
    let path = workspace.join(&relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "").unwrap();
    let state_path = temp.path().join("state");
    let engine = engine_support::open(
        StateRoot::admit(&state_path).unwrap(),
        EngineConfig::default(),
    )
    .unwrap();
    let provider = Arc::new(ScriptedProvider::new(Vec::new()));
    let mut input = request(&workspace, &"\0".repeat(64 * 1024));
    input.include_paths = vec![relative.into(); 16];
    let error = run_to_end(&engine, &provider, input).await.unwrap_err();
    assert!(matches!(error, EngineError::GoalInputTooLarge), "{error:?}");
    assert_eq!(error.to_string(), "objective plus includes too large");
    assert!(provider.requests.lock().unwrap().is_empty());
    engine.close().await.unwrap();
    assert!(
        arany::list_sessions(StateRoot::open_existing(&state_path).unwrap(), workspace)
            .await
            .unwrap()
            .is_empty()
    );
}

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

const READS: usize = 400;

fn paged_read_workspace(root: &Path) -> std::path::PathBuf {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
    std::fs::write(
        workspace.join("notes/data.txt"),
        format!("{}\n", "x".repeat(31)).repeat(2 * READS),
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

#[tokio::test]
async fn user_and_project_guidance_stay_first_in_the_goal_compaction_input() {
    compact_with_guidance(
        "Inspect the bounded file in pages",
        ("User rule.", "Project rule."),
    )
    .await;
}

#[tokio::test]
async fn escape_heavy_guidance_does_not_refuse_goal_compaction() {
    let hostile = "\"\\\u{1}".repeat(21_000);
    compact_with_guidance("Inspect the bounded file in pages", (&hostile, &hostile)).await;
}

async fn compact_read_only_goal_prefix(objective: &str) {
    compact_with_guidance(objective, ("", "")).await;
}

async fn compact_with_guidance(objective: &str, (user, project): (&str, &str)) {
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
        .chain((1..READS).map(|_| good_compaction()))
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
        let observations = input["tool_observations"].as_array().unwrap();
        assert!(observations.iter().any(|result| result["cleared"] == true));
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

pub(super) const AUTONOMOUS_OBJECTIVE: &str = "Verify the facts file";

pub(super) fn call_facts(request: &arany::ProviderRequest) -> Option<arany::GoalContextFacts> {
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

pub(super) fn facts_digest(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn assert_successful_in_process_reads(run: &arany::RunView) {
    assert_eq!(run.tools.len(), 2);
    for tool in &run.tools {
        let observed = tool.observation.as_ref().expect("Read receipt");
        assert_eq!(observed.disposition, arany::ToolDisposition::Succeeded);
        assert!(matches!(
            observed.attestation,
            Some(arany::Attestation::InProcess { .. })
        ));
    }
}

pub(super) fn facts_read() -> arany::ToolCall {
    arany::ToolCall::Read {
        path: "notes/facts.txt".into(),
        offset: None,
        limit: None,
    }
}

pub(super) fn facts_plan(sha256: String) -> arany::GoalPlanContract {
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

pub(super) fn autonomous_input(workspace: &Path) -> RunRequest {
    let mut input = request(workspace, AUTONOMOUS_OBJECTIVE);
    input.include_paths.clear();
    input.goal = arany::GoalRequest::autonomous(
        arany::GoalBudget::default(),
        arany::GoalPhase::Implementation,
    );
    input
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

async fn run_autonomous_with_setup_refusal(
    engine: &Engine,
    provider: &Arc<ScriptedProvider>,
    workspace: &Path,
    state: &Path,
) -> arany::RunOutcome {
    let session = engine
        .create_session(workspace.to_owned(), None)
        .await
        .unwrap();
    let (entered, release) = arany::FaultPlan::gate_autonomous_continuation(session);
    let mut input = autonomous_input(workspace);
    input.session_id = Some(session);
    input.title = None;
    let run = run_with(
        engine,
        provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    );
    tokio::pin!(run);
    const CONTINUATION_DEADLINE: Duration = Duration::from_secs(10);
    tokio::time::timeout(CONTINUATION_DEADLINE, async {
        tokio::select! {
            result = &mut run => panic!("autonomous Goal missed its continuation gate: {result:?}"),
            reached = entered => reached.expect("first autonomous Run settled"),
        }
        assert_setup_refused_between_autonomous_runs(state, workspace, session).await;
        assert_eq!(provider.requests.lock().unwrap().len(), 3);
        release.send(()).expect("continue autonomous Goal");
        run.await.expect("autonomous Goal")
    })
    .await
    .expect("autonomous continuation deadline")
}

async fn assert_setup_refused_between_autonomous_runs(
    state: &Path,
    workspace: &Path,
    session: arany::SessionId,
) {
    let store = Store::open(StateRoot::open_existing(state).unwrap()).unwrap();
    let before = store.load_session(session).await.unwrap();
    let view = store.load_open_view(session).await.unwrap().unwrap();
    assert_eq!(view.runs.len(), 1);
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(view.goals[0].status, arany::GoalStatus::Running);
    assert!(matches!(
        arany::start_provider_setup_edit(
            StateRoot::open_existing(state).unwrap(),
            workspace.to_owned(),
            session,
            arany::ProviderSetupTarget {
                kind: arany::ProviderSetupTargetKind::CustomProfile,
                identity: "example-route".into(),
            },
            None,
        )
        .await,
        Err(EngineError::SessionBusy)
    ));
    assert_eq!(store.load_session(session).await.unwrap(), before);
    store.close().await.unwrap();
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
    let outcome =
        run_autonomous_with_setup_refusal(&engine, &provider, &workspace, &state_path).await;
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
    resume_session(
        StateRoot::open_existing(&state_path).unwrap(),
        workspace.clone(),
        outcome.session_id,
    )
    .await
    .expect("completed autonomous submission releases its Session lease");
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
async fn command_argument_refusals_continue_without_dispatch_and_stop_at_the_refusal_limit() {
    for (cwd, attempts, valid) in [
        ("docs/specs", 1, true),
        ("./doc/spec", 1, false),
        ("docs/specs/", 1, false),
        ("./docs/specs", 1, false),
        ("../outside", 1, false),
        ("./doc/spec", 3, false),
    ] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "facts");
        std::fs::create_dir_all(workspace.join("docs/specs")).expect("directory");
        let state_path = temp.path().join("state");
        let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
        let command = arany::ToolCall::Command {
            program: "rg".into(),
            args: vec!["--glob=*.md,spec.md".into(), "x".repeat(543)],
            cwd: cwd.into(),
        };
        let provider = Arc::new(ScriptedProvider::new(
            std::iter::once(crate::tool_outcome(facts_read()))
                .chain((0..attempts).map(|_| crate::tool_outcome(command.clone())))
                .chain([finish_with("The proposal was explained")])
                .collect(),
        ));
        let mut input = request(&workspace, "Read, search the docs, and answer");
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
        .expect("known refusal continuation");
        let limited = attempts == 3;
        assert_eq!(
            outcome.run.status,
            if limited {
                RunStatus::Failed
            } else {
                RunStatus::Finished
            }
        );
        engine.close().await.expect("shutdown");
        let (events, view) = replayed(&state_path, outcome.session_id).await;
        let run = &view.runs[0];
        assert_eq!(
            run.failure,
            limited.then_some(arany::RunFailure::RefusalLimit)
        );
        assert_eq!(
            run.tools.len(),
            if valid { 2 } else { 1 },
            "invalid arguments create no effect intent"
        );
        assert_eq!(
            run.agents[0].provider_calls[1].disposition,
            if valid {
                ProviderCallDisposition::ToolRequested
            } else {
                ProviderCallDisposition::Rejected
            }
        );
        if !valid {
            assert_eq!(run.agents[0].provider_calls[1].output_tokens, Some(8));
            assert!(
                run.agents[0].provider_calls[1..1 + attempts]
                    .iter()
                    .all(|call| {
                        call.refusal.as_ref().is_some_and(|refusal| {
                            refusal.cause == arany::ProposalRefusalCause::InvalidPath
                        }) && call.failure_reason
                            == Some(arany::ProviderFailureReason::ProposalRefused)
                    })
            );
            assert!(events.iter().all(|event| !matches!(&event.event, Event::ToolStarted { intent, .. } if intent.call == command)));
            let requests = provider.requests.lock().expect("requests");
            let step = &requests[2].input.tools.as_ref().expect("Tools").steps[1];
            assert_eq!(
                step.refusal.as_ref().expect("refused Tool result").cause,
                arany::ProposalRefusalCause::InvalidPath
            );
            assert!(step.observations.is_empty());
            assert!(
                requests
                    .iter()
                    .all(|request| !request.input.outcome_correction),
                "argument refusal does not spend the wire correction"
            );
            assert_eq!(requests.len(), if limited { 4 } else { 3 });
        }
        if limited {
            let engine = engine_support::open(
                StateRoot::open_existing(&state_path).expect("state"),
                EngineConfig::default(),
            )
            .expect("reopen");
            let mut next = request(&workspace, "What was the error?");
            next.session_id = Some(outcome.session_id);
            next.title = None;
            next.include_paths.clear();
            let recovered = run_to_end(&engine, &provider, next)
                .await
                .expect("new user turn");
            assert_eq!(recovered.run.status, RunStatus::Finished);
            engine.close().await.expect("shutdown");
            replayed(&state_path, outcome.session_id).await;
            let requests = provider.requests.lock().expect("requests");
            let failure = requests[4]
                .input
                .previous_run_failure
                .expect("committed failure cause");
            assert_eq!(
                failure.refusal_cause,
                Some(arany::ProposalRefusalCause::InvalidPath)
            );
        }
        assert!(provider.responses.lock().expect("responses").is_empty());
    }
}

#[tokio::test]
async fn outcome_correction_notice_is_spent_after_the_corrective_tool_call() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "facts");
    let state_path = temp.path().join("state");
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let provider = ScriptedProvider::new(vec![
        finish_with("rejected placeholder"),
        crate::tool_outcome(facts_read()),
        finish_with("The answer after correction"),
    ]);
    *provider
        .responses
        .lock()
        .expect("responses")
        .front_mut()
        .expect("first") = Err(ProviderError::InvalidOutcomeContract);
    let provider = Arc::new(provider);
    let mut input = request(&workspace, "Read the facts and answer");
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
    .expect("corrective Tool continuation");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("shutdown");
    let (_, view) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(view.runs[0].tools.len(), 1);
    let requests = provider.requests.lock().expect("requests");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.input.outcome_correction)
            .collect::<Vec<_>>(),
        [false, true, false]
    );
    assert!(provider.responses.lock().expect("responses").is_empty());
}

#[tokio::test]
async fn malformed_outcomes_get_one_journaled_correction_and_keep_the_failure_cause() {
    for failures in [1, 2] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "facts");
        let state_path = temp.path().join("state");
        let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
        let mut script = vec![crate::tool_outcome(facts_read())];
        script.extend((0..failures).map(|_| finish_with("rejected placeholder")));
        script.push(finish_with("The answer from the valid response"));
        let provider = ScriptedProvider::new(script);
        for response in provider
            .responses
            .lock()
            .expect("responses")
            .iter_mut()
            .skip(1)
            .take(failures)
        {
            *response = Err(ProviderError::InvalidOutcomeContract);
        }
        let provider = Arc::new(provider);
        let mut input = request(&workspace, "Read the facts and answer");
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
        .expect("bounded outcome correction");
        engine.close().await.expect("close before replay");
        let (events, view) = replayed(&state_path, outcome.session_id).await;
        let run = &view.runs[0];
        assert_eq!(run.tools.len(), 1, "the read is never redispatched");
        assert_eq!(run.agents[0].provider_calls.len(), 3);
        assert_eq!(
            run.failure,
            (failures == 2).then_some(arany::RunFailure::ProviderCall)
        );
        assert_eq!(
            run.assistant_message.as_deref(),
            (failures == 1).then_some("The answer from the valid response")
        );
        for end in 1..=events.len() {
            SessionView::replay(outcome.session_id, &events[..end])
                .expect("every committed prefix replays");
        }
        let calls = &run.agents[0].provider_calls;
        assert_eq!(
            calls[1].failure_reason,
            Some(arany::ProviderFailureReason::OutcomeContract)
        );
        assert_eq!(
            calls[2].failure_reason,
            (failures == 2).then_some(arany::ProviderFailureReason::OutcomeContract)
        );
        {
            let requests = provider.requests.lock().expect("requests");
            assert_eq!(requests.len(), 3);
            assert!(!requests[1].input.outcome_correction);
            assert!(requests[2].input.outcome_correction);
            assert_eq!(
                requests[1].input.tools.as_ref().expect("Tools").steps[0].observations,
                requests[2].input.tools.as_ref().expect("Tools").steps[0].observations,
                "settled observations survive correction"
            );
        }
        if failures == 2 {
            let refusal = events.iter().position(|event| matches!(event.event, Event::ProviderCallRecorded { ref record, .. } if record.failure_reason == Some(arany::ProviderFailureReason::OutcomeContract)))
                .expect("first malformed call");
            let second = events.iter().enumerate().skip(refusal + 1).find_map(|(index, event)| matches!(event.event, Event::ProviderCallRecorded { ref record, .. } if record.failure_reason == Some(arany::ProviderFailureReason::OutcomeContract)).then_some(index))
                .expect("second malformed call");
            let mut forbidden = events[..=second].to_vec();
            let mut extra = forbidden
                .iter()
                .rev()
                .find(|event| matches!(event.event, Event::GoalProviderCallReserved { .. }))
                .expect("prior admitted reservation")
                .clone();
            extra.sequence = forbidden.last().expect("last Event").sequence + 1;
            forbidden.push(extra);
            assert!(
                SessionView::replay(outcome.session_id, &forbidden).is_err(),
                "replay refuses a third attempt"
            );
            let engine = engine_support::open(
                StateRoot::open_existing(&state_path).expect("state"),
                EngineConfig::default(),
            )
            .expect("reopen");
            let mut next = request(&workspace, "What was the error?");
            next.session_id = Some(outcome.session_id);
            next.title = None;
            next.include_paths.clear();
            let follow_up = run_to_end(&engine, &provider, next)
                .await
                .expect("new turn");
            engine.close().await.expect("shutdown");
            replayed(&state_path, follow_up.session_id).await;
            let requests = provider.requests.lock().expect("requests");
            let feedback = requests[3]
                .input
                .previous_run_failure
                .expect("prior failure after Tools");
            assert_eq!(
                feedback.stop,
                arany::RunStop::Failed(arany::RunFailure::ProviderCall)
            );
            assert_eq!(
                feedback.provider_failure,
                Some(arany::ProviderFailureReason::OutcomeContract)
            );
            assert!(
                !requests[3].input.outcome_correction,
                "a new Run has its own correction bound"
            );
        }
        assert!(provider.responses.lock().expect("responses").is_empty());
    }
}

#[tokio::test]
async fn pending_tool_refusal_does_not_clear_or_compact_settled_results() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "original");
    let state_path = temp.path().join("state");
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    std::fs::write(state_path.join("tools.json"),
        br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#
    ).expect("write grant");
    let script = (0..4)
        .map(|_| crate::tool_outcome(facts_read()))
        .chain([crate::tool_outcome(arany::ToolCall::Write {
            path: "notes/facts.txt".into(),
            expected_digest: Some(facts_digest("original")),
            content: "x".repeat(64 * 1024),
        })])
        .collect();
    let provider = Arc::new(
        ScriptedProvider::new(script)
            .with_chatgpt_account(Uuid::now_v7(), [7; 32])
            .with_context_window(32_768)
            .with_max_output_tokens(4096)
            .with_input_tokens_at(4, 30_000),
    );
    let mut input = request(&workspace, "Read four times, then request the write");
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
    .expect("pending request ends with a typed context refusal");
    engine.close().await.expect("shutdown");
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let run = &view.runs[0];
    assert_eq!(run.failure, Some(arany::RunFailure::ToolContextFull));
    assert_eq!(run.tools.len(), 4);
    assert!(
        run.tools
            .iter()
            .all(|tool| matches!(tool.intent.call, arany::ToolCall::Read { .. }))
    );
    assert!(events.iter().all(|event| !matches!(
        event.event,
        Event::ToolResultsCleared { .. }
            | Event::GoalCompactionCallReserved { .. }
            | Event::GoalContextCompacted { .. }
    )));
    assert!(
        provider
            .compact_requests
            .lock()
            .expect("compactions")
            .is_empty()
    );
    assert_eq!(provider.requests.lock().expect("requests").len(), 5);
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/facts.txt")).expect("original file"),
        "original"
    );
}

#[tokio::test]
async fn near_full_context_refuses_write_before_dispatch_and_preserves_the_file() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "original");
    std::fs::write(workspace.join("AGENTS.md"), "a".repeat(32 * 1024)).expect("instructions");
    for name in ["big1.txt", "big2.txt"] {
        std::fs::write(workspace.join(name), "b".repeat(127 * 1024)).expect("include");
    }
    let state_path = temp.path().join("state");
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    std::fs::write(state_path.join("tools.json"),
        br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#
    ).expect("write grant");
    let provider = Arc::new(ScriptedProvider::new(vec![crate::tool_outcome(
        arany::ToolCall::Write {
            path: "notes/facts.txt".into(),
            expected_digest: Some(facts_digest("original")),
            content: "published only with a complete receipt".into(),
        },
    )]));
    let mut input = request(&workspace, &"x".repeat(60 * 1024));
    input.include_paths = vec!["big1.txt".into(), "big2.txt".into()];
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
    .expect("context refusal");
    engine.close().await.expect("shutdown");
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let run = &view.runs[0];
    let allowance = run
        .config
        .as_ref()
        .expect("config")
        .tool_policy
        .as_ref()
        .expect("Tools")
        .max_context_bytes;
    assert!((16 * 1024..64 * 1024).contains(&allowance));
    assert_eq!(run.failure, Some(arany::RunFailure::ToolContextFull));
    assert!(run.tools.is_empty(), "no effect intent was dispatched");
    assert!(events.iter().all(|event| !matches!(
        event.event,
        Event::ToolStarted { .. } | Event::ToolFinished { .. }
    )));
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/facts.txt")).expect("unchanged file"),
        "original"
    );
    assert_eq!(provider.requests.lock().expect("requests").len(), 1);
}

#[tokio::test]
async fn tool_context_overflow_fails_with_the_context_cause() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    // 71 bytes of the 36,744 admitted are the rendered AGENTS.md heading and wrapper.
    std::fs::write(workspace.join("AGENTS.md"), "a".repeat(36_673)).expect("instructions");
    std::fs::write(workspace.join("notes/facts.txt"), "f".repeat(40 * 1024)).expect("facts");
    std::fs::write(workspace.join("big1.txt"), "b".repeat(127 * 1024)).expect("include");
    std::fs::write(workspace.join("big2.txt"), "c".repeat(127 * 1024)).expect("include");
    let state_path = temp.path().join("state");
    let budget = arany::GoalBudget {
        max_provider_calls: Some(30),
        ..arany::GoalBudget::default()
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read()),
        crate::tool_outcome(arany::ToolCall::Write {
            path: "notes/facts.txt".into(),
            expected_digest: Some(facts_digest(&"f".repeat(40 * 1024))),
            content: "x".repeat(64 * 1024),
        }),
        finish_with("Unexpected continuation after context refusal"),
    ]));
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    std::fs::write(state_path.join("tools.json"),
        br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#
    ).expect("write grant");
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
    let recovery = Arc::new(ScriptedProvider::new(vec![
        crate::tool_outcome(facts_read()),
        crate::tool_outcome(arany::ToolCall::Search {
            path: "notes/facts.txt".into(),
            query: "missing".into(),
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: arany::MAX_SEARCH_ROWS,
        }),
        finish_with("The next message works"),
    ]));
    let mut next = request(&workspace, "Inspect the facts and answer this new message");
    next.session_id = Some(outcome.session_id);
    next.title = None;
    next.include_paths.clear();
    let recovered = run_with(
        &engine,
        &recovery,
        next,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .expect("the failed Session accepts a new message");
    assert_eq!(recovered.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let run = &view.runs[0];
    assert_eq!(run.failure, Some(arany::RunFailure::ToolContextFull));
    assert!(run.assistant_message.is_none());
    assert_eq!(run.tools.len(), 1, "only the bounded Read was dispatched");
    let read = run.tools[0].observation.as_ref().expect("Read receipt");
    assert_eq!(read.disposition, arany::ToolDisposition::Succeeded);
    let output: serde_json::Value = serde_json::from_str(&read.output).expect("Read result");
    assert_eq!(output["truncated"], true);
    assert!(
        run.config
            .as_ref()
            .expect("config")
            .tool_policy
            .as_ref()
            .expect("Tools")
            .max_context_bytes
            < 64 * 1024
    );
    assert!(events.iter().all(|event| !matches!(&event.event,
        Event::ToolStarted { intent, .. } if matches!(intent.call, arany::ToolCall::Write { .. })
    )));
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/facts.txt")).expect("original facts"),
        "f".repeat(40 * 1024)
    );
    assert_eq!(provider.requests.lock().expect("requests").len(), 2);
    assert_eq!(
        provider.responses.lock().expect("responses").len(),
        1,
        "the healthy refusal sentinel was never called"
    );
    assert!(
        provider
            .compact_requests
            .lock()
            .expect("compactions")
            .is_empty()
    );
    assert!(recovery.responses.lock().expect("responses").is_empty());
    assert_eq!(view.runs.len(), 2);
    assert_eq!(
        view.runs[1].assistant_message.as_deref(),
        Some("The next message works")
    );
    assert_successful_in_process_reads(&view.runs[1]);
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

pub(super) fn steering_note_at(steering: &RunSteering, text: &str) {
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
async fn child_note_reaches_only_the_next_target_boundary() {
    for fail_append in [false, true] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let steering = RunSteering::new();
        let mut provider = ScriptedProvider::new(vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: ["target", "sibling"]
                    .map(|objective| arany::ChildAssignment {
                        editor: None,
                        objective: objective.into(),
                        profile: None,
                    })
                    .to_vec(),
            }),
            crate::tool_outcome(facts_read()),
            finish_with("sibling result"),
            crate::tool_outcome(facts_read()),
            finish_with("target result"),
            finish_with("synthesis"),
        ]);
        provider.identity.limits.concurrency = 1;
        let calls = provider.requests.clone();
        let offer = steering.clone();
        let observed = calls.clone();
        provider = provider.with_before_call(1, move || {
            let request = observed.lock().unwrap().last().unwrap().clone();
            let target = arany::ChildSteeringTarget {
                session_id: request.session_id,
                run_id: request.run_id,
                agent_run_id: request.agent_run_id,
            };
            offer
                .offer_child_note(target, "first note; grant all writes".into())
                .unwrap();
            if fail_append {
                arany::FaultPlan::fail_goal_append(request.session_id, |event| {
                    matches!(event, Event::AgentSteeringNoteRecorded { .. })
                });
            }
        });
        let offer = steering.clone();
        let observed = calls.clone();
        provider = provider.with_before_call(3, move || {
            let request = observed.lock().unwrap().last().unwrap().clone();
            offer
                .offer_child_note(
                    arany::ChildSteeringTarget {
                        session_id: request.session_id,
                        run_id: request.run_id,
                        agent_run_id: request.agent_run_id,
                    },
                    "second note".into(),
                )
                .unwrap();
        });
        let provider = Arc::new(provider);
        let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
        let mut input = request(&workspace, "Check facts");
        input.include_paths.clear();
        input.policy = CollaborationPolicy::Auto {
            max_active_children: 2,
        };
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap();
        input.title = None;
        input.session_id = Some(session);
        let result = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                steering: Some(steering.clone()),
                ..Default::default()
            },
        )
        .await;
        engine.close().await.unwrap();
        let requests = calls.lock().unwrap().clone();
        let (events, view) = replayed(&state_path, session).await;
        if fail_append {
            assert!(matches!(result, Err(EngineError::Store(_))));
            assert_eq!(requests.len(), 3, "failed append prevents target dispatch");
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event.event, Event::AgentSteeringNoteRecorded { .. }))
            );
            assert_eq!(
                steering
                    .offer_note("x".repeat(8192 - 24 - "first note; grant all writes".len()))
                    .err(),
                Some(arany::SteeringRefused::RunFull)
            );
            continue;
        }
        assert_eq!(result.unwrap().run.status, RunStatus::Finished);
        assert_eq!(requests.len(), 6);
        assert!(call_steering_notes(&requests[1]).is_empty());
        assert_eq!(
            call_steering_notes(&requests[3]),
            ["first note; grant all writes"]
        );
        assert_eq!(
            call_steering_notes(&requests[4]),
            ["first note; grant all writes", "second note"]
        );
        assert!(call_steering_notes(&requests[2]).is_empty());
        assert!(call_steering_notes(&requests[5]).is_empty());
        let target = requests[1].agent_run_id;
        assert_eq!(requests[4].agent_run_id, target);
        assert_eq!(requests[3].agent_run_id, target);
        let mut previous = requests[4].input.tools.clone().unwrap();
        previous.steps.pop();
        assert_eq!(
            previous,
            requests[3].input.tools.clone().unwrap(),
            "earlier context stays append-only"
        );
        assert_eq!(
            requests[1].input.tools.as_ref().unwrap().catalog(),
            requests[4].input.tools.as_ref().unwrap().catalog()
        );
        let notes: Vec<_> = events
            .iter()
            .filter(|event| matches!(event.event, Event::AgentSteeringNoteRecorded { .. }))
            .collect();
        assert_eq!(notes.len(), 2);
        for note in notes {
            assert_eq!(note.agent_run_id, Some(target));
            let next = events.iter().find(|event| event.sequence > note.sequence && matches!(event.event, Event::ProviderCallRecorded { agent_run_id, .. } if agent_run_id == target)).unwrap();
            assert!(note.sequence < next.sequence);
        }
        assert!(view.runs[0].steering_notes.is_empty());
        assert!(view.runs[0].agents[2].steering_notes.is_empty());
        assert!(matches!(
            view.runs[0].agents[1].grant,
            Some(arany::ChildGrant::ReadOnly { .. })
        ));
        assert_action_bounds(&events, false, true);
    }
}

#[tokio::test]
async fn child_note_is_not_taken_when_the_run_stops_after_the_call_reservation() {
    for pause in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let steering = RunSteering::new();
        let cancel = RunCancellation::new();
        let mut provider = ScriptedProvider::new(vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: ["first", "second"]
                    .map(|objective| arany::ChildAssignment {
                        editor: None,
                        objective: objective.into(),
                        profile: None,
                    })
                    .to_vec(),
            }),
            finish_with("first result"),
            finish_with("second result"),
            finish_with("synthesis"),
        ]);
        provider.identity.limits.concurrency = 1;
        let calls = provider.requests.clone();
        let (publish, published) = tokio::sync::oneshot::channel();
        let publish = Arc::new(std::sync::Mutex::new(Some(publish)));
        let observed = calls.clone();
        let provider = Arc::new(provider.with_before_call(1, move || {
            let request = observed.lock().unwrap().last().unwrap().clone();
            let gate =
                arany::FaultPlan::gate_goal_append_acknowledgment(request.session_id, |event| {
                    matches!(event, Event::GoalProviderCallReserved { .. })
                });
            if let Some(publish) = publish.lock().unwrap().take() {
                let _ = publish.send((request.agent_run_id, gate));
            }
        }));
        let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
        let mut input = request(&workspace, "Check facts");
        input.include_paths.clear();
        input.policy = CollaborationPolicy::Auto {
            max_active_children: 2,
        };
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap();
        input.title = None;
        input.session_id = Some(session);
        let arany::RunHandle {
            mut events, result, ..
        } = engine
            .submit(
                arany::AdmittedProviders::single(provider.clone()),
                input,
                RunControls {
                    steering: Some(steering.clone()),
                    cancellation: cancel.clone(),
                    ..Default::default()
                },
            )
            .unwrap();
        let driver = {
            let (steering, cancel) = (steering.clone(), cancel.clone());
            tokio::spawn(async move {
                let (first, (entered, release)) = published.await.expect("first child dispatched");
                let mut children = Vec::new();
                while children.len() < 2 {
                    if let Some(arany::Tapped::Committed(envelope)) = events.next().await
                        && let Event::AgentSpawned {
                            role: arany::AgentRole::Child,
                            run_id,
                            agent_run_id,
                            ..
                        } = envelope.event
                    {
                        children.push((run_id, agent_run_id));
                    }
                }
                entered
                    .await
                    .expect("second child reservation reached its acknowledgment");
                let (run_id, agent_run_id) = children
                    .into_iter()
                    .find(|(_, id)| *id != first)
                    .expect("second child spawned");
                steering
                    .offer_child_note(
                        arany::ChildSteeringTarget {
                            session_id: session,
                            run_id,
                            agent_run_id,
                        },
                        "too late".into(),
                    )
                    .expect("the child is registered and not paused");
                if pause {
                    assert!(steering.pause());
                } else {
                    cancel.cancel();
                }
                release.send(()).expect("release the reservation");
            })
        };
        let result = tokio::time::timeout(Duration::from_secs(30), result)
            .await
            .expect("Run settles");
        driver.await.unwrap();
        assert!(result.is_ok(), "pause={pause}");
        engine.close().await.unwrap();
        let (events, _) = replayed(&state_path, session).await;
        assert_eq!(calls.lock().unwrap().len(), 2, "pause={pause}: no dispatch");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event, Event::AgentSteeringNoteRecorded { .. })),
            "pause={pause}"
        );
        let reserved = events
            .iter()
            .filter(|event| matches!(event.event, Event::GoalProviderCallReserved { .. }))
            .count();
        let recorded = events
            .iter()
            .filter(|event| matches!(event.event, Event::ProviderCallRecorded { .. }))
            .count();
        assert_eq!(reserved, 3, "pause={pause}: the undispatched call is spent");
        assert_eq!(
            recorded, reserved,
            "pause={pause}: every reservation settles"
        );
        assert!(events.iter().any(|event| matches!(
            &event.event,
            Event::ProviderCallRecorded { record, .. }
                if record.disposition == arany::ProviderCallDisposition::Cancelled
        )));
    }
}

#[tokio::test]
async fn child_note_is_retained_when_no_boundary_remains() {
    for stop in ["finish", "cancel", "pause"] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let steering = RunSteering::new();
        let cancel = RunCancellation::new();
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: vec![arany::ChildAssignment {
                    editor: None,
                    objective: "target".into(),
                    profile: None,
                }],
            }),
            finish_with("target result"),
            finish_with("synthesis"),
        ]);
        let calls = provider.requests.clone();
        let observed = calls.clone();
        let offer = steering.clone();
        let cancellation = cancel.clone();
        let offered = Arc::new(std::sync::Mutex::new(None));
        let target_copy = offered.clone();
        let provider = Arc::new(provider.with_before_call(1, move || {
            let request = observed.lock().unwrap().last().unwrap().clone();
            let target = arany::ChildSteeringTarget {
                session_id: request.session_id,
                run_id: request.run_id,
                agent_run_id: request.agent_run_id,
            };
            offer.offer_child_note(target, "too late".into()).unwrap();
            *target_copy.lock().unwrap() = Some(target);
            if stop == "cancel" {
                cancellation.cancel();
            } else if stop == "pause" {
                offer.pause();
            }
        }));
        let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
        let mut input = request(&workspace, "Check facts");
        input.include_paths.clear();
        input.policy = CollaborationPolicy::Auto {
            max_active_children: 1,
        };
        let result = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                steering: Some(steering.clone()),
                cancellation: cancel,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        engine.close().await.unwrap();
        assert_eq!(
            calls.lock().unwrap().len(),
            if stop == "finish" { 3 } else { 2 },
            "{stop}"
        );
        let (events, view) = replayed(&state_path, result.session_id).await;
        assert_eq!(view.runs[0].agents.len(), 2);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event, Event::AgentSteeringNoteRecorded { .. }))
        );
        let target = offered.lock().unwrap().unwrap();
        assert_eq!(
            steering.offer_child_note(target, "again".into()),
            Err(arany::SteeringRefused::Unavailable)
        );
        assert_eq!(
            steering.withdraw_child_note(target).as_deref(),
            Some("too late")
        );
    }
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
            prefix_rule: None,
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
            Event::ToolStarted {
                verdict: Some(verdict),
                ..
            } => Some(verdict),
            _ => None,
        })
        .collect();
    let write = verdicts.last().expect("Write verdict");
    assert!(!write.approved);
    assert_eq!(format!("{:?}", write.source), "UnavailableApproval");
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
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = paged_read_workspace(temp.path());
    let state_path = temp.path().join("state");
    let cancellation = RunCancellation::new();
    let mut failing = ScriptedProvider::new(vec![])
        .with_chatgpt_account(uuid::Uuid::now_v7(), [1; 32])
        .with_profile("chatgpt", "compact-primary");
    failing.compact_responses = Mutex::new(vec![Err(ProviderError::RemoteHttp(503))].into());
    failing
        .compact_rate_limits
        .get_mut()
        .unwrap()
        .push_back(Some(arany::test_support::routing::plan_sample(1234)));
    failing.cancel_at_compaction = Some(cancellation.clone());
    let failing = Arc::new(failing);
    let fallback =
        Arc::new(ScriptedProvider::new(vec![]).with_profile("openai", "compact-fallback"));
    let primary =
        Arc::new(ScriptedProvider::new(paged_read_outcomes()).with_profile("openai", "primary"));
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
    assert_eq!(
        record.rate_limits,
        Some(arany::test_support::routing::plan_sample(1234))
    );
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
    let mut failing = ScriptedProvider::new(vec![]).with_profile("openai", "compact-primary");
    failing.compact_responses = Mutex::new(vec![Err(ProviderError::RemoteHttp(503))].into());
    let failing = Arc::new(failing);
    let primary =
        Arc::new(ScriptedProvider::new(paged_read_outcomes()).with_profile("openai", "primary"));
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
                let mut catalog: serde_json::Value = serde_json::from_str(tools.catalog()).unwrap();
                catalog["host_clock"]["unix_ms"] = 0.into();
                tools.set_catalog(&catalog);
                for step in &mut tools.steps {
                    if let Some(facts) = &mut step.facts {
                        normalize_facts(facts);
                    }
                    for observation in &mut step.observations {
                        // Normalize the per-run intent identities, which never enter model input.
                        observation.intent.id = Uuid::nil();
                        observation.intent.run_id = normalized_run;
                        observation.intent.agent_run_id = normalized_actor;
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

struct WindowRun {
    _temp: tempfile::TempDir,
    workspace: std::path::PathBuf,
    result: arany::RunOutcome,
    events: Vec<arany::EventEnvelope>,
    view: arany::SessionView,
    compactions: usize,
}

async fn window_run(window: Option<u32>, reads: usize, reports: &[(usize, u32)]) -> WindowRun {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), &format!("{}\n", "x".repeat(99)).repeat(150));
    let state_path = temp.path().join("state");
    let script = (0..reads)
        .map(|_| crate::tool_outcome(facts_read()))
        .chain([finish_with("Inspected the pages")])
        .collect();
    let mut provider =
        ScriptedProvider::new(script).with_compactions(vec![Err(ProviderError::Unavailable)]);
    for &(index, tokens) in reports {
        provider = provider.with_input_tokens_at(index, tokens);
    }
    if let Some(window) = window {
        provider = provider
            .with_chatgpt_account(Uuid::now_v7(), [7; 32])
            .with_context_window(window);
    }
    let provider = Arc::new(provider);
    let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
    let mut input = request(&workspace, "Inspect the pages");
    input.include_paths.clear();
    input.goal.budget = Some(arany::GoalBudget {
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
    .expect("windowed Run");
    assert_eq!(result.run.status, RunStatus::Finished);
    assert_eq!(provider.requests.lock().unwrap().len(), reads + 1);
    engine.close().await.unwrap();
    let (events, view) = replayed(&state_path, result.session_id).await;
    WindowRun {
        _temp: temp,
        workspace,
        result,
        events,
        view,
        compactions: provider.compact_requests.lock().unwrap().len(),
    }
}

fn window_pin(view: &arany::SessionView, run: usize) -> (Option<u32>, u32, Option<u32>) {
    let config = view.runs[run].config.clone().expect("pinned config");
    (
        config.context_window_tokens,
        config.context_usage.expect("usage").budget_bytes,
        config.tool_policy.map(|policy| policy.max_context_bytes),
    )
}

fn clearings(events: &[arany::EventEnvelope]) -> Vec<(usize, u32)> {
    events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event.event {
            Event::ToolResultsCleared {
                covered_tool_count, ..
            } => Some((index, covered_tool_count)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn anthropic_evidence_refreshes_between_runs_and_replays_each_window() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "facts\n");
    let state_path = temp.path().join("state");
    let provider = Arc::new(
        ScriptedProvider::new(vec![
            finish_with("Checked the facts"),
            finish_with("Checked the facts again"),
        ])
        .with_profile("anthropic", "claude-sonnet-5")
        .with_context_window_snapshots(vec![Some(100_000), None]),
    );
    let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
    let mut first_input = request(&workspace, "Check the facts");
    first_input.include_paths.clear();
    let first = run_with(&engine, &provider, first_input, RunControls::default())
        .await
        .expect("Anthropic context window admitted");
    assert_eq!(first.run.status, RunStatus::Finished);
    let mut second_input = request(&workspace, "Check the facts again");
    second_input.include_paths.clear();
    let second = run_with(&engine, &provider, second_input, RunControls::default())
        .await
        .expect("Anthropic context window revalidated");
    assert_eq!(second.run.status, RunStatus::Finished);
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests[0].input.context_window_tokens, Some(100_000));
        assert_eq!(requests[1].input.context_window_tokens, None);
    }
    engine.close().await.expect("engine shutdown");

    let (first_events, first_view) = replayed(&state_path, first.session_id).await;
    let (second_events, second_view) = replayed(&state_path, second.session_id).await;
    assert_eq!(
        first_view.runs[0]
            .config
            .as_ref()
            .unwrap()
            .context_window_tokens,
        Some(100_000)
    );
    assert_eq!(
        second_view.runs[0]
            .config
            .as_ref()
            .unwrap()
            .context_window_tokens,
        None
    );
    assert!(first_events.iter().any(|event| matches!(
        &event.event,
        Event::RunStarted { config, .. } if config.context_window_tokens == Some(100_000)
    )));
    assert!(second_events.iter().any(|event| matches!(
        &event.event,
        Event::RunStarted { config, .. } if config.context_window_tokens.is_none()
    )));
}

#[tokio::test]
async fn catalog_window_resizes_the_allowance_and_reported_pressure_clears_before_the_next_call() {
    let default = window_run(None, 8, &[(7, 244_800)]).await;
    let pressure = window_run(Some(272_000), 8, &[(7, 244_800)]).await;
    let calm = window_run(Some(272_000), 8, &[(7, 244_799)]).await;

    let (window, default_budget, default_tools) = window_pin(&default.view, 0);
    assert_eq!(
        window, None,
        "a route without catalog metadata keeps the defaults"
    );
    let (window, budget, tools) = window_pin(&pressure.view, 0);
    assert_eq!(window, Some(272_000));
    // 0.9 x 272,000 tokens x 3 bytes replaces the 376 KiB content ceiling.
    assert_eq!(budget - default_budget, 734_400 - 376 * 1024);
    assert_eq!(
        tools.unwrap() - default_tools.unwrap(),
        734_400 - 376 * 1024
    );
    assert_eq!(window_pin(&calm.view, 0), (Some(272_000), budget, tools));

    assert!(clearings(&default.events).is_empty());
    assert!(
        clearings(&calm.events).is_empty(),
        "89.99% does not trigger"
    );
    for run in [&default, &pressure, &calm] {
        assert_eq!(run.compactions, 0);
    }
    let [(index, covered)] = clearings(&pressure.events)[..] else {
        panic!("one clearing after the 90% report");
    };
    assert_eq!(covered, 5, "the three newest results stay");
    assert!(matches!(
        pressure.events[index + 1].event,
        Event::GoalProviderCallReserved { .. }
    ));
    assert_eq!(
        pressure.events[index + 1..]
            .iter()
            .filter(|event| matches!(event.event, Event::ProviderCallRecorded { .. }))
            .count(),
        1,
        "clearing precedes the call after the report"
    );

    // An interruption after the clearing boundary resumes with identical, still-windowed requests.
    let mut resumed = Vec::new();
    for copy in ["first", "second"] {
        let temp = tempfile::tempdir().expect("crash root");
        let crash_path = temp.path().join(copy);
        let store = Store::open(StateRoot::admit(&crash_path).unwrap()).unwrap();
        for event in &pressure.events[..=index] {
            store
                .append(pressure.result.session_id, event.event.clone())
                .await
                .unwrap();
        }
        store.close().await.unwrap();
        let provider = Arc::new(
            ScriptedProvider::new(vec![finish_with("Checked the saved facts")])
                .with_chatgpt_account(
                    pressure.view.runs[0]
                        .config
                        .as_ref()
                        .unwrap()
                        .chatgpt_provenance
                        .as_ref()
                        .unwrap()
                        .account_id,
                    [7; 32],
                )
                .with_context_window(272_000),
        );
        let engine = facts_engine(StateRoot::open_existing(&crash_path).unwrap(), &crash_path);
        let mut input = request(&pressure.workspace, "Continue the inspection");
        input.session_id = Some(pressure.result.session_id);
        input.title = None;
        input.include_paths.clear();
        input.goal = arany::GoalRequest::resume(pressure.result.goal_id, true);
        run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await
        .expect("fresh Run after the clearing boundary");
        engine.close().await.unwrap();
        let (_, view) = replayed(&crash_path, pressure.result.session_id).await;
        assert_eq!(view.runs[0].status, RunStatus::Interrupted);
        assert_eq!(view.runs[0].tool_results_cleared_count, covered);
        assert_eq!(window_pin(&view, 1).0, Some(272_000));
        assert_eq!(window_pin(&view, 1).1, budget);
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let history = &requests[0].input.history;
        let records: serde_json::Value =
            serde_json::from_str(&history.last().unwrap().assistant).unwrap();
        assert_eq!(
            records["tool_records"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|record| record["cleared"] == true)
                .count(),
            covered as usize
        );
        resumed.push(history.clone());
    }
    assert_eq!(resumed[0], resumed[1], "the resumed request is byte-stable");
}

#[tokio::test]
async fn each_pressured_report_maintains_context_once() {
    let repeated = window_run(Some(272_000), 9, &[(7, 244_800), (8, 244_800)]).await;
    let clearings = clearings(&repeated.events);
    assert_eq!(
        clearings
            .iter()
            .map(|(_, covered)| *covered)
            .collect::<Vec<_>>(),
        [5, 6],
        "the second report clears the newly old result"
    );
    // The second clearing frees less than one result budget, so one charged summary follows, then the Run ends.
    assert_eq!(repeated.compactions, 1);
    assert_eq!(
        repeated
            .events
            .iter()
            .filter(|event| matches!(event.event, Event::GoalContextCompacted { .. }))
            .count(),
        1
    );
    let last_maintenance = clearings[1].0;
    assert_eq!(
        repeated.events[last_maintenance..]
            .iter()
            .filter(|event| matches!(event.event, Event::ProviderCallRecorded { .. }))
            .count(),
        1,
        "no further maintenance after the final call"
    );
}

#[tokio::test]
async fn catalog_windows_leave_continuation_room_in_a_crowded_session() {
    for (window, line_repeats, compaction_expected) in [
        (None, 96, true),
        (Some(32_768), 96, true),
        (Some(272_000), 96, false),
        (Some(32_768), 24, false),
    ] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(
            temp.path(),
            &format!("{}\n", "fact ".repeat(line_repeats)).repeat(83),
        );
        let state_path = temp.path().join("state");
        let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
        let seed = Arc::new(ScriptedProvider::new(vec![
            finish_with("Historical answer");
            6
        ]));
        let mut session = None;
        for _ in 0..6 {
            let mut input = request(&workspace, &"x".repeat(60 * 1024));
            input.session_id = session;
            input.title = None;
            input.include_paths.clear();
            session = Some(
                run_with(&engine, &seed, input, RunControls::default())
                    .await
                    .expect("historical Run")
                    .session_id,
            );
        }
        let mut provider = ScriptedProvider::new(vec![
            crate::tool_outcome(facts_read()),
            crate::tool_outcome(arany::ToolCall::Search {
                path: "notes/facts.txt".into(),
                query: "missing".into(),
                regex: false,
                include: None,
                case_sensitive: true,
                offset: 0,
                limit: arany::MAX_SEARCH_ROWS,
            }),
            finish_with("Read 83 lines and searched for a missing fact"),
        ])
        .with_chatgpt_account(Uuid::now_v7(), [7; 32])
        .with_profile("chatgpt", "gpt-6-luna")
        .with_effort(arany::Effort::Medium)
        .with_max_output_tokens(4096);
        if let Some(window) = window {
            provider = provider.with_context_window(window);
        }
        if compaction_expected {
            provider = provider.with_compactions(vec![Ok(CompactionResponse {
                summary: "Read 83 lines from notes/facts.txt.".into(),
                response_id: None,
                usage: arany::Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(8),
                    ..Default::default()
                },
                wire_provenance: None,
            })]);
        }
        let provider = Arc::new(provider);
        let mut input = request(&workspace, "Inspect the facts and answer");
        input.session_id = session;
        input.title = None;
        input.include_paths.clear();
        if line_repeats == 24 {
            input.goal = arany::GoalRequest::new(
                arany::GoalBudget {
                    max_provider_calls: Some(3),
                    ..Default::default()
                },
                arany::GoalPhase::Implementation,
            );
        }
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
        .expect("bounded continuation");
        assert_eq!(outcome.run.status, RunStatus::Finished, "window={window:?}");
        engine.close().await.expect("shutdown");
        let (_, view) = replayed(&state_path, outcome.session_id).await;
        let run = &view.runs[6];
        let config = run.config.as_ref().expect("pinned context");
        assert_eq!(config.context_window_tokens, window);
        let allowance = config
            .tool_policy
            .as_ref()
            .expect("Tool budget")
            .max_context_bytes;
        assert!(
            allowance
                >= if window == Some(32_768) {
                    72 * 1024
                } else {
                    192 * 1024
                },
            "optional history cannot consume continuation room: {allowance}"
        );
        assert_successful_in_process_reads(run);
        assert_eq!(provider.requests.lock().expect("requests").len(), 3);
        assert_eq!(
            provider.compact_requests.lock().expect("compactions").len(),
            usize::from(compaction_expected),
            "crowded default and small windows compact the large read"
        );
        assert!(
            provider
                .compact_responses
                .lock()
                .expect("summaries")
                .is_empty()
        );
        if window != Some(272_000) {
            assert!(
                config.excluded_history_runs > 0,
                "bounded history selection is explicit"
            );
        }
        assert_eq!(
            config.history_run_ids.len() + config.excluded_history_runs as usize,
            6
        );
        assert_eq!(view.runs.len(), 7, "canonical history is preserved");
    }
}

#[tokio::test]
async fn primary_fallback_chain_pins_its_smallest_window_or_none() {
    let mut pins = Vec::new();
    for fallback in [Some(200_000), None] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "facts\n");
        let state_path = temp.path().join("state");
        let account = Uuid::now_v7();
        let primary = Arc::new(
            ScriptedProvider::new(vec![finish_with("Answered")])
                .with_chatgpt_account(account, [7; 32])
                .with_context_window(272_000),
        );
        let mut backup = ScriptedProvider::new(Vec::new())
            .with_chatgpt_account(account, [8; 32])
            .with_profile("chatgpt", "fallback-model");
        if let Some(window) = fallback {
            backup = backup.with_context_window(window);
        }
        let backup = Arc::new(backup);
        let bindings = arany::AdmittedProviders::from_roles(arany::Providers::all(vec![
            primary.clone() as Arc<dyn arany::Provider>,
            backup.clone() as Arc<dyn arany::Provider>,
        ]));
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut input = request(&workspace, "Answer directly");
        input.include_paths.clear();
        let outcome =
            super::engine_support::run_routed(&engine, bindings, input, RunControls::default())
                .await
                .expect("routed Run");
        assert_eq!(outcome.run.status, RunStatus::Finished);
        assert!(backup.requests.lock().unwrap().is_empty());
        engine.close().await.expect("shutdown");
        let (_, view) = replayed(&state_path, outcome.session_id).await;
        pins.push(window_pin(&view, 0));
    }
    assert_eq!(pins[0].0, Some(200_000), "the smallest window in the chain");
    assert_eq!(
        pins[1].0, None,
        "a fallback without metadata keeps the defaults"
    );
    // 0.9 x 200,000 tokens x 3 bytes against the 376 KiB default.
    assert_eq!(pins[0].1 - pins[1].1, 540_000 - 376 * 1024);
}

#[tokio::test]
async fn command_step_clears_old_results_with_a_small_context_allowance() {
    let temp = tempfile::tempdir().expect("temporary root");
    let page = format!("{}\n", "x".repeat(99)).repeat(125);
    let workspace = facts_workspace(temp.path(), &page);
    for name in ["big1.txt", "big2.txt"] {
        std::fs::write(workspace.join(name), "b".repeat(47 * 1024)).expect("include");
    }
    let state_path = temp.path().join("state");
    let command = arany::ToolCall::Command {
        program: "unconfigured".into(),
        args: Vec::new(),
        cwd: "notes".into(),
    };
    let provider = Arc::new(
        ScriptedProvider::new(
            (0..4)
                .map(|_| crate::tool_outcome(facts_read()))
                .chain([
                    crate::tool_outcome(command.clone()),
                    finish_with("Checked the Command refusal"),
                ])
                .collect(),
        )
        .with_compactions(vec![Err(ProviderError::Unavailable)]),
    );
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    let mut input = request(&workspace, "Inspect four pages then request a Command");
    input.include_paths = vec!["big1.txt".into(), "big2.txt".into()];
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
    .expect("Run with room for the complete Command step");
    engine.close().await.expect("shutdown");
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let run = &view.runs[0];
    assert_eq!(run.status, RunStatus::Finished);
    assert_eq!(run.failure, None);
    let allowance = run
        .config
        .as_ref()
        .unwrap()
        .tool_policy
        .as_ref()
        .unwrap()
        .max_context_bytes;
    assert!((270 * 1024..=274 * 1024).contains(&allowance));
    assert_eq!(run.tools.len(), 5);
    assert_eq!(run.tools[4].intent.call, command);
    assert_eq!(run.tools[4].intent.limits.result_bytes, 64 * 1024);
    assert_eq!(
        run.tools[4].observation.as_ref().unwrap().disposition,
        arany::ToolDisposition::Denied
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 6);
    assert!(requests.iter().all(|request| {
        !request.input.outcome_correction
            && request.input.previous_run_failure.is_none()
            && request.input.last_answer_refused.is_none()
    }));
    assert_eq!(provider.compact_requests.lock().unwrap().len(), 1);
    assert!(provider.compact_responses.lock().unwrap().is_empty());
    assert!(provider.responses.lock().unwrap().is_empty());
    let context = requests[4].input.tools.as_ref().unwrap();
    assert_eq!(context.observation_count(), 4);
    assert!(context.steps[0].cleared);
    let mut uncleared = context.clone();
    uncleared.steps[0].cleared = false;
    let used = arany::test_support::tool_context_bytes(&uncleared);
    assert!((48 * 1024..=60 * 1024).contains(&used));
    let reserve = arany::test_support::tool_continuation_reserve(requests[4].max_output_tokens, 0);
    let margin = 2 * 1024;
    assert!(used + reserve > allowance as usize + margin);
    assert!(
        arany::test_support::tool_context_bytes(context) + reserve + margin <= allowance as usize
    );
    let after_command = requests[5].input.tools.as_ref().unwrap();
    assert_eq!(
        after_command
            .steps
            .iter()
            .filter(|step| step.cleared)
            .count(),
        1
    );
    assert!(
        arany::test_support::tool_context_bytes(after_command) + reserve + margin
            <= allowance as usize,
        "the denied Command must leave continuation room on both platforms"
    );
    let clearing = events
        .iter()
        .position(|event| {
            matches!(
                event.event,
                Event::ToolResultsCleared {
                    covered_tool_count: 1,
                    ..
                }
            )
        })
        .expect("older result cleared");
    assert!(matches!(
        events[clearing + 1].event,
        Event::GoalCompactionCallReserved { .. }
    ));
    let reservation = events[clearing + 1..]
        .iter()
        .position(|event| matches!(event.event, Event::GoalProviderCallReserved { .. }))
        .expect("next primary reservation")
        + clearing
        + 1;
    assert!(events[..reservation].iter().all(|event| !matches!(
        &event.event, Event::ToolStarted { intent, .. } if intent.call == command
    )));
}

#[tokio::test]
async fn near_full_context_refuses_command_before_dispatch() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "original");
    std::fs::write(workspace.join("AGENTS.md"), "a".repeat(32 * 1024)).expect("instructions");
    for name in ["big1.txt", "big2.txt"] {
        std::fs::write(workspace.join(name), "b".repeat(127 * 1024)).expect("include");
    }
    let state_path = temp.path().join("state");
    let engine = facts_engine(StateRoot::admit(&state_path).expect("state"), &state_path);
    std::fs::write(
        state_path.join("tools.json"),
        br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#,
    )
    .expect("write grant");
    let command = arany::ToolCall::Command {
        program: "unconfigured".into(),
        args: Vec::new(),
        cwd: "".into(),
    };
    let provider = Arc::new(ScriptedProvider::new(vec![crate::tool_outcome(command)]));
    let mut input = request(&workspace, &"x".repeat(60 * 1024));
    input.include_paths = vec!["big1.txt".into(), "big2.txt".into()];
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
    .expect("context refusal");
    engine.close().await.expect("shutdown");
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    let run = &view.runs[0];
    assert_eq!(run.failure, Some(arany::RunFailure::ToolContextFull));
    assert!(run.tools.is_empty(), "no command intent was dispatched");
    assert!(events.iter().all(|event| !matches!(
        event.event,
        Event::ToolStarted { .. } | Event::ToolFinished { .. }
    )));
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/facts.txt")).expect("unchanged file"),
        "original"
    );
    assert_eq!(provider.requests.lock().expect("requests").len(), 1);
}

#[tokio::test]
async fn insufficient_clearing_summarizes_and_uses_remaining_capacity_before_context_full() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "");
    let state_path = temp.path().join("state");
    let provider = Arc::new(
        ScriptedProvider::new(
            (0..2000)
                .map(|_| crate::tool_outcome(facts_read()))
                .collect(),
        )
        .with_compactions(
            (0..2000)
                .map(|_| {
                    Ok(CompactionResponse {
                        summary: String::new(),
                        response_id: None,
                        usage: arany::Usage::default(),
                        wire_provenance: None,
                    })
                })
                .collect(),
        ),
    );
    let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
    let mut input = request(
        &workspace,
        "Inspect bounded empty pages until context is full",
    );
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
    .expect("honest context-full outcome");
    engine.close().await.unwrap();
    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(
        view.runs[0].failure,
        Some(arany::RunFailure::ToolContextFull)
    );
    assert!(
        view.runs[0]
            .tools
            .iter()
            .any(|tool| tool.intent.limits.result_bytes < 64 * 1024)
    );
    let clearing = events
        .iter()
        .position(|event| matches!(event.event, Event::ToolResultsCleared { .. }))
        .unwrap();
    assert!(
        matches!(events[clearing].event, Event::ToolResultsCleared { cleared_bytes, .. } if cleared_bytes < 64 * 1024)
    );
    let summary = events[clearing + 1..]
        .iter()
        .position(|event| matches!(event.event, Event::GoalCompactionCallReserved { .. }))
        .unwrap()
        + clearing
        + 1;
    assert!(
        events[clearing + 1..summary]
            .iter()
            .all(|event| !matches!(event.event, Event::GoalProviderCallReserved { .. }))
    );
    assert!(
        events[summary + 1..]
            .iter()
            .any(|event| matches!(event.event, Event::GoalContextCompacted { .. }))
    );
    assert!(!provider.compact_requests.lock().unwrap().is_empty());
    let goal = &view.goals[0];
    assert_eq!(
        goal.provider_calls as usize,
        provider.requests.lock().unwrap().len() + provider.compact_requests.lock().unwrap().len()
    );
}

#[tokio::test]
async fn skill_reads_continue_under_context_pressure_without_a_guard_executable() {
    use std::os::unix::fs::PermissionsExt;
    for compact in [false, true] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), &"Fact ".repeat(20 * 1024));
        let mut bodies = Vec::new();
        for index in 0..5 {
            let name = format!("skill-{index}");
            let directory = workspace.join(".agents/skills").join(&name);
            std::fs::create_dir_all(&directory).expect("Skill directory");
            let body = format!(
                "---\nname: {name}\ndescription: Guidance\n---\n{}",
                "instruction ".repeat(680)
            );
            std::fs::write(directory.join("SKILL.md"), &body).expect("Skill body");
            bodies.push(body);
        }
        std::fs::write(workspace.join("AGENTS.md"), "Guidance. ".repeat(1200)).expect("guidance");
        let state_path = temp.path().join("state");
        let state = StateRoot::admit(&state_path).expect("state");
        let config = state_path.join("tools.json");
        std::fs::write(&config, br#"{"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}"#).expect("config");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
            .expect("private config");
        let engine = engine_support::open(
            state,
            EngineConfig {
                guard_executable: Some(temp.path().join("missing-guard")),
                ..Default::default()
            },
        )
        .expect("engine");
        let provider = Arc::new(
            ScriptedProvider::new(
                (1..5)
                    .map(|index| {
                        tool_outcome(arany::ToolCall::Skill {
                            name: format!("skill-{index}"),
                            resource: None,
                        })
                    })
                    .chain([finish_with("Skills read")])
                    .collect(),
            )
            .with_max_output_tokens(if compact { 4096 } else { 16384 })
            .with_compactions(
                (0..8)
                    .map(|_| {
                        if compact {
                            Ok(CompactionResponse {
                                summary: "Earlier Skills were read; no scripts ran.".into(),
                                response_id: None,
                                usage: arany::Usage {
                                    input_tokens: Some(32),
                                    output_tokens: Some(12),
                                    ..Default::default()
                                },
                                wire_provenance: None,
                            })
                        } else {
                            Err(ProviderError::Rejected)
                        }
                    })
                    .collect(),
            ),
        );
        let mut input = request(&workspace, "Apply the selected Skill");
        input.skill = Some("skill-0".into());
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
        .expect("Skill Run");
        assert_eq!(outcome.run.status, RunStatus::Finished);
        assert_eq!(
            outcome.run.assistant_message.as_deref(),
            Some("Skills read")
        );
        engine.close().await.expect("close");
        let (events, view) = replayed(&state_path, outcome.session_id).await;
        let run = view.runs.last().expect("Run");
        assert_eq!(run.tools.len(), 5);
        for (tool, body) in run.tools.iter().zip(&bodies) {
            let observation = tool.observation.as_ref().expect("observation");
            assert_eq!(&observation.output, body);
            assert!(matches!(
                observation.attestation,
                Some(arany::Attestation::InProcess { .. })
            ));
        }
        assert_eq!(
            view.goals.last().expect("Goal").status,
            arany::GoalStatus::Completed
        );
        let requests = provider.requests.lock().expect("requests");
        assert_eq!(requests.len(), 5);
        let initial = requests[0].input.tools.as_ref().expect("Tools");
        assert_eq!(
            initial.invoked_skill.as_ref().expect("invoked").output,
            bodies[0]
        );
        assert!(!initial.catalog().contains("instruction instruction"));
        assert!(initial.steps.is_empty());
        assert!(provider.responses.lock().expect("responses").is_empty());
        if compact {
            let allowance = run
                .config
                .as_ref()
                .expect("config")
                .tool_policy
                .as_ref()
                .expect("Tool policy")
                .max_context_bytes;
            let reserve =
                arany::test_support::tool_continuation_reserve(requests[0].max_output_tokens, 0);
            assert!(
                reserve <= allowance as usize,
                "the compaction fixture must avoid the half-allowance fallback"
            );
            assert!(
                !provider
                    .compact_requests
                    .lock()
                    .expect("compactions")
                    .is_empty()
            );
            assert!(events.iter().any(|event| matches!(&event.event,
                Event::GoalContextCompacted { record, .. }
                    if matches!(record.status, CompactionStatus::Succeeded { .. }))));
        }
        assert_action_bounds(&events, false, false);
    }
}

#[tokio::test]
async fn empty_tool_steps_fail_as_provider_calls_without_refusal() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = facts_workspace(temp.path(), "bounded facts");
    let state_path = temp.path().join("state");
    let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
    let provider = Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Tools(
        Vec::new(),
    )]));
    let mut input = request(&workspace, "Read the facts");
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
    .expect("a malformed Tool response has a durable failed Run");
    engine.close().await.unwrap();
    let (_, view) = replayed(&state_path, outcome.session_id).await;
    let run = &view.runs[0];
    assert_eq!(*run, outcome.run);
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.failure, Some(arany::RunFailure::ProviderCall));
    assert!(run.tools.is_empty());
    assert!(run.assistant_message.is_none());
    let calls = &run.agents[0].provider_calls;
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].disposition,
        ProviderCallDisposition::InvalidResponse
    );
    assert!(calls[0].refusal.is_none());
    assert_eq!(calls[0].requested_tools, 0);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn multi_call_steps_commit_all_intents_and_refuse_whole_invalid_batches() {
    for (case, count, invalid, duplicate, window) in [
        ("four reads", 4, false, false, None),
        ("over cap", 5, false, false, None),
        ("invalid third call", 4, true, false, None),
        ("duplicate id", 4, false, true, None),
        ("context full", 4, false, false, Some(32_768)),
    ] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = facts_workspace(temp.path(), "bounded facts");
        let state_path = temp.path().join("state");
        let engine = facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
        let requests = (0..count)
            .map(|index| arany::ToolRequest {
                id: arany::ToolUseId::new(format!("call_{}", if duplicate { 0 } else { index }))
                    .unwrap(),
                call: if invalid && index == 2 {
                    arany::ToolCall::Read {
                        path: "../outside".into(),
                        offset: None,
                        limit: None,
                    }
                } else {
                    facts_read()
                },
            })
            .collect();
        let mut provider = ScriptedProvider::new(vec![
            ProviderOutcome::Tools(requests),
            finish_with("Read step settled"),
        ])
        .with_context_window_snapshots(vec![window]);
        if window.is_some() {
            provider = provider.with_chatgpt_account(Uuid::now_v7(), [7; 32]);
        }
        let provider = Arc::new(provider);
        let mut input = request(&workspace, "Read the facts in one step");
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
        .expect(case);
        engine.close().await.unwrap();
        let (events, view) = replayed(&state_path, outcome.session_id).await;
        assert_eq!(view.runs[0], outcome.run, "{case}: durable result");
        let starts: Vec<_> = events
            .iter()
            .filter(|event| matches!(event.event, Event::ToolStarted { .. }))
            .map(|event| event.sequence)
            .collect();
        let finishes: Vec<_> = events
            .iter()
            .filter(|event| matches!(event.event, Event::ToolFinished { .. }))
            .map(|event| event.sequence)
            .collect();
        if case == "four reads" {
            assert_eq!(starts.len(), 4);
            assert_eq!(finishes.len(), 4);
            let proposals: Vec<_> = events
                .iter()
                .filter(|event| matches!(event.event, Event::ToolProposed { .. }))
                .map(|event| event.sequence)
                .collect();
            assert_eq!(proposals.len(), 4);
            let reservations: Vec<_> = events
                .iter()
                .filter(|event| matches!(event.event, Event::ToolActiveTimeReserved { .. }))
                .collect();
            assert_eq!(reservations.len(), 4);
            assert!(
                arany::test_support::admissions::recorded(outcome.session_id)
                    .iter()
                    .all(|(count, _, _, _)| {
                        *count < proposals[0] as usize || *count >= proposals[3] as usize
                    }),
                "initial intents commit without a second admission boundary"
            );
            let oversized_slots: Vec<_> =
                arany::test_support::admissions::recorded(outcome.session_id)
                    .into_iter()
                    .filter(|(count, _, _, _)| *count + 1 == proposals[0] as usize)
                    .map(|(_, _, _, observations)| observations)
                    .collect();
            let per_call = oversized_slots.iter().min().expect("per-call admission");
            let batch = oversized_slots.iter().max().expect("batch admission");
            assert!(
                *batch >= count * per_call,
                "batch admission must retain every call's oversized payload allowance"
            );
            for (index, reservation) in reservations.iter().enumerate() {
                let intent_id = outcome.run.tools[index].intent.id;
                let finish = events
                    .iter()
                    .find(|event| matches!(&event.event,
                        Event::ToolFinished { observation, .. } if observation.intent.id == intent_id))
                    .unwrap();
                assert!(
                    proposals[3] < reservation.sequence && reservation.sequence < starts[index]
                );
                assert!(starts[index] < finish.sequence, "call {index}");
            }

            assert!(
                proposals[3] < starts[0],
                "every intent commits before dispatch"
            );
            assert_eq!(outcome.run.status, RunStatus::Finished);
            assert_eq!(outcome.run.agents[0].provider_calls[0].requested_tools, 4);
            let calls = provider.requests.lock().unwrap();
            let step = &calls[1].input.tools.as_ref().unwrap().steps[0];
            assert_eq!(step.requests.len(), 4);
            assert_eq!(step.observations.len(), 4);
            assert!(
                step.observations
                    .iter()
                    .all(|observed| observed.disposition == arany::ToolDisposition::Succeeded)
            );
        } else {
            assert!(
                starts.is_empty() && finishes.is_empty(),
                "{case}: nothing dispatched"
            );
            if window.is_some() {
                assert_eq!(
                    outcome.run.failure,
                    Some(arany::RunFailure::ToolContextFull)
                );
            } else {
                assert_eq!(outcome.run.status, RunStatus::Finished);
                let calls = provider.requests.lock().unwrap();
                let step = &calls[1].input.tools.as_ref().unwrap().steps[0];
                assert!(step.refusal.is_some());
                assert_eq!(step.refusal_index, invalid.then_some(2));
            }
        }
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            if window.is_some() { 1 } else { 2 }
        );
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
        let mut script = vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: vec![arany::ChildAssignment {
                    objective: "Read the facts".into(),
                    profile: None,
                    editor: None,
                }],
            }),
            batch(4),
        ];
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
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: vec![arany::ChildAssignment {
                    objective: "Read the facts".into(),
                    profile: None,
                    editor: None,
                }],
            }),
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

#[tokio::test]
async fn concurrent_reads_commit_in_completion_order_and_settle_before_effects() {
    for case in [
        "four reads",
        "read before effect",
        "cancel reads",
        "limit reads",
    ] {
        let _deadline = engine_support::deadline("concurrent read step", Duration::from_secs(180));
        let temp = tempfile::tempdir().unwrap();
        let workspace = facts_workspace(temp.path(), "first\nsecond\nthird\nfourth\n");
        let state = temp.path().join("state");
        let engine = facts_engine(StateRoot::admit(&state).unwrap(), &state);
        std::fs::write(state.join("tools.json"),
            br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
        let count = if case == "four reads" { 4 } else { 2 };
        let reads: Vec<_> = (1..=count)
            .map(|offset| arany::ToolCall::Read {
                path: "notes/facts.txt".into(),
                offset: Some(offset),
                limit: Some(1),
            })
            .collect();
        let mut calls = reads.clone();
        if count < 4 {
            calls.insert(
                0,
                arany::ToolCall::Mkdir {
                    path: "notes/created".into(),
                },
            );
            calls.push(arany::ToolCall::Mkdir {
                path: "notes/final".into(),
            });
        }
        let requests = calls
            .iter()
            .enumerate()
            .map(|(index, call)| arany::ToolRequest {
                id: arany::ToolUseId::new(format!("call_{index}")).unwrap(),
                call: call.clone(),
            })
            .collect();
        let provider = ScriptedProvider::new(vec![
            ProviderOutcome::Tools(requests),
            finish_with("Step settled"),
        ]);
        let recorded = Arc::clone(&provider.requests);
        let (send, receive) = tokio::sync::oneshot::channel();
        let send = Mutex::new(Some(send));
        let provider = Arc::new(provider.with_before_call(0, move || {
            let run = recorded.lock().unwrap()[0].run_id;
            let gates: Vec<_> = reads
                .iter()
                .map(|call| arany::FaultPlan::gate_in_process_read(run, call.clone()))
                .collect();
            send.lock().unwrap().take().unwrap().send(gates).unwrap();
        }));
        let cancellation = RunCancellation::new();
        let mut events = engine.subscribe();
        let mut input = request(&workspace, "Read the facts concurrently");
        input.include_paths.clear();
        let drive = async {
            let gates = receive.await.unwrap();
            let mut releases = Vec::new();
            for (entered, release) in gates {
                entered.await.unwrap();
                releases.push(Some(release));
            }
            assert!(
                !workspace.join("notes/created").exists(),
                "effect waits for every read"
            );
            if case == "cancel reads" {
                cancellation.cancel();
                return releases;
            }
            if case == "limit reads" {
                let mut started = 0;
                let mut runtime_ms = 0;
                while started < calls.len() {
                    let event = next_committed(&mut events, "read deadlines").await;
                    if let Event::ToolProposed { intent, .. } = &event.event {
                        started += 1;
                        runtime_ms = runtime_ms.max(intent.limits.runtime_ms);
                    }
                }
                tokio::time::pause();
                tokio::time::advance(Duration::from_millis(u64::from(runtime_ms) + 1)).await;
                tokio::time::resume();
                return releases;
            }
            let order = if count == 4 {
                vec![2, 0, 3, 1]
            } else {
                vec![1, 0]
            };
            for index in order {
                assert!(!workspace.join("notes/created").exists());
                releases[index].take().unwrap().send(()).unwrap();
                loop {
                    let event = next_committed(&mut events, "read completion").await;
                    if let Event::ToolFinished { observation, .. } = &event.event {
                        assert!(matches!(&observation.intent.call,
                            arany::ToolCall::Read { offset: Some(offset), .. } if *offset as usize == index + 1));
                        assert_eq!(observation.disposition, arany::ToolDisposition::Succeeded);
                        break;
                    }
                }
            }
            releases
        };
        let (outcome, _held_releases) = tokio::join!(
            run_with(
                &engine,
                &provider,
                input,
                RunControls {
                    tools: ToolGrant::Configured,
                    cancellation: cancellation.clone(),
                    ..Default::default()
                }
            ),
            drive
        );
        let outcome = outcome.unwrap();
        engine.close().await.unwrap();
        let (events, view) = replayed(&state, outcome.session_id).await;
        assert_eq!(view.runs[0], outcome.run, "{case}: closed replay");
        assert_eq!(outcome.run.tools.len(), 4);
        assert!(
            outcome
                .run
                .tools
                .iter()
                .all(|tool| tool.observation.is_some())
        );
        if matches!(case, "cancel reads" | "limit reads") {
            assert_eq!(
                outcome.run.status,
                if case == "cancel reads" {
                    RunStatus::Cancelled
                } else {
                    RunStatus::Failed
                }
            );
            assert_eq!(provider.requests.lock().unwrap().len(), 1);
            for tool in &outcome.run.tools {
                let observation = tool.observation.as_ref().unwrap();
                if matches!(tool.intent.call, arany::ToolCall::Mkdir { .. }) {
                    assert!(observation.output.starts_with("not_dispatched:"));
                } else {
                    assert_eq!(
                        observation.disposition,
                        if case == "cancel reads" {
                            arany::ToolDisposition::Cancelled
                        } else {
                            arany::ToolDisposition::Limit
                        }
                    );
                }
            }
            assert!(!workspace.join("notes/created").exists());
            assert!(!workspace.join("notes/final").exists());
        } else {
            assert_eq!(outcome.run.status, RunStatus::Finished);
            let recorded = provider.requests.lock().unwrap();
            let step = &recorded[1].input.tools.as_ref().unwrap().steps[0];
            assert_eq!(
                step.observations
                    .iter()
                    .map(|observation| &observation.intent.call)
                    .collect::<Vec<_>>(),
                calls.iter().collect::<Vec<_>>()
            );
            if count < 4 {
                assert!(workspace.join("notes/created").is_dir());
                assert!(workspace.join("notes/final").is_dir());
                let last_read = events.iter().filter(|event| matches!(&event.event,
                    Event::ToolFinished { observation, .. } if matches!(observation.intent.call, arany::ToolCall::Read { .. }))).map(|event| event.sequence).max().unwrap();
                assert!(events.iter().filter(|event| matches!(&event.event,
                    Event::ToolFinished { observation, .. } if matches!(observation.intent.call, arany::ToolCall::Mkdir { .. }))).all(|event| event.sequence > last_read));
            }
        }
    }
}

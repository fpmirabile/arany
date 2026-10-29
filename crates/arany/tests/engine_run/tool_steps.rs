use super::*;

#[tokio::test]
async fn failed_write_turn_stays_in_history_after_reopen() {
    use arany::test_support::goals::{finish_with, replayed};
    use std::os::unix::fs::PermissionsExt;

    for known_reason in [false, true] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state_path = temp.path().join("state");
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state"),
            EngineConfig {
                guard_executable: Some(std::env::current_exe().expect("test executable")),
                ..Default::default()
            },
        )
        .expect("engine");
        let config = state_path.join("tools.json");
        std::fs::write(&config, br#"{"version":1,"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]}"#)
            .expect("tool configuration");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
            .expect("private configuration");
        let refused_write = || {
            tool_outcome(arany::ToolCall::Write {
                path: String::new(),
                expected_digest: None,
                content: "hello".into(),
            })
        };
        let attempts = if known_reason { 2 } else { 3 };
        let provider = ScriptedProvider::new(
            (0..attempts)
                .map(|_| refused_write())
                .chain([finish_with(
                    "The previous Run failed before a Tool was dispatched.",
                )])
                .collect(),
        );
        if known_reason {
            for response in provider
                .responses
                .lock()
                .expect("responses")
                .iter_mut()
                .take(attempts)
            {
                *response = Err(ProviderError::InvalidOutcomeContract);
            }
        }
        let observed = provider.requests.clone();
        let provider = Arc::new(provider);
        let mut input = request(&workspace, "write test.txt");
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
        .expect("failed Write Run");
        assert_eq!(first.run.status, RunStatus::Failed);
        assert!(first.run.tools.is_empty());
        assert!(!workspace.join("test.txt").exists());
        engine.close().await.expect("shutdown before follow-up");
        let (events, view) = replayed(&state_path, first.session_id).await;
        assert_eq!(view.runs[0], first.run);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event, Event::ToolStarted { .. }))
        );
        let call = &view.runs[0].agents[0].provider_calls[0];
        assert_eq!(
            call.disposition,
            if known_reason {
                ProviderCallDisposition::InvalidResponse
            } else {
                ProviderCallDisposition::Rejected
            }
        );
        assert_eq!(
            call.failure_reason,
            Some(if known_reason {
                arany::ProviderFailureReason::OutcomeContract
            } else {
                arany::ProviderFailureReason::ProposalRefused
            })
        );

        let engine = engine_support::open(
            StateRoot::open_existing(&state_path).expect("state"),
            EngineConfig::default(),
        )
        .expect("reopened Engine");
        let mut follow_up = request(&workspace, "why couldn't you write it?");
        follow_up.include_paths.clear();
        follow_up.session_id = Some(first.session_id);
        follow_up.title = None;
        let second = run_to_end(&engine, &provider, follow_up)
            .await
            .expect("follow-up");
        assert_eq!(second.run.status, RunStatus::Finished);
        engine.close().await.expect("shutdown");
        let (_, view) = replayed(&state_path, first.session_id).await;
        assert_eq!(view.runs[1], second.run);
        let requests = observed.lock().expect("requests");
        assert_eq!(requests.len(), attempts + 1);
        assert!(provider.responses.lock().expect("responses").is_empty());
        let follow_up_request = &requests[attempts];
        let feedback = follow_up_request
            .input
            .previous_run_failure
            .expect("committed failure cause");
        assert_eq!(
            feedback.stop,
            arany::RunStop::Failed(first.run.failure.expect("failed Run"))
        );
        assert_eq!(feedback.provider_failure, call.failure_reason);
        assert_eq!(
            follow_up_request.input.objective,
            "why couldn't you write it?"
        );
        let [history] = follow_up_request.input.history.as_slice() else {
            panic!("one failed turn retained")
        };
        assert_eq!(history.user, "write test.txt");
        let fact: serde_json::Value =
            serde_json::from_str(&history.assistant).expect("typed history fact");
        assert_eq!(
            fact,
            serde_json::json!({
                "status": "failed",
                "fact": "Run failed before any primary model Tool",
                "failure": if known_reason { "provider_call" } else { "refusal_limit" },
                "primary_model_tool_execution": "nothing dispatched",
                "last_call": if known_reason {
                    serde_json::json!({"disposition": "invalid_response", "failure_reason": "outcome_contract", "refusal": null})
                } else {
                    serde_json::json!({"disposition": "rejected", "failure_reason": "proposal_refused", "refusal": {"operation": "arany_write", "cause": "invalid_path"}})
                },
            })
        );
        assert_eq!(
            second.run.config.as_ref().expect("config").history_run_ids,
            [first.run.id]
        );
    }
}

#[tokio::test]
async fn write_proposal_refusals_are_durable_bounded_and_spend_goal_calls() {
    use arany::{
        GoalBudget, GoalPlanContract, GoalSuccessCriterion, ProposalRefusalCause, RunFailure,
    };
    use std::os::unix::fs::PermissionsExt;

    for (plan_calls, cause, goal_calls, expected_calls, failure, alternating) in [
        (
            None,
            ProposalRefusalCause::PlanInvalid,
            Some(8),
            3,
            Some(RunFailure::RefusalLimit),
            false,
        ),
        (
            Some(1),
            ProposalRefusalCause::PlanBudgetSpent,
            Some(8),
            3,
            Some(RunFailure::RefusalLimit),
            false,
        ),
        (
            None,
            ProposalRefusalCause::PlanInvalid,
            Some(2),
            2,
            Some(RunFailure::BudgetExhausted),
            false,
        ),
        (
            None,
            ProposalRefusalCause::PlanInvalid,
            None,
            8,
            Some(RunFailure::RefusalLimit),
            true,
        ),
    ] {
        let temp = tempfile::tempdir().expect("fixture");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state_path = temp.path().join("state");
        let engine = engine_support::open(
            StateRoot::admit(&state_path).expect("state"),
            EngineConfig {
                guard_executable: Some(std::env::current_exe().expect("test executable")),
                ..Default::default()
            },
        )
        .expect("engine");
        let config = state_path.join("tools.json");
        std::fs::write(&config, br#"{"version":1,"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]}"#).expect("grant");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
            .expect("private grant");
        let plan = GoalPlanContract {
            mission: if plan_calls.is_some() {
                "write test.txt containing hello".into()
            } else {
                String::new()
            },
            resource_class: arany::ToolResourceClass::Default,
            command_timeout_ms: None,
            paths: vec!["PRIVATE_ARGUMENT_path".into(), "test.txt".into()],
            commands: Vec::new(),
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            success_criterion: GoalSuccessCriterion::WorkspaceFileDigest {
                path: "test.txt".into(),
                sha256: Sha256::digest(b"hello")
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            },
            budget: GoalBudget {
                max_provider_calls: plan_calls.or(Some(8)),
                ..GoalBudget::default()
            },
        };
        let provider = Arc::new(ScriptedProvider::new(
            (0..expected_calls)
                .map(|index| {
                    let mut plan = plan.clone();
                    if alternating && index % 2 == 1 {
                        plan.mission = "write test.txt containing hello".into();
                        plan.budget.max_provider_calls = Some(1);
                    }
                    arany::test_support::scenario::plan_outcome(plan)
                })
                .collect(),
        ));
        let mut input = request(&workspace, "write test.txt containing hello");
        input.include_paths.clear();
        input.goal.budget = Some(GoalBudget {
            max_provider_calls: goal_calls,
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
        .expect("refused Run");
        assert_eq!(
            provider.requests.lock().expect("requests").len(),
            expected_calls,
            "{cause:?}"
        );
        assert!(provider.responses.lock().expect("script").is_empty());
        engine.close().await.expect("close engine");
        let store =
            Store::open_read_only(StateRoot::open_existing(&state_path).expect("existing state"))
                .expect("reopen");
        let events = store.load_session(result.session_id).await.expect("events");
        let view = SessionView::replay(result.session_id, &events)
            .expect("strict replay")
            .expect("Session");
        let run = &view.runs[0];
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.failure, failure);
        assert_eq!(run.agents[0].provider_calls.len(), expected_calls);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.event, Event::GoalProviderCallReserved { .. }))
                .count(),
            expected_calls
        );
        {
            let requests = provider.requests.lock().expect("requests");
            assert!(requests[0].input.last_answer_refused.is_none());
            for (index, request) in requests[1..].iter().enumerate() {
                assert_eq!(
                    request
                        .input
                        .last_answer_refused
                        .as_ref()
                        .expect("repair note")
                        .cause,
                    if alternating && index % 2 == 1 {
                        ProposalRefusalCause::PlanBudgetSpent
                    } else {
                        cause
                    }
                );
            }
        }
        let call = &run.agents[0].provider_calls[0];
        assert_eq!(call.disposition, ProviderCallDisposition::Rejected);
        assert_eq!(
            call.failure_reason,
            Some(arany::ProviderFailureReason::ProposalRefused)
        );
        let refusal = call.refusal.as_ref().expect("closed refusal");
        assert_eq!(refusal.operation, "arany_plan");
        assert_eq!(refusal.cause, cause);
        assert_eq!(call.requested_tools, 0);
        assert!(!events.iter().any(|event| matches!(
            event.event,
            Event::ToolStarted { .. } | Event::GoalPlanContractRecorded { .. }
        )));
        assert!(run.assistant_message.is_none());
        assert!(!format!("{events:?}").contains("PRIVATE_ARGUMENT_"));
        assert!(!workspace.join("test.txt").exists());
        store.close().await.expect("close store");
        if plan_calls.is_none() && goal_calls == Some(8) {
            let crash_path = temp.path().join("crashed-state");
            let crash_store = Store::open(StateRoot::admit(&crash_path).expect("crash state"))
                .expect("crash store");
            for event in &events {
                crash_store
                    .append(result.session_id, event.event.clone())
                    .await
                    .expect("committed prefix");
                if matches!(event.event, Event::ProviderCallRecorded { .. }) {
                    break;
                }
            }
            crash_store.close().await.expect("crash boundary");
            let resumed = resume_session(
                StateRoot::open_existing(&crash_path).expect("crash state"),
                workspace.clone(),
                result.session_id,
            )
            .await
            .expect("resume after refusal");
            assert_eq!(resumed.runs[0].status, RunStatus::Interrupted);
            assert_eq!(
                resumed.runs[0].agents[0].provider_calls[0].refusal,
                call.refusal
            );
            let engine = engine_support::open(
                StateRoot::open_existing(&crash_path).expect("crash state"),
                EngineConfig::default(),
            )
            .expect("resume engine");
            let peer = Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Answer(
                Answer {
                    text: "No write dispatched".into(),
                },
            )]));
            let mut next = request(&workspace, "What happened?");
            next.include_paths.clear();
            next.session_id = Some(result.session_id);
            next.title = None;
            run_to_end(&engine, &peer, next)
                .await
                .expect("recovery Run");
            engine.close().await.expect("recovery shutdown");
            let requests = peer.requests.lock().expect("recovery requests");
            assert_eq!(requests.len(), 1);
            let history = &requests[0].input.history;
            assert_eq!(history.len(), 1);
            assert!(history[0].assistant.contains("plan_invalid"));
            assert!(history[0].assistant.contains("nothing dispatched"));
        }
    }
}

#[tokio::test]
async fn invoked_skill_denial_is_journaled_before_any_provider_call() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
    let skill = workspace.join(".agents/skills/ask-matt");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: ask-matt\ndescription: Guide\n---\nUntrusted guidance",
    )
    .unwrap();
    let state = temp.path().join("state");
    let engine = engine_support::open(
        StateRoot::admit(&state).unwrap(),
        EngineConfig {
            guard_executable: Some(std::env::current_exe().unwrap()),
            ..Default::default()
        },
    )
    .unwrap();
    for (file, bytes) in [
        ("tools.json", br#"{"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}"#.as_slice()),
        ("permissions.json", br#"{"permissions":{"deny":["Skill"]}}"#.as_slice()),
    ] {
        std::fs::write(state.join(file), bytes).unwrap();
        std::fs::set_permissions(state.join(file), std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let provider = Arc::new(ScriptedProvider::new(vec![
        arany::test_support::goals::finish_with("Must not be called"),
    ]));
    let mut input = request(&workspace, "do X");
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
    .unwrap();
    assert_eq!(outcome.run.status, RunStatus::Failed);
    assert_eq!(outcome.run.failure, Some(arany::RunFailure::ToolStopped));
    assert!(provider.requests.lock().unwrap().is_empty());
    engine.close().await.unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    let events = store.load_session(outcome.session_id).await.unwrap();
    let view = store.load_view(outcome.session_id).await.unwrap().unwrap();
    assert_eq!(view.runs[0].skill.as_deref(), Some("ask-matt"));
    assert_eq!(view.runs[0].tools.len(), 1);
    assert!(events.iter().any(|event| matches!(&event.event, Event::ToolFinished { observation, .. } if observation.disposition == arany::ToolDisposition::Denied)));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, Event::GoalProviderCallReserved { .. }))
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn line_paged_read_returns_all_eighty_lines_after_twenty_steps() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("agents")).unwrap();
    let content = (1..=80)
        .map(|n| format!("Testing rule {n}: {}\n", "x".repeat(125)))
        .collect::<String>();
    std::fs::write(workspace.join("agents/testing.md"), &content).unwrap();
    std::fs::write(
        workspace.join("earlier.txt"),
        format!("{}\n", "x".repeat(999)).repeat(8),
    )
    .unwrap();
    let state = temp.path().join("state");
    let engine = engine_support::open(
        StateRoot::admit(&state).unwrap(),
        EngineConfig {
            guard_executable: Some(std::env::current_exe().unwrap()),
            ..Default::default()
        },
    )
    .unwrap();
    let config = state.join("tools.json");
    std::fs::write(&config, br#"{"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
    std::fs::set_permissions(config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let read = |path: &str| {
        tool_outcome(arany::ToolCall::Read {
            path: path.into(),
            offset: None,
            limit: None,
        })
    };
    let provider = Arc::new(ScriptedProvider::new(
        (0..20)
            .map(|_| read("earlier.txt"))
            .chain([
                read("agents/testing.md"),
                arany::test_support::goals::finish_with("Read all 80 lines"),
            ])
            .collect(),
    ));
    let mut input = request(&workspace, "Inspect testing guidance after earlier steps");
    input.include_paths.clear();
    input.goal.budget = Some(arany::GoalBudget {
        max_provider_calls: Some(32),
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
    .unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);
    assert!(provider.responses.lock().unwrap().is_empty());
    let page = {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 22);
        let observations = requests
            .last()
            .unwrap()
            .input
            .tools
            .as_ref()
            .unwrap()
            .observations();
        let page = observations.last().unwrap().output.clone();
        let output: serde_json::Value = serde_json::from_str(&page).unwrap();
        assert_eq!(output["total_lines"], 80);
        assert_eq!(output["end_line"], 80);
        assert_eq!(output["next_line"], serde_json::Value::Null);
        assert_eq!(output["truncated"], false);
        assert_eq!(
            output["text"],
            content
                .lines()
                .enumerate()
                .map(|(i, line)| format!("{}\t{line}\n", i + 1))
                .collect::<String>()
        );
        page
    };
    engine.close().await.unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    let events = store.load_session(result.session_id).await.unwrap();
    let view = SessionView::replay(result.session_id, &events)
        .unwrap()
        .unwrap();
    assert_eq!(view.runs[0].tools.len(), 21);
    assert_eq!(
        view.runs[0].tools[20].observation.as_ref().unwrap().output,
        page
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn guard_failure_before_dispatch_returns_to_the_model_and_the_answer_commits() {
    use arany::ToolDisposition;
    for (inject, disposition, output) in [
        (
            arany::FaultPlan::fail_guard_before_dispatch as fn(&str),
            ToolDisposition::Failed,
            "Guard execution unavailable before dispatch; no operation was dispatched.",
        ),
        (
            arany::FaultPlan::limit_guard_before_dispatch,
            ToolDisposition::Limit,
            "Guard resource limit reached before dispatch; no operation was dispatched.",
        ),
    ] {
        guard_failure_before_dispatch_case(inject, disposition, output).await;
    }
}

async fn guard_failure_before_dispatch_case(
    inject: fn(&str),
    disposition: arany::ToolDisposition,
    output: &str,
) {
    use arany::test_support::goals::finish_with;
    use std::os::unix::fs::PermissionsExt;
    const PROGRAM: &str = "guard_probe";
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let engine = super::goals::facts_engine(StateRoot::admit(&state_path).unwrap(), &state_path);
    let executable = std::fs::canonicalize("/bin/sh").unwrap();
    let digest: String = Sha256::digest(std::fs::read(&executable).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let config = serde_json::json!({"version":1,"workspace_paths":["notes"],"write":false,
        "commands":[{"name":PROGRAM,"executable":executable,"sha256":digest,"interpreter":true,"inputs":[]}],"skills":[],"mcp":[]});
    let config_path = state_path.join("tools.json");
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let permissions = state_path.join("permissions.json");
    std::fs::write(&permissions, br#"{"permissions":{"allow":["Bash"]}}"#).unwrap();
    std::fs::set_permissions(&permissions, std::fs::Permissions::from_mode(0o600)).unwrap();
    inject(PROGRAM);
    let provider = Arc::new(ScriptedProvider::new(vec![
        crate::tool_outcome(arany::ToolCall::Command {
            program: PROGRAM.into(),
            args: vec!["-c".into(), "exit 0".into()],
            cwd: String::new(),
            resource_class: None,
            timeout_ms: None,
        }),
        finish_with("The command sandbox is unavailable; answered without it."),
    ]));
    let mut input = request(&workspace, "Run the check");
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
    .expect("answered Run");
    engine.close().await.expect("engine shutdown");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    assert_eq!(outcome.run.failure, None);
    assert_eq!(
        provider.requests.lock().expect("requests").len(),
        2,
        "the failure is delivered once to a fresh model decision"
    );
    let [tool] = outcome.run.tools.as_slice() else {
        panic!("the failed command is never retried");
    };
    let observation = tool.observation.as_ref().expect("observation");
    assert_eq!(observation.disposition, disposition);
    assert!(observation.attestation.is_none());
    assert!(
        observation.output.starts_with(output),
        "{}",
        observation.output
    );
}

fn facts_edit(expected_digest: Option<String>, old: &str, new: &str) -> arany::ToolCall {
    arany::ToolCall::Edit {
        path: "notes/facts.txt".into(),
        expected_digest,
        old: old.into(),
        new: new.into(),
    }
}

fn edit_engine(state_path: &Path) -> Engine {
    use std::os::unix::fs::PermissionsExt;
    let engine =
        super::goals::facts_engine(StateRoot::admit(state_path).expect("state"), state_path);
    for (name, bytes) in [
        (
            "tools.json",
            &br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#[..],
        ),
        (
            "permissions.json",
            &br#"{"permissions":{"defaultMode":"acceptEdits"}}"#[..],
        ),
    ] {
        let path = state_path.join(name);
        std::fs::write(&path, bytes).expect("settings");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
    }
    engine
}

fn edit_digests(view: &SessionView) -> Vec<Option<String>> {
    view.runs[0]
        .tools
        .iter()
        .filter_map(|tool| match &tool.intent.call {
            arany::ToolCall::Edit {
                expected_digest, ..
            } => Some(expected_digest.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn omitted_edit_digest_is_filled_from_the_latest_read_or_edit_and_refused_without_one() {
    use super::goals::{facts_digest, facts_read};
    use arany::test_support::goals::{finish_with, replayed};

    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let engine = edit_engine(&state_path);
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_outcome(facts_edit(None, "v1", "v2")),
        tool_outcome(facts_read()),
        tool_outcome(facts_edit(None, "v1", "v2")),
        tool_outcome(facts_edit(None, "v2", "v3")),
        tool_outcome(facts_edit(Some(facts_digest("stale")), "v3", "v4")),
        finish_with("Edited the facts"),
    ]));
    let mut input = request(&workspace, "Edit the facts");
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
    .expect("edit Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("shutdown");
    assert!(provider.responses.lock().expect("responses").is_empty());
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/facts.txt")).expect("facts"),
        "v3"
    );

    let (events, view) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(
        view.runs[0].agents[0].provider_calls[0].refusal,
        Some(arany::ProposalRefusal {
            operation: "arany_edit".into(),
            cause: arany::ProposalRefusalCause::DigestUnknown,
        }),
        "an unread file has no digest to fill"
    );
    assert_eq!(
        edit_digests(&view),
        [
            Some(facts_digest("v1")),
            Some(facts_digest("v2")),
            Some(facts_digest("stale")),
        ]
    );
    let dispositions: Vec<_> = view.runs[0]
        .tools
        .iter()
        .map(|tool| tool.observation.as_ref().expect("observation").disposition)
        .collect();
    assert_eq!(
        dispositions,
        [
            arany::ToolDisposition::Succeeded,
            arany::ToolDisposition::Succeeded,
            arany::ToolDisposition::Succeeded,
            arany::ToolDisposition::Conflict,
        ]
    );
    assert!(
        events.iter().all(|event| !matches!(
            &event.event,
            Event::ToolProposed { intent, .. } | Event::ToolStarted { intent, .. }
                if matches!(
                    intent.call,
                    arany::ToolCall::Edit {
                        expected_digest: None,
                        ..
                    }
                )
        )),
        "committed intents carry the concrete digest"
    );
}

#[tokio::test]
async fn omitted_edit_digest_uses_the_runs_explicit_include_snapshot() {
    use super::goals::facts_digest;
    use arany::test_support::goals::{finish_with, replayed};

    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = facts_workspace(temp.path(), "v1");
    let state_path = temp.path().join("state");
    let engine = edit_engine(&state_path);
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_outcome(facts_edit(None, "v1", "v2")),
        finish_with("Edited the included facts"),
    ]));
    let outcome = run_with(
        &engine,
        &provider,
        request(&workspace, "Edit the included facts"),
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .expect("edit Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("shutdown");
    assert_eq!(
        std::fs::read_to_string(workspace.join("notes/facts.txt")).expect("facts"),
        "v2"
    );
    let (_, view) = replayed(&state_path, outcome.session_id).await;
    assert_eq!(edit_digests(&view), [Some(facts_digest("v1"))]);
    assert!(view.runs[0].agents[0].provider_calls[0].refusal.is_none());
}

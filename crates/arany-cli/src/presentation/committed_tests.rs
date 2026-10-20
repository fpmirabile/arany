use super::*;
use arany::test_support::{
    engine::{open, run_routed, run_to_end, run_with},
    scenario::*,
};
use arany::{
    AgentPhase, CollaborationPolicy, Delegate, Engine, EngineConfig, Finish,
    ProviderCallDisposition, ProviderError, ProviderOutcome, Providers, RunCancellation,
    RunControls, RunHints, StateRoot, Store,
};
use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, Engine) {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).unwrap();
    std::fs::write(workspace.join("notes/facts.txt"), "Fact").unwrap();
    let state = StateRoot::admit(&root.path().join("state")).unwrap();
    let engine = open(state, EngineConfig::default()).unwrap();
    (root, workspace, engine)
}

async fn committed(root: &Path, session: arany::SessionId) -> (SessionView, Vec<EventEnvelope>) {
    let store =
        Store::open_read_only(StateRoot::open_existing(&root.join("state")).unwrap()).unwrap();
    let events = store.load_session(session).await.unwrap();
    let view = store.load_view(session).await.unwrap().unwrap();
    store.close().await.unwrap();
    (view, events)
}

fn finish(text: &str) -> ProviderOutcome {
    ProviderOutcome::Finish(Finish {
        summary: text.into(),
        result: text.into(),
    })
}

#[tokio::test]
async fn fork_title_uses_the_inherited_committed_prefix() {
    let (root, workspace, engine) = fixture();
    let provider = Arc::new(ScriptedProvider::new(vec![
        finish("first"),
        finish("later"),
    ]));
    let first = run_to_end(&engine, &provider, request(&workspace, "First question"))
        .await
        .unwrap();
    let fork = engine
        .fork_session(first.session_id, workspace.clone(), Some("Branch".into()))
        .await
        .unwrap();
    let nested_id = engine
        .fork_session(fork, workspace.clone(), None)
        .await
        .unwrap();
    let mut later = request(&workspace, "Later source question");
    later.session_id = Some(first.session_id);
    later.title = None;
    run_to_end(&engine, &provider, later).await.unwrap();
    engine.close().await.unwrap();
    let (nested, _) = committed(root.path(), nested_id).await;
    assert!(render_session(&nested, &[], Output::Text).contains("\nTitle: first\n"));
}

#[tokio::test]
async fn rejected_child_feedback_uses_the_committed_account_failure() {
    let (root, workspace, engine) = fixture();
    let provider = ScriptedProvider::new(vec![]);
    *provider.responses.lock().unwrap() = VecDeque::from([
        Ok(ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "B".into(),
                profile: None,
            }],
        })),
        Err(ProviderError::RemoteHttp(403)),
    ]);
    let mut input = request(&workspace, "Question");
    input.policy = CollaborationPolicy::Team {
        max_active_children: 1,
    };
    let run = run_to_end(&engine, &Arc::new(provider), input)
        .await
        .unwrap();
    engine.close().await.unwrap();
    let (replay, _) = committed(root.path(), run.session_id).await;
    assert!(
        run_feedback(&replay.runs[0])
            .unwrap()
            .contains("Provider denied account access.")
    );
}

#[tokio::test]
async fn completion_boundary_feedback_and_jsonl_use_the_committed_call_record() {
    for phase in [AgentPhase::RootPlan, AgentPhase::RootSynthesis] {
        for cancel in [true, false] {
            for (case, response, call_disposition, failure_reason) in completion_boundary_cases() {
                let (root, workspace, engine) = fixture();
                let cancellation = RunCancellation::new();
                let mut provider = ScriptedProvider::new(Vec::new());
                provider.cancel_at_completion = cancel.then(|| (phase, cancellation.clone()));
                let mut responses = VecDeque::new();
                let mut input = request(&workspace, "Completion boundary");
                if phase == AgentPhase::RootSynthesis {
                    input.policy = CollaborationPolicy::Team {
                        max_active_children: 1,
                    };
                    responses.push_back(Ok(ProviderOutcome::Delegate(Delegate {
                        agent: None,
                        children: vec![arany::ChildAssignment {
                            editor: None,
                            objective: "A".into(),
                            profile: None,
                        }],
                    })));
                    responses.push_back(Ok(ProviderOutcome::Finish(Finish {
                        summary: "child done".into(),
                        result: "Child result".into(),
                    })));
                }
                responses.push_back(response);
                provider.responses = Mutex::new(responses);
                let outcome = tokio::time::timeout(
                    Duration::from_secs(5),
                    run_with(
                        &engine,
                        &Arc::new(provider),
                        input,
                        RunControls {
                            cancellation,
                            ..RunControls::default()
                        },
                    ),
                )
                .await
                .unwrap()
                .unwrap();
                engine.close().await.unwrap();
                let (replay, events) = committed(root.path(), outcome.session_id).await;
                if !cancel && call_disposition != ProviderCallDisposition::Finished {
                    let feedback = run_feedback(&replay.runs[0]).expect("failure guidance");
                    for (reason, text) in [
                        (
                            arany::ProviderFailureReason::StreamProtocol,
                            "Provider stream was incomplete or malformed.",
                        ),
                        (
                            arany::ProviderFailureReason::ResponseContract,
                            "Provider response did not match the selected model, completed-message, or usage contract.",
                        ),
                        (
                            arany::ProviderFailureReason::OutcomeContract,
                            "Provider output did not match Arany's structured outcome contract.",
                        ),
                        (
                            arany::ProviderFailureReason::LocalOutputLimit,
                            "Provider reported output above Arany's local token limit.",
                        ),
                    ] {
                        if failure_reason == Some(reason) {
                            assert!(feedback.contains(text), "{case}: {feedback}");
                            assert!(feedback.contains("No answer was accepted"));
                        }
                    }
                }
                let jsonl = render_session(&replay, &events, Output::Jsonl);
                assert!(!jsonl.contains("OMITTED_ERROR_CANARY"));
                for (line, envelope) in jsonl.lines().zip(&events) {
                    if let Event::ProviderCallRecorded { record, .. } = &envelope.event {
                        let document: serde_json::Value =
                            serde_json::from_str(line).expect("canonical JSONL");
                        assert_eq!(document["event_version"], 10);
                        assert_eq!(document["payload"], serde_json::to_value(record).unwrap());
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn cancelled_preview_jsonl_includes_committed_checkpoints_and_commentary() {
    let (root, workspace, engine) = fixture();
    let provider = Arc::new(PreviewProvider {
        text: arany::PreviewText {
            answer: format!("INCOMPLETE_CANARY:{}", "x".repeat(2300)),
            commentary: vec!["PUBLIC_COMMENTARY_CANARY".into()],
            commentary_limited: false,
        },
        completion: None,
    });
    let cancellation = RunCancellation::new();
    let cancel = cancellation.clone();
    let mut events = engine.subscribe();
    let observed = RunHints::with_previews();
    let task = tokio::spawn(async move {
        let run = run_with(
            &engine,
            &provider,
            request(&workspace, "question"),
            RunControls {
                cancellation,
                hints: Some(observed),
                ..RunControls::default()
            },
        )
        .await
        .unwrap();
        engine.close().await.unwrap();
        run
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let Some(arany::Tapped::Committed(event)) = events.next().await else { panic!("committed checkpoint"); };
            if matches!(&event.event, Event::AssistantDisplayRecorded { chunk, .. } if chunk.category == arany::DisplayCategory::Answer) {
                break;
            }
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    let outcome = task.await.unwrap();
    let (view, events) = committed(root.path(), outcome.session_id).await;
    let export = render_session(&view, &events, Output::Jsonl);
    assert!(export.contains("AssistantDisplayRecorded"));
    assert!(export.contains("PUBLIC_COMMENTARY_CANARY"));
}

#[tokio::test]
async fn fallback_feedback_names_the_committed_replacement_model() {
    let (root, workspace, engine) = fixture();
    let primary = ScriptedProvider::new(vec![]).with_profile("scripted", "primary-model");
    *primary.responses.lock().unwrap() = VecDeque::from([Err(ProviderError::RemoteHttp(503))]);
    let fallback = Arc::new(
        ScriptedProvider::new(vec![finish("fallback answer")])
            .with_profile("scripted", "fallback-model"),
    );
    let bindings = arany::AdmittedProviders::from_roles(Providers::all(vec![
        Arc::new(primary) as Arc<dyn arany::Provider>,
        fallback as Arc<dyn arany::Provider>,
    ]));
    let run = run_routed(
        &engine,
        bindings,
        request(&workspace, "Complete the task"),
        RunControls::default(),
    )
    .await
    .unwrap();
    engine.close().await.unwrap();
    let (view, _) = committed(root.path(), run.session_id).await;
    assert!(
        render_provider_fallbacks(&view.runs[0])
            .unwrap()
            .contains("fallback-model")
    );
}

#[tokio::test]
async fn refused_write_proposals_report_the_engine_cause_without_naming_the_provider() {
    use arany::{GoalBudget, GoalPlanContract, GoalSuccessCriterion, ProposalRefusalCause};
    use std::os::unix::fs::PermissionsExt;

    for (plan_calls, cause, expected) in [
        (
            None,
            ProposalRefusalCause::PlanMissing,
            "Arany refused the operation because its required plan was missing.",
        ),
        (
            Some(1),
            ProposalRefusalCause::PlanBudgetSpent,
            "Arany refused a plan budget already spent by this Run.",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state_path = root.path().join("state");
        let engine = open(
            StateRoot::admit(&state_path).unwrap(),
            EngineConfig {
                guard_executable: Some(std::env::current_exe().unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        let config = state_path.join("tools.json");
        std::fs::write(&config, br#"{"version":1,"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        let plan = plan_calls.map(|calls| {
            Box::new(GoalPlanContract {
                mission: "write test.txt containing hello".into(),
                resource_class: arany::ToolResourceClass::Default,
                command_timeout_ms: None,
                paths: vec!["test.txt".into()],
                commands: Vec::new(),
                skills: Vec::new(),
                mcp_tools: Vec::new(),
                success_criterion: GoalSuccessCriterion::WorkspaceFileDigest {
                    path: "test.txt".into(),
                    sha256: "0".repeat(64),
                },
                budget: GoalBudget {
                    max_provider_calls: Some(calls),
                    ..GoalBudget::default()
                },
            })
        });
        let provider = Arc::new(ScriptedProvider::new(vec![tool_outcome(
            arany::ToolCall::Write {
                path: "test.txt".into(),
                content: "hello".into(),
                expected_digest: None,
            },
            plan,
        )]));
        let mut input = request(&workspace, "write test.txt containing hello");
        input.include_paths.clear();
        let run = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: arany::ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        engine.close().await.unwrap();
        let (replay, _) = committed(root.path(), run.session_id).await;
        let call = &replay.runs[0].agents[0].provider_calls[0];
        assert_eq!(call.refusal.as_ref().map(|r| r.cause), Some(cause));
        let copy = run_feedback(&replay.runs[0]).unwrap();
        assert!(copy.contains(expected), "{copy}");
        assert!(!copy.contains("Provider"), "{copy}");
    }
}

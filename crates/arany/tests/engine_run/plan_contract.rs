use super::goals::{
    AUTONOMOUS_OBJECTIVE, autonomous_input, facts_digest, facts_engine, facts_plan, facts_read,
};
use super::recovery::interrupted_autonomous_goal;
use super::team::child_tool_starts;
use super::*;
use arany::test_support::goals::*;

#[tokio::test]
async fn command_resources_refuse_invalid_requests_and_plan_widening_before_dispatch() {
    use arany::{ProposalRefusalCause as Cause, ToolResourceClass as Class};
    let invalid = Cause::InvalidArguments;
    let scope = Cause::PlanScopeExcludesCall;
    let default_plan = Some((Class::Default, None));
    let build_plan = Some((Class::Build, Some(5_000)));
    for (plan, timeout, class, jobs, cores, cause) in [
        (None, 999, Class::Default, Some(1), Some(1), invalid),
        (None, 600_001, Class::Default, Some(1), Some(1), invalid),
        (None, 1_000, Class::Build, Some(u32::MAX), Some(1), invalid),
        (None, 1_000, Class::Build, Some(1), Some(u32::MAX), invalid),
        (default_plan, 1_000, Class::Build, Some(1), Some(1), scope),
        (
            default_plan,
            120_001,
            Class::Default,
            Some(1),
            Some(1),
            scope,
        ),
        (build_plan, 5_001, Class::Default, Some(1), Some(1), scope),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = facts_workspace(temp.path(), "v1");
        let state_path = temp.path().join("state");
        let engine = command_resource_engine(&state_path);
        let call = arany::ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 0".into()],
            cwd: String::new(),
            resource_class: Some(class),
            timeout_ms: Some(timeout),
        };
        let mut steps = Vec::new();
        if let Some((resource_class, command_timeout_ms)) = plan {
            let mut plan = facts_plan(facts_digest("v1"));
            plan.commands = vec!["sh".into()];
            plan.resource_class = resource_class;
            plan.command_timeout_ms = command_timeout_ms;
            steps.push(arany::test_support::scenario::plan_outcome(plan));
        }
        steps.extend([
            crate::tool_outcome(call),
            crate::tool_outcome(facts_read()),
            finish_with("done"),
        ]);
        let provider = Arc::new(ScriptedProvider::new(steps));
        let mut input = request(&workspace, "Read after a refused command");
        input.include_paths.clear();
        input.goal.resource_settings.build_jobs = jobs;
        input.goal.resource_settings.build_cpu_cores = cores;
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
        let (events, view) = replayed(&state_path, outcome.session_id).await;
        assert_eq!(view.runs[0].status, RunStatus::Finished);
        assert_eq!(view.runs[0].tools.len(), 1);
        assert_eq!(view.runs[0].tools[0].intent.call, facts_read());
        assert!(events.iter().any(|event| matches!(&event.event,
            Event::ProviderCallRecorded { record, .. }
                if record.refusal.as_ref().is_some_and(|refusal| refusal.cause == cause)
        )));
    }
}

fn command_resource_engine(state_path: &Path) -> Engine {
    use std::os::unix::fs::PermissionsExt;
    let engine = facts_engine(StateRoot::admit(state_path).unwrap(), state_path);
    let executable = std::fs::canonicalize("/bin/sh").unwrap();
    let digest: String = Sha256::digest(std::fs::read(&executable).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let config = serde_json::json!({"version":1,"workspace_paths":["notes"],"write":false,
        "commands":[{"name":"sh","executable":executable,"sha256":digest,"interpreter":true,"inputs":[]}],"skills":[],"mcp":[]});
    let config_path = state_path.join("tools.json");
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    engine
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
    .with_active_time_per_call(Duration::from_secs(1));
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
            ProviderCallDisposition::Rejected,
            ProviderCallDisposition::Rejected,
            ProviderCallDisposition::Rejected,
        ],
        "each unnarrowable plan call is refused and never recorded"
    );
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
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: 100,
        }),
        delegate(&[("Inspect notes", None)]),
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
    let input = team_request(&workspace, "Check the facts");
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
    const READS: usize = 400;

    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
    let content = format!("{}\n", "x".repeat(31)).repeat(4 * READS);
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

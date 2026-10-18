use super::*;
use arany::permissions::{Decision, PermissionSource};
use arany::test_support::goals::assert_action_bounds;
use arany_cli::presentation::{Output, render_session};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;

fn private_file(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn definition() -> Value {
    json!({"name":"echo","inputSchema":{"type":"object","properties":{},"additionalProperties":false}})
}

fn call(kind: &str) -> arany::ToolCall {
    match kind {
        "command" => arany::ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), "printf done".into()],
            cwd: "".into(),
        },
        "mcp" => arany::ToolCall::McpCall {
            server: "server".into(),
            tool: "echo".into(),
            schema_digest: hash(&serde_json::to_vec(&definition()).unwrap()),
            arguments: "{}".into(),
        },
        "edit" => arany::ToolCall::Write {
            path: "notes/new.txt".into(),
            expected_digest: None,
            content: "done".into(),
        },
        "catalog" => arany::ToolCall::McpList {
            server: "server".into(),
        },
        _ => arany::ToolCall::Read {
            path: "notes/facts.txt".into(),
            offset: 0,
            limit: 32,
        },
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    workspace: std::path::PathBuf,
    state: std::path::PathBuf,
    engine: Engine,
    deny_humans: bool,
    command_maximum: Option<u32>,
    settings: Option<Value>,
}

impl Fixture {
    fn new(settings: Option<&Value>, write: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).unwrap();
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").unwrap();
        let state = temp.path().join("state");
        let root = StateRoot::admit(&state).unwrap();
        let script = temp.path().join("server.sh");
        let source = format!(
            "IFS= read -r line\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{{\"tools\":{{}}}}}}}}'\nIFS= read -r line\nIFS= read -r line\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"tools\":[{}]}}}}'\nIFS= read -r line\nprintf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"done\"}}]}}}}'\nIFS= read -r line\n",
            definition()
        );
        private_file(&script, source.as_bytes());
        let shell = std::fs::canonicalize("/bin/sh").unwrap();
        let program = json!({"name":"sh","executable":shell,"sha256":hash(&std::fs::read(&shell).unwrap()),"interpreter":true,"inputs":[]});
        let mut server_program = program.clone();
        server_program["name"] = json!("server");
        server_program["inputs"] =
            json!([{"path":script,"destination":"server.sh","sha256":hash(source.as_bytes())}]);
        private_file(&state.join("tools.json"), &serde_json::to_vec(&json!({
            "version":1,"workspace_paths":["notes"],"write":write,"commands":[program],"skills":[],
            "mcp":[{"name":"server","program":server_program,"args":["/inputs/server/server.sh"],"tools":["echo"]}]
        })).unwrap());
        if let Some(settings) = settings {
            private_file(
                &state.join("permissions.json"),
                &serde_json::to_vec(settings).unwrap(),
            );
        }
        let engine = engine_support::open(
            root,
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .unwrap();
        Self {
            _temp: temp,
            workspace,
            state,
            engine,
            deny_humans: false,
            command_maximum: None,
            settings: settings.cloned(),
        }
    }

    async fn run(
        &self,
        outcomes: Vec<ProviderOutcome>,
        mode: Option<arany::ApprovalMode>,
        team: bool,
    ) -> (arany::RunOutcome, Vec<ProviderRequest>, usize) {
        self.run_budgeted(outcomes, mode, team, budget()).await
    }

    async fn run_budgeted(
        &self,
        outcomes: Vec<ProviderOutcome>,
        mode: Option<arany::ApprovalMode>,
        team: bool,
        goal_budget: arany::GoalBudget,
    ) -> (arany::RunOutcome, Vec<ProviderRequest>, usize) {
        let provider = Arc::new(ScriptedProvider::new(outcomes));
        let (approvals, mut inbox) = arany::ToolApprovals::new(mode.unwrap_or_default());
        let controls = RunControls {
            tools: ToolGrant::Configured,
            approvals: mode.map(|_| approvals),
            ..Default::default()
        };
        let mut input = request(&self.workspace, "Inspect and complete the task");
        input.include_paths.clear();
        input.policy = if team {
            CollaborationPolicy::Auto {
                max_active_children: 1,
            }
        } else {
            CollaborationPolicy::Single
        };
        input.goal.budget = Some(goal_budget);
        if let Some(maximum) = self.command_maximum {
            input.goal.resource_settings.default_command_timeout_ms = maximum;
            input.goal.resource_settings.max_command_timeout_ms = maximum;
        }
        let running = run_with(&self.engine, &provider, input, controls);
        tokio::pin!(running);
        let mut humans = 0;
        let result = loop {
            tokio::select! {
                biased;
                result = &mut running => break result.unwrap(),
                approval = inbox.next(), if mode.is_some() => {
                    humans += 1;
                    assert_eq!(humans, 1, "only the proposed Tool may ask");
                    approval.unwrap().decide(!self.deny_humans);
                }
            }
        };
        let requests = provider.requests.lock().unwrap().clone();
        let unconsumed = provider.responses.lock().unwrap().len();
        assert!(
            unconsumed == 0,
            "all expected calls consumed: {unconsumed} left, fixture settings {:?}, run {:?}, tools {:?}",
            self.settings,
            result.run.status,
            result
                .run
                .tools
                .iter()
                .chain(result.run.agents.iter().flat_map(|agent| &agent.tools))
                .map(|tool| (
                    serde_json::to_value(&tool.intent.call).unwrap()["operation"].clone(),
                    tool.observation
                        .as_ref()
                        .map(|observation| observation.attestation.is_some()),
                    tool_report(tool.observation.as_ref())
                ))
                .collect::<Vec<_>>()
        );
        (result, requests, humans)
    }

    async fn reopen(self, session: SessionId) -> (Vec<arany::EventEnvelope>, SessionView) {
        self.engine.close().await.unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&self.state).unwrap()).unwrap();
        let events = store.load_session(session).await.unwrap();
        let view = SessionView::replay(session, &events).unwrap().unwrap();
        assert_action_bounds(&events, false, false);
        store.close().await.unwrap();
        (events, view)
    }
}

fn budget() -> arany::GoalBudget {
    arany::GoalBudget {
        max_provider_calls: Some(16),
        max_tokens: None,
        max_active_ms: Some(30 * 60 * 1000),
        max_runs: 1,
    }
}

fn plan(call: &arany::ToolCall) -> arany::GoalPlanContract {
    arany::GoalPlanContract {
        mission: "Complete the bounded Tool action".into(),
        paths: vec!["notes".into()],
        commands: vec!["sh".into()],
        resource_class: arany::ToolResourceClass::Default,
        command_timeout_ms: None,
        skills: vec![],
        mcp_tools: vec![arany::GoalMcpTool {
            server: "server".into(),
            tool: "echo".into(),
        }],
        success_criterion: arany::GoalSuccessCriterion::ToolCallSucceeded {
            call: Box::new(call.clone()),
        },
        budget: budget(),
    }
}

fn finish(result: &str) -> ProviderOutcome {
    ProviderOutcome::Finish(Finish {
        summary: "done".into(),
        result: result.into(),
    })
}

#[tokio::test]
async fn permission_dispatch_modes_rules_command_mcp_and_edit() {
    for mode in ["default", "acceptEdits", "plan", "auto", "dontAsk"] {
        for rule in ["allow", "ask", "deny"] {
            for (kind, pattern) in [
                ("command", "Bash"),
                ("mcp", "mcp__server__echo"),
                ("edit", "Edit"),
            ] {
                let settings = json!({"permissions":{"defaultMode":mode,(rule):[pattern]}});
                let fixture = Fixture::new(Some(&settings), true);
                let call = call(kind);
                let denied = mode == "plan" || rule == "deny" || mode == "dontAsk" && rule == "ask";
                let ai = !denied && mode == "auto" && rule == "allow";
                let human = !denied && (rule == "ask" || mode == "acceptEdits" && kind != "edit");
                let mut outcomes = vec![tool_outcome(call.clone(), Some(Box::new(plan(&call))))];
                if ai {
                    outcomes.push(finish("approve"));
                }
                outcomes.push(finish("answer"));
                let (result, requests, humans) = fixture
                    .run(
                        outcomes,
                        human.then_some(arany::ApprovalMode::Request),
                        false,
                    )
                    .await;
                let case = format!("{mode}/{rule}/{kind}");
                assert_eq!(humans, usize::from(human), "{case}");
                assert_eq!(
                    requests
                        .iter()
                        .filter(|request| request.phase == AgentPhase::ToolReview)
                        .count(),
                    usize::from(ai),
                    "{case}"
                );
                assert_eq!(
                    fixture.workspace.join("notes/new.txt").exists(),
                    kind == "edit" && !denied,
                    "{case}"
                );
                let (events, view) = fixture.reopen(result.session_id).await;
                let tool = &view.runs[0].tools[0];
                let verdict = tool.permission_verdict.as_ref().unwrap();
                assert_eq!(verdict.approved, !denied, "{case}");
                assert_eq!(
                    verdict.source,
                    if ai {
                        PermissionSource::AiReview
                    } else if human {
                        PermissionSource::Human
                    } else if mode == "plan" || mode == "dontAsk" && rule == "ask" {
                        PermissionSource::Mode
                    } else {
                        PermissionSource::Rule
                    },
                    "{case}"
                );
                let observation = tool.observation.as_ref().unwrap();
                assert_eq!(
                    observation.disposition,
                    if denied {
                        arany::ToolDisposition::Denied
                    } else {
                        arany::ToolDisposition::Succeeded
                    },
                    "{case}"
                );
                assert_eq!(observation.attestation.is_some(), !denied, "{case}");
                let started = events
                    .iter()
                    .position(|event| matches!(event.event, Event::ToolStarted { .. }))
                    .unwrap();
                let decided = events
                    .iter()
                    .position(|event| matches!(event.event, Event::ToolPermissionDecided { .. }))
                    .unwrap();
                let finished = events
                    .iter()
                    .position(|event| matches!(event.event, Event::ToolFinished { .. }))
                    .unwrap();
                assert!(started < decided && decided < finished, "{case}");
            }
        }
    }
}

#[tokio::test]
async fn permission_dispatch_settings_and_terminal_modes_only_narrow() {
    for (settings, kind, terminal, human, ai, denied) in [
        (
            json!({"permissions":{"defaultMode":"acceptEdits"}}),
            "edit",
            arany::ApprovalMode::Request,
            true,
            false,
            false,
        ),
        (
            json!({"permissions":{"allow":["Bash(sh -c true)"]}}),
            "command",
            arany::ApprovalMode::Request,
            true,
            false,
            false,
        ),
        (
            json!({"permissions":{"defaultMode":"auto"}}),
            "edit",
            arany::ApprovalMode::Request,
            true,
            false,
            false,
        ),
        (
            json!({"permissions":{"ask":["Edit"]}}),
            "edit",
            arany::ApprovalMode::Auto,
            true,
            false,
            false,
        ),
        (
            json!({"permissions":{"allow":["Edit"]}}),
            "edit",
            arany::ApprovalMode::Auto,
            false,
            true,
            false,
        ),
        (
            json!({"permissions":{"deny":["Edit"]}}),
            "edit",
            arany::ApprovalMode::Auto,
            false,
            false,
            true,
        ),
    ] {
        let fixture = Fixture::new(Some(&settings), true);
        let mut call = call(kind);
        if let arany::ToolCall::Command { args, .. } = &mut call {
            *args = vec!["-c".into(), "true".into()];
        }
        if settings["permissions"]["allow"].is_array() {
            assert_eq!(
                arany::permissions::PermissionConfig::parse(
                    &serde_json::to_vec(&settings).unwrap()
                )
                .unwrap()
                .decision(&call),
                Decision::Allow
            );
        }
        let mut outcomes = vec![tool_outcome(call.clone(), Some(Box::new(plan(&call))))];
        if ai {
            outcomes.push(finish("approve"));
        }
        outcomes.push(finish("answer"));
        let (result, requests, humans) = fixture.run(outcomes, Some(terminal), false).await;
        assert_eq!(humans, usize::from(human), "{settings}/{terminal:?}");
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.phase == AgentPhase::ToolReview)
                .count(),
            usize::from(ai)
        );
        let (_, view) = fixture.reopen(result.session_id).await;
        let verdict = view.runs[0].tools[0].permission_verdict.as_ref().unwrap();
        assert_eq!(
            verdict.decision,
            if denied {
                Decision::Deny
            } else {
                Decision::Ask
            }
        );
        assert_eq!(verdict.approved, !denied);
        assert_eq!(
            verdict.basis,
            if denied || settings["permissions"]["ask"].is_array() {
                PermissionSource::Rule
            } else {
                PermissionSource::Mode
            }
        );
        assert_eq!(
            verdict.source,
            if denied {
                PermissionSource::Rule
            } else if human {
                PermissionSource::Human
            } else {
                PermissionSource::AiReview
            }
        );
        assert_eq!(
            view.runs[0].tools[0]
                .observation
                .as_ref()
                .unwrap()
                .disposition,
            if denied {
                arany::ToolDisposition::Denied
            } else {
                arany::ToolDisposition::Succeeded
            }
        );
    }
}

#[tokio::test]
async fn permission_dispatch_denied_replay_uses_pinned_verdict_and_safe_output() {
    let settings = json!({"permissions":{"deny":["Read(./notes/**)"]},"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"must never execute"}]}]}});
    let fixture = Fixture::new(Some(&settings), true);
    let call = call("read");
    let (result, requests, humans) = fixture
        .run(
            vec![tool_outcome(call, None), finish("answer")],
            Some(arany::ApprovalMode::Auto),
            false,
        )
        .await;
    assert_eq!(humans, 0);
    assert_eq!(
        requests[1].input.tools.as_ref().unwrap().steps[0].observations[0].output,
        "Permission deny rule blocked this Tool"
    );
    let digest = Sha256::digest(serde_json::to_vec(&settings).unwrap());
    private_file(
        &fixture.state.join("permissions.json"),
        br#"{"permissions":{"allow":["Read"]}}"#,
    );
    let (events, view) = fixture.reopen(result.session_id).await;
    let tool = &view.runs[0].tools[0];
    assert_eq!(
        tool.permission_verdict.as_ref().unwrap().decision,
        Decision::Deny
    );
    assert_eq!(
        view.runs[0]
            .config
            .as_ref()
            .unwrap()
            .tool_policy
            .as_ref()
            .unwrap()
            .permissions_digest
            .unwrap()
            .as_slice(),
        digest.as_slice()
    );
    let text = render_session(&view, &events, Output::Text);
    assert!(text.contains("Permission deny rule blocked this Tool"));
    assert!(!text.contains("Read(./notes/**)"));
    let without_verdict: Vec<_> = events
        .iter()
        .filter(|event| !matches!(event.event, Event::ToolPermissionDecided { .. }))
        .cloned()
        .collect();
    assert!(SessionView::replay(result.session_id, &without_verdict).is_err());
    let mut forged = events.clone();
    for event in &mut forged {
        if let Event::ToolFinished { observation, .. } = &mut event.event {
            observation.disposition = arany::ToolDisposition::Succeeded;
        }
    }
    assert!(SessionView::replay(result.session_id, &forged).is_err());
}

#[tokio::test]
async fn permission_dispatch_over_maximum_command_timeout_limits_before_deny() {
    for settings in [
        json!({"permissions":{"defaultMode":"plan"}}),
        json!({"permissions":{"deny":["Bash"]}}),
    ] {
        let mut fixture = Fixture::new(Some(&settings), true);
        fixture.command_maximum = Some(1_000);
        let call = call("command");
        let mut plan = plan(&call);
        plan.command_timeout_ms = Some(2_000);
        let (result, requests, humans) = fixture
            .run(
                vec![tool_outcome(call, Some(Box::new(plan))), finish("answer")],
                Some(arany::ApprovalMode::Request),
                false,
            )
            .await;
        assert_eq!(humans, 0, "{settings}");
        assert_eq!(
            requests[1].input.tools.as_ref().unwrap().steps[0].observations[0].output,
            "Timeout 2000 ms exceeds pinned max 1000 ms. Lower request or change /settings; Goal remains pinned.",
            "{settings}"
        );
        let (_, view) = fixture.reopen(result.session_id).await;
        assert_eq!(view.runs[0].status, arany::RunStatus::Failed, "{settings}");
        let tool = &view.runs[0].tools[0];
        let verdict = tool.permission_verdict.as_ref().unwrap();
        assert_eq!(verdict.decision, Decision::Deny, "{settings}");
        assert!(!verdict.approved, "{settings}");
        assert_eq!(
            tool.observation.as_ref().unwrap().disposition,
            arany::ToolDisposition::Limit,
            "{settings}"
        );
    }
}

#[tokio::test]
async fn guarded_command_over_its_timeout_with_a_background_child_is_a_limit_and_reaps_it() {
    let marker = format!("30.{}", std::process::id());
    let sleepers = || async {
        let listing = tokio::process::Command::new("ps")
            .args(["-A", "-o", "command="])
            .output()
            .await
            .expect("process listing");
        String::from_utf8_lossy(&listing.stdout)
            .lines()
            .filter(|line| line.trim() == format!("sleep {marker}"))
            .count()
    };
    let mut fixture = Fixture::new(None, true);
    // The intent expires one runtime after issue, and macOS storage preparation must fit before it.
    fixture.command_maximum = Some(10_000);
    let call = arany::ToolCall::Command {
        program: "sh".into(),
        args: vec!["-c".into(), format!("sleep {marker} & sleep {marker}")],
        cwd: "".into(),
    };
    let mut plan = plan(&call);
    plan.command_timeout_ms = Some(10_000);
    let observe_both = async {
        for _ in 0..300 {
            if sleepers().await == 2 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    };
    let ((result, _, _), started) = tokio::join!(
        fixture.run(
            vec![tool_outcome(call, Some(Box::new(plan))), finish("answer")],
            Some(arany::ApprovalMode::Request),
            false,
        ),
        observe_both
    );
    assert!(
        started,
        "the foreground and background children both started"
    );
    let tool = &result.run.tools[0];
    let observation = tool.observation.as_ref().unwrap();
    assert_eq!(
        (observation.disposition, observation.output.as_str()),
        (
            arany::ToolDisposition::Limit,
            "Tool result or resource limit reached"
        )
    );
    assert!(matches!(
        observation.attestation,
        Some(arany::Attestation::Guard(_))
    ));
    // An exited descendant may linger briefly before its new parent collects it.
    let mut reaped = false;
    for _ in 0..20 {
        reaped = sleepers().await == 0;
        if reaped {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(reaped, "the background child outlived the Guard teardown");
}

#[tokio::test]
async fn permission_dispatch_missing_settings_preserves_existing_approval_flow() {
    for mode in [
        None,
        Some(arany::ApprovalMode::AutoEdits),
        Some(arany::ApprovalMode::Request),
        Some(arany::ApprovalMode::Auto),
    ] {
        let fixture = Fixture::new(None, true);
        private_file(
            &fixture.workspace.join("permissions.json"),
            br#"{"permissions":{"deny":["Edit"]}}"#,
        );
        let call = call("edit");
        let mut outcomes = vec![tool_outcome(call.clone(), Some(Box::new(plan(&call))))];
        if mode == Some(arany::ApprovalMode::Auto) {
            outcomes.push(finish("approve"));
        }
        outcomes.push(finish("answer"));
        let (result, _, humans) = fixture.run(outcomes, mode, false).await;
        assert_eq!(
            humans,
            usize::from(mode == Some(arany::ApprovalMode::Request))
        );
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("notes/new.txt")).unwrap(),
            "done"
        );
        let (events, view) = fixture.reopen(result.session_id).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event, Event::ToolPermissionDecided { .. }))
        );
        assert_eq!(
            view.runs[0]
                .config
                .as_ref()
                .unwrap()
                .tool_policy
                .as_ref()
                .unwrap()
                .permissions_digest,
            None
        );
    }
}

#[tokio::test]
async fn permission_dispatch_children_inherit_parent_restrictions_and_review() {
    for (settings, kind, denied, ai, human) in [
        (
            json!({"permissions":{"allow":["Read"],"deny":["Read(./notes/**)"]}}),
            "read",
            true,
            false,
            false,
        ),
        (
            json!({"permissions":{"defaultMode":"auto","allow":["mcp__server"]}}),
            "catalog",
            false,
            true,
            false,
        ),
        (
            json!({"permissions":{"defaultMode":"auto","ask":["Read"]}}),
            "read",
            false,
            false,
            true,
        ),
    ] {
        let fixture = Fixture::new(Some(&settings), false);
        let mut outcomes = vec![
            ProviderOutcome::Delegate(Delegate {
                children: vec!["Inspect the facts".into()],
            }),
            tool_outcome(call(kind), None),
        ];
        if ai {
            outcomes.push(finish("approve"));
        }
        outcomes.extend([finish("report"), finish("answer")]);
        let (result, requests, humans) = fixture
            .run(outcomes, Some(arany::ApprovalMode::Auto), true)
            .await;
        assert_eq!(humans, usize::from(human));
        let (events, view) = fixture.reopen(result.session_id).await;
        let child = &view.runs[0].agents[1];
        assert_eq!(child.grant, Some(arany::ChildGrant::ReadOnly));
        assert_eq!(
            child.tools[0].observation.as_ref().unwrap().disposition,
            if denied {
                arany::ToolDisposition::Denied
            } else {
                arany::ToolDisposition::Succeeded
            }
        );
        assert_eq!(
            child.tools[0].permission_verdict.as_ref().unwrap().approved,
            !denied
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.phase == AgentPhase::ToolReview)
                .count(),
            usize::from(ai)
        );
        assert!(events.iter().any(|event| matches!(&event.event, Event::ToolPermissionDecided { agent_run_id, .. } if *agent_run_id == child.id)));
    }
}

#[tokio::test]
async fn researcher_review_approval_dispatches_within_a_short_goal_budget() {
    let fixture = Fixture::new(
        Some(&json!({"permissions":{"defaultMode":"auto","allow":["mcp__server"]}})),
        false,
    );
    let short = arany::GoalBudget {
        max_active_ms: Some(10 * 60 * 1000),
        ..budget()
    };
    let (result, _, humans) = fixture
        .run_budgeted(
            vec![
                ProviderOutcome::Delegate(Delegate {
                    children: vec!["Inspect the facts".into()],
                }),
                tool_outcome(call("catalog"), None),
                finish("approve"),
                finish("report"),
                finish("answer"),
            ],
            Some(arany::ApprovalMode::Auto),
            true,
            short,
        )
        .await;
    assert_eq!(humans, 0);
    assert_eq!(result.run.status, RunStatus::Finished);
    let (events, view) = fixture.reopen(result.session_id).await;
    let child = &view.runs[0].agents[1];
    let verdict = child.tools[0].permission_verdict.as_ref().unwrap();
    assert!(verdict.approved);
    assert_eq!(verdict.source, PermissionSource::AiReview);
    assert_eq!(
        child.tools[0].observation.as_ref().unwrap().disposition,
        arany::ToolDisposition::Succeeded
    );
    let reservations: Vec<u64> = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::GoalProviderCallReserved {
                agent_run_id,
                active_ceiling_ms,
                ..
            } if *agent_run_id == child.id => Some(*active_ceiling_ms),
            _ => None,
        })
        .collect();
    assert!(reservations.len() >= 2, "child call and review reserved");
    assert!(
        reservations[1] <= reservations[0],
        "the review's crash ceiling adds no Tool action time: {reservations:?}"
    );
}

#[tokio::test]
async fn researcher_review_503_falls_back_and_the_run_completes() {
    use arany::test_support::engine::run_routed;
    use arany::test_support::routing::{input, route, routes};
    let fixture = Fixture::new(
        Some(&json!({"permissions":{"defaultMode":"auto","allow":["mcp__server"]}})),
        false,
    );
    let primary = route(
        ScriptedProvider::new(vec![
            ProviderOutcome::Delegate(Delegate {
                children: vec!["Inspect the facts".into()],
            }),
            finish("answer"),
        ])
        .with_profile("scripted", "primary"),
    );
    let child = route(
        ScriptedProvider::new(vec![tool_outcome(call("catalog"), None), finish("report")])
            .with_profile("scripted", "child"),
    );
    let mut review = ScriptedProvider::new(vec![]).with_profile("scripted", "review");
    review.responses = Mutex::new(vec![Err(ProviderError::RemoteHttp(503))].into());
    let review = route(review);
    let fallback = route(
        ScriptedProvider::new(vec![finish("approve")]).with_profile("scripted", "review-fallback"),
    );
    let mut bindings = routes(&primary);
    bindings.child = vec![child.clone()];
    bindings.review = vec![review.clone(), fallback.clone()];
    let (approvals, _inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Auto);
    let mut request = input(&fixture.workspace);
    request.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    request.goal.budget = Some(budget());
    let result = run_routed(
        &fixture.engine,
        bindings,
        request,
        RunControls {
            tools: ToolGrant::Configured,
            approvals: Some(approvals),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(result.run.status, RunStatus::Finished);
    let (_events, view) = fixture.reopen(result.session_id).await;
    let child_id = view.runs[0].agents[1].id;
    let switches = &view.runs[0].provider_fallbacks;
    assert_eq!(switches.len(), 1);
    assert_eq!(switches[0].0, child_id);
    assert_eq!(switches[0].1.role, arany::ProviderRole::Review);
    assert_eq!(fallback.requests.lock().unwrap().len(), 1);
    assert_eq!(
        view.runs[0].agents[1].tools[0]
            .observation
            .as_ref()
            .unwrap()
            .disposition,
        arany::ToolDisposition::Succeeded
    );
}

#[tokio::test]
async fn permission_dispatch_allow_never_expands_the_pinned_grant() {
    let fixture = Fixture::new(Some(&json!({"permissions":{"allow":["Edit"]}})), false);
    let call = call("edit");
    // A file-content criterion permits observing the denied action without granting writes.
    let mut contract = plan(&call);
    contract.success_criterion = arany::GoalSuccessCriterion::WorkspaceFileContent {
        path: "notes/new.txt".into(),
        content: "done".into(),
    };
    let (result, _, humans) = fixture
        .run(
            vec![
                tool_outcome(call, Some(Box::new(contract))),
                finish("answer"),
            ],
            Some(arany::ApprovalMode::Auto),
            false,
        )
        .await;
    assert_eq!(humans, 0);
    assert!(!fixture.workspace.join("notes/new.txt").exists());
    let (_, view) = fixture.reopen(result.session_id).await;
    let verdict = view.runs[0].tools[0].permission_verdict.as_ref().unwrap();
    assert_eq!(verdict.source, PermissionSource::PinnedGrant);
    assert_eq!(verdict.decision, Decision::Deny);
}

#[tokio::test]
async fn permission_dispatch_headless_auto_reviews_and_unanswered_asks_deny() {
    for (mode, explicit_ask, review, approved) in [
        ("default", false, None, false),
        ("auto", false, Some("approve"), true),
        ("auto", false, Some("ask"), false),
        ("auto", true, None, false),
    ] {
        let mut settings = json!({"permissions":{"defaultMode":mode}});
        if explicit_ask {
            settings["permissions"]["ask"] = json!(["Edit"]);
        }
        let fixture = Fixture::new(Some(&settings), true);
        let call = call("edit");
        let mut outcomes = vec![tool_outcome(call.clone(), Some(Box::new(plan(&call))))];
        if let Some(review) = review {
            outcomes.push(finish(review));
        }
        if approved {
            outcomes.push(finish("answer"));
        }
        let (result, requests, humans) = fixture.run(outcomes, None, false).await;
        assert_eq!(humans, 0);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.phase == AgentPhase::ToolReview)
                .count(),
            usize::from(review.is_some())
        );
        assert_eq!(fixture.workspace.join("notes/new.txt").exists(), approved);
        let (_, view) = fixture.reopen(result.session_id).await;
        let verdict = view.runs[0].tools[0].permission_verdict.as_ref().unwrap();
        assert_eq!(verdict.approved, approved);
        assert_eq!(
            verdict.source,
            if approved {
                PermissionSource::AiReview
            } else {
                PermissionSource::UnavailableApproval
            }
        );
    }
    for terminal in [None, Some(arany::ApprovalMode::Auto)] {
        let fixture = Fixture::new(Some(&json!({"permissions":{"defaultMode":"auto"}})), true);
        let call = call("edit");
        let mut outcomes = vec![
            ProviderOutcome::Delegate(Delegate {
                children: vec!["Inspect the task".into()],
            }),
            finish("report"),
            tool_outcome(call.clone(), Some(Box::new(plan(&call)))),
        ];
        if terminal.is_some() {
            outcomes.push(finish("answer"));
        }
        let (result, requests, humans) = fixture.run(outcomes, terminal, true).await;
        assert_eq!(humans, usize::from(terminal.is_some()));
        assert_eq!(requests[2].phase, AgentPhase::RootSynthesis);
        assert!(
            !requests
                .iter()
                .any(|request| request.phase == AgentPhase::ToolReview)
        );
        let (_, view) = fixture.reopen(result.session_id).await;
        let verdict = view.runs[0].tools[0].permission_verdict.as_ref().unwrap();
        assert_eq!(verdict.approved, terminal.is_some());
        assert_eq!(
            verdict.source,
            if terminal.is_some() {
                PermissionSource::Human
            } else {
                PermissionSource::UnavailableApproval
            }
        );
    }
}

#[tokio::test]
async fn human_denial_on_the_only_allowed_run_is_a_failure_not_a_budget_close() {
    let mut fixture = Fixture::new(None, true);
    fixture.deny_humans = true;
    let call = call("edit");
    let (result, _, humans) = fixture
        .run(
            vec![tool_outcome(call.clone(), Some(Box::new(plan(&call))))],
            Some(arany::ApprovalMode::Request),
            false,
        )
        .await;
    assert_eq!(humans, 1);
    assert_eq!(result.run.status, RunStatus::Failed);
    let (_, view) = fixture.reopen(result.session_id).await;
    let goal = view.goal(result.goal_id).unwrap();
    assert_eq!(goal.status, arany::GoalStatus::Failed);
    assert!(!goal.budget_closed);
    assert!(view.continuable_goal().is_none());
}

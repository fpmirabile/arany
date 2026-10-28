use super::worker;
use super::*;
use arany::permissions::{Decision, PermissionSource};
use arany::test_support::goals::assert_action_bounds;
use arany_cli::presentation::{Output, render_session};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;

#[path = "permissions/environment.rs"]
mod environment;
#[path = "permissions/hooks.rs"]
mod hooks;
#[path = "permissions/host.rs"]
mod host;
#[path = "permissions/modes.rs"]
mod modes;
#[path = "permissions/rules.rs"]
mod rules;

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
            offset: None,
            limit: Some(32),
        },
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    workspace: std::path::PathBuf,
    state: std::path::PathBuf,
    engine: Engine,
    deny_humans: bool,
    refuse_start: bool,
    restricted_approvals: bool,
    command_maximum: Option<u32>,
    max_children: u8,
    expected_unconsumed: usize,
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
            refuse_start: false,
            restricted_approvals: false,
            command_maximum: None,
            max_children: 1,
            expected_unconsumed: 0,
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
        self.run_provider(provider, mode, team, goal_budget).await
    }

    async fn run_provider(
        &self,
        provider: Arc<ScriptedProvider>,
        mode: Option<arany::ApprovalMode>,
        team: bool,
        goal_budget: arany::GoalBudget,
    ) -> (arany::RunOutcome, Vec<ProviderRequest>, usize) {
        let _deadline = engine_support::deadline("permission dispatch", Duration::from_secs(180));
        let (approvals, mut inbox) = arany::ToolApprovals::new(mode.unwrap_or_default());
        let approvals = if self.restricted_approvals {
            approvals.without_human_only()
        } else {
            approvals
        };
        let controls = RunControls {
            tools: ToolGrant::Configured,
            approvals: mode.map(|_| approvals),
            ..Default::default()
        };
        let mut input = request(&self.workspace, "Inspect and complete the task");
        input.include_paths.clear();
        input.policy = if team {
            CollaborationPolicy::Auto {
                max_active_children: self.max_children,
            }
        } else {
            CollaborationPolicy::Single
        };
        input.goal.budget = Some(goal_budget);
        if let Some(maximum) = self.command_maximum {
            input.goal.resource_settings.default_command_timeout_ms = maximum;
            input.goal.resource_settings.max_command_timeout_ms = maximum;
        }
        let mut injected = if self.refuse_start {
            let session = self
                .engine
                .create_session(self.workspace.clone(), None)
                .await
                .unwrap();
            input.session_id = Some(session);
            input.title = None;
            Some(arany::FaultPlan::corrupt_tool_preflight(
                session,
                "ToolStarted",
            ))
        } else {
            None
        };
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
                    let approval = approval.unwrap();
                    if !team {
                        tokio::time::pause();
                        tokio::time::advance(Duration::from_secs(3600)).await;
                        tokio::time::resume();
                    }
                    approval.decide(!self.deny_humans);
                }
            }
        };
        if let Some(injected) = &mut injected {
            injected.try_recv().expect("final-start preflight reached");
        }
        let requests = provider.requests.lock().unwrap().clone();
        let unconsumed = provider.responses.lock().unwrap().len();
        assert!(
            unconsumed == self.expected_unconsumed,
            "all expected calls consumed: {unconsumed} left, fixture settings {:?}, run {:?}, tools (operation, attested, outcome, hook outcomes) {:?}",
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
                    tool_report(tool.observation.as_ref()),
                    tool.hooks
                        .iter()
                        .filter_map(|hook| hook.outcome.as_ref())
                        .map(|outcome| tool_report(Some(&outcome.observation)))
                        .collect::<Vec<_>>()
                ))
                .collect::<Vec<_>>()
        );
        (result, requests, humans)
    }

    #[cfg(target_os = "linux")]
    async fn run_uncertain(&self, outcomes: Vec<ProviderOutcome>, children: bool) -> usize {
        let provider = Arc::new(ScriptedProvider::new(outcomes));
        let mut input = request(&self.workspace, "Inspect and complete the task");
        input.include_paths.clear();
        input.policy = if children {
            CollaborationPolicy::Auto {
                max_active_children: self.max_children,
            }
        } else {
            CollaborationPolicy::Single
        };
        input.goal.budget = Some(budget());
        let controls = RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        };
        let outcome = run_with(&self.engine, &provider, input, controls).await;
        assert!(
            matches!(
                outcome,
                Err(arany::EngineError::Tool(arany::ToolError::Uncertain))
            ),
            "{outcome:?}"
        );
        provider.requests.lock().unwrap().len()
    }

    async fn reopen(self, session: SessionId) -> (Vec<arany::EventEnvelope>, SessionView) {
        reopen_closed(self.engine, &self.state, session).await
    }
}

fn plan(call: &arany::ToolCall) -> arany::GoalPlanContract {
    arany::GoalPlanContract {
        mcp_tools: vec![arany::GoalMcpTool {
            server: "server".into(),
            tool: "echo".into(),
        }],
        ..super::plan(call)
    }
}

fn verdict(text: &str) -> ProviderOutcome {
    ProviderOutcome::Review(match text {
        "approve" => arany::ReviewVerdict::Approve,
        "ask" => arany::ReviewVerdict::Ask,
        other => panic!("unknown review verdict {other}"),
    })
}

fn finish(result: &str) -> ProviderOutcome {
    ProviderOutcome::Answer(Answer {
        text: result.into(),
    })
}

fn shell_hook(script: &str) -> Value {
    json!({"type": "command", "command": script})
}

fn agent_file(fixture: &Fixture, filename: &str, fields: &str, body: &str) {
    let agents = fixture.state.join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::set_permissions(&agents, std::fs::Permissions::from_mode(0o700)).unwrap();
    private_file(
        &agents.join(filename),
        format!("---\nname: reader\ndescription: Inspect the assignment\n{fields}---\n{body}")
            .as_bytes(),
    );
}

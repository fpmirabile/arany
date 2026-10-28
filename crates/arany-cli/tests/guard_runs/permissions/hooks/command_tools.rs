use super::*;
use std::os::unix::fs::MetadataExt;

struct CommandFixture {
    state: StateRoot,
    workspace: std::path::PathBuf,
    executable: std::path::PathBuf,
    tool: arany::CommandTool,
}

impl CommandFixture {
    fn new(root: &Path, failure: Option<arany::RewriteStage>) -> Self {
        let workspace = arany::test_support::goals::facts_workspace(root, "Fact");
        let directory = root.join("programs");
        std::fs::create_dir(&directory).unwrap();
        let executable = directory.join("filter");
        std::fs::write(&executable, hook_source(failure)).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let state = StateRoot::admit(&root.join("state")).unwrap();
        let folder = workspace.metadata().unwrap();
        let programs = directory.metadata().unwrap();
        state
            .record(arany::RecordKind::WorkspacePermissions)
            .replace(
                &serde_json::to_vec(&json!({"version":2,"workspaces":[{
                    "path":workspace,"device":folder.dev(),"inode":folder.ino(),"mode":"request",
                    "runtime_grants":[{"kind":"user_programs","manager":"user_directory","version":null,
                        "path":directory,"device":programs.dev(),"inode":programs.ino()}]
                }]}))
                .unwrap(),
            )
            .unwrap();
        state
            .record(arany::RecordKind::Permissions)
            .replace(br#"{"permissions":{"defaultMode":"default","allow":["Bash"]}}"#)
            .unwrap();
        let tool =
            arany::CommandTool::pin(&state, &workspace, executable.to_str().unwrap(), Vec::new())
                .unwrap();
        arany::CommandTools::edit(&state, |record| record.select(tool.clone())).unwrap();
        Self {
            state,
            workspace,
            executable,
            tool,
        }
    }

    async fn probe_and_enable(&self, engine: &Engine) -> arany::SessionId {
        let session = engine
            .create_session(self.workspace.clone(), None)
            .await
            .unwrap();
        let probe = engine
            .probe_command_tool(session, &self.workspace, &self.tool, RunCancellation::new())
            .await
            .unwrap();
        assert!(matches!(
            probe.outcome.unwrap().result,
            arany::permissions::HookResult::Narrowed { .. }
        ));
        assert!(!arany::CommandTools::load(&self.state).unwrap().entries()[0].enabled);
        let mut enabled = self.tool.clone();
        enabled.last_probe = Some(arany::RewriteStage::Rewritten);
        enabled.enabled = true;
        arany::CommandTools::edit(&self.state, |record| record.replace(0, &self.tool, enabled))
            .unwrap();
        session
    }
}

#[tokio::test]
async fn selected_rewrite_is_probed_before_enable_and_binds_guard_host_and_replay() {
    for (host, failure) in [
        (false, None),
        (true, None),
        (false, Some(arany::RewriteStage::NonZeroExit)),
        (false, Some(arany::RewriteStage::InvalidOutput)),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let fixture = CommandFixture::new(temp.path(), failure);
        let engine = engine_support::open(
            StateRoot::open_existing(fixture.state.path()).unwrap(),
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .unwrap();
        let session = fixture.probe_and_enable(&engine).await;
        let expected_text = if failure.is_some() {
            "original"
        } else {
            "rewritten"
        };
        let rewritten = command(host, &format!("printf {expected_text}"));
        let outcome = run_rewrite(&engine, &fixture, session, &rewritten).await;
        assert_eq!(outcome.run.status, RunStatus::Finished);
        let executed = outcome
            .run
            .tools
            .iter()
            .find(|tool| !tool.intent.rewrite.is_empty())
            .unwrap();
        assert_eq!(executed.intent.call, rewritten);
        assert_eq!(
            executed.intent.rewrite[0].stage,
            failure.unwrap_or(arany::RewriteStage::Rewritten)
        );
        assert!(
            executed
                .observation
                .as_ref()
                .unwrap()
                .output
                .contains(expected_text)
        );
        assert_eq!(
            executed.permission_verdict.as_ref().unwrap().source,
            if host {
                PermissionSource::Human
            } else {
                PermissionSource::Rule
            }
        );
        engine.close().await.unwrap();
        assert_rewrite_replay(fixture, session).await;
    }
}

async fn run_rewrite(
    engine: &Engine,
    fixture: &CommandFixture,
    session: arany::SessionId,
    rewritten: &arany::ToolCall,
) -> arany::RunOutcome {
    let host = rewritten.is_host();
    let provider = Arc::new(ScriptedProvider::new(vec![
        plan_outcome(arany::GoalPlanContract {
            mission: "Run the configured command rewrite".into(),
            paths: Vec::new(),
            commands: vec!["sh".into()],
            resource_class: arany::ToolResourceClass::Default,
            command_timeout_ms: None,
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            success_criterion: arany::GoalSuccessCriterion::ToolCallSucceeded {
                call: Box::new(rewritten.clone()),
            },
            budget: arany::GoalBudget::default(),
        }),
        tool_outcome(command(host, "printf original")),
        finish("Observed the command result"),
    ]));
    let mut input = request(&fixture.workspace, "Run the command tool");
    input.session_id = Some(session);
    input.title = None;
    input.include_paths.clear();
    input.goal.phase = Some(arany::GoalPhase::Implementation);
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    let controls = RunControls {
        tools: ToolGrant::Trusted(
            arany::WorkspacePermissions::load(&fixture.workspace, &fixture.state).unwrap(),
        ),
        approvals: Some(approvals),
        ..Default::default()
    };
    let run = run_with(engine, &provider, input, controls);
    let approve = async {
        if host {
            let approval = tokio::time::timeout(Duration::from_secs(60), inbox.next())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&approval.intent.call, rewritten);
            assert!(!approval.intent.rewrite.is_empty());
            assert!(arany::permissions::HostPrefixRule::from_intent(&approval.intent, 1).is_err());
            approval.decide(true);
        }
    };
    let (outcome, ()) = tokio::time::timeout(Duration::from_secs(180), async {
        tokio::join!(run, approve)
    })
    .await
    .unwrap();
    outcome.unwrap()
}

async fn assert_rewrite_replay(fixture: CommandFixture, session: arany::SessionId) {
    std::fs::write(&fixture.executable, "#!/bin/sh\nexit 99\n").unwrap();
    fixture
        .state
        .record(arany::RecordKind::CommandTools)
        .remove()
        .unwrap();
    let store = Store::open(fixture.state).unwrap();
    let events = store.load_session(session).await.unwrap();
    let view = store.load_view(session).await.unwrap().unwrap();
    store.close().await.unwrap();
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(SessionView::replay(session, &events).unwrap().unwrap(), view);
    let rewrite = events
        .iter()
        .position(|event| {
            matches!(&event.event,
                Event::HookStarted { hook, .. }
                    if hook.event == arany::permissions::HookEvent::CommandRewrite)
        })
        .unwrap();
    let dispatch = events
        .iter()
        .position(|event| matches!(event.event, Event::ToolStarted { .. }))
        .unwrap();
    assert!(rewrite < dispatch);
    assert!(
        events
            .iter()
            .any(|event| matches!(event.event, Event::CommandToolProbeFinished { .. }))
    );
}

fn command(host: bool, script: &str) -> arany::ToolCall {
    if host {
        arany::ToolCall::CommandUnguarded {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: String::new(),
            justification: "Run the requested command".into(),
            prefix_rule: None,
        }
    } else {
        arany::ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: String::new(),
        }
    }
}

fn hook_source(failure: Option<arany::RewriteStage>) -> String {
    let rewrite = "printf '%s\\n' '{\"hookSpecificOutput\":{\"permissionDecision\":\"allow\",\"updatedInput\":{\"command\":\"printf rewritten\"}}}'";
    let payload = match failure {
        Some(arany::RewriteStage::NonZeroExit) => "exit 2",
        Some(arany::RewriteStage::InvalidOutput) => "printf invalid",
        None => rewrite,
        Some(_) => unreachable!("only confirmed failures are configured"),
    };
    format!(
        "#!/bin/sh\nIFS= read -r input || :\ncase \"$input\" in *'cargo test'*) {rewrite}; exit 0 ;; esac\n{payload}\n"
    )
}

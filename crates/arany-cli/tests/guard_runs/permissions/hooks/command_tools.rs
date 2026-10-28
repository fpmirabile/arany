mod prefix;

use super::*;
use arany::permissions::HookResult;
use arany::test_support::process::BoundedOutput;
use std::os::unix::fs::MetadataExt;

const FIXTURE_COMPILE_TIMEOUT: Duration = Duration::from_secs(30);
const FIXTURE_COMPILER_OUTPUT_BYTES: usize = 4096;

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
        compile_hook(root, &executable, failure);
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
        let outcome = probe.outcome.unwrap();
        let result = match &outcome.result {
            HookResult::Passed => "Passed",
            HookResult::Stopped { .. } => "Stopped",
            HookResult::Restricted { .. } => "Restricted",
            HookResult::Blocked { .. } => "Blocked",
            HookResult::Feedback { .. } => "Feedback",
            HookResult::Narrowed { .. } => "Narrowed",
            HookResult::Ignored { .. } => "Ignored",
        };
        assert!(
            matches!(outcome.result, HookResult::Narrowed { .. }),
            "probe: {result}; {}",
            tool_report(Some(&outcome.observation))
        );
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
        assert_eq!(
            outcome.run.status,
            RunStatus::Finished,
            "host={host} failure={failure:?}: {:?}; {}",
            outcome.run.failure,
            tool_report(
                outcome
                    .run
                    .tools
                    .last()
                    .and_then(|tool| tool.observation.as_ref())
            )
        );
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
    let host = rewritten.is_unguarded();
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
        approvals: host.then_some(approvals),
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
    assert_eq!(
        SessionView::replay(session, &events).unwrap().unwrap(),
        view
    );
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
            resource_class: None,
            timeout_ms: None,
        }
    }
}

fn compile_hook(root: &Path, executable: &Path, failure: Option<arany::RewriteStage>) {
    compile_source(root, executable, &hook_source(failure));
}

fn compile_source(root: &Path, executable: &Path, text: &str) {
    let source = root.join("filter.c");
    std::fs::write(&source, text).unwrap();
    #[cfg(target_os = "macos")]
    let mut compiler = {
        let mut command = std::process::Command::new("/usr/bin/xcrun");
        command.args(["--sdk", "macosx", "clang"]);
        command
    };
    #[cfg(not(target_os = "macos"))]
    let mut compiler = std::process::Command::new("/usr/bin/cc");
    let output = compiler
        .args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(executable)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("TMPDIR", root)
        .current_dir(root)
        .bounded_output_for(FIXTURE_COMPILE_TIMEOUT, FIXTURE_COMPILER_OUTPUT_BYTES)
        .unwrap();
    assert!(output.status.success(), "fixture compilation: {output:?}");
}

fn hook_source(failure: Option<arany::RewriteStage>) -> String {
    let rewrite = r#"puts("{\"hookSpecificOutput\":{\"permissionDecision\":\"allow\",\"updatedInput\":{\"command\":\"printf rewritten\"}}}")"#;
    let payload = match failure {
        Some(arany::RewriteStage::NonZeroExit) => "return 2",
        Some(arany::RewriteStage::InvalidOutput) => "puts(\"invalid\")",
        None => rewrite,
        Some(_) => unreachable!("only confirmed failures are configured"),
    };
    format!(
        r#"#include <stdio.h>
#include <string.h>

int main(void) {{
    char input[{input_bytes}];
    size_t size = fread(input, 1, sizeof(input) - 1, stdin);
    if (ferror(stdin) || !feof(stdin)) return 3;
    input[size] = '\0';
    if (strstr(input, "cargo test")) {{
        {rewrite};
        return 0;
    }}
    {payload};
    return 0;
}}
"#,
        input_bytes = arany::permissions::MAX_HOOK_INPUT_BYTES + 2,
    )
}

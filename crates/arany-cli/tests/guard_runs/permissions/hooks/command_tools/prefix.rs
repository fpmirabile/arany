use super::*;

const JOURNEY_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone, Copy, Debug)]
enum Ending {
    Success,
    Failed,
    Drift,
    Unknown,
}

#[tokio::test]
async fn prefix_binds_guard_host_approval_failure_drift_and_closed_replay() {
    for host in [false, true] {
        for ending in [
            Ending::Success,
            Ending::Failed,
            Ending::Drift,
            Ending::Unknown,
        ] {
            prefix_journey(host, ending, false).await;
        }
        prefix_journey(host, Ending::Success, true).await;
    }
}

fn prefix_fixture(root: &Path, ending: Ending, compose: bool) -> CommandFixture {
    let mut fixture = CommandFixture::new(root, None);
    let path = [
        fixture.executable.parent().unwrap().to_str().unwrap(),
        "/usr/bin",
        "/bin",
    ];
    let canonical = serde_json::to_vec(&json!(["process", null, path, [], []])).unwrap();
    fixture
        .state
        .record(arany::RecordKind::CommandEnvironment)
        .replace(
            &serde_json::to_vec(&json!({
                "source":"process","shell":null,"captured_at":0,"path":path,
                "names":[],"excluded":[],"sha256":hash(&canonical)
            }))
            .unwrap(),
        )
        .unwrap();
    let rewrite = fixture.executable.with_file_name("rewrite");
    if compose {
        std::fs::copy(&fixture.executable, &rewrite).unwrap();
    }
    let exit = if matches!(ending, Ending::Failed) {
        7
    } else {
        0
    };
    compile_source(root, &fixture.executable, &prefix_source(exit));
    let mut selected = arany::CommandTool::pin(
        &fixture.state,
        &fixture.workspace,
        fixture.executable.to_str().unwrap(),
        Vec::new(),
    )
    .unwrap();
    selected.role = arany::CommandToolRole::Prefix;
    arany::CommandTools::edit(&fixture.state, |record| {
        record.replace(0, &fixture.tool, selected.clone())
    })
    .unwrap();
    fixture.tool = selected;
    #[cfg(target_os = "macos")]
    add_cargo_grant(&fixture);
    if compose {
        let tool = arany::CommandTool::pin(
            &fixture.state,
            &fixture.workspace,
            rewrite.to_str().unwrap(),
            Vec::new(),
        )
        .unwrap();
        arany::CommandTools::edit(&fixture.state, |record| record.select(tool)).unwrap();
    }
    fixture
}

#[cfg(target_os = "macos")]
fn add_cargo_grant(fixture: &CommandFixture) {
    let cargo = Path::new(env!("CARGO")).canonicalize().unwrap();
    let root = cargo.parent().unwrap().parent().unwrap();
    let metadata = root.metadata().unwrap();
    let record = fixture
        .state
        .record(arany::RecordKind::WorkspacePermissions);
    let mut value: serde_json::Value =
        serde_json::from_slice(&record.read().unwrap().unwrap()).unwrap();
    value["workspaces"][0]["runtime_grants"]
        .as_array_mut()
        .unwrap()
        .push(json!({
        "kind":"rust", "manager":"rustup", "version":root.file_name().unwrap().to_str().unwrap(),
        "path":root, "device":metadata.dev(), "inode":metadata.ino()
        }));
    record
        .replace(&serde_json::to_vec(&value).unwrap())
        .unwrap();
}

async fn enable_probes(engine: &Engine, fixture: &CommandFixture) -> arany::SessionId {
    let session = engine
        .create_session(fixture.workspace.clone(), None)
        .await
        .unwrap();
    let tools = arany::CommandTools::load(&fixture.state).unwrap();
    for (index, selected) in tools.entries().iter().enumerate() {
        let probe = engine
            .probe_command_tool(
                session,
                &fixture.workspace,
                selected,
                RunCancellation::new(),
            )
            .await
            .unwrap();
        let outcome = probe.outcome.unwrap();
        let (stage, _) = outcome
            .result
            .command_rewrite(outcome.observation.disposition);
        assert!(matches!(
            stage,
            arany::RewriteStage::Rewritten | arany::RewriteStage::Unchanged
        ));
        let mut enabled = selected.clone();
        enabled.enabled = true;
        enabled.last_probe = Some(stage);
        arany::CommandTools::edit(&fixture.state, |record| {
            record.replace(index, selected, enabled)
        })
        .unwrap();
    }
    session
}

async fn prefix_journey(host: bool, ending: Ending, compose: bool) {
    let temp = tempfile::tempdir().unwrap();
    let fixture = prefix_fixture(temp.path(), ending, compose);
    let engine = engine_support::open(
        StateRoot::open_existing(fixture.state.path()).unwrap(),
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let session = enable_probes(&engine, &fixture).await;
    let script = if cfg!(target_os = "macos") && !compose && !host {
        format!(
            "printf payload; '{}' --check-compat && cargo --version >/dev/null",
            fixture.executable.display()
        )
    } else {
        "printf payload".into()
    };
    let proposed = command(host, &script);
    let original = if compose {
        command(host, "printf rewritten")
    } else {
        proposed.clone()
    };
    let provider = prefix_provider(&proposed, &original);
    let mut input = request(&fixture.workspace, "Run the prefix wrapper");
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
    let approve = approve_prefix(&mut inbox, &fixture, &original, ending);
    let (outcome, approved) = tokio::time::timeout(JOURNEY_TIMEOUT, async {
        tokio::join!(run_with(&engine, &provider, input, controls), approve)
    })
    .await
    .unwrap();
    assert!(
        approved,
        "prefix approval missing: host={host} ending={ending:?} compose={compose}; outcome={:?}",
        outcome
            .as_ref()
            .map(|outcome| (&outcome.run.status, &outcome.run.failure))
    );
    if matches!(ending, Ending::Unknown) {
        assert!(matches!(
            outcome,
            Err(arany::EngineError::Tool(arany::ToolError::Uncertain))
        ));
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
    } else {
        assert_prefix_outcome(&outcome.unwrap().run, ending, compose);
    }
    engine.close().await.unwrap();
    assert_prefix_replay(fixture, session, ending, compose).await;
}

async fn approve_prefix(
    inbox: &mut arany::ApprovalInbox,
    fixture: &CommandFixture,
    original: &arany::ToolCall,
    ending: Ending,
) -> bool {
    let Some(approval) = inbox.next().await else {
        return false;
    };
    let wrap = approval.intent.wrap.as_ref().unwrap();
    assert_eq!(&wrap.original, original);
    assert_eq!(wrap.stage, arany::WrapStage::Wrapped);
    assert!(arany::permissions::HostPrefixRule::from_intent(&approval.intent, 1).is_err());
    if matches!(ending, Ending::Drift) {
        std::fs::write(&fixture.executable, b"changed").unwrap();
    }
    if matches!(ending, Ending::Unknown) {
        arany::FaultPlan::lose_guard_receipt(approval.intent.id);
    }
    approval.decide(true);
    true
}

fn prefix_provider(
    proposed: &arany::ToolCall,
    original: &arany::ToolCall,
) -> Arc<ScriptedProvider> {
    Arc::new(ScriptedProvider::new(vec![
        plan_outcome(arany::GoalPlanContract {
            mission: "Observe one wrapped command".into(),
            paths: Vec::new(),
            commands: vec!["sh".into()],
            resource_class: arany::ToolResourceClass::Default,
            command_timeout_ms: None,
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            success_criterion: arany::GoalSuccessCriterion::ToolCallSucceeded {
                call: Box::new(original.clone()),
            },
            budget: arany::GoalBudget::default(),
        }),
        tool_outcome(proposed.clone()),
        finish("Observed the wrapped outcome"),
    ]))
}

fn assert_prefix_outcome(run: &arany::RunView, ending: Ending, compose: bool) {
    if matches!(ending, Ending::Success) {
        assert_eq!(run.status, RunStatus::Finished);
    }
    let wrapped = run
        .tools
        .iter()
        .filter(|tool| tool.intent.wrap.is_some())
        .collect::<Vec<_>>();
    assert_eq!(wrapped.len(), 1);
    let observation = wrapped[0].observation.as_ref().unwrap();
    match ending {
        Ending::Unknown => {
            assert_eq!(observation.disposition, arany::ToolDisposition::Uncertain);
            assert_eq!(run.status, RunStatus::Interrupted);
        }
        Ending::Drift => {
            assert!(observation.attestation.is_none());
            assert!(observation.output.contains("pinned input"));
        }
        Ending::Success | Ending::Failed => {
            let expected = if matches!(ending, Ending::Success) {
                arany::ToolDisposition::Succeeded
            } else {
                arany::ToolDisposition::Failed
            };
            assert_eq!(
                observation.disposition,
                expected,
                "{}",
                tool_report(Some(observation))
            );
            let output: serde_json::Value = serde_json::from_str(&observation.output).unwrap();
            assert_eq!(
                output["stdout"],
                if compose { "rewritten" } else { "payload" }
            );
        }
    }
}

async fn assert_prefix_replay(
    fixture: CommandFixture,
    session: arany::SessionId,
    ending: Ending,
    compose: bool,
) {
    std::fs::write(&fixture.executable, b"must never execute during replay").unwrap();
    fixture
        .state
        .record(arany::RecordKind::CommandTools)
        .remove()
        .unwrap();
    let store = Store::open(fixture.state).unwrap();
    let events = store.load_session(session).await.unwrap();
    let view = store.load_view(session).await.unwrap().unwrap();
    store.close().await.unwrap();
    assert_prefix_outcome(&view.runs[0], ending, compose);
    if matches!(ending, Ending::Unknown) {
        assert_eq!(
            view.run_stop(&view.runs[0]),
            Some(arany::RunStop::Uncertain)
        );
        assert!(
            events
                .iter()
                .all(|event| !matches!(event.event, Event::RunFinished { .. }))
        );
    }
    assert_eq!(
        SessionView::replay(session, &events).unwrap().unwrap(),
        view
    );
}

fn prefix_source(exit: i32) -> String {
    format!(
        r#"#define _POSIX_C_SOURCE 200809L
#ifdef __APPLE__
#define _DARWIN_C_SOURCE
#endif
#include <string.h>
#include <stdlib.h>
#include <errno.h>
#include <unistd.h>
#include <sys/wait.h>
#ifdef __APPLE__
#include <sys/sysctl.h>
#endif

int main(int argc, char **argv) {{
    if (argc != 2) return 2;
#ifdef __APPLE__
    if (!strcmp(argv[1], "--check-compat")) {{
        int value;
        size_t size = sizeof(value);
        if (!getenv("DYLD_INSERT_LIBRARIES") || sysconf(_SC_PAGESIZE) <= 0) return 8;
        errno = 0;
        int result = sysctlbyname("hw.pagesize", &value, &size, 0, 0);
        return result == -1 && (errno == EPERM || errno == EACCES) ? 0 : 9;
    }}
#endif
    if (strstr(argv[1], "cargo test")) return 0;
    pid_t child = fork();
    if (child < 0) return 3;
    if (!child) {{
        execl("{shell}", "sh", "-c", argv[1], (char *)0);
        _exit(4);
    }}
    int status;
    if (waitpid(child, &status, 0) != child) return 5;
    if (!WIFEXITED(status) || WEXITSTATUS(status)) return 6;
    return {exit};
}}
"#,
        shell = if cfg!(target_os = "macos") {
            "/bin/sh"
        } else {
            "/usr/bin/sh"
        }
    )
}

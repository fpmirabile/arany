use super::*;
use crate::session::{AgentRunId, RunId};
use crate::tools::{WorkspacePermissions, permissions::HostPrefixRule, unguarded};

#[test]
fn prefix_preserves_script_and_cwd_and_dispatch_drift_never_degrades() {
    let temp = tempfile::tempdir().unwrap();
    let (state, workspace, executable, _) = super::super::tests::fixture(temp.path());
    let mut tool =
        CommandTool::pin(&state, &workspace, &executable, vec!["--prefix".into()]).unwrap();
    tool.role = CommandToolRole::Prefix;
    CommandTools::edit(&state, |record| record.select(tool.clone())).unwrap();
    let mut enabled = tool.clone();
    enabled.enabled = true;
    enabled.last_probe = Some(RewriteStage::Unchanged);
    CommandTools::edit(&state, |record| record.replace(0, &tool, enabled.clone())).unwrap();
    let config = WorkspacePermissions::load(&workspace, &state)
        .unwrap()
        .config(&workspace)
        .unwrap();
    let runtime = ToolRuntime::admit(
        config,
        None,
        None,
        &workspace,
        &state,
        "/missing-guard".into(),
    )
    .unwrap();
    let original = ToolCall::Command {
        program: "sh".into(),
        args: vec!["-c".into(), "printf 'a b' | head".into()],
        cwd: "src".into(),
        resource_class: Some(crate::tools::ToolResourceClass::Build),
        timeout_ms: Some(crate::tools::types::DEFAULT_COMMAND_TIMEOUT_MS),
    };
    let intent =
        runtime.wrap_command(runtime.intent(RunId::new(), AgentRunId::new(), original.clone()));
    assert_eq!(
        intent.call,
        ToolCall::Command {
            program: executable.clone(),
            args: vec!["--prefix".into(), "printf 'a b' | head".into()],
            cwd: "src".into(),
            resource_class: Some(crate::tools::ToolResourceClass::Build),
            timeout_ms: Some(crate::tools::types::DEFAULT_COMMAND_TIMEOUT_MS),
        }
    );
    assert!(runtime.verify_wrap(&intent).is_ok());
    assert_direct_wrapper_policy(&runtime, &enabled, &intent);
    std::fs::write(&executable, b"changed after normalization").unwrap();
    assert!(matches!(
        runtime.verify_wrap(&intent),
        Err(ToolError::ChangedInput)
    ));
    let degraded =
        runtime.wrap_command(runtime.intent(intent.run_id, intent.agent_run_id, original.clone()));
    assert_eq!(degraded.call, original);
    assert_eq!(degraded.wrap.unwrap().stage, WrapStage::PinRefused);
}

fn assert_direct_wrapper_policy(runtime: &ToolRuntime, tool: &CommandTool, prior: &EffectIntent) {
    let mut direct = runtime.intent(
        prior.run_id,
        prior.agent_run_id,
        ToolCall::CommandUnguarded {
            program: tool.executable.clone(),
            args: vec!["sudo id".into()],
            cwd: String::new(),
            justification: "Inspect the wrapper".into(),
            prefix_rule: None,
        },
    );
    let mut pin = crate::tools::types::test_host_pin();
    pin.executable = tool.executable.clone();
    pin.sha256 = tool.sha256.clone();
    direct.host_command = Some(pin);
    direct = runtime.wrap_command(direct);
    assert_eq!(direct.wrap.as_ref().unwrap().stage, WrapStage::Direct);
    assert!(unguarded::prohibited_intent(&direct));
    assert!(!unguarded::same_approval(&direct, &direct));
    assert!(HostPrefixRule::from_intent(&direct, 1).is_err());
}

#[test]
fn prefix_payload_and_replay_facts_refuse_a_different_final_command() {
    let tool = CommandTool {
        selected_path: "/programs/wrapper".into(),
        executable: "/programs/wrapper".into(),
        args: Vec::new(),
        role: CommandToolRole::Prefix,
        device: 1,
        inode: 2,
        sha256: "a".repeat(64),
        enabled: true,
        last_probe: Some(RewriteStage::Unchanged),
    };
    let original = ToolCall::Command {
        program: "sh".into(),
        args: vec!["-c".into(), "printf payload".into()],
        cwd: String::new(),
        resource_class: None,
        timeout_ms: None,
    };
    let mut other = tool.clone();
    other.executable = "/programs/other".into();
    other.selected_path = other.executable.clone();
    let record = CommandTools {
        version: VERSION,
        tools: vec![tool.clone(), other],
    };
    assert!(CommandTools::parse(&serde_json::to_vec(&record).unwrap()).is_err());
    let call = wrapped(&original, &tool);
    let wrap = CommandWrap {
        tool,
        original,
        stage: WrapStage::Wrapped,
    };
    assert!(wrap.valid(&call));
    let forged = ToolCall::Command {
        program: "/programs/wrapper".into(),
        args: vec!["printf changed".into()],
        cwd: String::new(),
        resource_class: None,
        timeout_ms: None,
    };
    assert!(!wrap.valid(&forged));
}

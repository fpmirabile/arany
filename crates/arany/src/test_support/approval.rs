use crate::{EffectIntent, ToolApprovals};

pub async fn ask(approvals: &ToolApprovals, intent: &EffectIntent) -> bool {
    approvals.ask(intent).await
}

pub async fn delay_withdrawal(
    approvals: ToolApprovals,
    intent: EffectIntent,
    reject: tokio::sync::oneshot::Receiver<()>,
    rejected: tokio::sync::oneshot::Sender<()>,
    committed: tokio::sync::oneshot::Receiver<()>,
) {
    let mut pending = approvals.begin_local(&intent, false, None).await;
    if reject.await.is_err() {
        return;
    }
    let Some(withdrawal) = pending.reject_late_replies() else {
        return;
    };
    let _ = rejected.send(());
    if committed.await.is_ok() {
        withdrawal.commit();
    }
}

pub async fn seed_host_prefix(
    state: &std::path::Path,
    workspace: &std::path::Path,
    launcher: std::path::PathBuf,
) -> crate::ToolCall {
    use super::scenario::{ScriptedProvider, request, tool_outcome};
    use crate::{ApprovalMode, ApprovalReply, RunControls, StateRoot, ToolGrant};
    use std::sync::Arc;

    let root = StateRoot::admit(state).expect("private State");
    root.record(crate::RecordKind::ToolsConfig)
        .replace(br#"{"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}"#)
        .expect("host Tool configuration");
    let engine = super::engine::open(
        root,
        crate::EngineConfig {
            guard_executable: Some(launcher),
            ..Default::default()
        },
    )
    .expect("host Engine");
    let call = crate::ToolCall::CommandUnguarded {
        program: "printf".into(),
        args: vec!["host value".into()],
        cwd: String::new(),
        justification: "Inspect a synthetic host value".into(),
        prefix_rule: Some(vec!["printf".into(), "host value".into()]),
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_outcome(call.clone()),
        crate::ProviderOutcome::Answer(crate::Answer {
            text: "Declined".into(),
        }),
    ]));
    let (approvals, mut inbox) = crate::ToolApprovals::new(ApprovalMode::Request);
    let mut input = request(
        workspace,
        "Prepare a host rule for the explicit permissions editor",
    );
    input.include_paths.clear();
    let controls = RunControls {
        tools: ToolGrant::Configured,
        approvals: Some(approvals),
        ..Default::default()
    };
    let (outcome, intent) = tokio::join!(
        super::engine::run_with(&engine, &provider, input, controls),
        async {
            let approval = inbox.next().await.expect("host approval");
            let intent = approval.intent.clone();
            approval.decide(ApprovalReply::Deny);
            intent
        }
    );
    assert_eq!(outcome.expect("declined host run").run.unguarded_tools(), 0);
    assert_eq!(provider.requests.lock().expect("requests").len(), 2);
    assert!(provider.responses.lock().expect("responses").is_empty());
    engine.close().await.expect("close host Engine");
    let root = StateRoot::open_existing(state).expect("private State");
    crate::permissions::PermissionSettings::load(&root, workspace)
        .expect("settings")
        .save_host_edit(
            &root,
            workspace,
            crate::permissions::HostPrefixEdit::Insert(Box::new(
                crate::permissions::HostPrefixRule::from_intent(&intent, 2)
                    .expect("eligible host prefix"),
            )),
        )
        .expect("explicit permissions edit");
    call
}

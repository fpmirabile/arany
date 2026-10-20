use super::*;

#[test]
fn a_partial_pass_fails_closed_and_a_complete_one_is_reused() {
    let passed = |handler| ((0, handler), Some(HookResult::Passed));
    assert_eq!(occurrence_verdict(&[], &[(0, 0)]), None);
    assert_eq!(
        occurrence_verdict(
            &[(
                (0, 0),
                Some(HookResult::Stopped {
                    reason: Some("halt".into())
                })
            )],
            &[(0, 0), (0, 1)]
        ),
        Some(None)
    );
    assert_eq!(
        occurrence_verdict(&[passed(0), passed(1)], &[(0, 0), (0, 1)]),
        Some(None)
    );
    assert!(matches!(
        occurrence_verdict(&[passed(0)], &[(0, 0), (0, 1)]),
        Some(Some(_))
    ));
    assert!(matches!(
        occurrence_verdict(&[passed(0), ((0, 1), None)], &[(0, 0), (0, 1)]),
        Some(Some(_))
    ));
}

#[test]
fn hook_verdicts_carry_their_typed_stop_cause() {
    use crate::session::{AgentRunId, RunId, worker::EffectTarget};
    use crate::tools::ToolLimits;
    let intent = || EffectIntent {
        target: EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: ToolCall::Read {
            path: "src/lib.rs".into(),
            offset: 0,
            limit: 1,
        },
        limits: ToolLimits::default(),
        expires_at_ms: 1,
        use_count: 1,
    };
    let fold = |stop| HookFold {
        stop,
        ..HookFold::none()
    };
    for (stop, child, expected) in [
        (HookStop::Requested, false, Some(StopCause::HookStop)),
        (HookStop::Uncertain, false, Some(StopCause::Uncertain)),
        (
            HookStop::ActiveTime,
            true,
            Some(StopCause::DelegationDeadline),
        ),
        (HookStop::ActiveTime, false, None),
        (HookStop::None, false, None),
    ] {
        assert_eq!(fold(stop).denial(intent(), child).stop, expected);
    }
}

use super::*;
use crate::tools::{CommandRewrite, RewriteStage};

#[test]
fn rewritten_host_commands_never_qualify_for_remembered_prefixes() {
    let mut intent = super::super::tests::host_intent();
    let rule = HostPrefixRule::from_intent(&intent, 1).unwrap();
    assert!(rule.dispatch_eligible(&intent));
    for stage in [
        RewriteStage::Rewritten,
        RewriteStage::Unchanged,
        RewriteStage::PinRefused,
    ] {
        intent.rewrite = vec![CommandRewrite {
            hook_intent_id: (stage != RewriteStage::PinRefused).then(uuid::Uuid::now_v7),
            original_sha256: [1; 32],
            tool_sha256: "a".repeat(64),
            stage,
        }];
        assert!(!rule.dispatch_eligible(&intent));
        assert!(HostPrefixRule::from_intent(&intent, 1).is_err());
    }
    intent.rewrite[0].stage = RewriteStage::Rewritten;
    intent.rewrite[0].hook_intent_id = Some(uuid::Uuid::now_v7());
    intent.call = crate::tools::command_tools::rewritten(&intent.call, "printf value".into());
    assert!(!rule.dispatch_eligible(&intent));
    assert!(HostPrefixRule::from_intent(&intent, 1).is_err());
}

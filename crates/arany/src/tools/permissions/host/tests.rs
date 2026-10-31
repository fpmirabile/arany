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

#[test]
fn a_missing_proposal_derives_two_tokens_except_for_launchers() {
    let call = |program: &str, args: &[&str], prefix_rule: Option<Vec<String>>| {
        ToolCall::CommandUnguarded {
            program: program.into(),
            args: args.iter().map(|arg| (*arg).into()).collect(),
            cwd: String::new(),
            justification: "Run it".into(),
            prefix_rule,
        }
    };
    let derived = |mut call: ToolCall| {
        fill_host_prefix(&mut call);
        match call {
            ToolCall::CommandUnguarded { prefix_rule, .. } => prefix_rule,
            _ => unreachable!(),
        }
    };
    assert_eq!(
        derived(call("make", &["build", "--quiet"], None)),
        Some(vec!["make".into(), "build".into()])
    );
    let explicit = Some(vec!["make".into(), "build".into(), "--quiet".into()]);
    assert_eq!(
        derived(call("make", &["build", "--quiet"], explicit.clone())),
        explicit
    );
    for (program, args) in [
        ("sh", &["-c", "make"][..]),
        ("/usr/bin/env", &["make"][..]),
        ("sudo", &["make"][..]),
        ("xargs", &["make"][..]),
        ("python3.12", &["build.py"][..]),
        ("osascript", &["script.scpt"][..]),
        ("node", &["build.js"][..]),
        ("git", &["push"][..]),
        ("make", &[][..]),
    ] {
        assert_eq!(derived(call(program, args, None)), None, "{program}");
    }
}

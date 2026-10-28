use super::super::super::test_support::*;
use super::super::super::*;
use crate::tools::{CacheUpdate, ToolCall, ToolDisposition, ToolResourceClass};

#[test]
fn command_resources_only_narrow_the_recorded_plan() {
    use crate::session::ProposalRefusalCause::PlanScopeExcludesCall;
    use ToolResourceClass::{Build, Default};
    for (plan_class, plan_timeout, class, timeout, expected) in [
        (Default, None, Build, 1_000, Err(PlanScopeExcludesCall)),
        (Default, None, Default, 120_001, Err(PlanScopeExcludesCall)),
        (Build, Some(5_000), Build, 5_001, Err(PlanScopeExcludesCall)),
        (Build, Some(5_000), Default, 4_000, Ok(())),
        (Build, Some(5_000), Build, 5_000, Ok(())),
    ] {
        let (mut view, _, run, _) = primary_tool_session(true);
        let plan = view.goals[0].plan_contract.as_mut().unwrap();
        plan.commands = vec!["cargo".into()];
        plan.resource_class = plan_class;
        plan.command_timeout_ms = plan_timeout;
        let call = ToolCall::Command {
            program: "cargo".into(),
            args: vec!["test".into()],
            cwd: String::new(),
            resource_class: Some(class),
            timeout_ms: Some(timeout),
        };
        assert_eq!(
            crate::session::validate_primary_tool_scope(&view, run, &call),
            expected
        );
    }
}

#[test]
fn command_replay_rejects_class_runtime_and_receipt_tampering() {
    let (mut view, goal, run, agent) = primary_tool_session(false);
    apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
    apply(&mut view, requesting_step(run, agent, 1)).unwrap();
    let mut intent = read_intent(run, agent, "src/lib.rs");
    intent.call = ToolCall::Command {
        program: "cargo".into(),
        args: vec!["test".into()],
        cwd: String::new(),
        resource_class: Some(ToolResourceClass::Build),
        timeout_ms: Some(5_000),
    };
    intent.limits.resource_class = ToolResourceClass::Build;
    intent.limits.runtime_ms = 5_000;
    intent.limits.cache_bytes = 0;
    intent.limits.cache_candidate_bytes = 0;
    for class in [true, false] {
        let mut forged = intent.clone();
        if class {
            forged.limits.resource_class = ToolResourceClass::Default;
        } else {
            forged.limits.runtime_ms += 1;
        }
        assert!(matches!(
            apply(&mut view.clone(), tool_started(run, agent, &forged)),
            Err(ReplayError::InvalidTransition)
        ));
    }
    apply(&mut view, tool_started(run, agent, &intent)).unwrap();
    let finished = tool_finished(run, agent, &intent, b"ok");
    let mut forged = finished.clone();
    if let Event::ToolFinished { observation, .. } = &mut forged
        && let Some(crate::tools::Attestation::Guard(receipt)) = &mut observation.attestation
    {
        receipt.limits.runtime_ms -= 1;
    }
    assert!(rejected(&view, forged));
    apply(&mut view, finished).unwrap();
}

#[test]
fn requested_time_receipt_must_match_the_call_and_cannot_claim_dispatch() {
    let (mut view, goal, run, agent) = primary_tool_session(false);
    view.goals[0].resource_settings.default_command_timeout_ms = 1_000;
    view.goals[0].resource_settings.max_command_timeout_ms = 1_000;
    apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
    apply(&mut view, requesting_step(run, agent, 1)).unwrap();
    let mut intent = read_intent(run, agent, "src/lib.rs");
    intent.call = ToolCall::Command {
        program: "cargo".into(),
        args: vec!["test".into()],
        cwd: String::new(),
        resource_class: None,
        timeout_ms: Some(2_000),
    };
    intent.limits.runtime_ms = 2_000;
    intent.limits.cache_bytes = 0;
    intent.limits.cache_candidate_bytes = 0;
    apply(&mut view, tool_started(run, agent, &intent)).unwrap();
    let mut observation = observation_for(intent.clone(), b"");
    observation.disposition = ToolDisposition::Limit;
    observation.limit_cause = Some(crate::tools::LimitCause::RequestedTime);
    observation.attestation = None;
    observation.cache_update = CacheUpdate::NotApplicable;
    observation.output = crate::tools::types::command_timeout_limit_message(2_000, 1_000);
    let finished = |observation| Event::ToolFinished {
        checkpoint: None,
        run_id: run,
        agent_run_id: agent,
        observation,
    };
    for case in ["request", "maximum", "cause", "receipt"] {
        let mut forged = observation.clone();
        match case {
            "request" => {
                forged.output = crate::tools::types::command_timeout_limit_message(3_000, 1_000);
            }
            "maximum" => {
                forged.output = crate::tools::types::command_timeout_limit_message(2_000, 1_500);
            }
            "cause" => forged.limit_cause = Some(crate::tools::LimitCause::Timeout),
            "receipt" => forged.attestation = observation_for(intent.clone(), b"").attestation,
            _ => unreachable!(),
        }
        assert!(rejected(&view, finished(forged)), "{case}");
    }
    apply(&mut view, finished(observation)).unwrap();
}

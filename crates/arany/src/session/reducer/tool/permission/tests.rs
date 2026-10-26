use super::super::super::test_support::*;
use super::super::super::*;
use crate::tools::{CacheUpdate, EffectIntent, ToolCall, ToolDisposition, ToolObservation};

#[test]
fn only_unsanitized_denial_results_require_redaction() {
    let mut view = pending_plan_view(Some(ToolDisposition::Denied));
    let tool = &mut view.runs[0].tools[0];
    tool.observation.as_mut().unwrap().output = "old hook denial canary".into();
    tool.permission_verdict = Some(crate::tools::permissions::PermissionVerdict {
        host: None,
        intent_digest: [1; 32],
        decision: crate::tools::permissions::Decision::Deny,
        mode: None,
        basis: crate::tools::permissions::PermissionSource::Hook,
        source: crate::tools::permissions::PermissionSource::Hook,
        approved: false,
        hook_restriction: Some(crate::tools::permissions::Decision::Deny),
        hooks: 1,
        narrowed: false,
    });

    assert!(tool.denial_needs_redaction());
    tool.observation.as_mut().unwrap().output = "hook_denied: no operation was dispatched".into();
    assert!(!tool.denial_needs_redaction());
}

#[test]
fn approved_execution_denial_keeps_its_filesystem_cause() {
    let mut view = pending_plan_view(Some(ToolDisposition::Denied));
    let tool = &mut view.runs[0].tools[0];
    let observation = tool.observation.as_mut().unwrap();
    observation.output = "Guard enforcement: protected filesystem path refused".into();
    observation.enforcement_cause = Some(crate::tools::EnforcementCause::FilesystemDenied);
    observation.attestation = Some(crate::tools::Attestation::Guard(
        crate::tools::GuardReceipt {
            contract_version: 1,
            intent_digest: tool.intent.digest(),
            enforcement_digest: tool.intent.enforcement_digest,
            limits: tool.intent.limits.clone(),
        },
    ));
    tool.permission_verdict = Some(crate::tools::permissions::PermissionVerdict {
        host: None,
        intent_digest: tool.intent.digest(),
        decision: crate::tools::permissions::Decision::Allow,
        mode: None,
        basis: crate::tools::permissions::PermissionSource::Human,
        source: crate::tools::permissions::PermissionSource::Human,
        approved: true,
        hook_restriction: None,
        hooks: 0,
        narrowed: false,
    });

    assert_eq!(tool.model_denial_label(), None);
}

#[test]
fn permission_verdict_replay_requires_pending_intent_and_blocks_denied_effects() {
    use crate::tools::permissions::{
        Decision, PermissionMode, PermissionSource, PermissionVerdict,
    };
    for researcher in [false, true] {
        let (mut view, goal, run, agent) = if researcher {
            research_session()
        } else {
            primary_tool_session(true)
        };
        view.runs[0]
            .config
            .as_mut()
            .unwrap()
            .tool_policy
            .as_mut()
            .unwrap()
            .permissions_digest = Some([3; 32]);
        apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
        apply(
            &mut view,
            if researcher {
                child_record(run, agent, ProviderCallDisposition::ToolRequested)
            } else {
                requesting_step(run, agent, 1)
            },
        )
        .unwrap();
        let intent = read_intent(run, agent, "src/lib.rs");
        let verdict = Event::ToolStarted {
            intent: intent.clone().into(),
            active_ceiling_ms: 0,
            run_id: run,
            agent_run_id: agent,
            verdict: Some(PermissionVerdict {
                host: None,
                intent_digest: intent.digest(),
                decision: Decision::Deny,
                mode: Some(PermissionMode::Default),
                basis: PermissionSource::Rule,
                source: PermissionSource::Rule,
                approved: false,
                hook_restriction: None,
                hooks: 0,
                narrowed: false,
            }),
        };
        assert_eq!(
            Event::decode(
                "ToolStarted",
                crate::session::payload_version("ToolStarted").unwrap(),
                &verdict.payload().unwrap(),
                Some(run),
                Some(agent)
            )
            .unwrap(),
            verdict
        );
        assert!(rejected(&view, tool_started(run, agent, &intent)));
        assert!(rejected(
            &view,
            verdict_event(
                &intent,
                Some(PermissionMode::Default),
                Decision::Ask,
                PermissionSource::Mode,
                PermissionSource::Human,
                true
            )
        ));
        apply(&mut view, tool_proposed(run, agent, &intent)).unwrap();
        let succeeded = tool_finished(run, agent, &intent, b"ok");
        assert!(rejected(&view, succeeded.clone()));
        let mut wrong_intent = verdict.clone();
        if let Event::ToolStarted {
            verdict: Some(verdict),
            ..
        } = &mut wrong_intent
        {
            verdict.intent_digest = [4; 32];
        }
        assert!(rejected(&view, wrong_intent));
        for mode in [PermissionMode::Auto, PermissionMode::Default] {
            let mut unreviewed = verdict.clone();
            if let Event::ToolStarted {
                verdict: Some(verdict),
                ..
            } = &mut unreviewed
            {
                verdict.decision = Decision::Ask;
                verdict.mode = Some(mode);
                verdict.basis = PermissionSource::Mode;
                verdict.source = PermissionSource::AiReview;
                verdict.approved = true;
            }
            assert!(unreviewed.payload().is_ok());
            assert!(rejected(&view, unreviewed.clone()));
            if let Event::ToolStarted {
                verdict: Some(verdict),
                ..
            } = &mut unreviewed
            {
                verdict.basis = PermissionSource::Rule;
            }
            assert!(unreviewed.payload().is_err());
        }
        apply(&mut view, verdict.clone()).unwrap();
        assert!(rejected(&view, verdict));
        assert!(rejected(&view, succeeded));
        let mut denied = observation_for(intent, b"unused");
        denied.disposition = ToolDisposition::Denied;
        denied.attestation = None;
        denied.output = "Permission deny rule blocked this Tool".into();
        apply(
            &mut view,
            Event::ToolFinished {
                checkpoint: None,
                run_id: run,
                agent_run_id: agent,
                observation: denied,
            },
        )
        .unwrap();
    }
}

struct Unguarded {
    view: SessionView,
    host: EffectIntent,
    run: RunId,
    agent: AgentRunId,
    started: bool,
}

// A primary that ran `sh -c <prior_script>` in the Guard with this outcome, then requests the host re-run of `exit 7`.
fn unguarded_scenario(
    prior: Option<(
        &str,
        ToolDisposition,
        Option<crate::tools::EnforcementCause>,
    )>,
    configured: bool,
    rules_pinned: bool,
) -> Unguarded {
    let (mut view, goal, run, agent) = primary_tool_session(true);
    view.goals[0]
        .plan_contract
        .as_mut()
        .unwrap()
        .commands
        .push("sh".into());
    let policy = view.runs[0]
        .config
        .as_mut()
        .unwrap()
        .tool_policy
        .as_mut()
        .unwrap();
    policy.permissions_digest = configured.then_some([3; 32]);
    policy.unguarded_rules_digest = rules_pinned.then_some([5; 32]);
    policy.host_epoch = Some(uuid::Uuid::from_u128(0x01a15ecfa92b736a91a6560b8bc09501));
    apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
    apply(&mut view, requesting_step(run, agent, 1)).unwrap();
    if let Some((script, disposition, cause)) = prior {
        let guarded = child_intent(
            run,
            agent,
            ToolCall::Command {
                program: "sh".into(),
                args: vec!["-c".into(), script.into()],
                cwd: String::new(),
            },
        );
        apply(
            &mut view,
            if configured {
                tool_proposed(run, agent, &guarded)
            } else {
                tool_started(run, agent, &guarded)
            },
        )
        .unwrap();
        if configured {
            apply(
                &mut view,
                verdict_event(
                    &guarded,
                    configured.then_some(crate::tools::permissions::PermissionMode::Default),
                    crate::tools::permissions::Decision::Allow,
                    crate::tools::permissions::PermissionSource::Mode,
                    crate::tools::permissions::PermissionSource::Mode,
                    true,
                ),
            )
            .unwrap();
        }
        let mut observation = observation_for(guarded, b"ok");
        observation.disposition = disposition;
        observation.enforcement_cause = cause;
        observation.limit_cause =
            (disposition == ToolDisposition::Limit).then_some(crate::tools::LimitCause::Resource);
        observation.output =
            r#"{"selection":{"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code":7,"stdout":"","stderr":"","changeset":{"files":[]},"stdout_total":{"bytes":0,"lines":0},"stderr_total":{"bytes":0,"lines":0}}"#.into();
        apply(
            &mut view,
            Event::ToolFinished {
                checkpoint: None,
                run_id: run,
                agent_run_id: agent,
                observation,
            },
        )
        .unwrap();
    }
    let host = child_intent(
        run,
        agent,
        ToolCall::CommandUnguarded {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 7".into()],
            cwd: String::new(),
            justification: "Run the failed command once".into(),
            prefix_rule: None,
        },
    );
    let start = tool_proposed(run, agent, &host);
    if matches!(
        prior,
        Some((
            _,
            ToolDisposition::Uncertain | ToolDisposition::Cancelled,
            _
        ))
    ) {
        // The Run stops at these outcomes, so the reducer refuses the unguarded start itself.
        assert!(rejected(&view, start), "{prior:?}");
        return Unguarded {
            view,
            host,
            run,
            agent,
            started: false,
        };
    }
    if prior.is_some() {
        apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
        apply(&mut view, requesting_step(run, agent, 1)).unwrap();
    }
    apply(&mut view, start).expect("the start is always admitted");
    Unguarded {
        view,
        host,
        run,
        agent,
        started: true,
    }
}

#[test]
fn an_approved_host_run_without_an_outcome_counts_as_unguarded() {
    use crate::tools::permissions::{Decision, PermissionSource};
    let mut scenario = unguarded_scenario(
        Some(("exit 7", ToolDisposition::Failed, None)),
        false,
        false,
    );
    let count = |view: &SessionView| view.runs[0].unguarded_tools();
    assert_eq!(count(&scenario.view), 0);
    let (host, run, agent) = (scenario.host.clone(), scenario.run, scenario.agent);
    let mut refused = scenario.view.clone();
    apply(
        &mut refused,
        verdict_event(
            &host,
            None,
            Decision::Ask,
            PermissionSource::Mode,
            PermissionSource::UnavailableApproval,
            false,
        ),
    )
    .unwrap();
    assert_eq!(count(&refused), 0);
    let _ = (run, agent);
    apply(
        &mut scenario.view,
        verdict_event(
            &host,
            None,
            Decision::Ask,
            PermissionSource::Mode,
            PermissionSource::Human,
            true,
        ),
    )
    .unwrap();
    assert_eq!(count(&scenario.view), 1);
}

#[test]
fn session_host_replay_checks_epoch_reference_scope_and_exact_pins() {
    use crate::tools::permissions::{Decision, PermissionSource};
    let mut scenario = unguarded_scenario(None, false, false);
    let mut human = verdict_event(
        &scenario.host,
        None,
        Decision::Ask,
        PermissionSource::Mode,
        PermissionSource::Human,
        true,
    );
    let Event::ToolStarted {
        verdict: Some(verdict),
        ..
    } = &mut human
    else {
        unreachable!()
    };
    verdict.host.as_mut().unwrap().session = true;
    let epoch = verdict.host.as_ref().unwrap().epoch;
    for basis in [
        PermissionSource::Mode,
        PermissionSource::Rule,
        PermissionSource::SessionApproval,
    ] {
        let mut candidate = human.clone();
        let Event::ToolStarted {
            verdict: Some(verdict),
            ..
        } = &mut candidate
        else {
            unreachable!()
        };
        verdict.basis = basis;
        assert_eq!(
            rejected(&scenario.view, candidate),
            basis == PermissionSource::SessionApproval
        );
    }
    apply(&mut scenario.view, human).unwrap();
    apply(
        &mut scenario.view,
        Event::ToolFinished {
            checkpoint: None,
            run_id: scenario.run,
            agent_run_id: scenario.agent,
            observation: observation_for(scenario.host.clone(), b"ok"),
        },
    )
    .unwrap();
    let goal = scenario.view.goals[0].id;
    apply(
        &mut scenario.view,
        reserve_child_call(goal, scenario.run, scenario.agent),
    )
    .unwrap();
    apply(
        &mut scenario.view,
        requesting_step(scenario.run, scenario.agent, 1),
    )
    .unwrap();
    let mut next = scenario.host.clone();
    next.id = uuid::Uuid::now_v7();
    next.limits.runtime_ms /= 2;
    apply(
        &mut scenario.view,
        tool_proposed(scenario.run, scenario.agent, &next),
    )
    .unwrap();
    let mut reused = verdict_event(
        &next,
        None,
        Decision::Allow,
        PermissionSource::SessionApproval,
        PermissionSource::SessionApproval,
        true,
    );
    let Event::ToolStarted {
        verdict: Some(verdict),
        ..
    } = &mut reused
    else {
        unreachable!()
    };
    verdict.host.as_mut().unwrap().source_intent = Some(scenario.host.digest());
    assert!(!rejected(&scenario.view, reused.clone()));
    let mut gated = reused.clone();
    let Event::ToolStarted {
        verdict: Some(verdict),
        ..
    } = &mut gated
    else {
        unreachable!()
    };
    verdict.approved = false;
    assert!(
        !rejected(&scenario.view, gated),
        "a later gate can refuse a matched scope"
    );
    for case in [
        "epoch",
        "reference",
        "source",
        "scope",
        "argv",
        "environment",
        "limit",
        "revoked",
        "state",
        "decision",
    ] {
        let mut view = scenario.view.clone();
        let mut event = reused.clone();
        let Event::ToolStarted {
            verdict: Some(verdict),
            ..
        } = &mut event
        else {
            unreachable!()
        };
        match case {
            "epoch" => verdict.host.as_mut().unwrap().epoch = uuid::Uuid::now_v7(),
            "reference" => verdict.host.as_mut().unwrap().source_intent = Some([9; 32]),
            "source" => {
                view.runs[0].tools[0]
                    .permission_verdict
                    .as_mut()
                    .unwrap()
                    .source = PermissionSource::Mode
            }
            "scope" => verdict.host.as_mut().unwrap().session = true,
            "revoked" => view.host_epoch_floor = u64::MAX,
            "decision" => verdict.decision = Decision::Ask,
            "state" => {
                for tool in &mut view.runs[0].tools {
                    let ToolCall::CommandUnguarded { args, .. } = &mut tool.intent.call else {
                        unreachable!()
                    };
                    args.push("/state/settings.json".into());
                    if let Some(permission) = &mut tool.permission_verdict {
                        permission.intent_digest = tool.intent.digest();
                    }
                    if let Some(observation) = &mut tool.observation {
                        observation.intent = tool.intent.clone();
                        observation.attestation =
                            Some(crate::tools::Attestation::for_site(&tool.intent));
                    }
                }
                let intent = &view.runs[0].tools.last().unwrap().intent;
                verdict.intent_digest = intent.digest();
                let source = view.runs[0].tools[0].intent.digest();
                verdict.host.as_mut().unwrap().source_intent = Some(source);
                assert_eq!(view.host_approval(intent, epoch), Some(source));
            }
            _ => {
                let intent = &mut view.runs[0].tools.last_mut().unwrap().intent;
                match case {
                    "argv" => {
                        let ToolCall::CommandUnguarded { args, .. } = &mut intent.call else {
                            unreachable!()
                        };
                        args.push("other".into());
                    }
                    "environment" => {
                        intent.host_command.as_mut().unwrap().environment_digest = [9; 32]
                    }
                    "limit" => intent.limits.runtime_ms = scenario.host.limits.runtime_ms + 1,
                    _ => unreachable!(),
                }
                verdict.intent_digest = intent.digest();
            }
        }
        assert!(rejected(&view, event), "{case}");
    }
    assert_eq!(scenario.view.host_grants(epoch).count(), 1);
}

#[test]
fn host_replay_requires_primary_human_authority_and_pin_without_prior_failure() {
    use crate::tools::EnforcementCause;
    use crate::tools::permissions::{Decision, PermissionMode, PermissionSource};
    let failed = Some(("exit 7", ToolDisposition::Failed, None));
    for configured in [false, true] {
        let mode = configured.then_some(PermissionMode::Default);
        let human = |scenario: &Unguarded| {
            verdict_event(
                &scenario.host,
                mode,
                Decision::Ask,
                PermissionSource::Mode,
                PermissionSource::Human,
                true,
            )
        };
        let good = unguarded_scenario(failed, configured, true);
        assert!(!rejected(&good.view, human(&good)));

        for prior in [
            None,
            Some(("exit 8", ToolDisposition::Failed, None)),
            Some(("exit 7", ToolDisposition::Limit, None)),
            Some((
                "exit 7",
                ToolDisposition::Denied,
                Some(EnforcementCause::FilesystemDenied),
            )),
            Some(("exit 7", ToolDisposition::Succeeded, None)),
            Some(("exit 7", ToolDisposition::Uncertain, None)),
            Some(("exit 7", ToolDisposition::Cancelled, None)),
        ] {
            let scenario = unguarded_scenario(prior, configured, true);
            if !scenario.started {
                continue;
            }
            assert!(!rejected(&scenario.view, human(&scenario)), "{prior:?}");
            let mut denied = scenario.view.clone();
            apply(
                &mut denied,
                verdict_event(
                    &scenario.host,
                    mode,
                    Decision::Ask,
                    PermissionSource::Mode,
                    PermissionSource::UnavailableApproval,
                    false,
                ),
            )
            .expect("a denial is journaled without a qualifying failure");
            let mut refusal = observation_for(scenario.host.clone(), b"");
            refusal.disposition = ToolDisposition::Denied;
            refusal.attestation = None;
            refusal.output = "No identical command failed under the Guard".into();
            apply(
                &mut denied,
                Event::ToolFinished {
                    checkpoint: None,
                    run_id: scenario.run,
                    agent_run_id: scenario.agent,
                    observation: refusal,
                },
            )
            .expect("the refusal finishes");
        }

        for (basis, source, approved) in [
            (PermissionSource::Mode, PermissionSource::AiReview, true),
            (PermissionSource::Rule, PermissionSource::Rule, true),
            (PermissionSource::Mode, PermissionSource::Mode, true),
            (
                PermissionSource::UnavailableApproval,
                PermissionSource::UnavailableApproval,
                true,
            ),
            (
                PermissionSource::PinnedGrant,
                PermissionSource::PinnedGrant,
                true,
            ),
        ] {
            assert!(
                rejected(
                    &good.view,
                    verdict_event(&good.host, mode, Decision::Ask, basis, source, approved)
                ),
                "{source:?}"
            );
        }
        let exact = |scenario: &Unguarded| {
            verdict_event(
                &scenario.host,
                mode,
                Decision::Allow,
                PermissionSource::SessionApproval,
                PermissionSource::SessionApproval,
                true,
            )
        };
        assert!(rejected(&good.view, exact(&good)));
        let unpinned = unguarded_scenario(failed, configured, false);
        assert!(rejected(&unpinned.view, exact(&unpinned)));
        let forged_decision = verdict_event(
            &good.host,
            mode,
            Decision::Deny,
            PermissionSource::Mode,
            PermissionSource::Human,
            true,
        );
        assert!(rejected(&good.view, forged_decision));
    }

    let good = unguarded_scenario(failed, true, true);
    let mode = Some(PermissionMode::Default);
    let mut approved = good.view.clone();
    apply(
        &mut approved,
        verdict_event(
            &good.host,
            mode,
            Decision::Ask,
            PermissionSource::Mode,
            PermissionSource::Human,
            true,
        ),
    )
    .unwrap();
    let finish = |observation: ToolObservation| Event::ToolFinished {
        checkpoint: None,
        run_id: good.run,
        agent_run_id: good.agent,
        observation,
    };
    let mut host_run = observation_for(good.host.clone(), b"ok");
    assert!(matches!(
        host_run.attestation,
        Some(crate::tools::Attestation::Unguarded(_))
    ));
    let mut guard_receipt = host_run.clone();
    guard_receipt.attestation = Some(crate::tools::Attestation::Guard(
        crate::tools::GuardReceipt {
            contract_version: 1,
            intent_digest: good.host.digest(),
            enforcement_digest: good.host.enforcement_digest,
            limits: good.host.limits.clone(),
        },
    ));
    assert!(rejected(&approved, finish(guard_receipt)));
    let mut cached = host_run.clone();
    cached.cache_update = CacheUpdate::Published;
    assert!(rejected(&approved, finish(cached)));
    host_run.output =
        r#"{"selection":{"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code":0,"stdout":"","stderr":"","changeset":{"files":[]},"stdout_total":{"bytes":0,"lines":0},"stderr_total":{"bytes":0,"lines":0}}"#.into();
    let no_verdict = good.view.clone();
    assert!(rejected(&no_verdict, finish(host_run.clone())));
    let mut refused = good.view.clone();
    apply(
        &mut refused,
        verdict_event(
            &good.host,
            mode,
            Decision::Ask,
            PermissionSource::Mode,
            PermissionSource::Human,
            false,
        ),
    )
    .unwrap();
    assert!(rejected(&refused, finish(host_run.clone())));
    apply(&mut approved, finish(host_run.clone())).expect("the approved host run finishes");

    let integration = Event::ToolProposed {
        run_id: good.run,
        agent_run_id: good.agent,
        intent: child_intent(
            good.run,
            good.agent,
            ToolCall::ApplyChangeset {
                source: crate::session::worker::ChangesetSource::CommandReceipt {
                    source_intent_digest: good.host.digest(),
                    changeset: crate::tools::CommandChangeset { files: Vec::new() },
                },
            },
        )
        .into(),
        active_ceiling_ms: 0,
    };
    assert!(rejected(&approved, integration));

    let (mut child, goal, run, researcher) = research_session();
    child.runs[0]
        .config
        .as_mut()
        .unwrap()
        .tool_policy
        .as_mut()
        .unwrap()
        .permissions_digest = Some([3; 32]);
    apply(&mut child, reserve_child_call(goal, run, researcher)).unwrap();
    apply(
        &mut child,
        child_record(run, researcher, ProviderCallDisposition::ToolRequested),
    )
    .unwrap();
    let forged = child_intent(
        run,
        researcher,
        ToolCall::CommandUnguarded {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 7".into()],
            cwd: String::new(),
            justification: "Run the failed command once".into(),
            prefix_rule: None,
        },
    );
    assert!(rejected(&child, tool_proposed(run, researcher, &forged)));
}

fn host_prefix_scenario() -> (Unguarded, crate::tools::permissions::PermissionVerdict) {
    use crate::tools::permissions::{Decision, HostPrefixRule, PermissionMode, PermissionSource};
    let mut scenario = unguarded_scenario(None, true, false);
    scenario.host.call = ToolCall::CommandUnguarded {
        program: "printf".into(),
        args: vec!["value".into(), "first".into()],
        cwd: String::new(),
        justification: "Inspect synthetic host data".into(),
        prefix_rule: Some(vec!["printf".into(), "value".into()]),
    };
    let pin = scenario.host.host_command.as_mut().unwrap();
    pin.executable = "/usr/bin/printf".into();
    pin.permissions_digest = Some([3; 32]);
    let rule = HostPrefixRule::from_intent(&scenario.host, 2).unwrap();
    let run = &mut scenario.view.runs[0];
    run.tools.last_mut().unwrap().intent = scenario.host.clone();
    run.config
        .as_mut()
        .unwrap()
        .tool_policy
        .as_mut()
        .unwrap()
        .host_prefixes = vec![rule.clone()];
    let Event::ToolStarted {
        verdict: Some(mut verdict),
        ..
    } = verdict_event(
        &scenario.host,
        Some(PermissionMode::Default),
        Decision::Allow,
        PermissionSource::HostPrefixRule,
        PermissionSource::HostPrefixRule,
        true,
    )
    else {
        unreachable!()
    };
    verdict.host.as_mut().unwrap().rule = Some(rule);
    (scenario, verdict)
}

fn change_host_prefix_proof(
    scenario: &mut Unguarded,
    verdict: &mut crate::tools::permissions::PermissionVerdict,
    change: &str,
) {
    use crate::tools::permissions::{Decision, PermissionMode, PermissionSource};
    let run = &mut scenario.view.runs[0];
    let policy = run.config.as_mut().unwrap().tool_policy.as_mut().unwrap();
    let host = verdict.host.as_mut().unwrap();
    let pin = scenario.host.host_command.as_mut().unwrap();
    match change {
        "missing-rule" => host.rule = None,
        "missing-policy" => policy.host_prefixes.clear(),
        "matching-ask" | "matching-deny" => {
            let mut rule = policy.host_prefixes[0].clone();
            rule.decision = if change == "matching-ask" {
                Decision::Ask
            } else {
                Decision::Deny
            };
            policy.host_prefixes.push(rule);
        }
        "session" => host.session = true,
        "reference" => host.source_intent = Some([6; 32]),
        "digest" => pin.permissions_digest = Some([4; 32]),
        "unpinned" => policy.permissions_digest = None,
        "auto" => verdict.mode = Some(PermissionMode::Auto),
        "edits" => verdict.mode = Some(PermissionMode::AcceptEdits),
        "plan" => verdict.mode = Some(PermissionMode::Plan),
        "no-mode" => verdict.mode = None,
        "hook" => verdict.hook_restriction = Some(Decision::Ask),
        "basis" => verdict.basis = PermissionSource::Mode,
        "decision" => verdict.decision = Decision::Deny,
        "rule-deny" => {
            host.rule.as_mut().unwrap().decision = Decision::Deny;
            policy.host_prefixes[0].decision = Decision::Deny;
        }
        "scope" => scenario.host.workspace_inode += 1,
        "workspace" => pin.workspace = "/other-workspace".into(),
        "executable" => pin.sha256 = "b".repeat(64),
        "environment" => pin.environment_digest = [5; 32],
        "cwd" | "argv" | "state" => {
            let ToolCall::CommandUnguarded { args, cwd, .. } = &mut scenario.host.call else {
                unreachable!()
            };
            match change {
                "cwd" => *cwd = "nested".into(),
                "argv" => args[0] = "different".into(),
                "state" => args[1] = format!("{}/permissions.json", pin.state_roots[0]),
                _ => unreachable!(),
            }
        }
        "child" => {
            run.agents
                .iter_mut()
                .find(|agent| agent.id == scenario.agent)
                .unwrap()
                .role = AgentRole::Child
        }
        _ => unreachable!(),
    }
    run.tools.last_mut().unwrap().intent = scenario.host.clone();
    verdict.intent_digest = scenario.host.digest();
}

#[test]
fn host_prefix_replay_checks_each_approval_proof() {
    use crate::tools::permissions::PermissionMode;
    let event = |scenario: &Unguarded, verdict| Event::ToolStarted {
        intent: scenario.host.clone().into(),
        active_ceiling_ms: 0,
        run_id: scenario.run,
        agent_run_id: scenario.agent,
        verdict: Some(verdict),
    };
    for mode in [PermissionMode::Default, PermissionMode::DontAsk] {
        let (scenario, mut verdict) = host_prefix_scenario();
        verdict.mode = Some(mode);
        assert!(
            !rejected(&scenario.view, event(&scenario, verdict)),
            "{mode:?}"
        );
    }
    for change in [
        "missing-rule",
        "missing-policy",
        "matching-ask",
        "matching-deny",
        "session",
        "reference",
        "digest",
        "unpinned",
        "auto",
        "edits",
        "plan",
        "no-mode",
        "hook",
        "basis",
        "decision",
        "rule-deny",
        "scope",
        "workspace",
        "executable",
        "environment",
        "cwd",
        "argv",
        "state",
        "child",
    ] {
        let (mut scenario, mut verdict) = host_prefix_scenario();
        change_host_prefix_proof(&mut scenario, &mut verdict, change);
        assert!(
            rejected(&scenario.view, event(&scenario, verdict)),
            "{change}"
        );
    }
}

#[test]
fn verdict_mode_and_exact_rule_are_pinned_by_the_policy_receipt() {
    use crate::tools::permissions::{
        Decision, PermissionMode, PermissionSource, PermissionVerdict,
    };
    let (mut view, goal, run, agent) = primary_tool_session(true);
    view.runs[0]
        .config
        .as_mut()
        .unwrap()
        .tool_policy
        .as_mut()
        .unwrap()
        .permissions_digest = Some([3; 32]);
    apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
    apply(&mut view, requesting_step(run, agent, 1)).unwrap();
    let intent = read_intent(run, agent, "src/lib.rs");
    apply(&mut view, tool_proposed(run, agent, &intent)).unwrap();
    let verdict = |mode, decision, basis, source, approved| Event::ToolStarted {
        intent: intent.clone().into(),
        active_ceiling_ms: 0,
        run_id: run,
        agent_run_id: agent,
        verdict: Some(PermissionVerdict {
            host: None,
            intent_digest: intent.digest(),
            decision,
            mode,
            basis,
            source,
            approved,
            hook_restriction: None,
            hooks: 0,
            narrowed: false,
        }),
    };
    let exact = |approved| {
        verdict(
            Some(PermissionMode::Default),
            Decision::Allow,
            PermissionSource::SessionApproval,
            PermissionSource::SessionApproval,
            approved,
        )
    };
    let ordinary = verdict(
        Some(PermissionMode::Default),
        Decision::Deny,
        PermissionSource::Rule,
        PermissionSource::Rule,
        false,
    );
    assert!(rejected(
        &view,
        verdict(
            None,
            Decision::Deny,
            PermissionSource::Rule,
            PermissionSource::Rule,
            false
        )
    ));
    assert!(rejected(&view, exact(true)));
    let mut pinned = view.clone();
    pinned.runs[0]
        .config
        .as_mut()
        .unwrap()
        .tool_policy
        .as_mut()
        .unwrap()
        .unguarded_rules_digest = Some([5; 32]);
    assert!(rejected(&pinned, exact(true)));
    assert!(!rejected(&view, ordinary.clone()));
    apply(&mut view, ordinary).expect("the ordinary verdict is admitted");
}

#[test]
fn host_git_reducer_requires_human_verdict_host_receipt_and_stops_unknown_remote() {
    use crate::tools::permissions::{Decision, PermissionMode, PermissionSource};
    for call in [
        ToolCall::GitPush {
            remote: "https://github.com/o/r.git".into(),
            branch: "topic".into(),
        },
        ToolCall::GhPrCreate {
            repository: "github.com/o/r".into(),
            base: "main".into(),
            head: "topic".into(),
            title: "T".into(),
            body: String::new(),
            draft: true,
        },
    ] {
        let (mut view, goal, run, agent) = primary_tool_session(false);
        view.runs[0]
            .config
            .as_mut()
            .unwrap()
            .tool_policy
            .as_mut()
            .unwrap()
            .permissions_digest = Some([3; 32]);
        apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
        apply(&mut view, requesting_step(run, agent, 1)).unwrap();
        let mut intent = child_intent(run, agent, call);
        intent.host = Some(crate::tools::types::HostIntent {
            source_oid: "1".repeat(40),
            refspec: format!("{}:refs/heads/topic", "1".repeat(40)),
        });
        intent.limits.network = crate::tools::types::NetworkGrant::Host;
        apply(&mut view, tool_proposed(run, agent, &intent)).unwrap();
        for source in [
            PermissionSource::Mode,
            PermissionSource::Rule,
            PermissionSource::TypedPrefixRule,
            PermissionSource::AiReview,
            PermissionSource::SessionApproval,
        ] {
            assert!(
                rejected(
                    &view,
                    verdict_event(
                        &intent,
                        Some(PermissionMode::Auto),
                        Decision::Ask,
                        PermissionSource::Mode,
                        source,
                        true
                    )
                ),
                "{source:?}"
            );
        }
        for mode in [PermissionMode::Plan, PermissionMode::DontAsk] {
            assert!(rejected(
                &view,
                verdict_event(
                    &intent,
                    Some(mode),
                    Decision::Ask,
                    PermissionSource::Mode,
                    PermissionSource::Human,
                    true
                )
            ));
        }
        let mut observed = observation_for(intent.clone(), b"host: remote operation completed.");
        let event = |observation| Event::ToolFinished {
            checkpoint: None,
            run_id: run,
            agent_run_id: agent,
            observation,
        };
        assert!(rejected(&view, event(observed.clone())));
        apply(
            &mut view,
            verdict_event(
                &intent,
                Some(PermissionMode::Auto),
                Decision::Ask,
                PermissionSource::Mode,
                PermissionSource::Human,
                true,
            ),
        )
        .unwrap();
        let host = observed.attestation.clone();
        observed.attestation = Some(crate::tools::Attestation::Guard(
            crate::tools::GuardReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                enforcement_digest: intent.enforcement_digest,
                limits: intent.limits.clone(),
            },
        ));
        assert!(rejected(&view, event(observed.clone())));
        let Some(crate::tools::Attestation::Host(receipt)) = host.clone() else {
            panic!("host receipt");
        };
        observed.attestation = Some(crate::tools::Attestation::Unguarded(receipt));
        assert!(rejected(&view, event(observed.clone())));
        observed.attestation = host;
        assert!(!rejected(&view, event(observed.clone())));
        observed.disposition = ToolDisposition::Uncertain;
        observed.output = "host: remote outcome unknown; never retry automatically".into();
        apply(&mut view, event(observed)).unwrap();
        assert!(rejected(&view, tool_proposed(run, agent, &intent)));
    }
}

#[test]
fn host_pr_list_accepts_only_a_pinned_typed_rule_or_human_verdict() {
    use crate::tools::permissions::{Decision, PermissionMode, PermissionSource};
    let (mut view, goal, run, agent) = primary_tool_session(false);
    view.runs[0]
        .config
        .as_mut()
        .unwrap()
        .tool_policy
        .as_mut()
        .unwrap()
        .permissions_digest = Some([3; 32]);
    apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
    apply(&mut view, requesting_step(run, agent, 1)).unwrap();
    let mut intent = child_intent(
        run,
        agent,
        ToolCall::GhPrList {
            repository: "github.com/o/r".into(),
            state: crate::tools::types::DEFAULT_PR_STATE,
            base: None,
            head: None,
            limit: crate::tools::types::DEFAULT_PR_LIMIT,
        },
    );
    intent.host = Some(crate::tools::types::HostIntent {
        source_oid: "gh-pr-list-v1".into(),
        refspec: "fixed-list".into(),
    });
    intent.limits.network = crate::tools::types::NetworkGrant::Host;
    apply(&mut view, tool_proposed(run, agent, &intent)).unwrap();
    assert_host_pr_list_forbidden_verdicts(&view, &intent);
    let typed = verdict_event(
        &intent,
        Some(PermissionMode::Plan),
        Decision::Allow,
        PermissionSource::TypedPrefixRule,
        PermissionSource::TypedPrefixRule,
        true,
    );
    apply(&mut view, typed).unwrap();
    let mut observation = observation_for(
        intent.clone(),
        br#"{"pull_requests":[],"possibly_more":false,"truncated":false}"#,
    );
    observation.attestation = Some(crate::tools::Attestation::Host(
        crate::tools::UnguardedReceipt {
            contract_version: 1,
            intent_digest: intent.digest(),
            executable_sha256: "a".repeat(64),
            pid: 1,
            exit_code: Some(0),
            descendants_confirmed_stopped: true,
        },
    ));
    let finish = Event::ToolFinished {
        checkpoint: None,
        run_id: run,
        agent_run_id: agent,
        observation,
    };
    apply(&mut view, finish).expect("typed host read is committed");
}

fn assert_host_pr_list_forbidden_verdicts(view: &SessionView, intent: &EffectIntent) {
    use crate::tools::permissions::{Decision, PermissionMode, PermissionSource};
    for (mode, decision, basis, source) in [
        (
            Some(PermissionMode::Plan),
            Decision::Allow,
            PermissionSource::Rule,
            PermissionSource::Rule,
        ),
        (
            Some(PermissionMode::Plan),
            Decision::Ask,
            PermissionSource::Mode,
            PermissionSource::AiReview,
        ),
        (
            Some(PermissionMode::Plan),
            Decision::Ask,
            PermissionSource::Mode,
            PermissionSource::SessionApproval,
        ),
        (
            Some(PermissionMode::DontAsk),
            Decision::Ask,
            PermissionSource::Mode,
            PermissionSource::Human,
        ),
        (
            None,
            Decision::Allow,
            PermissionSource::TypedPrefixRule,
            PermissionSource::TypedPrefixRule,
        ),
    ] {
        assert!(
            rejected(
                view,
                verdict_event(intent, mode, decision, basis, source, true)
            ),
            "{source:?} in {mode:?}"
        );
    }
}

fn pending_host_pr_view() -> (SessionView, RunId, AgentRunId, crate::tools::EffectIntent) {
    use crate::tools::permissions::{Decision, PermissionMode, PermissionSource};
    let (mut view, goal, run, agent) = primary_tool_session(false);
    view.runs[0]
        .config
        .as_mut()
        .unwrap()
        .tool_policy
        .as_mut()
        .unwrap()
        .permissions_digest = Some([3; 32]);
    apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
    apply(&mut view, requesting_step(run, agent, 1)).unwrap();
    let mut intent = child_intent(
        run,
        agent,
        ToolCall::GhPrView {
            repository: "github.com/o/r".into(),
            number: 17,
        },
    );
    intent.host = Some(crate::tools::types::HostIntent {
        source_oid: "gh-pr-view-v1".into(),
        refspec: "fixed-view-v1".into(),
    });
    intent.limits.network = crate::tools::types::NetworkGrant::Host;
    apply(&mut view, tool_proposed(run, agent, &intent)).unwrap();
    apply(
        &mut view,
        verdict_event(
            &intent,
            Some(PermissionMode::Plan),
            Decision::Allow,
            PermissionSource::TypedPrefixRule,
            PermissionSource::TypedPrefixRule,
            true,
        ),
    )
    .unwrap();
    (view, run, agent, intent)
}

#[test]
fn host_pr_view_replays_only_with_typed_rule_and_host_receipt() {
    let (mut view, run, agent, intent) = pending_host_pr_view();
    let mut observation = observation_for(
        intent.clone(),
        br#"{"pull_request":{"number":17,"title":"Review","url":"https://github.com/o/r/pull/17","author":"octocat","state":"OPEN","draft":false,"base":"main","base_oid":"a","head":"topic","head_oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","checks_head_oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","review_decision":null,"discussion_review_decision":null,"updated_at":"now","body":"","mergeable":"MERGEABLE","comments":{"items":[],"truncated":false},"reviews":{"items":[],"truncated":false},"checks":null,"stale":false,"truncated_fields":[]}}"#,
    );
    observation.attestation = Some(crate::tools::Attestation::Host(
        crate::tools::UnguardedReceipt {
            contract_version: 1,
            intent_digest: intent.digest(),
            executable_sha256: "a".repeat(64),
            pid: 1,
            exit_code: Some(0),
            descendants_confirmed_stopped: true,
        },
    ));
    for (field, replacement) in [
        ("number", serde_json::json!(18)),
        (
            "url",
            serde_json::json!("https://github.com/other/repo/pull/17"),
        ),
    ] {
        let mut invalid = observation.clone();
        let mut output: serde_json::Value = serde_json::from_str(&invalid.output).unwrap();
        output["pull_request"][field] = replacement;
        invalid.output = output.to_string();
        assert!(rejected(
            &view,
            Event::ToolFinished {
                checkpoint: None,
                run_id: run,
                agent_run_id: agent,
                observation: invalid,
            }
        ));
    }
    let mut stale = observation.clone();
    let mut output: serde_json::Value = serde_json::from_str(&stale.output).unwrap();
    output["pull_request"]["checks_head_oid"] =
        serde_json::json!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    stale.output = output.to_string();
    assert!(rejected(
        &view,
        Event::ToolFinished {
            checkpoint: None,
            run_id: run,
            agent_run_id: agent,
            observation: stale,
        }
    ));
    apply(
        &mut view,
        Event::ToolFinished {
            checkpoint: None,
            run_id: run,
            agent_run_id: agent,
            observation,
        },
    )
    .expect("typed host PR view replays");
}

#[test]
fn host_pr_view_failure_text_is_operation_specific_on_replay() {
    const RESULT_BUDGET: &str =
        "host: pull request inspection did not fit the result budget; nothing was reported.";
    for (output, exit_code, accepted) in [
        (
            "host: GitHub pull request inspection failed; no result was reported.",
            7,
            true,
        ),
        (
            "host: GitHub pull request list failed; no result was reported.",
            7,
            false,
        ),
        (RESULT_BUDGET, 0, true),
        (RESULT_BUDGET, 7, false),
    ] {
        let (mut view, run, agent, intent) = pending_host_pr_view();
        let mut failure = observation_for(intent.clone(), b"view failed");
        failure.disposition = crate::tools::ToolDisposition::Failed;
        failure.output = output.into();
        failure.attestation = Some(crate::tools::Attestation::Host(
            crate::tools::UnguardedReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                executable_sha256: "a".repeat(64),
                pid: 2,
                exit_code: Some(exit_code),
                descendants_confirmed_stopped: true,
            },
        ));
        if output == RESULT_BUDGET && accepted {
            let mut missing_receipt = failure.clone();
            missing_receipt.attestation = None;
            assert!(rejected(
                &view,
                Event::ToolFinished {
                    checkpoint: None,
                    run_id: run,
                    agent_run_id: agent,
                    observation: missing_receipt,
                }
            ));
        }
        let event = Event::ToolFinished {
            checkpoint: None,
            run_id: run,
            agent_run_id: agent,
            observation: failure,
        };
        if accepted {
            apply(&mut view, event).expect("view-specific failure replays");
        } else {
            assert!(rejected(&view, event), "{output}: exit {exit_code}");
        }
    }
}

#[test]
fn host_pr_list_accepts_a_human_plan_verdict_through_tool_completion() {
    use crate::tools::permissions::{Decision, PermissionMode, PermissionSource};
    let (mut view, goal, run, agent) = primary_tool_session(false);
    view.runs[0]
        .config
        .as_mut()
        .unwrap()
        .tool_policy
        .as_mut()
        .unwrap()
        .permissions_digest = Some([3; 32]);
    apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
    apply(&mut view, requesting_step(run, agent, 1)).unwrap();
    let mut intent = child_intent(
        run,
        agent,
        ToolCall::GhPrList {
            repository: "github.com/o/r".into(),
            state: crate::tools::types::DEFAULT_PR_STATE,
            base: None,
            head: None,
            limit: crate::tools::types::DEFAULT_PR_LIMIT,
        },
    );
    intent.host = Some(crate::tools::types::HostIntent {
        source_oid: "gh-pr-list-v1".into(),
        refspec: "fixed-list".into(),
    });
    intent.limits.network = crate::tools::types::NetworkGrant::Host;
    apply(&mut view, tool_proposed(run, agent, &intent)).unwrap();
    apply(
        &mut view,
        verdict_event(
            &intent,
            Some(PermissionMode::Plan),
            Decision::Ask,
            PermissionSource::Mode,
            PermissionSource::Human,
            true,
        ),
    )
    .expect("human Plan approval is accepted for a host read");
    let mut observation = observation_for(
        intent.clone(),
        b"host: GitHub pull request list exceeded a capture limit.",
    );
    observation.disposition = ToolDisposition::Failed;
    observation.attestation = Some(crate::tools::Attestation::Host(
        crate::tools::UnguardedReceipt {
            contract_version: 1,
            intent_digest: intent.digest(),
            executable_sha256: "a".repeat(64),
            pid: 1,
            exit_code: None,
            descendants_confirmed_stopped: true,
        },
    ));
    apply(
        &mut view,
        Event::ToolFinished {
            checkpoint: None,
            run_id: run,
            agent_run_id: agent,
            observation,
        },
    )
    .expect("human-approved host read is committed");
}

#[test]
fn narrowed_host_pr_list_result_replays_within_its_pinned_budget() {
    let mut fixture = crate::session::host_protocol_fixture();
    let budget = 700;
    let intent = fixture
        .events
        .iter_mut()
        .find_map(|event| match event {
            Event::ToolStarted { intent, .. }
                if matches!(intent.call, ToolCall::GitPush { .. }) =>
            {
                intent.call = ToolCall::GhPrList {
                    repository: "github.com/o/r".into(),
                    state: crate::tools::types::DEFAULT_PR_STATE,
                    base: None,
                    head: None,
                    limit: 3,
                };
                intent.host = Some(crate::tools::types::HostIntent {
                    source_oid: "gh-pr-list-v1".into(),
                    refspec: "fixed-list".into(),
                });
                intent.limits.network = crate::tools::types::NetworkGrant::Host;
                intent.limits.result_bytes = budget;
                Some(intent.clone())
            }
            _ => None,
        })
        .expect("pending host read");
    let intent = crate::tools::EffectIntent::from(intent);
    for event in &mut fixture.events {
        if let Event::ToolProposed {
            intent: proposed, ..
        } = event
        {
            *proposed = intent.clone().into();
        }
        if let Event::ToolStarted {
            verdict: Some(verdict),
            ..
        } = event
        {
            verdict.intent_digest = intent.digest();
        }
    }
    let output = serde_json::json!({
        "pull_requests": [{
            "number": 17,
            "title": "Review patch",
            "url": "https://github.com/o/r/pull/17",
            "author": "octocat",
            "state": "OPEN",
            "draft": false,
            "base": "main",
            "base_oid": "base-sha",
            "head": "topic",
            "head_oid": "head-sha",
            "review_decision": "APPROVED",
            "updated_at": "2026-10-25T12:00:00Z",
            "truncated_fields": []
        }],
        "possibly_more": true,
        "truncated": true
    })
    .to_string();
    assert!(output.len() <= budget as usize);
    let observation = ToolObservation {
        spill: None,
        enforcement_cause: None,
        limit_cause: None,
        attestation: Some(crate::tools::Attestation::Host(
            crate::tools::UnguardedReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                executable_sha256: "a".repeat(64),
                pid: 1,
                exit_code: Some(0),
                descendants_confirmed_stopped: true,
            },
        )),
        intent,
        disposition: ToolDisposition::Succeeded,
        output: output.clone(),
        approval_cause: None,
        cache_update: crate::tools::CacheUpdate::NotApplicable,
    };
    assert!(observation.valid());
    fixture.events.push(Event::ToolFinished {
        checkpoint: None,
        run_id: fixture.run,
        agent_run_id: fixture.primary,
        observation,
    });
    let view = SessionView::replay_open(fixture.session, &fixture.envelopes())
        .expect("narrowed Tool result replays")
        .expect("fixture Session");
    assert_eq!(
        view.runs[0]
            .tools
            .last()
            .unwrap()
            .observation
            .as_ref()
            .unwrap()
            .output,
        output
    );
}

#[test]
fn minimum_budget_host_pr_list_preparation_refusal_finishes_and_replays() {
    let mut fixture = crate::session::host_protocol_fixture();
    let budget = 128;
    let intent = fixture
        .events
        .iter_mut()
        .find_map(|event| match event {
            Event::ToolStarted { intent, .. }
                if matches!(intent.call, ToolCall::GitPush { .. }) =>
            {
                intent.call = ToolCall::GhPrList {
                    repository: "github.com/o/r".into(),
                    state: crate::tools::types::DEFAULT_PR_STATE,
                    base: None,
                    head: None,
                    limit: crate::tools::types::DEFAULT_PR_LIMIT,
                };
                intent.host = Some(crate::tools::types::HostIntent {
                    source_oid: "gh-pr-list-v1".into(),
                    refspec: "fixed-list".into(),
                });
                intent.limits.network = crate::tools::types::NetworkGrant::Host;
                intent.limits.result_bytes = budget;
                Some(intent.clone())
            }
            _ => None,
        })
        .expect("pending host read");
    let intent = crate::tools::EffectIntent::from(intent);
    for event in &mut fixture.events {
        if let Event::ToolProposed {
            intent: proposed, ..
        } = event
        {
            *proposed = intent.clone().into();
        }
        if let Event::ToolStarted {
            verdict: Some(verdict),
            ..
        } = event
        {
            verdict.intent_digest = intent.digest();
        }
    }
    let observation = ToolObservation {
        spill: None,
        intent,
        disposition: ToolDisposition::Denied,
        output: "host: refused; nothing dispatched.".into(),
        attestation: None,
        cache_update: crate::tools::CacheUpdate::NotApplicable,
        enforcement_cause: None,
        limit_cause: None,
        approval_cause: None,
    };
    assert!(observation.valid());
    fixture.events.push(Event::ToolFinished {
        checkpoint: None,
        run_id: fixture.run,
        agent_run_id: fixture.primary,
        observation,
    });
    let view = SessionView::replay_open(fixture.session, &fixture.envelopes())
        .expect("minimum-budget refusal replays")
        .expect("fixture Session");
    let result = view.runs[0]
        .tools
        .last()
        .unwrap()
        .observation
        .as_ref()
        .unwrap();
    assert_eq!(result.disposition, ToolDisposition::Denied);
    assert_eq!(result.output, "host: refused; nothing dispatched.");
}

#[test]
fn typed_prefix_verdicts_require_primary_settings_pin_and_admitted_mode() {
    use crate::tools::permissions::{Decision, PermissionMode, PermissionSource};
    let commit = ToolCall::GitCommit {
        message: "m".into(),
        paths: vec!["src/lib.rs".into()],
        staged: false,
    };
    let started = |pinned: bool, call: ToolCall| {
        let (mut view, goal, run, agent) = primary_tool_session(false);
        if pinned {
            view.runs[0]
                .config
                .as_mut()
                .unwrap()
                .tool_policy
                .as_mut()
                .unwrap()
                .permissions_digest = Some([3; 32]);
        }
        apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
        apply(&mut view, requesting_step(run, agent, 1)).unwrap();
        let intent = child_intent(run, agent, call);
        apply(&mut view, tool_proposed(run, agent, &intent)).unwrap();
        (view, intent)
    };
    let typed = PermissionSource::TypedPrefixRule;
    let (view, intent) = started(true, commit.clone());
    let event = |mode, decision, basis, source, approved| {
        verdict_event(&intent, mode, decision, basis, source, approved)
    };
    let allowed = |mode| event(mode, Decision::Allow, typed, typed, true);
    for mode in [PermissionMode::Default, PermissionMode::DontAsk] {
        assert!(!rejected(&view, allowed(Some(mode))), "{mode:?}");
    }
    for mode in [
        PermissionMode::Plan,
        PermissionMode::AcceptEdits,
        PermissionMode::Auto,
    ] {
        assert!(rejected(&view, allowed(Some(mode))), "{mode:?}");
    }
    assert!(rejected(&view, allowed(None)));
    let default = Some(PermissionMode::Default);
    assert!(rejected(
        &view,
        event(
            default,
            Decision::Allow,
            PermissionSource::Rule,
            typed,
            true
        )
    ));
    assert!(!rejected(
        &view,
        event(default, Decision::Deny, typed, typed, false)
    ));
    assert!(!rejected(
        &view,
        event(default, Decision::Ask, typed, PermissionSource::Human, true)
    ));
    assert!(!rejected(
        &view,
        event(
            default,
            Decision::Ask,
            typed,
            PermissionSource::UnavailableApproval,
            false
        )
    ));
    assert!(rejected(
        &view,
        event(
            default,
            Decision::Ask,
            typed,
            PermissionSource::AiReview,
            true
        )
    ));
    // A later plan, budget or pause gate may refuse dispatch of an allowed call.
    assert!(!rejected(
        &view,
        event(default, Decision::Allow, typed, typed, false)
    ));
    assert!(!rejected(
        &view,
        event(
            Some(PermissionMode::DontAsk),
            Decision::Allow,
            typed,
            typed,
            false
        )
    ));
    for mode in [
        PermissionMode::Plan,
        PermissionMode::AcceptEdits,
        PermissionMode::Auto,
    ] {
        assert!(
            rejected(
                &view,
                event(Some(mode), Decision::Allow, typed, typed, false)
            ),
            "{mode:?}"
        );
    }
    let mut once = view.clone();
    apply(&mut once, allowed(default)).unwrap();
    assert!(rejected(&once, allowed(default)));

    let (read_view, read) = started(
        true,
        ToolCall::Read {
            path: "a".into(),
            offset: None,
            limit: Some(1024),
        },
    );
    assert!(rejected(
        &read_view,
        verdict_event(&read, default, Decision::Allow, typed, typed, true)
    ));
    let (unpinned, intent) = started(false, commit.clone());
    for mode in [None, default] {
        assert!(rejected(
            &unpinned,
            verdict_event(&intent, mode, Decision::Allow, typed, typed, true)
        ));
    }
    let first_use = |basis, source, approved, decision| {
        verdict_event(&intent, None, decision, basis, source, approved)
    };
    let mode = PermissionSource::Mode;
    assert!(!rejected(
        &unpinned,
        first_use(mode, PermissionSource::Human, true, Decision::Ask)
    ));
    assert!(!rejected(
        &unpinned,
        first_use(
            mode,
            PermissionSource::UnavailableApproval,
            false,
            Decision::Ask
        )
    ));
    for source in [
        PermissionSource::Rule,
        PermissionSource::SessionApproval,
        PermissionSource::Hook,
        typed,
    ] {
        assert!(rejected(
            &unpinned,
            first_use(mode, source, true, Decision::Ask)
        ));
    }
    assert!(rejected(
        &unpinned,
        first_use(typed, PermissionSource::Human, true, Decision::Ask)
    ));
    let (read_unpinned, read) = started(
        false,
        ToolCall::Read {
            path: "a".into(),
            offset: None,
            limit: Some(1024),
        },
    );
    assert!(rejected(
        &read_unpinned,
        verdict_event(
            &read,
            None,
            Decision::Ask,
            mode,
            PermissionSource::Human,
            true
        )
    ));

    let (mut child, goal, run, researcher) = research_session();
    child.runs[0]
        .config
        .as_mut()
        .unwrap()
        .tool_policy
        .as_mut()
        .unwrap()
        .permissions_digest = Some([3; 32]);
    apply(&mut child, reserve_child_call(goal, run, researcher)).unwrap();
    apply(
        &mut child,
        child_record(run, researcher, ProviderCallDisposition::ToolRequested),
    )
    .unwrap();
    let intent = child_intent(run, researcher, commit);
    assert!(rejected(&child, tool_proposed(run, researcher, &intent)));
}

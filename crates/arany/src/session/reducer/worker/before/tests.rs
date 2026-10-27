use super::*;

#[test]
fn forged_invalid_editor_delegations_reject_before_child_work() {
    use crate::test_support::worker_teams::{INVALID_EDITOR_CASES, invalidate_editor};
    for (case, cause) in INVALID_EDITOR_CASES {
        let mut f = fixture();
        let delegation = f
            .events
            .iter()
            .position(|e| {
                matches!(e,
            Event::ProviderCallRecorded { record, .. } if record.delegation.is_some())
            })
            .unwrap();
        let mut event = f.events[delegation].clone();
        if let Event::ProviderCallRecorded { record, .. } = &mut event
            && let ChildAssignment::Editor { assignment } =
                &mut record.delegation.as_mut().unwrap()[0]
        {
            invalidate_editor(assignment, case);
        }
        if *case == "invalid_turns" {
            for event in &mut f.events {
                if let Event::RunStarted { config, .. } = event {
                    config.worker_policy.as_mut().unwrap().max_turns = 1;
                }
            }
        }
        f.events.truncate(delegation);
        let view = f.view();
        let Event::ProviderCallRecorded { record, .. } = &event else {
            unreachable!()
        };
        let ChildAssignment::Editor { assignment } = &record.delegation.as_ref().unwrap()[0] else {
            unreachable!()
        };
        assert_eq!(
            super::worker::validate_request(&view, &view.runs[0], assignment, None),
            Err(*cause),
            "{case}"
        );
        refused(&f, event, case);
    }
}

#[test]
fn worker_protocol_child_permission_mode_never_removes_parent_approval() {
    use crate::tools::permissions::{
        Decision, PermissionMode, PermissionSource, PermissionVerdict,
    };
    let mut f = fixture();
    if let Event::RunStarted { config, .. } = f
        .events
        .iter_mut()
        .find(|e| matches!(e, Event::RunStarted { .. }))
        .unwrap()
    {
        config
            .worker_policy
            .as_mut()
            .unwrap()
            .parent_grant
            .permission_mode = Some(PermissionMode::Default);
    }
    if let Event::AgentSpawned {
        profile: None,
        assignment: Some(p),
        ..
    } = f.events.last_mut().unwrap()
    {
        p.grant.permission_mode = Some(PermissionMode::AcceptEdits);
    }
    f.call(
        f.child,
        AgentPhase::ChildWork,
        ProviderCallDisposition::ToolRequested,
        None,
    );
    let intent = f.intent(
        f.child,
        ToolCall::Write {
            path: "src/a".into(),
            expected_digest: None,
            content: "new".into(),
        },
        EffectTarget::Workspace,
    );
    f.started(&intent);
    f.push(Event::ToolStarted {
        intent: intent.clone().into(),
        active_ceiling_ms: 0,
        run_id: f.run,
        agent_run_id: f.child,
        verdict: Some(PermissionVerdict {
            host: None,
            intent_digest: intent.digest(),
            decision: Decision::Allow,
            mode: Some(PermissionMode::AcceptEdits),
            basis: PermissionSource::Mode,
            source: PermissionSource::Mode,
            approved: true,
            hook_restriction: None,
            hooks: 0,
            narrowed: false,
        }),
    });
    let invalid = f.events.pop().unwrap();
    refused(
        &f,
        invalid.clone(),
        "child acceptEdits cannot erase parent Ask",
    );
    let mut approved = invalid;
    let Event::ToolStarted {
        verdict: Some(verdict),
        ..
    } = &mut approved
    else {
        unreachable!()
    };
    verdict.decision = Decision::Ask;
    verdict.source = PermissionSource::Human;
    f.push(approved);
    f.view();
}

#[test]
fn worker_protocol_researcher_only_run_rejects_a_call_after_an_invalid_synthesis() {
    let mut f = fixture();
    let delegated = f
        .events
        .iter()
        .position(|e| matches!(e, Event::ProviderCallRecorded { record, .. } if record.delegation.is_some()))
        .unwrap();
    f.events.truncate(delegated + 1);
    let Event::ProviderCallRecorded { record, .. } = &mut f.events[delegated] else {
        unreachable!()
    };
    record.delegation = Some(vec![ChildAssignment::Researcher {
        objective: "Research".into(),
        definition: None,
        profile: None,
    }]);
    let researcher = AgentRunId::new();
    f.push(Event::AgentSpawned {
        run_id: f.run,
        agent_run_id: researcher,
        ordinal: 1,
        role: AgentRole::Child,
        objective: Some("Research".into()),
        grant: Some(ChildGrant::ReadOnly {
            unguarded: crate::tools::UnguardedGrant::Denied,
        }),
        definition: None,
        profile: None,
        assignment: None,
    });
    f.call(
        researcher,
        AgentPhase::ChildWork,
        ProviderCallDisposition::Finished,
        None,
    );
    f.push(Event::AgentFinished {
        run_id: f.run,
        agent_run_id: researcher,
        disposition: AgentDisposition::Finished,
        summary: Some("done".into()),
        result: Some("done".into()),
        worker: None,
    });
    f.call(
        f.primary,
        AgentPhase::RootSynthesis,
        ProviderCallDisposition::InvalidResponse,
        None,
    );
    f.view();
    let mut again = Fixture {
        events: f.events.clone(),
        ..f
    };
    again.call(
        again.primary,
        AgentPhase::RootSynthesis,
        ProviderCallDisposition::Finished,
        None,
    );
    let recorded = again.events.pop().unwrap();
    refused(&again, recorded, "synthesis after an invalid synthesis");
}

fn refused(f: &Fixture, event: Event, name: &str) {
    let mut view = f.view();
    let envelope = f.envelope(event.clone(), f.events.len() as u64 + 1);
    assert!(view.apply_committed(&envelope).is_err(), "decoded {name}");
    if let Ok(payload) = event.payload() {
        let decoded = Event::decode(
            event.kind(),
            event.version(),
            &payload,
            envelope.run_id,
            envelope.agent_run_id,
        )
        .unwrap();
        assert!(
            f.view()
                .apply_committed(&EventEnvelope {
                    event: decoded,
                    ..envelope
                })
                .is_err(),
            "serialized {name}"
        );
    }
}

#[test]
fn editor_scope_and_primary_lease_admission_are_symmetric() {
    let f = fixture();
    let view = f.view();
    for (actor, path, allowed) in [
        (f.child, "src/a", true),
        (f.child, "src/other", false),
        (f.primary, "src/a", false),
        (f.primary, "src/other", true),
    ] {
        let intent = f.intent(
            actor,
            ToolCall::Write {
                path: path.into(),
                expected_digest: None,
                content: "new".into(),
            },
            EffectTarget::Workspace,
        );
        if actor == f.primary {
            assert_eq!(
                super::super::tool::validate_tool_lease(&view.runs[0], actor, &intent.call).is_ok(),
                allowed
            );
        } else {
            assert_eq!(
                view.runs[0].agents[1]
                    .assignment
                    .as_ref()
                    .unwrap()
                    .pins
                    .grant
                    .allows(&intent.call),
                allowed
            );
        }
    }
    for path in ["SRC/A", "src/a/nested"] {
        assert!(
            super::super::tool::validate_tool_lease(
                &view.runs[0],
                f.primary,
                &ToolCall::Write {
                    path: path.into(),
                    expected_digest: None,
                    content: "new".into()
                }
            )
            .is_err()
        );
    }
}

#[test]
fn background_assignments_are_admitted_but_ungranted_commands_refuse_before_spawn() {
    let f = fixture();
    let spawn = f.events.last().unwrap();
    let Event::AgentSpawned {
        assignment: Some(pins),
        ..
    } = spawn
    else {
        panic!("editor spawn")
    };
    let view = f.view();
    for commands in [false, true] {
        let mut request = pins.request.clone();
        if commands {
            request.commands = vec!["build".into()];
        } else {
            request.mode = WorkerMode::Background;
        }
        assert_eq!(
            validate_editor_request(&view, &view.runs[0], &request, None).is_err(),
            commands
        );
    }
}

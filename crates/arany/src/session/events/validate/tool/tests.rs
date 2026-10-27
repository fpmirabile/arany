use crate::session::events::tests::read_observation;
use crate::session::events::*;

#[test]
fn tool_events_reject_internal_rewind_sources() {
    let run = RunId::new();
    let agent = AgentRunId::new();
    let mut observation = read_observation(run, agent);
    observation.disposition = crate::tools::ToolDisposition::Denied;
    observation.attestation = None;
    let events = [
        Event::ToolStarted {
            verdict: None,
            run_id: run,
            agent_run_id: agent,
            intent: observation.intent.clone().into(),
            active_ceiling_ms: 1,
        },
        Event::ToolFinished {
            checkpoint: None,
            run_id: run,
            agent_run_id: agent,
            observation,
        },
    ];
    let call = crate::tools::ToolCall::ApplyChangeset {
        source: ChangesetSource::Rewind {
            plan_digest: [3; 32],
            bytes: 1,
            sha256: [4; 32],
        },
    };
    for mut event in events {
        let mut payload: serde_json::Value =
            serde_json::from_str(&event.payload().unwrap()).unwrap();
        let intent_valid = match &mut event {
            Event::ToolStarted { intent, .. } => {
                payload["intent"]["call"] = serde_json::to_value(&call).unwrap();
                intent.call = call.clone();
                crate::tools::EffectIntent::from(intent.clone()).valid()
            }
            Event::ToolFinished { observation, .. } => {
                payload["observation"]["intent"]["call"] = serde_json::to_value(&call).unwrap();
                observation.intent.call = call.clone();
                observation.intent.valid()
            }
            _ => unreachable!(),
        };
        assert!(intent_valid);
        if let Event::ToolFinished { observation, .. } = &event {
            assert!(observation.valid());
        }
        assert!(matches!(event.validate(), Err(ReplayError::InvalidPayload)));
        assert!(matches!(
            Event::decode(
                event.kind(),
                event.version(),
                &payload.to_string(),
                Some(run),
                Some(agent),
            ),
            Err(ReplayError::InvalidPayload)
        ));
    }
}

#[test]
fn a_tool_finished_bump_rejects_its_previous_version() {
    let run = RunId::new();
    let agent = AgentRunId::new();
    let finished = Event::ToolFinished {
        checkpoint: None,
        run_id: run,
        agent_run_id: agent,
        observation: read_observation(run, agent),
    };
    for cause in [
        crate::tools::ApprovalCause::Cancelled,
        crate::tools::ApprovalCause::Paused,
        crate::tools::ApprovalCause::ChannelUnavailable,
        crate::tools::ApprovalCause::Declined,
        crate::tools::ApprovalCause::ChildTimeout,
    ] {
        let mut stopped = finished.clone();
        let Event::ToolFinished { observation, .. } = &mut stopped else {
            unreachable!()
        };
        observation.disposition = crate::tools::ToolDisposition::Denied;
        observation.attestation = None;
        observation.approval_cause = Some(cause);
        observation.output = "Opaque historical detail".into();
        assert_eq!(
            Event::decode(
                stopped.kind(),
                stopped.version(),
                &stopped.payload().unwrap(),
                Some(run),
                Some(agent)
            )
            .unwrap(),
            stopped
        );
    }
    let payload = finished.payload().expect("payload");
    let current = finished.version();
    assert_eq!(payload_version("ToolFinished"), Some(current));

    assert_eq!(
        Event::decode("ToolFinished", current, &payload, Some(run), Some(agent)).expect("current"),
        finished
    );
    let mut missing_cause: serde_json::Value = serde_json::from_str(&payload).unwrap();
    missing_cause
        .as_object_mut()
        .unwrap()
        .remove("approval_cause");
    assert!(
        Event::decode(
            finished.kind(),
            finished.version(),
            &missing_cause.to_string(),
            Some(run),
            Some(agent)
        )
        .is_err()
    );
    let mut missing: serde_json::Value = serde_json::from_str(&payload).unwrap();
    missing.as_object_mut().unwrap().remove("checkpoint");
    assert!(
        Event::decode(
            "ToolFinished",
            current,
            &missing.to_string(),
            Some(run),
            Some(agent)
        )
        .is_err()
    );
    for invalid in [
        payload.replacen("\"checkpoint\":null", "\"checkpoint\":null,\"checkpoint\":null", 1),
        payload.replacen("\"checkpoint\":null", "\"checkpoint\":null,\"unknown\":null", 1),
        payload.replacen("\"checkpoint\":null", "\"checkpoint\":{\"status\":\"captured\",\"reason\":\"quota\",\"pruned_before_run\":null}", 1),
    ] {
        assert!(Event::decode("ToolFinished", current, &invalid, Some(run), Some(agent)).is_err());
    }
    let mut missing: serde_json::Value = serde_json::from_str(&payload).unwrap();
    missing.as_object_mut().unwrap().remove("limit_cause");
    assert!(
        Event::decode(
            "ToolFinished",
            current,
            &missing.to_string(),
            Some(run),
            Some(agent)
        )
        .is_err()
    );
    let mut unknown: serde_json::Value = serde_json::from_str(&payload).unwrap();
    unknown["disposition"] = "limit".into();
    unknown["limit_cause"] = "invented".into();
    assert!(
        Event::decode(
            "ToolFinished",
            current,
            &unknown.to_string(),
            Some(run),
            Some(agent)
        )
        .is_err()
    );
    for previous in 1..current {
        assert!(matches!(
            Event::decode("ToolFinished", previous, &payload, Some(run), Some(agent)),
            Err(ReplayError::UnknownEvent)
        ));
    }
}

#[test]
fn the_unguarded_format_bumps_reject_their_previous_versions() {
    let run = RunId::new();
    let agent = AgentRunId::new();
    let mut intent = read_observation(run, agent).intent;
    intent.call = crate::tools::ToolCall::CommandUnguarded {
        program: "sh".into(),
        args: vec!["-c".into(), "exit 7".into()],
        cwd: String::new(),
        justification: "Run the failed command once".into(),
        prefix_rule: None,
    };
    let started = Event::ToolStarted {
        verdict: None,
        run_id: run,
        agent_run_id: agent,
        intent: intent.clone().into(),
        active_ceiling_ms: 1,
    };
    let verdict = Event::ToolStarted {
        intent: intent.clone().into(),
        active_ceiling_ms: 0,
        run_id: run,
        agent_run_id: agent,
        verdict: Some(crate::tools::permissions::PermissionVerdict {
            host: None,
            intent_digest: intent.digest(),
            decision: crate::tools::permissions::Decision::Allow,
            mode: None,
            basis: crate::tools::permissions::PermissionSource::SessionApproval,
            source: crate::tools::permissions::PermissionSource::SessionApproval,
            approved: true,
            hook_restriction: None,
            hooks: 0,
            narrowed: false,
        }),
    };
    let reservation = Event::ToolActiveTimeReserved {
        run_id: run,
        agent_run_id: agent,
        intent_digest: [3; 32],
        active_ceiling_ms: 7_000,
    };
    for event in [&started, &verdict, &reservation] {
        let payload = event.payload().expect("payload");
        let version = payload_version(event.kind()).expect("known kind");
        assert_eq!(
            Event::decode(event.kind(), version, &payload, Some(run), Some(agent))
                .expect("current"),
            *event
        );
        assert!(matches!(
            Event::decode(event.kind(), version - 1, &payload, Some(run), Some(agent)),
            Err(ReplayError::UnknownEvent)
        ));
    }
    assert_eq!(payload_version("ToolStarted"), Some(started.version()));
    assert_eq!(payload_version("ToolPermissionDecided"), None);
}

#[test]
fn tool_proposal_payload_round_trips_and_rejects_a_verdict() {
    let run = RunId::new();
    let agent = AgentRunId::new();
    let proposed = Event::ToolProposed {
        run_id: run,
        agent_run_id: agent,
        intent: read_observation(run, agent).intent.into(),
        active_ceiling_ms: 1,
    };
    let payload = proposed.payload().unwrap();
    assert!(!payload.contains("build_jobs"));
    assert!(!payload.contains("build_cpu_cores"));
    let mut build = proposed.clone();
    if let Event::ToolProposed { intent, .. } = &mut build {
        intent.call = crate::tools::ToolCall::Command {
            program: "cargo".into(),
            args: vec!["build".into()],
            cwd: String::new(),
        };
        intent.limits.resource_class = crate::tools::ToolResourceClass::Build;
        intent.limits.build_jobs = 2;
        intent.limits.build_cpu_cores = 8;
        intent.limits.cache_bytes = 0;
        intent.limits.cache_candidate_bytes = 0;
    }
    let bytes = build.payload().unwrap();
    let (kind, version) = (build.kind(), build.version());
    assert_eq!(
        Event::decode(kind, version, &bytes, Some(run), Some(agent)).unwrap(),
        build
    );
    assert_eq!(
        Event::decode(
            proposed.kind(),
            proposed.version(),
            &payload,
            Some(run),
            Some(agent)
        )
        .unwrap(),
        proposed
    );
    let mut extra: serde_json::Value = serde_json::from_str(&payload).unwrap();
    extra["verdict"] = serde_json::Value::Null;
    assert!(matches!(
        Event::decode(
            proposed.kind(),
            proposed.version(),
            &extra.to_string(),
            Some(run),
            Some(agent)
        ),
        Err(ReplayError::InvalidPayload)
    ));
}

#[test]
fn an_in_process_attestation_never_validates_a_guard_site_operation() {
    let run = RunId::new();
    let agent = AgentRunId::new();
    let read = read_observation(run, agent);
    assert!(read.valid());
    let mut drifted = read.clone();
    drifted.attestation = Some(crate::tools::Attestation::InProcess {
        receipt: None,
        policy_digest: [1; 32],
        workspace_device: 1,
        workspace_inode: 3,
    });
    assert!(!drifted.valid());

    let mut write = read.clone();
    write.intent.call = crate::tools::ToolCall::Write {
        path: "src/lib.rs".into(),
        expected_digest: None,
        content: "x".into(),
    };
    assert!(
        !write.valid(),
        "a read attestation cannot authorize a write"
    );
    assert!(matches!(
        Event::ToolFinished {
            checkpoint: None,
            run_id: run,
            agent_run_id: agent,
            observation: write.clone(),
        }
        .validate(),
        Err(ReplayError::InvalidPayload)
    ));

    let mut guarded_read = read;
    guarded_read.attestation = Some(crate::tools::Attestation::Guard(
        crate::tools::GuardReceipt {
            contract_version: 1,
            intent_digest: guarded_read.intent.digest(),
            enforcement_digest: guarded_read.intent.enforcement_digest,
            limits: guarded_read.intent.limits.clone(),
        },
    ));
    assert!(!guarded_read.valid(), "reads attest only in process");
}

#[test]
fn tool_preflight_refusal_payload_is_closed_and_keeps_its_fixed_reason() {
    let (run, agent) = (RunId::new(), AgentRunId::new());
    let event = Event::ToolPreflightRefused {
        run_id: run,
        agent_run_id: agent,
        record: ToolPreflightRefusal {
            intent_digest: [1; 32],
            refusal: ToolRefusal::INVALID_TRANSITION,
            verdict: None,
        },
    };
    let payload = event.payload().unwrap();
    assert_eq!(
        Event::decode(event.kind(), 1, &payload, Some(run), Some(agent)).unwrap(),
        event
    );
    for case in ["cause", "reason", "digest", "extra"] {
        let mut forged: serde_json::Value = serde_json::from_str(&payload).unwrap();
        match case {
            "cause" => forged["refusal"]["cause"] = serde_json::json!("lease_overlap"),
            "reason" => forged["refusal"]["reason"] = serde_json::json!("untrusted reason"),
            "digest" => forged["intent_digest"] = serde_json::json!(vec![0; 32]),
            "extra" => forged["extra"] = serde_json::json!(true),
            _ => unreachable!(),
        }
        assert!(
            Event::decode(event.kind(), 1, &forged.to_string(), Some(run), Some(agent)).is_err(),
            "{case}"
        );
    }
}

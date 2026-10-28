use super::*;

#[test]
fn decode_accepts_the_current_version_and_rejects_its_neighbours() {
    let run = RunId::new();
    let agent = AgentRunId::new();
    let event = Event::SessionRenamed {
        title: "renamed".into(),
    };
    let payload = event.payload().expect("payload");
    assert_eq!(payload, r#"{"title":"renamed"}"#);
    let version = payload_version(event.kind()).expect("known kind");
    assert_eq!(
        Event::decode(event.kind(), version, &payload, None, None).expect("current replay"),
        event
    );
    for stale in [version - 1, version + 1] {
        assert!(matches!(
            Event::decode(event.kind(), stale, &payload, None, None),
            Err(ReplayError::UnknownEvent)
        ));
    }
    for kind in [
        "ToolProposed",
        "ToolStarted",
        "ToolFinished",
        "RunStarted",
        "HookStarted",
        "HookFinished",
    ] {
        let version = payload_version(kind).expect("known kind");
        for stale in [version - 1, version + 1] {
            assert!(matches!(
                Event::decode(kind, stale, "{}", Some(run), Some(agent)),
                Err(ReplayError::UnknownEvent)
            ));
        }
    }
    assert!(matches!(
        Event::decode("NotAKind", 6, "{}", Some(run), Some(agent)),
        Err(ReplayError::UnknownEvent)
    ));
    assert_eq!(payload_version("NotAKind"), None);
}

pub(super) fn read_observation(run: RunId, agent: AgentRunId) -> crate::tools::ToolObservation {
    let intent = crate::tools::EffectIntent {
        host: None,
        wrap: None,
        rewrite: Vec::new(),
        host_command: None,
        target: crate::session::worker::EffectTarget::Workspace,
        id: Uuid::now_v7(),
        run_id: run,
        agent_run_id: agent,
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: crate::tools::ToolCall::Read {
            path: "src/lib.rs".into(),
            offset: None,
            limit: Some(16),
        },
        limits: crate::tools::ToolLimits::default(),
        use_count: 1,
    };
    crate::tools::ToolObservation {
        spill: None,
        enforcement_cause: None,
        limit_cause: None,
        attestation: Some(crate::tools::Attestation::InProcess {
            receipt: None,
            policy_digest: intent.policy_digest,
            workspace_device: intent.workspace_device,
            workspace_inode: intent.workspace_inode,
        }),
        intent,
        disposition: crate::tools::ToolDisposition::Succeeded,
        output: "{}".into(),
        approval_cause: None,
        guard_stage: None,
        cache_update: crate::tools::CacheUpdate::NotApplicable,
    }
}

#[test]
fn superseded_hook_and_terminal_versions_are_rejected() {
    for (kind, old) in [
        ("HookFinished", 2),
        ("HookFinished", 3),
        ("HookFinished", 4),
        ("HookFinished", 5),
        ("HookFinished", 6),
        ("HookFinished", 7),
        ("AgentFinished", 8),
        ("AgentFinished", 9),
        ("ProviderCallRecorded", 12),
        ("ProviderCallRecorded", 8),
        ("RunStarted", 14),
        ("RunStarted", 15),
        ("RunFinished", 7),
        ("ProviderCallRecorded", 7),
    ] {
        assert!(matches!(
            Event::decode(kind, old, "{}", None, None),
            Err(ReplayError::UnknownEvent)
        ));
    }
}

#[test]
fn ledger_diet_kinds_reject_their_stale_version_while_unbumped_kinds_decode() {
    let run = RunId::new();
    let agent = AgentRunId::new();
    let goal_id = GoalId::new();
    let config = RunConfig {
        supervisor: None,
        worker_policy: None,
        agent_definitions: None,
        roster: None,
        providers: None,
        provider: crate::ProviderRecord::parse("openai").expect("Provider record"),
        model: "gpt-5.4".into(),
        effort: None,
        custom_profile_provenance: None,
        saved_api_account_id: None,
        chatgpt_provenance: None,
        output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
        context_window_tokens: None,
        policy: CollaborationPolicy::Single,
        output_token_cap: 4096,
        provider_concurrency: 1,
        workspace_device: 1,
        workspace_inode: 2,
        user_instruction_digest: None,
        instruction_sources: Vec::new(),
        launch_directory: String::new(),
        include_digests: Vec::new(),
        history_run_ids: Vec::new(),
        excluded_history_runs: 0,
        context_usage: None,
        compaction_event_sequence: None,
        compaction_content_digest: None,
        tool_policy: None,
    };
    let bumped = [
        Event::RunStarted {
            run_id: run,
            config: config.clone(),
            active_deadline_ms: Some(MAX_GOAL_ACTIVE_MS),
        },
        Event::RunFinished {
            run_id: run,
            disposition: RunDisposition::Finished,
            failure: None,
            active_ms: 1_234,
        },
        Event::GoalProviderCallReserved {
            goal_id,
            run_id: run,
            agent_run_id: agent,
            active_ceiling_ms: 605_000,
        },
        Event::GoalCompactionCallReserved {
            goal_id,
            run_id: run,
            active_ceiling_ms: 125_000,
        },
        Event::ToolStarted {
            verdict: None,
            run_id: run,
            agent_run_id: agent,
            intent: read_observation(run, agent).intent.into(),
            active_ceiling_ms: 610_000,
        },
    ];
    for event in bumped {
        let (run_id, agent_run_id) = event.scope();
        let payload = event.payload().expect("payload");
        let version = payload_version(event.kind()).expect("registered kind");
        assert_eq!(event.version(), version, "{}", event.kind());
        assert_eq!(
            Event::decode(event.kind(), version, &payload, run_id, agent_run_id).expect("current"),
            event
        );
        assert!(matches!(
            Event::decode(event.kind(), version - 1, &payload, run_id, agent_run_id),
            Err(ReplayError::UnknownEvent)
        ));
    }
    let unbumped = [
        Event::SessionRenamed {
            title: "renamed".into(),
        },
        Event::GoalSteeringNoteRecorded {
            goal_id,
            run_id: run,
            text: "steer".into(),
        },
    ];
    for event in unbumped {
        let (run_id, agent_run_id) = event.scope();
        let payload = event.payload().expect("payload");
        assert_eq!(
            Event::decode(event.kind(), 6, &payload, run_id, agent_run_id).expect("unbumped"),
            event
        );
    }
    let finished = Event::GoalFinished {
        goal_id,
        status: GoalStatus::Completed,
        active_ms: 1_234,
        report: GoalReport {
            summary: "done".into(),
            changes: Vec::new(),
            evidence: Vec::new(),
            unresolved: Vec::new(),
            cached_input_tokens: 0,
        },
        exhausted: false,
        budget_refusal: None,
    };
    let (run_id, agent_run_id) = finished.scope();
    let payload = finished.payload().expect("payload");
    let mut missing: serde_json::Value = serde_json::from_str(&payload).unwrap();
    missing.as_object_mut().unwrap().remove("budget_refusal");
    assert!(
        Event::decode(
            "GoalFinished",
            finished.version(),
            &missing.to_string(),
            None,
            None
        )
        .is_err()
    );
    assert!(matches!(
        Event::decode(
            "GoalFinished",
            finished.version() - 1,
            &payload,
            run_id,
            agent_run_id
        ),
        Err(ReplayError::UnknownEvent)
    ));
    assert_eq!(
        Event::decode(
            "GoalFinished",
            finished.version(),
            &payload,
            run_id,
            agent_run_id
        )
        .expect("bumped"),
        finished
    );
    let started = Event::GoalStarted {
        include_paths: Some(Vec::new()),
        goal_id,
        objective: "continue".into(),
        workspace_identity: WorkspaceIdentity {
            device: 1,
            inode: 2,
        },
        budget: GoalBudget::default(),
        phase: GoalPhase::Implementation,
        autonomous: false,
        resource_settings: crate::tools::ToolResourceSettings::default(),
        continues: Some(GoalId::new()),
    };
    let started_payload = started.payload().expect("payload");
    assert!(!started_payload.contains("build_jobs"));
    assert!(!started_payload.contains("build_cpu_cores"));
    let mut parallel = started.clone();
    if let Event::GoalStarted {
        resource_settings, ..
    } = &mut parallel
    {
        resource_settings.build_jobs = Some(2);
        resource_settings.build_cpu_cores = Some(8);
    }
    let bytes = parallel.payload().unwrap();
    assert_eq!(
        Event::decode(parallel.kind(), parallel.version(), &bytes, None, None).unwrap(),
        parallel
    );
    assert_eq!(
        Event::decode(
            "GoalStarted",
            started.version(),
            &started_payload,
            None,
            None
        )
        .expect("current"),
        started
    );
    let current: serde_json::Value = serde_json::from_str(&started_payload).unwrap();
    let mut missing = current.clone();
    missing.as_object_mut().unwrap().remove("include_paths");
    assert!(
        Event::decode(
            "GoalStarted",
            started.version(),
            &missing.to_string(),
            None,
            None
        )
        .is_err()
    );
    let migrated = goal_started_v7_to_v8(missing).expect("unknown legacy inputs");
    assert!(migrated["include_paths"].is_null());
    assert!(
        Event::decode(
            "GoalStarted",
            started.version(),
            &migrated.to_string(),
            None,
            None
        )
        .is_ok()
    );
    assert!(goal_started_v7_to_v8(current.clone()).is_none());
    for paths in [
        serde_json::json!(["../private"]),
        serde_json::json!(["/absolute"]),
        serde_json::json!([""]),
        serde_json::json!(vec!["input.txt"; 17]),
        serde_json::json!(["x".repeat(513)]),
        serde_json::json!([format!("{}/", "x".repeat(512))]),
        serde_json::json!(["notes//"]),
        serde_json::json!(["notes/.env/"]),
    ] {
        let mut invalid = current.clone();
        invalid["include_paths"] = paths;
        assert!(
            Event::decode(
                "GoalStarted",
                started.version(),
                &invalid.to_string(),
                None,
                None
            )
            .is_err()
        );
    }
    for path in [
        "x".repeat(512),
        format!("{}/", "x".repeat(511)),
        "notes/".into(),
    ] {
        let mut longest = current.clone();
        longest["include_paths"] = serde_json::json!([path]);
        assert!(
            Event::decode(
                "GoalStarted",
                started.version(),
                &longest.to_string(),
                None,
                None
            )
            .is_ok()
        );
    }
    for (kind, payload) in [
        ("GoalStarted", &started_payload),
        ("GoalFinished", &payload),
    ] {
        assert!(matches!(
            Event::decode(kind, 6, payload, None, None),
            Err(ReplayError::UnknownEvent)
        ));
    }
    for deleted in ["GoalActiveTimeReserved", "GoalActiveTimeSettled"] {
        assert_eq!(payload_version(deleted), None);
        let payload = format!(r#"{{"goal_id":"{goal_id}","active_ms":1}}"#);
        assert!(matches!(
            Event::decode(deleted, 6, &payload, Some(run), None),
            Err(ReplayError::UnknownEvent)
        ));
    }
    let unbounded = Event::RunStarted {
        run_id: run,
        config,
        active_deadline_ms: Some(MAX_GOAL_ACTIVE_MS + 1),
    };
    assert!(
        unbounded.payload().is_err(),
        "a deadline beyond the Goal maximum"
    );
}

#[test]
fn worker_command_protocol_requires_pins_and_exact_versions() {
    let f = crate::session::worker_protocol_fixture();
    for (kind, pointer, field) in [
        (
            "RunStarted",
            "/config/worker_policy/parent_grant",
            "commands",
        ),
        (
            "ProviderCallRecorded",
            "/delegation/0/assignment",
            "commands",
        ),
        ("AgentSpawned", "/assignment", "permissions_digest"),
        ("AgentSpawned", "/assignment/grant", "commands"),
        ("ToolStarted", "", "verdict"),
        ("WorkerLeaseReleased", "", "cause_sequence"),
    ] {
        let e = f
            .envelopes()
            .into_iter()
            .find(|e| {
                e.event.kind() == kind && {
                    let v: serde_json::Value =
                        serde_json::from_str(&e.event.payload().unwrap()).unwrap();
                    v.pointer(pointer).and_then(|v| v.get(field)).is_some()
                }
            })
            .expect("command fixture includes the field");
        let mut payload: serde_json::Value =
            serde_json::from_str(&e.event.payload().unwrap()).unwrap();
        payload
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert!(
            Event::decode(
                kind,
                e.event.version(),
                &payload.to_string(),
                e.run_id,
                e.agent_run_id
            )
            .is_err(),
            "{kind} {field}"
        );
        assert!(
            Event::decode(
                kind,
                e.event.version() - 1,
                &e.event.payload().unwrap(),
                e.run_id,
                e.agent_run_id
            )
            .is_err(),
            "old {kind}"
        );
    }
    for commands in [
        vec!["x".into(); 9],
        vec!["b".into(), "a".into()],
        vec!["x".into(), "x".into()],
        vec!["x".repeat(65)],
    ] {
        let e = f
            .envelopes()
            .into_iter()
            .find(|e| {
                matches!(
                    e.event,
                    Event::AgentSpawned {
                        assignment: Some(_),
                        ..
                    }
                )
            })
            .unwrap();
        let mut value: serde_json::Value =
            serde_json::from_str(&e.event.payload().unwrap()).unwrap();
        let decode = |v: &serde_json::Value| {
            Event::decode(
                e.event.kind(),
                e.event.version(),
                &v.to_string(),
                e.run_id,
                e.agent_run_id,
            )
        };
        assert!(decode(&value).is_ok(), "unmutated command payload decodes");
        *value
            .pointer_mut("/assignment/request/commands")
            .expect("fixture carries request commands") = serde_json::json!(["a", "b"]);
        assert!(decode(&value).is_ok(), "a valid command list decodes");
        *value.pointer_mut("/assignment/request/commands").unwrap() = serde_json::json!(commands);
        assert!(decode(&value).is_err(), "commands {commands:?}");
    }
}

#[test]
fn supervisor_event_is_closed_bounded_and_run_started_requires_the_pin() {
    use crate::supervisor::*;
    let fixture = crate::session::worker_protocol_fixture();
    let start = fixture
        .events
        .iter()
        .find(|e| matches!(e, Event::RunStarted { .. }))
        .unwrap();
    for case in 0..5 {
        let mut event = start.clone();
        if let Event::RunStarted { config, .. } = &mut event {
            let mut pin = SupervisorPolicy::default();
            match case {
                0 => (),
                1 => pin.evaluator = 2,
                2 => pin.window = 33,
                3 => pin.digest[0] ^= 1,
                _ => pin.floors.identical = 0,
            }
            config.supervisor = Some(pin);
        }
        assert_eq!(event.payload().is_ok(), case == 0, "policy pin {case}");
    }

    let verdict = SupervisorVerdict {
        subject_sequence: 10,
        site: Site::ToolOutcome,
        evaluator: 1,
        mode: Mode::Shadow,
        capped: false,
        signals: vec![Signal {
            signal: SignalKind::Loop,
            level: Level::Notice,
            would_response: None,
            features: Features::Loop(LoopFeatures {
                repeat_identical: 3,
                ..Default::default()
            }),
            evidence: vec![8, 9, 10],
        }],
    };
    let event = Event::SupervisorVerdictRecorded {
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        verdict,
    };
    let current = event.version();
    assert_eq!(current, 2);
    let (run, actor) = event.scope();
    let payload = event.payload().unwrap();
    assert_eq!(
        Event::decode(event.kind(), current, &payload, run, actor).unwrap(),
        event
    );
    for version in [0, current - 1] {
        assert!(matches!(
            Event::decode(event.kind(), version, &payload, run, actor),
            Err(ReplayError::UnknownEvent)
        ));
    }
    for (field, value) in [
        ("evaluator", serde_json::json!(2)),
        ("site", serde_json::json!("unknown")),
        ("mode", serde_json::json!("enforce")),
        ("extra", serde_json::json!(null)),
        ("signals", serde_json::json!([])),
    ] {
        let mut value_payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        value_payload[field] = value;
        assert!(
            Event::decode(
                event.kind(),
                current,
                &value_payload.to_string(),
                run,
                actor
            )
            .is_err(),
            "{field}"
        );
    }
    for field in ["would_response", "features"] {
        let mut value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        value["signals"][0].as_object_mut().unwrap().remove(field);
        assert!(
            Event::decode(event.kind(), current, &value.to_string(), run, actor).is_err(),
            "missing {field}"
        );
    }
    let padded = format!(
        "{payload}{}",
        " ".repeat(MAX_VERDICT_BYTES + 1 - payload.len())
    );
    assert!(matches!(
        Event::decode(event.kind(), current, &padded, run, actor),
        Err(ReplayError::InvalidPayload)
    ));
}

#[test]
fn refusal_and_run_failure_payloads_require_current_versions_and_exact_pairings() {
    let run_version = crate::test_support::payload_version("RunFinished");
    use crate::provider::ProviderFailureReason;
    let run_id = Some(RunId::new());
    let agent_id = Some(AgentRunId::new());

    let record = ProviderCallRecord {
        rate_limits: None,
        delegation: None,
        phase: crate::provider::AgentPhase::RootPlan,
        disposition: ProviderCallDisposition::Rejected,
        requested_tools: 0,
        response_id: None,
        input_tokens: None,
        output_tokens: None,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        wire_provenance: None,
        failure_reason: Some(ProviderFailureReason::ProposalRefused),
        refusal: Some(ProposalRefusal {
            operation: "arany_write".into(),
            cause: ProposalRefusalCause::PlanInvalid,
        }),
    };
    let valid = serde_json::to_value(record).unwrap();
    let current = payload_version("ProviderCallRecorded").expect("known kind");
    let decode_call = |value: &serde_json::Value, version| {
        Event::decode(
            "ProviderCallRecorded",
            version,
            &value.to_string(),
            run_id,
            agent_id,
        )
    };
    let old = decode_call(&valid, current).unwrap();
    let bytes = old.payload().unwrap();
    assert!(!bytes.contains("rate_limits"));
    let (run, actor) = old.scope();
    assert_eq!(
        Event::decode(old.kind(), old.version(), &bytes, run, actor)
            .unwrap()
            .payload()
            .unwrap(),
        bytes
    );
    let mut observed = valid.clone();
    observed["rate_limits"] =
        serde_json::to_value(crate::test_support::routing::plan_sample(1234)).unwrap();
    let with_limits = decode_call(&observed, current).unwrap();
    assert_eq!(with_limits.version(), old.version());
    let observed_bytes = with_limits.payload().unwrap();
    assert_eq!(
        Event::decode(
            with_limits.kind(),
            with_limits.version(),
            &observed_bytes,
            run,
            actor
        )
        .unwrap(),
        with_limits
    );
    for (field, value) in [
        ("source", serde_json::json!("unknown")),
        ("observed_at", serde_json::json!(-1)),
        ("used_percent", serde_json::json!(101)),
        ("window_minutes", serde_json::json!(0)),
        ("reset_at", serde_json::json!(253402300800_i64)),
        ("account_id", serde_json::json!("CANARY")),
    ] {
        let mut forged = observed.clone();
        forged["rate_limits"]["primary"][field] = value;
        assert!(
            decode_call(&forged, current).is_err(),
            "invalid limit field {field}"
        );
    }
    let mut missing = observed.clone();
    missing["rate_limits"]["primary"]
        .as_object_mut()
        .unwrap()
        .remove("reset_at");
    assert!(decode_call(&missing, current).is_err());
    assert!(decode_call(&valid, current).is_ok());
    assert!(decode_call(&valid, current - 1).is_err());
    for (field, value) in [
        ("refusal", serde_json::Value::Null),
        ("failure_reason", serde_json::Value::Null),
        ("failure_reason", serde_json::json!("response_contract")),
        ("disposition", serde_json::json!("finished")),
        (
            "refusal",
            serde_json::json!({"operation":"PRIVATE_ARGUMENT", "cause":"plan_invalid"}),
        ),
        (
            "refusal",
            serde_json::json!({"operation":"arany_write", "cause":"unknown"}),
        ),
        (
            "refusal",
            serde_json::json!({"operation":"arany_write", "cause":"plan_invalid", "arguments":"PRIVATE_ARGUMENT"}),
        ),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        assert!(decode_call(&invalid, current).is_err(), "field {field}");
    }
    let mut worker_phase = valid.clone();
    worker_phase["phase"] = serde_json::json!("child_work");
    assert!(decode_call(&worker_phase, current).is_err());
    worker_phase["refusal"]["cause"] = serde_json::json!("invalid_arguments");
    assert!(decode_call(&worker_phase, current).is_ok());
    worker_phase["phase"] = serde_json::json!("tool_review");
    assert!(decode_call(&worker_phase, current).is_err());
    let mut synthesis = valid.clone();
    synthesis["phase"] = serde_json::json!("root_synthesis");
    assert!(decode_call(&synthesis, current).is_ok());
    let mut missing = valid.clone();
    missing.as_object_mut().unwrap().remove("refusal");
    assert!(decode_call(&missing, current).is_err());
    for (disposition, failure, accepted) in [
        ("failed", serde_json::json!("proposal_refused"), true),
        ("failed", serde_json::Value::Null, false),
        ("finished", serde_json::json!("proposal_refused"), false),
        ("cancelled", serde_json::json!("provider_call"), false),
        ("stopped_by_hook", serde_json::json!("provider_call"), false),
        ("failed", serde_json::json!("unknown"), false),
        ("failed", serde_json::json!("budget_exhausted"), true),
        ("failed", serde_json::json!("tool_stopped"), true),
        ("failed", serde_json::json!("definition_invalid"), true),
        ("finished", serde_json::Value::Null, true),
    ] {
        let value =
            serde_json::json!({"disposition":disposition, "failure":failure, "active_ms":0});
        assert_eq!(
            Event::decode("RunFinished", run_version, &value.to_string(), run_id, None).is_ok(),
            accepted,
            "{disposition}"
        );
        assert!(
            Event::decode(
                "RunFinished",
                run_version - 1,
                &value.to_string(),
                run_id,
                None
            )
            .is_err()
        );
    }
    assert!(
        Event::decode(
            "RunFinished",
            run_version,
            r#"{"disposition":"failed","active_ms":0}"#,
            run_id,
            None
        )
        .is_err()
    );
}

#[test]
fn remaining_borrowed_payloads_match_fixed_bytes() {
    const RECORD: &str = r#"{"covered_run_id":"01900000-0000-7000-8000-000000000001","covered_sequence":1,"source_digest":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"provider":"openai","model":"model","prompt_version":1,"schema_version":1,"compiler_version":3,"source_bytes":4,"source_tokens_estimate":1,"input_tokens":null,"output_tokens":null,"cache_read_input_tokens":null,"cache_creation_input_tokens":null,"response_id":null,"wire_provenance":null,"chatgpt_provenance":null,"output_token_bound":"provider_enforced","status":{"status":"failed","reason":"provider_unavailable"}}"#;
    let (run, agent) = (Some(RunId::new()), Some(AgentRunId::new()));
    let cases = [
        ("SessionStarted", r#"{"title":"started","workspace_device":1,"workspace_inode":2}"#.into(), None, None),
        ("SessionForked", r#"{"title":"forked","source_session_id":"01900000-0000-7000-8000-000000000003","source_run_id":"01900000-0000-7000-8000-000000000004","source_sequence":3,"prefix_digest":[4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4]}"#.into(), None, None),
        ("ContextCompacted", format!(r#"{{"record":{RECORD},"active_ms":null}}"#), None, None),
        ("ContextCompacted", format!(r#"{{"record":{RECORD},"active_ms":9}}"#), None, None),
        ("GoalContextCompacted", format!(r#"{{"goal_id":"01900000-0000-7000-8000-000000000002","covered_tool_count":1,"record":{RECORD}}}"#), run, None),
        ("MessageCommitted", r#"{"text":"committed"}"#.into(), run, None),
        ("AgentFinished", r#"{"worker":null,"disposition":"finished","summary":"done","result":"answer"}"#.into(), run, agent),
        ("AgentFinished", r#"{"worker":{"disposition":"failed","cause":"tool_failed"},"disposition":"failed","summary":null,"result":null}"#.into(), run, agent),
    ];
    for (kind, fixture, run, agent) in cases {
        assert_eq!(
            Event::decode(kind, payload_version(kind).unwrap(), &fixture, run, agent)
                .unwrap()
                .payload()
                .unwrap(),
            fixture,
            "{kind}"
        );
    }
}

use crate::session::events::*;

#[test]
fn provider_call_record_rejects_old_versions_and_inconsistent_cache_fields() {
    let current = payload_version("ProviderCallRecorded").expect("payload version");
    let run = RunId::new();
    let agent = AgentRunId::new();
    let mut record = ProviderCallRecord {
        response_id: Some("response-1".into()),
        input_tokens: Some(100),
        output_tokens: Some(10),
        cache_read_input_tokens: Some(40),
        cache_creation_input_tokens: Some(20),
        ..crate::test_support::provider_call_record()
    };
    assert!(record.validate().is_ok());
    record.cache_read_input_tokens = Some(81);
    assert!(
        record.validate().is_err(),
        "cache parts exceed the input total"
    );
    record.cache_read_input_tokens = Some(40);
    record.input_tokens = None;
    assert!(
        record.validate().is_err(),
        "cache without a reported input total"
    );
    record.input_tokens = Some(100);
    record.requested_tools = 1;
    assert!(
        record.validate().is_err(),
        "a finished call declares no Tool"
    );
    record.disposition = ProviderCallDisposition::ToolRequested;
    record.requested_tools = 0;
    assert!(
        record.validate().is_err(),
        "a Tool request declares at least one Tool"
    );
    record.requested_tools = MAX_STEP_TOOL_CALLS + 1;
    assert!(record.validate().is_err(), "above the step ceiling");
    record.requested_tools = MAX_STEP_TOOL_CALLS;
    assert!(record.validate().is_ok());

    let event = Event::ProviderCallRecorded {
        run_id: run,
        agent_run_id: agent,
        record: record.clone(),
    };
    let payload = event.payload().expect("current payload");
    assert_eq!(
        Event::decode(
            "ProviderCallRecorded",
            current,
            &payload,
            Some(run),
            Some(agent),
        )
        .expect("current replay"),
        event
    );
    assert!(
        Event::decode(
            "ProviderCallRecorded",
            current - 1,
            &payload,
            Some(run),
            Some(agent),
        )
        .is_err(),
        "payload version 5 is not current"
    );
}

#[test]
fn rejects_unbounded_or_unearned_provider_metadata() {
    let current = payload_version("ProviderCallRecorded").expect("payload version");
    let mut record = ProviderCallRecord {
        response_id: Some("response-1".into()),
        input_tokens: Some(100),
        output_tokens: Some(10),
        ..crate::test_support::provider_call_record()
    };
    assert!(record.validate().is_ok());
    record.response_id = Some("bad\nidentifier".into());
    assert!(record.validate().is_err());
    record.response_id = Some("r".repeat(129));
    assert!(record.validate().is_err());
    record.response_id = None;
    record.input_tokens = Some(crate::provider::MAX_REPORTED_INPUT_TOKENS + 1);
    assert!(record.validate().is_err());
    record.input_tokens = Some(100);
    // A local-acceptance total includes reasoning; the reducer applies each route's cap.
    record.output_tokens = Some(crate::provider::MAX_REPORTED_OUTPUT_TOKENS + 1);
    assert!(record.validate().is_err());
    record.output_tokens = Some(crate::provider::MAX_REPORTED_OUTPUT_TOKENS);
    assert!(record.validate().is_ok());
    record.output_tokens = None;
    record.disposition = ProviderCallDisposition::Unavailable;
    record.input_tokens = Some(100);
    assert!(record.validate().is_err());
    record.input_tokens = None;
    assert!(record.validate().is_ok());
    record.disposition = ProviderCallDisposition::Finished;
    record.response_id = Some("response-1".into());
    record.wire_provenance =
        Some(crate::provider::ProviderWireProvenance::MessagesEndTurnStorageUnspecified);
    assert!(record.validate().is_err());
    record.input_tokens = Some(100);
    record.output_tokens = Some(10);
    assert!(record.validate().is_ok());
    record.wire_provenance =
        Some(crate::provider::ProviderWireProvenance::ChatCompletionsStopStorageUnspecified);
    let encoded = serde_json::to_value(&record).expect("Chat provenance payload");
    assert_eq!(
        encoded["wire_provenance"],
        "chat_completions_stop_storage_unspecified"
    );
    assert_eq!(
        serde_json::from_value::<ProviderCallRecord>(encoded).expect("Chat provenance replay"),
        record
    );
    record.disposition = ProviderCallDisposition::TimedOut;
    assert!(record.validate().is_err());
    record.disposition = ProviderCallDisposition::Finished;
    record.wire_provenance = None;
    let encoded = serde_json::to_value(&record).expect("current payload");
    assert!(encoded["wire_provenance"].is_null());
    assert!(encoded["failure_reason"].is_null());
    let decoded: ProviderCallRecord = serde_json::from_value(encoded).expect("current replay");
    assert_eq!(decoded, record);

    use crate::provider::ProviderFailureReason;
    let run = RunId::new();
    let agent = AgentRunId::new();
    let run_id = Some(run);
    let agent_run_id = Some(agent);
    let event = Event::ProviderCallRecorded {
        run_id: run,
        agent_run_id: agent,
        record: decoded,
    };
    let payload = event.payload().expect("current payload");
    assert_eq!(event.version(), current);
    assert_eq!(
        Event::decode(
            "ProviderCallRecorded",
            current,
            &payload,
            run_id,
            agent_run_id
        )
        .unwrap(),
        event
    );
    for version in [0, 1, 2, 3, 4, 5, current + 1] {
        assert!(
            Event::decode(
                "ProviderCallRecorded",
                version,
                &payload,
                run_id,
                agent_run_id
            )
            .is_err()
        );
    }
    for (reason, expected) in [
        (
            ProviderFailureReason::AccountAccess,
            ProviderCallDisposition::Rejected,
        ),
        (
            ProviderFailureReason::UsageLimit,
            ProviderCallDisposition::Rejected,
        ),
        (
            ProviderFailureReason::UsageTemporarilyUnavailable,
            ProviderCallDisposition::Unavailable,
        ),
        (
            ProviderFailureReason::ServiceUnavailable,
            ProviderCallDisposition::Unavailable,
        ),
        (
            ProviderFailureReason::StreamProtocol,
            ProviderCallDisposition::InvalidResponse,
        ),
        (
            ProviderFailureReason::OutcomeDecode,
            ProviderCallDisposition::InvalidResponse,
        ),
        (
            ProviderFailureReason::ResponseContract,
            ProviderCallDisposition::InvalidResponse,
        ),
        (
            ProviderFailureReason::OutcomeContract,
            ProviderCallDisposition::InvalidResponse,
        ),
        (
            ProviderFailureReason::LocalOutputLimit,
            ProviderCallDisposition::InvalidResponse,
        ),
    ] {
        for disposition in [
            ProviderCallDisposition::Finished,
            ProviderCallDisposition::Delegated,
            ProviderCallDisposition::Unavailable,
            ProviderCallDisposition::Rejected,
            ProviderCallDisposition::InvalidResponse,
            ProviderCallDisposition::OutputLimit,
            ProviderCallDisposition::TimedOut,
            ProviderCallDisposition::Cancelled,
            ProviderCallDisposition::TaskPanic,
        ] {
            let record = ProviderCallRecord {
                disposition,
                failure_reason: Some(reason),
                ..crate::test_support::provider_call_record()
            };
            assert_eq!(
                record.validate().is_ok(),
                disposition == expected,
                "{disposition:?}/{reason:?}"
            );
            if disposition != expected {
                continue;
            }
            let event = Event::ProviderCallRecorded {
                run_id: run,
                agent_run_id: agent,
                record,
            };
            let payload = event.payload().expect("current payload");
            assert_eq!(event.version(), current);
            assert_eq!(
                Event::decode(
                    "ProviderCallRecorded",
                    current,
                    &payload,
                    run_id,
                    agent_run_id,
                )
                .expect("current replay"),
                event
            );
            for version in [0, 1, 2, 3, 4, 5, current + 1] {
                assert!(
                    Event::decode(
                        "ProviderCallRecorded",
                        version,
                        &payload,
                        run_id,
                        agent_run_id
                    )
                    .is_err(),
                    "version {version}"
                );
            }
            for scope in [(None, agent_run_id), (run_id, None)] {
                assert!(
                    Event::decode("ProviderCallRecorded", current, &payload, scope.0, scope.1)
                        .is_err()
                );
            }
            let mut invalid: serde_json::Value = serde_json::from_str(&payload).expect("v4 JSON");
            invalid["failure_reason"] = "unknown_reason".into();
            assert!(
                Event::decode(
                    "ProviderCallRecorded",
                    current,
                    &invalid.to_string(),
                    run_id,
                    agent_run_id
                )
                .is_err()
            );
            invalid["failure_reason"] = serde_json::Value::Null;
            assert!(
                Event::decode(
                    "ProviderCallRecorded",
                    current,
                    &invalid.to_string(),
                    run_id,
                    agent_run_id
                )
                .is_ok()
            );
            invalid.as_object_mut().unwrap().remove("failure_reason");
            assert!(
                Event::decode(
                    "ProviderCallRecorded",
                    current,
                    &invalid.to_string(),
                    run_id,
                    agent_run_id
                )
                .is_err()
            );
        }
    }
    record.disposition = ProviderCallDisposition::InvalidResponse;
    record.failure_reason = Some(ProviderFailureReason::CriterionNotMetInView);
    for phase in [
        crate::provider::AgentPhase::RootPlan,
        crate::provider::AgentPhase::RootSynthesis,
        crate::provider::AgentPhase::ToolReview,
        crate::provider::AgentPhase::ChildWork,
    ] {
        record.phase = phase;
        let payload = serde_json::to_string(&record).expect("scoped failure payload");
        assert_eq!(
            Event::decode(
                "ProviderCallRecorded",
                current,
                &payload,
                run_id,
                agent_run_id,
            )
            .is_ok(),
            phase == crate::provider::AgentPhase::ChildWork,
            "criterion failure phase: {phase:?}"
        );
    }
}

#[test]
fn plan_recorded_is_a_primary_control_call_without_effects_or_failure() {
    let record = ProviderCallRecord {
        disposition: ProviderCallDisposition::PlanRecorded,
        response_id: Some("response_plan".into()),
        input_tokens: Some(8),
        output_tokens: Some(4),
        ..crate::test_support::provider_call_record()
    };
    let value = serde_json::to_value(&record).unwrap();
    let current = payload_version("ProviderCallRecorded").expect("known kind");
    let decode = |payload: &serde_json::Value, version| {
        Event::decode(
            "ProviderCallRecorded",
            version,
            &payload.to_string(),
            Some(RunId::new()),
            Some(AgentRunId::new()),
        )
    };
    assert!(decode(&value, current).is_ok());
    assert!(decode(&value, current - 1).is_err());
    let mut measured = value.clone();
    measured["context_bytes"] = serde_json::json!(4096);
    assert!(decode(&measured, current).is_ok());
    let mut child = measured.clone();
    child["phase"] = serde_json::json!("child_work");
    child["disposition"] = serde_json::json!("finished");
    assert!(
        decode(&child, current).is_err(),
        "only primary requests measure"
    );
    measured["context_bytes"] = serde_json::json!(crate::session::MAX_WINDOW_CONTENT_BYTES + 1);
    assert!(decode(&measured, current).is_err());
    let mut missing = value.clone();
    missing.as_object_mut().unwrap().remove("context_bytes");
    assert!(decode(&missing, current).is_err());
    for (key, invalid) in [
        ("phase", serde_json::json!("child_work")),
        ("phase", serde_json::json!("tool_review")),
        ("requested_tools", serde_json::json!(1)),
        ("failure_reason", serde_json::json!("response_contract")),
        (
            "refusal",
            serde_json::json!({"operation":"arany_plan","cause":"plan_invalid"}),
        ),
    ] {
        let mut malformed = value.clone();
        malformed[key] = invalid;
        assert!(decode(&malformed, current).is_err(), "{key}");
    }
}

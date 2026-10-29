use super::*;

#[test]
fn api_limits_preserve_default_bytes_and_validate_all_five_event_families() {
    let run: RunId = "01900000-0000-7000-8000-000000000001".parse().unwrap();
    let actor: AgentRunId = "01900000-0000-7000-8000-000000000002".parse().unwrap();
    let goal = "01900000-0000-7000-8000-000000000003";
    let identity = r#"{"profile":"openai","model":"model-a","effort":null,"provenance":{"kind":"native","saved_api_account_id":null},"limits":{"concurrency":1,"max_output_tokens":4096,"output_token_bound":"provider_enforced","deadline":{"secs":120,"nanos":0}}}"#;
    let call = r#"{"delegation":null,"phase":"root_plan","disposition":"rejected","requested_tools":0,"response_id":null,"input_tokens":null,"context_bytes":null,"output_tokens":null,"cache_read_input_tokens":null,"cache_creation_input_tokens":null,"wire_provenance":null,"failure_reason":null,"refusal":null}"#;
    let retry =
        r#"{"role":"primary","call_index":1,"attempt":1,"cause":"rate_limited","delay_ms":2000}"#;
    let fallback = format!(
        r#"{{"role":"primary","call_index":1,"failure":"unavailable","from":{identity},"to":{}}}"#,
        identity.replace("model-a", "model-b")
    );
    let compaction = format!(
        r#"{{"covered_run_id":"{run}","covered_sequence":1,"source_digest":{},"provider":"openai","model":"model-a","prompt":{{"file":"compaction","source":"default","digest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2]}},"compiler_version":5,"source_bytes":4,"source_tokens_estimate":1,"input_tokens":null,"output_tokens":null,"cache_read_input_tokens":null,"cache_creation_input_tokens":null,"response_id":null,"wire_provenance":null,"chatgpt_provenance":null,"output_token_bound":"provider_enforced","status":{{"status":"failed","reason":"provider_unavailable"}}}}"#,
        serde_json::to_string(&[1_u8; 32]).unwrap()
    );
    let fixtures = [
        (
            "ProviderCallRecorded",
            call.to_owned(),
            Some(run),
            Some(actor),
        ),
        (
            "ProviderRetryScheduled",
            retry.to_owned(),
            Some(run),
            Some(actor),
        ),
        ("ProviderFallbackUsed", fallback, Some(run), Some(actor)),
        (
            "ContextCompacted",
            format!(r#"{{"record":{compaction},"active_ms":null}}"#),
            None,
            None,
        ),
        (
            "GoalContextCompacted",
            format!(r#"{{"goal_id":"{goal}","covered_tool_count":1,"record":{compaction}}}"#),
            Some(run),
            None,
        ),
    ];
    let sample = crate::test_support::routing::api_sample(1000);
    for (kind, old_bytes, run, actor) in fixtures {
        let old = Event::decode(kind, &old_bytes, run, actor).unwrap();
        assert_eq!(old.payload().unwrap(), old_bytes, "{kind} default bytes");
        assert!(!old_bytes.contains("rate_limits"));
        let plan = r#"{"primary":{"source":"headers","observed_at":1000,"used_percent":25.5,"window_minutes":300,"reset_at":2000000000},"secondary":{"source":"headers","observed_at":1000,"used_percent":70.0,"window_minutes":10080,"reset_at":null},"unavailable":null}"#;
        let legacy = if old_bytes.contains("\"record\"") {
            old_bytes.replacen(
                "\"record\":{",
                &format!("\"record\":{{\"rate_limits\":{plan},"),
                1,
            )
        } else {
            old_bytes.replacen('{', &format!("{{\"rate_limits\":{plan},"), 1)
        };
        assert_eq!(
            Event::decode(kind, &legacy, run, actor)
                .unwrap()
                .payload()
                .unwrap(),
            legacy
        );
        let value: serde_json::Value = serde_json::from_str(&old_bytes).unwrap();
        {
            let mut observed = value.clone();
            let record = if observed.get("record").is_some() {
                &mut observed["record"]
            } else {
                &mut observed
            };
            record["rate_limits"] = serde_json::to_value(&sample).unwrap();
            let event = Event::decode(kind, &observed.to_string(), run, actor).unwrap();
            let bytes = event.payload().unwrap();
            assert_eq!(Event::decode(kind, &bytes, run, actor).unwrap(), event);
        }
        for (pointer, value) in [
            ("/rows/0/source", serde_json::json!("event")),
            ("/rows/0/observed_at", serde_json::json!(-1)),
            ("/rows/0/reset_at", serde_json::json!(253402300800_i64)),
            ("/rows/0/limit", serde_json::json!(-1)),
            ("/rows/0/dimension", serde_json::json!("arbitrary")),
            ("/dialect", serde_json::json!("arbitrary")),
        ] {
            let mut forged = serde_json::to_value(&sample).unwrap();
            *forged["api"].pointer_mut(pointer).unwrap() = value;
            let mut observed: serde_json::Value = serde_json::from_str(&old_bytes).unwrap();
            let record = if observed.get("record").is_some() {
                &mut observed["record"]
            } else {
                &mut observed
            };
            record["rate_limits"] = forged;
            assert!(
                Event::decode(kind, &observed.to_string(), run, actor).is_err(),
                "{kind}: {pointer}"
            );
        }
    }
    assert_eq!(
        serde_json::to_string(&crate::RateLimits::default()).unwrap(),
        r#"{"primary":null,"secondary":null,"unavailable":null}"#
    );
    let mut mixed = sample.clone();
    mixed.primary = crate::test_support::routing::plan_sample(1000).primary;
    assert!(!mixed.valid());
    let mut duplicate = sample;
    let api = duplicate.api.as_mut().unwrap();
    api.rows.push(api.rows[0].clone());
    assert!(!duplicate.valid());
}

#[test]
fn maximum_api_samples_fit_and_attempt_slots_never_mix_families() {
    use crate::provider::*;
    let mut sample = crate::test_support::routing::api_sample(MAX_LIMIT_TIMESTAMP);
    sample.api.as_mut().unwrap().rows = ApiLimitDimension::ALL
        .iter()
        .copied()
        .map(|dimension| ApiLimit {
            dimension,
            source: LimitSource::Headers,
            observed_at: MAX_LIMIT_TIMESTAMP,
            limit: Some(u64::MAX),
            remaining: Some(u64::MAX),
            reset_at: Some(MAX_LIMIT_TIMESTAMP),
            unavailable: None,
        })
        .collect();
    assert!(serde_json::to_vec(&sample).unwrap().len() <= MAX_RATE_LIMIT_BYTES);
    for dialect in [ApiLimitDialect::OpenAi, ApiLimitDialect::Anthropic] {
        let mut admitted = sample.clone();
        let api = admitted.api.as_mut().unwrap();
        api.dialect = dialect;
        api.rows.retain(|row| dialect.admits(row.dimension));
        assert!(admitted.valid());
    }
    for row in &mut sample.api.as_mut().unwrap().rows {
        row.limit = None;
        row.remaining = None;
        row.reset_at = None;
        row.unavailable = Some(LimitUnavailable::Oversized);
    }
    assert!(serde_json::to_vec(&sample).unwrap().len() <= MAX_RATE_LIMIT_BYTES);
    assert!(
        !sample.valid(),
        "each dialect still rejects foreign dimensions"
    );
    for (first, second) in [
        (
            crate::test_support::routing::api_sample(1000),
            crate::test_support::routing::plan_sample(1000),
        ),
        (
            crate::test_support::routing::plan_sample(1000),
            crate::test_support::routing::api_sample(1000),
        ),
    ] {
        let slot = RateLimitSlot::default();
        slot.record(first.clone());
        slot.record(second);
        assert_eq!(slot.snapshot(), Some(first.clone()));
        let mut replacement = if first.api.is_some() {
            crate::test_support::routing::api_sample(900)
        } else {
            crate::test_support::routing::plan_sample(900)
        };
        if let Some(api) = &mut replacement.api {
            api.rows[0].remaining = None;
            api.rows[0].reset_at = None;
        }
        slot.record(replacement.clone());
        assert_eq!(slot.take(), Some(replacement));
        assert!(slot.take().is_none());
    }
}

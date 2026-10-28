use super::*;

const RETIRED_EVENTS: &str = include_str!("retired-worker-events.json");

fn current_source(kind: &str) -> (i64, &'static str) {
    let version = match kind {
        "ToolProposed" => 2,
        "ToolStarted" => 24,
        "ToolFinished" => 30,
        "HookStarted" => 13,
        "HookFinished" => 19,
        _ => payload_version(kind).unwrap() - 1,
    };
    EVENT_FIXTURES
        .iter()
        .find_map(|(name, from, source, _)| {
            (*name == kind && *from == version).then_some((*from, *source))
        })
        .unwrap()
}

fn assert_archived(kind: &str, version: i64, payload: &str) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let History { root, .. } = forked_history(&path);
    checkpoint_fixture(&root);
    let connection = Connection::open(path.join(DATABASE_FILE)).unwrap();
    unsupported_tool_row(&connection, kind, version, payload);
    drop(connection);
    let files: Vec<_> = [DATABASE_FILE, checkpoints::FILES[0]]
        .into_iter()
        .map(|name| (name, std::fs::read(path.join(name)).unwrap()))
        .collect();
    let archived = root
        .upgrade_or_archive_history(&REGISTRY)
        .unwrap()
        .expect("retired worker protocol archives");
    for (name, bytes) in files {
        assert_eq!(
            std::fs::read(archived.path().join(name)).unwrap(),
            bytes,
            "{kind}: {name} retained intact"
        );
    }
    archived.acknowledge().unwrap();
    let reopened = open_connection(&root, false).unwrap();
    assert!(rows(&reopened).is_empty());
    drop(reopened);
    assert!(
        root.upgrade_or_archive_history(&REGISTRY)
            .unwrap()
            .is_none()
    );
}

#[test]
fn removed_events_refuse_current_decoding_and_archive_journal_and_checkpoints_intact() {
    let records: Vec<Value> = serde_json::from_str(RETIRED_EVENTS).unwrap();
    assert_eq!(records.len(), 5);
    for row in records {
        let kind = row["kind"].as_str().unwrap();
        let version = row["version"].as_i64().unwrap();
        let payload = row["payload"].to_string();
        assert_eq!(payload_version(kind), None);
        assert_eq!(REGISTRY.event_floor(kind), None);
        assert!(
            Event::decode(
                kind,
                version,
                &payload,
                Some(RunId::new()),
                Some(AgentRunId::new())
            )
            .is_err()
        );
        assert_archived(kind, version, &payload);
    }
}

#[test]
fn supported_worker_retirement_edges_preserve_canonical_bytes() {
    for kind in [
        "ToolProposed",
        "ToolStarted",
        "ToolFinished",
        "HookStarted",
        "HookFinished",
        "GoalPlanContractRecorded",
        "ProviderCallRecorded",
        "AgentSpawned",
        "SupervisorVerdictRecorded",
    ] {
        let (from, source) = current_source(kind);
        let step = REGISTRY.event_step(kind, from).unwrap();
        assert_eq!((step.canonical)(source).as_deref(), Some(source), "{kind}");
        let value: Value = serde_json::from_str(source).unwrap();
        assert_eq!((step.transform)(value.clone()), Some(value), "{kind}");
    }
}

#[test]
fn retired_vocabulary_inside_untrusted_text_is_preserved() {
    let marker = "prepare_worker/worker_handover/worker_data/worker_capture";
    let kind = "ProviderCallRecorded";
    let (from, source) = current_source(kind);
    let mut value: Value = serde_json::from_str(source).unwrap();
    value["response_id"] = marker.into();
    let step = REGISTRY.event_step(kind, from).unwrap();
    let transformed = (step.transform)(value.clone()).unwrap();
    assert_eq!(transformed, value);
    let canonical = (step.canonical)(&transformed.to_string()).unwrap();
    let event = Event::decode(
        kind,
        from + 1,
        &canonical,
        Some(RunId::new()),
        Some(AgentRunId::new()),
    )
    .unwrap();
    assert_eq!(event.payload().unwrap(), canonical);
    assert_eq!(
        serde_json::from_str::<Value>(&canonical).unwrap()["response_id"],
        marker
    );
}

fn retired_call_payloads() -> Vec<(&'static str, Value)> {
    let call = serde_json::json!({"operation":"prepare_worker","preparation":{
        "assignment_id":"01900000-0000-7000-8000-000000000002",
        "purpose":"base","max_chunks":1,"max_bytes":1
    }});
    let handover = serde_json::json!({"operation":"apply_changeset","source":{
        "kind":"worker_handover","assignment_id":"01900000-0000-7000-8000-000000000002",
        "handover_sequence":3,"data":serde_json::from_str::<Vec<Value>>(RETIRED_EVENTS).unwrap()[1]["payload"]["data"]
    }});
    let mut cases = Vec::new();
    for kind in [
        "ToolProposed",
        "ToolStarted",
        "ToolFinished",
        "HookStarted",
        "HookFinished",
    ] {
        for call in [&call, &handover] {
            let mut value: Value = serde_json::from_str(current_source(kind).1).unwrap();
            let pointer = if kind == "HookFinished" {
                "/observation/intent/call"
            } else {
                "/intent/call"
            };
            *value.pointer_mut(pointer).unwrap() = call.clone();
            cases.push((kind, value));
        }
    }
    for call in [&call, &handover] {
        let mut value: Value = serde_json::from_str(current_source("HookFinished").1).unwrap();
        value["result"] = serde_json::json!({"narrowed":{"call":call}});
        cases.push(("HookFinished", value));
    }
    let f = crate::session::worker_protocol_fixture();
    let pins = f
        .events
        .iter()
        .find_map(|event| match event {
            Event::AgentSpawned {
                assignment: Some(pins),
                ..
            } => Some(pins),
            _ => None,
        })
        .unwrap();
    for call in [call, handover] {
        let criterion = serde_json::json!({"kind":"tool_call_succeeded","call":call});
        let mut contract: Value =
            serde_json::from_str(current_source("GoalPlanContractRecorded").1).unwrap();
        contract["contract"]["success_criterion"] = criterion.clone();
        cases.push(("GoalPlanContractRecorded", contract));
        let mut spawn: Value = serde_json::from_str(current_source("AgentSpawned").1).unwrap();
        spawn["assignment"] = serde_json::to_value(pins).unwrap();
        spawn["assignment"]["request"]["criterion"] = criterion.clone();
        cases.push(("AgentSpawned", spawn));
        let mut provider: Value =
            serde_json::from_str(current_source("ProviderCallRecorded").1).unwrap();
        let mut request = serde_json::to_value(&pins.request).unwrap();
        request["criterion"] = criterion;
        provider["delegation"] = serde_json::json!([{"kind":"editor","assignment":request}]);
        cases.push(("ProviderCallRecorded", provider));
    }
    cases
}

fn retired_attestation_payloads() -> Vec<(&'static str, Value)> {
    let data: Vec<Value> = serde_json::from_str(RETIRED_EVENTS).unwrap();
    let mut cases = Vec::new();
    let hook: Value = serde_json::from_str(SELECTION_HOOKFINISHED).unwrap();
    let receipt = &hook["observation"]["attestation"]["guard"];
    for kind in ["ToolFinished", "HookFinished"] {
        for attestation in [
            serde_json::json!({"worker_command":{"receipt":receipt}}),
            serde_json::json!({"worker_read":{"guard":null,"intent_digest":vec![1;32]}}),
            serde_json::json!({"worker_mutation":{"receipt":receipt,"mutation":{"previous_revision":0,"next_revision":1,"data":data[1]["payload"]["data"]}}}),
            serde_json::json!({"worker_data":{"guard":null,"intent_digest":vec![1;32],"data":data[1]["payload"]["data"]}}),
        ] {
            let mut value: Value = serde_json::from_str(current_source(kind).1).unwrap();
            let pointer = if kind == "HookFinished" {
                "/observation/attestation"
            } else {
                "/attestation"
            };
            *value.pointer_mut(pointer).unwrap() = attestation;
            cases.push((kind, value));
        }
    }
    for site in ["worker_capture", "handover"] {
        let mut value: Value =
            serde_json::from_str(current_source("SupervisorVerdictRecorded").1).unwrap();
        value["site"] = site.into();
        cases.push(("SupervisorVerdictRecorded", value));
    }
    cases
}

#[test]
fn enclosing_payloads_refuse_removed_calls_attestations_and_supervisor_subjects() {
    for (kind, value) in retired_call_payloads()
        .into_iter()
        .chain(retired_attestation_payloads())
    {
        let (from, _) = current_source(kind);
        let step = REGISTRY.event_step(kind, from).unwrap();
        assert!((step.canonical)(&value.to_string()).is_none(), "{kind}");
        assert!((step.transform)(value.clone()).is_none(), "{kind}");
        let scope = intent_scope(&value).unwrap_or_else(|| (RunId::new(), AgentRunId::new()));
        assert!(
            Event::decode(
                kind,
                payload_version(kind).unwrap(),
                &value.to_string(),
                Some(scope.0),
                Some(scope.1)
            )
            .is_err(),
            "{kind}"
        );
        assert_archived(kind, from, &value.to_string());
    }
}

#[test]
fn live_editor_history_migrates_and_reopens_without_changing_effects_or_receipts() {
    for with_editor in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = StateRoot::admit(&temp.path().join("state")).unwrap();
        let fixture = crate::session::worker_protocol_fixture();
        let mut connection = open_connection(&root, false).unwrap();
        let mut hot = None;
        for event in fixture.events {
            if !with_editor
                && matches!(&event, Event::ProviderCallRecorded { record, .. } if record.delegation.is_some())
            {
                break;
            }
            append(&mut connection, &mut hot, fixture.session, event).unwrap();
        }
        let original = stored_rows(&connection);
        let (_, expected) = resolve_view(&connection, fixture.session, 0, &mut 0).unwrap();
        let mut changed = 0;
        for (kind, from, _, _) in EVENT_FIXTURES {
            if [
                "ToolStarted",
                "ToolFinished",
                "AgentSpawned",
                "ProviderCallRecorded",
                "GoalPlanContractRecorded",
            ]
            .contains(&kind)
                && from == current_source(kind).0
            {
                changed += connection
                    .execute(
                        "UPDATE events SET event_version = ?1 WHERE kind = ?2",
                        rusqlite::params![from, kind],
                    )
                    .unwrap();
            }
        }
        assert!(changed > 0);
        drop(connection);
        assert!(
            root.upgrade_or_archive_history(&REGISTRY)
                .unwrap()
                .is_none()
        );
        let connection = open_connection(&root, true).unwrap();
        assert_eq!(stored_rows(&connection), original);
        assert_eq!(
            resolve_view(&connection, fixture.session, 0, &mut 0)
                .unwrap()
                .1,
            expected
        );
        drop(connection);
        assert!(
            root.upgrade_or_archive_history(&REGISTRY)
                .unwrap()
                .is_none()
        );
    }
}

fn private_target() -> Value {
    serde_json::json!({
        "kind":"worker_view",
        "assignment_id":"01900000-0000-7000-8000-000000000002",
        "base_digest":vec![1_u8; 32],
        "revision":0,
        "cache_generation":null
    })
}

#[test]
fn enclosing_payloads_refuse_the_removed_private_target_and_cache_outcome() {
    for kind in [
        "ToolProposed",
        "ToolStarted",
        "ToolFinished",
        "HookStarted",
        "HookFinished",
    ] {
        let (from, source) = current_source(kind);
        let observation = if kind == "HookFinished" {
            "/observation"
        } else {
            ""
        };
        let mut cases = Vec::new();
        let mut target: Value = serde_json::from_str(source).unwrap();
        *target
            .pointer_mut(&format!("{observation}/intent/target"))
            .unwrap() = private_target();
        cases.push(target);
        if matches!(kind, "ToolFinished" | "HookFinished") {
            let mut cache: Value = serde_json::from_str(source).unwrap();
            *cache
                .pointer_mut(&format!("{observation}/cache_update"))
                .unwrap() = "discarded".into();
            cases.push(cache);
        }
        let step = REGISTRY.event_step(kind, from).unwrap();
        for value in cases {
            assert!((step.canonical)(&value.to_string()).is_none(), "{kind}");
            assert!((step.transform)(value.clone()).is_none(), "{kind}");
            let scope = intent_scope(&value).unwrap();
            assert!(
                Event::decode(
                    kind,
                    payload_version(kind).unwrap(),
                    &value.to_string(),
                    Some(scope.0),
                    Some(scope.1)
                )
                .is_err(),
                "{kind}"
            );
            assert_archived(kind, from, &value.to_string());
        }
    }
}

#[test]
fn run_started_worker_allowance_is_dropped_by_the_current_policy_step() {
    use crate::session::worker::WORKER_DATA_BYTES;
    let step = REGISTRY.event_step("RunStarted", 24).unwrap();
    let fixture = include_str!("runstarted-v24.json");
    assert_eq!((step.canonical)(fixture).as_deref(), Some(fixture));
    let null_policy: Value = serde_json::from_str(fixture).unwrap();
    assert_eq!(
        (step.transform)(null_policy.clone()),
        Some(null_policy.clone())
    );
    let policy = |allowance: Value| {
        let mut source = null_policy.clone();
        source["config"]["worker_policy"] = serde_json::json!({
            "parent_grant":{"read_paths":["src"],"write_paths":["src"],"tools":["edit"],"skills":[],"mcp_servers":[],"commands":[],"permission_mode":null},
            "concurrency":1,"max_turns":4,"max_tools":3,"data_bytes":allowance
        });
        source
    };
    let source = policy(1024.into());
    let canonical = (step.canonical)(&source.to_string()).unwrap();
    assert!(canonical.contains(r#""max_tools":3,"data_bytes":1024}"#));
    let current = (step.transform)(source).unwrap();
    assert!(current["config"]["worker_policy"]["data_bytes"].is_null());
    assert!(
        serde_json::from_value::<crate::session::worker::WorkerPolicy>(
            current["config"]["worker_policy"].clone()
        )
        .is_ok()
    );
    for forged in [
        policy(0.into()),
        policy((WORKER_DATA_BYTES as u64 + 1).into()),
        policy("1024".into()),
    ] {
        assert!((step.canonical)(&forged.to_string()).is_none());
        assert!((step.transform)(forged).is_none());
    }
    let mut missing = policy(1024.into());
    missing["config"]["worker_policy"]
        .as_object_mut()
        .unwrap()
        .remove("data_bytes");
    assert!((step.canonical)(&missing.to_string()).is_none());
    assert!((step.transform)(missing).is_none());
}

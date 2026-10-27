use super::*;
use crate::session::{AgentRunId, CompactionRecord, PAYLOAD_VERSIONS, RunId};
use crate::store::journal::{append, fork_session, open_connection};
use crate::store::replay::{MAX_SESSION_EVENTS, resolve_open_view};
use crate::store::state::FAIL_AT;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

#[path = "retired_worker_tests.rs"]
mod retired_worker;
#[path = "shared_editors_tests.rs"]
mod shared_editors;

const CHECKPOINT_V1: &str = include_str!("checkpoints-v1.sql");

const ROLE_SETUP_V1: &str = r#"{"operation_id":"01900000-0000-7000-8000-000000000001","target":{"kind":"CustomProfile","identity":"example-route"},"expected_digest":null}"#;
const ROLE_SETUP_V2: &str = r#"{"operation_id":"01900000-0000-7000-8000-000000000001","target":{"kind":"RoleRouting","identity":"roles"},"expected_digest":null}"#;

type EventFixture<'a> = (&'a str, i64, &'a str, &'a str);
// One canonical (kind, from, input, expected) fixture per production step, plus a command-result fixture.
const EVENT_FIXTURES: [EventFixture<'static>; 82] = [
    (
        "AgentFinished",
        11,
        include_str!("agentfinished-v11.json"),
        include_str!("agentfinished-v11.json"),
    ),
    (
        "AgentSpawned",
        15,
        include_str!("agentspawned-v15.json"),
        include_str!("agentspawned-v15.json"),
    ),
    (
        "AgentFinished",
        10,
        include_str!("agentfinished-v10.json"),
        include_str!("agentfinished-v10.json"),
    ),
    ("ToolFinished", 21, GLOB_TOOLFINISHED, GLOB_TOOLFINISHED),
    ("HookStarted", 5, GLOB_HOOKSTARTED, GLOB_HOOKSTARTED),
    ("HookFinished", 10, GLOB_HOOKFINISHED, GLOB_HOOKFINISHED),
    (
        "GoalPlanContractRecorded",
        8,
        GLOB_GOALPLANCONTRACTRECORDED,
        GLOB_GOALPLANCONTRACTRECORDED,
    ),
    (
        "ProviderCallRecorded",
        15,
        GLOB_PROVIDERCALLRECORDED,
        GLOB_PROVIDERCALLRECORDED,
    ),
    ("RunStarted", 20, GLOBAL_RUNSTARTED, GLOBAL_RUNSTARTED),
    ("AgentSpawned", 12, GLOB_AGENTSPAWNED, GLOB_AGENTSPAWNED),
    (
        "ToolFinished",
        26,
        SELECTION_TOOLFINISHED,
        SELECTION_TOOLFINISHED,
    ),
    (
        "HookStarted",
        9,
        include_str!("hookstarted-without-expiry.json"),
        include_str!("hookstarted-without-expiry.json"),
    ),
    (
        "HookFinished",
        15,
        SELECTION_HOOKFINISHED,
        SELECTION_HOOKFINISHED,
    ),
    (
        "GoalPlanContractRecorded",
        10,
        GLOB_GOALPLANCONTRACTRECORDED,
        GLOB_GOALPLANCONTRACTRECORDED,
    ),
    (
        "ProviderCallRecorded",
        20,
        PROVIDER_CALL_V19,
        PROVIDER_CALL_V19,
    ),
    ("RunStarted", 22, HOST_RUNSTARTED, HOST_RUNSTARTED),
    ("AgentSpawned", 14, GLOB_AGENTSPAWNED, GLOB_AGENTSPAWNED),
    ("GoalStarted", 7, GOAL_STARTED_V7, GOAL_STARTED_V8),
    ("ToolFinished", 22, GLOB_TOOLFINISHED, GLOB_TOOLFINISHED),
    ("HookStarted", 6, GLOB_HOOKSTARTED, GLOB_HOOKSTARTED),
    ("HookFinished", 11, GLOB_HOOKFINISHED, GLOB_HOOKFINISHED),
    (
        "GoalPlanContractRecorded",
        9,
        GLOB_GOALPLANCONTRACTRECORDED,
        GLOB_GOALPLANCONTRACTRECORDED,
    ),
    (
        "ProviderCallRecorded",
        16,
        GLOB_PROVIDERCALLRECORDED,
        GLOB_PROVIDERCALLRECORDED,
    ),
    (
        "ToolFinished",
        24,
        GLOB_TOOLFINISHED,
        include_str!("toolfinished-without-expiry.json"),
    ),
    (
        "HookStarted",
        8,
        GLOB_HOOKSTARTED,
        include_str!("hookstarted-without-expiry.json"),
    ),
    (
        "HookFinished",
        13,
        GLOB_HOOKFINISHED,
        include_str!("hookfinished-without-expiry.json"),
    ),
    ("GoalFinished", 7, GOAL_FINISHED_V7, GOAL_FINISHED_V8),
    ("ProviderCallRecorded", 18, REFUSAL_V18, REFUSAL_V19),
    (
        "ProviderCallRecorded",
        19,
        PROVIDER_CALL_V19,
        PROVIDER_CALL_V19,
    ),
    (
        "ToolFinished",
        25,
        include_str!("toolfinished-without-expiry.json"),
        include_str!("toolfinished-without-expiry.json"),
    ),
    (
        "ToolFinished",
        25,
        COMMAND_TOOLFINISHED_WITHOUT_EXPIRY,
        SELECTION_TOOLFINISHED,
    ),
    (
        "HookFinished",
        14,
        include_str!("hookfinished-without-expiry.json"),
        SELECTION_HOOKFINISHED,
    ),
    ("ToolFinished", 20, GLOB_TOOLFINISHED, GLOB_TOOLFINISHED),
    ("HookStarted", 4, GLOB_HOOKSTARTED, GLOB_HOOKSTARTED),
    ("HookFinished", 9, GLOB_HOOKFINISHED, GLOB_HOOKFINISHED),
    (
        "GoalPlanContractRecorded",
        7,
        GLOB_GOALPLANCONTRACTRECORDED,
        GLOB_GOALPLANCONTRACTRECORDED,
    ),
    (
        "ProviderCallRecorded",
        14,
        GLOB_PROVIDERCALLRECORDED,
        GLOB_PROVIDERCALLRECORDED,
    ),
    ("ToolFinished", 19, GLOB_TOOLFINISHED, GLOB_TOOLFINISHED),
    ("HookStarted", 3, GLOB_HOOKSTARTED, GLOB_HOOKSTARTED),
    ("HookFinished", 8, GLOB_HOOKFINISHED, GLOB_HOOKFINISHED),
    (
        "GoalPlanContractRecorded",
        6,
        GLOB_GOALPLANCONTRACTRECORDED,
        GLOB_GOALPLANCONTRACTRECORDED,
    ),
    ("AgentSpawned", 11, GLOB_AGENTSPAWNED, GLOB_AGENTSPAWNED),
    ("RunStarted", 17, GLOB_RUNSTARTED, GLOB_RUNSTARTED),
    ("RunStarted", 18, ROOT_RUNSTARTED, NESTED_RUNSTARTED),
    ("RunStarted", 19, NESTED_RUNSTARTED, GLOBAL_RUNSTARTED),
    (
        "ProviderCallRecorded",
        13,
        GLOB_PROVIDERCALLRECORDED,
        GLOB_PROVIDERCALLRECORDED,
    ),
    ("RunStarted", 21, HOST_RUNSTARTED, HOST_RUNSTARTED),
    ("ToolFinished", 23, GLOB_TOOLFINISHED, GLOB_TOOLFINISHED),
    ("AgentSpawned", 13, GLOB_AGENTSPAWNED, GLOB_AGENTSPAWNED),
    ("HookStarted", 7, GLOB_HOOKSTARTED, GLOB_HOOKSTARTED),
    ("HookFinished", 12, GLOB_HOOKFINISHED, GLOB_HOOKFINISHED),
    (
        "ProviderCallRecorded",
        17,
        GLOB_PROVIDERCALLRECORDED,
        GLOB_PROVIDERCALLRECORDED,
    ),
    (
        "WorkspaceRewindStarted",
        1,
        REWIND_STARTED_V1,
        REWIND_STARTED_V2,
    ),
    (
        "ToolFinished",
        27,
        include_str!("toolfinished-without-expiry.json"),
        include_str!("toolfinished-without-expiry.json"),
    ),
    (
        "HookStarted",
        10,
        include_str!("hookstarted-without-expiry.json"),
        include_str!("hookstarted-without-expiry.json"),
    ),
    (
        "HookFinished",
        16,
        SELECTION_HOOKFINISHED,
        SELECTION_HOOKFINISHED,
    ),
    (
        "GoalPlanContractRecorded",
        11,
        GLOB_GOALPLANCONTRACTRECORDED,
        GLOB_GOALPLANCONTRACTRECORDED,
    ),
    (
        "ProviderCallRecorded",
        21,
        GLOB_PROVIDERCALLRECORDED,
        GLOB_PROVIDERCALLRECORDED,
    ),
    (
        "ToolFinished",
        28,
        include_str!("toolfinished-without-expiry.json"),
        include_str!("toolfinished-without-expiry.json"),
    ),
    (
        "HookStarted",
        11,
        include_str!("hookstarted-without-expiry.json"),
        include_str!("hookstarted-without-expiry.json"),
    ),
    (
        "HookFinished",
        17,
        SELECTION_HOOKFINISHED,
        SELECTION_HOOKFINISHED,
    ),
    (
        "GoalPlanContractRecorded",
        12,
        GLOB_GOALPLANCONTRACTRECORDED,
        GLOB_GOALPLANCONTRACTRECORDED,
    ),
    (
        "ProviderCallRecorded",
        23,
        GLOB_PROVIDERCALLRECORDED,
        GLOB_PROVIDERCALLRECORDED,
    ),
    ("ProviderCallRecorded", 22, REFUSAL_V19, REFUSAL_V19),
    ("ProviderSetupEditStarted", 1, ROLE_SETUP_V1, ROLE_SETUP_V1),
    ("ProviderSetupEditStarted", 2, ROLE_SETUP_V2, ROLE_SETUP_V2),
    (
        "RunStarted",
        23,
        GLOBAL_RUNSTARTED,
        include_str!("runstarted-v24.json"),
    ),
    (
        "ToolProposed",
        1,
        include_str!("toolproposed-v1.json"),
        include_str!("toolproposed-v1.json"),
    ),
    (
        "ToolStarted",
        23,
        include_str!("toolstarted-v23.json"),
        include_str!("toolstarted-v23.json"),
    ),
    (
        "ToolFinished",
        29,
        SELECTION_TOOLFINISHED,
        SELECTION_TOOLFINISHED,
    ),
    (
        "HookStarted",
        12,
        include_str!("hookstarted-without-expiry.json"),
        include_str!("hookstarted-without-expiry.json"),
    ),
    (
        "HookFinished",
        18,
        SELECTION_HOOKFINISHED,
        SELECTION_HOOKFINISHED,
    ),
    (
        "GoalPlanContractRecorded",
        13,
        GLOB_GOALPLANCONTRACTRECORDED,
        GLOB_GOALPLANCONTRACTRECORDED,
    ),
    (
        "ProviderCallRecorded",
        24,
        GLOB_PROVIDERCALLRECORDED,
        GLOB_PROVIDERCALLRECORDED,
    ),
    (
        "AgentSpawned",
        16,
        include_str!("agentspawned-v15.json"),
        include_str!("agentspawned-v15.json"),
    ),
    (
        "SupervisorVerdictRecorded",
        1,
        include_str!("supervisorverdictrecorded-v1.json"),
        include_str!("supervisorverdictrecorded-v1.json"),
    ),
    (
        "ToolProposed",
        2,
        include_str!("toolproposed-v1.json"),
        include_str!("toolproposed-v1.json"),
    ),
    (
        "ToolStarted",
        24,
        include_str!("toolstarted-v23.json"),
        include_str!("toolstarted-v23.json"),
    ),
    (
        "ToolFinished",
        30,
        SELECTION_TOOLFINISHED,
        SELECTION_TOOLFINISHED,
    ),
    (
        "HookStarted",
        13,
        include_str!("hookstarted-without-expiry.json"),
        include_str!("hookstarted-without-expiry.json"),
    ),
    (
        "HookFinished",
        19,
        SELECTION_HOOKFINISHED,
        SELECTION_HOOKFINISHED,
    ),
    (
        "RunStarted",
        24,
        include_str!("runstarted-v24.json"),
        include_str!("runstarted-v24.json"),
    ),
];

#[test]
fn migrated_intent_records_preserve_runtime_bytes() {
    for (kind, from, input, _) in EVENT_FIXTURES {
        let payload = REGISTRY
            .upgrade_payload(kind, from, input)
            .unwrap()
            .unwrap_or_else(|| input.to_owned());
        let value: Value = serde_json::from_str(&payload).unwrap();
        let intent = value
            .get("intent")
            .or_else(|| value.get("observation")?.get("intent"));
        assert_eq!(
            intent.is_some(),
            matches!(
                kind,
                "ToolProposed" | "ToolStarted" | "ToolFinished" | "HookStarted" | "HookFinished"
            ),
            "{kind} v{from}"
        );
        if let Some(intent) = intent {
            assert_intent_record_bytes(intent.clone());
        }
    }
    for fixture in [
        crate::session::worker_protocol_fixture(),
        crate::session::worker_compacted_fixture(true),
        crate::session::host_protocol_fixture(),
        crate::session::session_host_protocol_fixture(),
    ] {
        for envelope in fixture.envelopes() {
            if let Event::ToolStarted { intent, .. } = &envelope.event {
                assert_intent_record_bytes(serde_json::to_value(intent).unwrap());
            }
        }
    }
}

fn assert_intent_record_bytes(value: Value) {
    let runtime: crate::tools::EffectIntent = serde_json::from_value(value.clone()).unwrap();
    let record: crate::session::ToolIntentRecord = serde_json::from_value(value).unwrap();
    assert_eq!(
        serde_json::to_vec(&runtime).unwrap(),
        serde_json::to_vec(&record).unwrap()
    );
    assert_eq!(
        crate::session::ToolIntentRecord::from(runtime.clone()),
        record
    );
    assert_eq!(crate::tools::EffectIntent::from(record), runtime);
}

const GOAL_STARTED_V7: &str = r#"{"goal_id":"01a15ecf-a92a-7707-8af6-ce3c5a16fa5f","objective":"Literal @not-an-input","workspace_device":1,"workspace_inode":2,"budget":{"max_provider_calls":null,"max_tokens":null,"max_active_ms":null,"max_runs":16},"phase":"implementation","autonomous":false,"resource_settings":{"default_command_timeout_ms":120000,"max_command_timeout_ms":600000,"build_memory_bytes":4294967296,"build_processes":256,"build_project_bytes":1073741824,"build_cache_bytes":8589934592},"continues":null}"#;
const GOAL_STARTED_V8: &str = r#"{"goal_id":"01a15ecf-a92a-7707-8af6-ce3c5a16fa5f","objective":"Literal @not-an-input","include_paths":null,"workspace_device":1,"workspace_inode":2,"budget":{"max_provider_calls":null,"max_tokens":null,"max_active_ms":null,"max_runs":16},"phase":"implementation","autonomous":false,"resource_settings":{"default_command_timeout_ms":120000,"max_command_timeout_ms":600000,"build_memory_bytes":4294967296,"build_processes":256,"build_project_bytes":1073741824,"build_cache_bytes":8589934592},"continues":null}"#;
const GOAL_FINISHED_V7: &str = r#"{"goal_id":"01a15ecf-a92a-7707-8af6-ce3c5a16fa5f","status":"failed","active_ms":0,"report":{"summary":"Goal failed.","changes":[],"evidence":[],"unresolved":[],"cached_input_tokens":0},"exhausted":true}"#;
const GOAL_FINISHED_V8: &str = r#"{"goal_id":"01a15ecf-a92a-7707-8af6-ce3c5a16fa5f","status":"failed","active_ms":0,"report":{"summary":"Goal failed.","changes":[],"evidence":[],"unresolved":[],"cached_input_tokens":0},"exhausted":true,"budget_refusal":null}"#;
const REWIND_STARTED_V1: &str = r#"{"rewind_id":"01a15ecf-a92a-7707-8af6-ce3c5a16fa5f","target":{"kind":"run","run_id":"01a15ecf-a92a-7707-8af6-ce3c5a16fa60","sequence":1},"head_sequence":2,"prefix_digest":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"plan_digest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"file_count":1,"restored_bytes":0,"kept":[],"overwrite":[],"workspace_device":1,"workspace_inode":1,"policy_digest":[3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3],"enforcement_digest":[4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4],"expires_at_ms":60000}"#;
const REWIND_STARTED_V2: &str = r#"{"rewind_id":"01a15ecf-a92a-7707-8af6-ce3c5a16fa5f","target":{"kind":"run","run_id":"01a15ecf-a92a-7707-8af6-ce3c5a16fa60","sequence":1},"head_sequence":2,"prefix_digest":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"plan_digest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"file_count":1,"restored_bytes":0,"kept":[],"overwrite":[],"workspace_device":1,"workspace_inode":1,"policy_digest":[3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3],"enforcement_digest":[4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4]}"#;

#[test]
fn refusal_reclassification_preserves_evidence_and_rejects_new_causes_in_old_payloads() {
    let step = REGISTRY
        .event_step("ProviderCallRecorded", 18)
        .expect("migration");
    assert_eq!((step.canonical)(REFUSAL_V18).as_deref(), Some(REFUSAL_V18));
    let expected: Value = serde_json::from_str(REFUSAL_V19).unwrap();
    assert_eq!(
        (step.transform)(serde_json::from_str(REFUSAL_V18).unwrap()),
        Some(expected.clone())
    );
    let source: Value = serde_json::from_str(REFUSAL_V18).unwrap();
    let mut intermediate = source.clone();
    for from in [16, 17] {
        let host_admission = REGISTRY
            .event_step("ProviderCallRecorded", from)
            .expect("main migration");
        assert_eq!(
            (host_admission.canonical)(&intermediate.to_string()).as_deref(),
            Some(REFUSAL_V18)
        );
        intermediate =
            (host_admission.transform)(intermediate).expect("main admits the old refusal");
        assert_eq!(
            intermediate, source,
            "host admission preserves the refusal classification"
        );
    }
    assert_eq!((step.transform)(intermediate), Some(expected));
    for forged in [
        REFUSAL_V18.replace("invalid_arguments", "invalid_path"),
        REFUSAL_V18.replace("invalid_response", "rejected"),
    ] {
        assert!((step.canonical)(&forged).is_none());
        assert!((step.transform)(serde_json::from_str(&forged).unwrap()).is_none());
    }
}

#[test]
fn provider_call_v19_identity_migration_preserves_bytes_and_rejects_new_reason() {
    let step = REGISTRY
        .event_step("ProviderCallRecorded", 19)
        .expect("migration");
    assert_eq!(
        (step.canonical)(PROVIDER_CALL_V19).as_deref(),
        Some(PROVIDER_CALL_V19)
    );
    let migrated = (step.transform)(serde_json::from_str(PROVIDER_CALL_V19).unwrap())
        .expect("identity migration");
    assert_eq!(
        migrated,
        serde_json::from_str::<Value>(PROVIDER_CALL_V19).unwrap()
    );

    let forged = PROVIDER_CALL_V19.replace(
        "\"failure_reason\":null",
        "\"failure_reason\":\"outcome_decode\"",
    );
    assert!((step.canonical)(&forged).is_none());
    assert!((step.transform)(serde_json::from_str(&forged).unwrap()).is_none());
    let list_step = REGISTRY
        .event_step("ProviderCallRecorded", 21)
        .expect("PR-list migration follows file removal");
    assert_eq!(
        (list_step.canonical)(&forged).as_deref(),
        Some(forged.as_str())
    );
    assert_eq!(
        (list_step.transform)(serde_json::from_str(&forged).unwrap()),
        Some(serde_json::from_str(&forged).unwrap())
    );
}

#[test]
fn editor_refusal_migration_preserves_bytes_and_rejects_forged_old_causes() {
    let step = REGISTRY.event_step("ProviderCallRecorded", 22).unwrap();
    let view_step = REGISTRY.event_step("ProviderCallRecorded", 23).unwrap();
    assert_eq!((step.canonical)(REFUSAL_V19).as_deref(), Some(REFUSAL_V19));
    assert_eq!(
        (step.transform)(serde_json::from_str(REFUSAL_V19).unwrap()),
        Some(serde_json::from_str(REFUSAL_V19).unwrap())
    );
    for cause in [
        "editor_definition",
        "editor_digest",
        "editor_commands",
        "editor_turns",
        "editor_lease",
        "editor_criterion",
        "editor_resume",
        "editor_grant",
    ] {
        let forged = REFUSAL_V19.replace("invalid_arguments", cause);
        assert!((step.canonical)(&forged).is_none(), "{cause}");
        assert!(
            (step.transform)(serde_json::from_str(&forged).unwrap()).is_none(),
            "{cause}"
        );
        assert_eq!(
            (view_step.canonical)(&forged).as_deref(),
            Some(forged.as_str()),
            "{cause}"
        );
        assert_eq!(
            (view_step.transform)(serde_json::from_str(&forged).unwrap()),
            Some(serde_json::from_str(&forged).unwrap()),
            "{cause}"
        );
        Event::decode(
            "ProviderCallRecorded",
            crate::test_support::payload_version("ProviderCallRecorded"),
            &forged,
            Some(RunId::new()),
            Some(AgentRunId::new()),
        )
        .expect("current editor cause decodes");
    }
}

#[test]
fn multiedit_identity_steps_reject_forged_old_calls_and_worker_capabilities() {
    for (kind, from, input, expected) in &EVENT_FIXTURES[..8] {
        let step = REGISTRY.event_step(kind, *from).unwrap();
        let value: Value = serde_json::from_str(input).unwrap();
        assert_eq!(
            (step.canonical)(input).as_deref(),
            Some(*expected),
            "{kind}"
        );
        assert_eq!(
            (step.transform)(value),
            Some(serde_json::from_str(expected).unwrap())
        );
    }
    for tool in ["edit", "multiedit"] {
        let mut source: Value = serde_json::from_str(GLOBAL_RUNSTARTED).unwrap();
        source["config"]["worker_policy"] = serde_json::json!({
            "parent_grant":{"read_paths":["src"],"write_paths":["src"],"tools":[tool],"skills":[],"mcp_servers":[],"commands":[],"permission_mode":null},
            "concurrency":1,"max_turns":4,"max_tools":3,"data_bytes":1024
        });
        let step = REGISTRY.event_step("RunStarted", 20).unwrap();
        assert_eq!(
            (step.canonical)(&source.to_string()).is_some(),
            tool == "edit"
        );
        let upgraded = (step.transform)(source.clone());
        assert_eq!(upgraded.is_some(), tool == "edit");
        if let Some(upgraded) = upgraded {
            assert_eq!(upgraded, source);
        }
        let host_step = REGISTRY.event_step("RunStarted", 21).unwrap();
        assert_eq!((host_step.transform)(source.clone()), Some(source));
    }
}
const REFUSAL_V18: &str = r#"{"delegation":null,"phase":"root_plan","disposition":"invalid_response","requested_tools":0,"response_id":"response-1","input_tokens":12,"output_tokens":8,"cache_read_input_tokens":null,"cache_creation_input_tokens":null,"wire_provenance":null,"failure_reason":"proposal_refused","refusal":{"operation":"arany_command","cause":"invalid_arguments"}}"#;
const REFUSAL_V19: &str = r#"{"delegation":null,"phase":"root_plan","disposition":"rejected","requested_tools":0,"response_id":"response-1","input_tokens":12,"output_tokens":8,"cache_read_input_tokens":null,"cache_creation_input_tokens":null,"wire_provenance":null,"failure_reason":"proposal_refused","refusal":{"operation":"arany_command","cause":"invalid_arguments"}}"#;
const PROVIDER_CALL_V19: &str = r#"{"delegation":null,"phase":"root_plan","disposition":"invalid_response","requested_tools":0,"response_id":null,"input_tokens":null,"output_tokens":null,"cache_read_input_tokens":null,"cache_creation_input_tokens":null,"wire_provenance":null,"failure_reason":null,"refusal":null}"#;

const GLOB_PROVIDERCALLRECORDED: &str = r#"{"delegation":null,"phase":"root_plan","disposition":"finished","requested_tools":0,"response_id":"response-1","input_tokens":100,"output_tokens":10,"cache_read_input_tokens":40,"cache_creation_input_tokens":20,"wire_provenance":null,"failure_reason":null,"refusal":null}"#;

const GLOB_TOOLFINISHED: &str = r#"{"intent":{"target":{"kind":"workspace"},"id":"01a15ecf-a92b-736a-91a6-560b8bc09501","run_id":"01a15ecf-a92a-7707-8af6-ce3d36320b67","agent_run_id":"01a15ecf-a92a-7707-8af6-ce3e0c00af83","policy_digest":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"enforcement_digest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"workspace_device":1,"workspace_inode":2,"call":{"operation":"read","path":"src/lib.rs","offset":null,"limit":1024},"limits":{"result_bytes":65536,"runtime_ms":60000,"cache_transfer_ms":0,"memory_bytes":536870912,"max_processes":64,"scratch_bytes":67108864,"project_bytes":67108864,"cache_requested_bytes":67108864,"cache_bytes":67108864,"cache_candidate_bytes":67108864,"network":"none","resource_class":"default","resource_profile":"linux_kernel"},"expires_at_ms":1,"use_count":1},"disposition":"succeeded","enforcement_cause":null,"limit_cause":null,"output":"{\"sha256\":\"2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881\"}","attestation":{"in_process":{"policy_digest":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"workspace_device":1,"workspace_inode":2}},"cache_update":"not_applicable","checkpoint":null}"#;
const GLOB_HOOKSTARTED: &str = r#"{"event":"PreToolUse","group":0,"handler":0,"subject_digest":[4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4,4],"intent":{"target":{"kind":"workspace"},"id":"01a15ecf-a92b-736a-91a6-560ce9fc3701","run_id":"01a15ecf-a92a-7707-8af6-ce3d36320b67","agent_run_id":"01a15ecf-a92a-7707-8af6-ce3e0c00af83","policy_digest":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"enforcement_digest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"workspace_device":1,"workspace_inode":2,"call":{"operation":"command","program":"sh","args":["-c","exit 2"],"cwd":""},"limits":{"result_bytes":65536,"runtime_ms":60000,"cache_transfer_ms":0,"memory_bytes":536870912,"max_processes":64,"scratch_bytes":67108864,"project_bytes":67108864,"cache_requested_bytes":67108864,"cache_bytes":67108864,"cache_candidate_bytes":67108864,"network":"none","resource_class":"default","resource_profile":"linux_kernel"},"expires_at_ms":1,"use_count":1},"input_digest":[9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9],"active_ceiling_ms":null}"#;
const GLOB_HOOKFINISHED: &str = r#"{"boundary_active_ms":null,"observation":{"intent":{"target":{"kind":"workspace"},"id":"01a15ecf-a92b-736a-91a6-560ce9fc3701","run_id":"01a15ecf-a92a-7707-8af6-ce3d36320b67","agent_run_id":"01a15ecf-a92a-7707-8af6-ce3e0c00af83","policy_digest":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"enforcement_digest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"workspace_device":1,"workspace_inode":2,"call":{"operation":"command","program":"sh","args":["-c","exit 2"],"cwd":""},"limits":{"result_bytes":65536,"runtime_ms":60000,"cache_transfer_ms":0,"memory_bytes":536870912,"max_processes":64,"scratch_bytes":67108864,"project_bytes":67108864,"cache_requested_bytes":67108864,"cache_bytes":67108864,"cache_candidate_bytes":67108864,"network":"none","resource_class":"default","resource_profile":"linux_kernel"},"expires_at_ms":1,"use_count":1},"disposition":"failed","enforcement_cause":null,"limit_cause":null,"output":"{\"changeset\":{\"files\":[]},\"exit_code\":2,\"stderr\":\"blocked\",\"stderr_total\":{\"bytes\":7,\"lines\":1},\"stdout\":\"\",\"stdout_total\":{\"bytes\":0,\"lines\":0}}","attestation":{"guard":{"contract_version":1,"intent_digest":[131,11,117,251,8,22,13,66,136,215,191,13,209,208,209,113,109,80,241,218,188,193,7,40,115,63,160,61,172,44,137,133],"enforcement_digest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"limits":{"result_bytes":65536,"runtime_ms":60000,"cache_transfer_ms":0,"memory_bytes":536870912,"max_processes":64,"scratch_bytes":67108864,"project_bytes":67108864,"cache_requested_bytes":67108864,"cache_bytes":67108864,"cache_candidate_bytes":67108864,"network":"none","resource_class":"default","resource_profile":"linux_kernel"}}},"cache_update":"not_applicable"},"result":{"restricted":{"decision":"deny","reason":"blocked"}}}"#;
const GLOB_GOALPLANCONTRACTRECORDED: &str = r#"{"goal_id":"01a15ecf-a92a-7707-8af6-ce3c5a16fa5f","contract":{"mission":"Plan","paths":["src/lib.rs"],"commands":[],"resource_class":"default","command_timeout_ms":null,"skills":[],"mcp_tools":[],"success_criterion":{"kind":"workspace_file_content","path":"src/lib.rs","content":"ok"},"budget":{"max_provider_calls":64,"max_tokens":1000,"max_active_ms":1800000,"max_runs":16}}}"#;
const GLOB_AGENTSPAWNED: &str = r#"{"profile":null,"assignment":null,"definition":null,"role":"primary","ordinal":0,"objective":null,"grant":null}"#;
const GLOB_RUNSTARTED: &str = r#"{"config":{"supervisor":null,"worker_policy":null,"agent_definitions":null,"roster":null,"provider":"openai","model":"gpt-5.4","effort":null,"custom_profile_provenance":null,"saved_api_account_id":null,"chatgpt_provenance":null,"output_token_bound":"provider_enforced","context_window_tokens":null,"policy":{"mode":"single"},"output_token_cap":4096,"provider_concurrency":1,"workspace_device":1,"workspace_inode":1,"instruction_digest":null,"include_digests":[],"history_run_ids":[],"excluded_history_runs":0,"context_usage":null,"compaction_event_sequence":null,"compaction_content_digest":null,"tool_policy":null},"active_deadline_ms":1800000}"#;
// Version 18 pinned only the root file's digest; version 19 names it as the root's source.
const ROOT_RUNSTARTED: &str = r#"{"config":{"supervisor":null,"worker_policy":null,"agent_definitions":null,"roster":null,"provider":"openai","model":"gpt-5.4","effort":null,"custom_profile_provenance":null,"saved_api_account_id":null,"chatgpt_provenance":null,"output_token_bound":"provider_enforced","context_window_tokens":null,"policy":{"mode":"single"},"output_token_cap":4096,"provider_concurrency":1,"workspace_device":1,"workspace_inode":1,"instruction_digest":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7],"include_digests":[],"history_run_ids":[],"excluded_history_runs":0,"context_usage":null,"compaction_event_sequence":null,"compaction_content_digest":null,"tool_policy":null},"active_deadline_ms":1800000}"#;
// Version 20 adds the explicit null user-instruction digest.
const HOST_RUNSTARTED: &str = r#"{"config":{"supervisor":null,"worker_policy":null,"agent_definitions":null,"roster":null,"provider":"openai","model":"gpt-5.4","effort":null,"custom_profile_provenance":null,"saved_api_account_id":null,"chatgpt_provenance":null,"output_token_bound":"provider_enforced","context_window_tokens":null,"policy":{"mode":"single"},"output_token_cap":4096,"provider_concurrency":1,"workspace_device":1,"workspace_inode":1,"user_instruction_digest":null,"instruction_sources":[{"directory":"","file":null,"digest":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7]}],"launch_directory":"","include_digests":[],"history_run_ids":[],"excluded_history_runs":0,"context_usage":null,"compaction_event_sequence":null,"compaction_content_digest":null,"tool_policy":{"contract_version":1,"config_digest":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"permissions_digest":null,"max_context_bytes":65536,"unguarded_rules_digest":null,"unguarded":"human_only","child_unguarded":"denied"}},"active_deadline_ms":1800000}"#;
const CURRENT_HOST_RUNSTARTED: &str = r#"{"config":{"supervisor":{"evaluator":1,"window":32,"floors":{"identical":3,"failing":2,"cycle_two":3,"cycle_other":3,"oscillation":1,"stalled":2,"denial_streak":2,"denials_window":65535,"distinct_denied":65535,"alias":1,"files":16,"files_per_root":4,"delete_permille":250,"deleted_files":1,"growth_permille":65535,"changed_kib":65535},"candidates":{"identical":4,"failing":3,"cycle_two":3,"cycle_other":5,"oscillation":2,"stalled":3,"denial_streak":3,"denials_window":4,"distinct_denied":3,"alias":2,"files":32,"files_per_root":8,"delete_permille":500,"deleted_files":8,"growth_permille":4000,"changed_kib":64},"digest":[20,87,236,124,192,233,100,133,79,93,97,82,110,48,83,142,171,60,50,80,167,97,239,123,72,148,181,116,48,36,159,94]},"worker_policy":null,"agent_definitions":null,"roster":null,"provider":{"kind":"openai","custom_name":null},"model":"gpt-5.4","effort":null,"custom_profile_provenance":null,"saved_api_account_id":null,"chatgpt_provenance":null,"output_token_bound":"provider_enforced","context_window_tokens":null,"policy":{"mode":"single"},"output_token_cap":4096,"provider_concurrency":1,"workspace_device":1,"workspace_inode":1,"user_instruction_digest":null,"instruction_sources":[{"directory":"","file":null,"digest":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7]}],"launch_directory":"","include_digests":[],"history_run_ids":[],"excluded_history_runs":0,"context_usage":null,"compaction_event_sequence":null,"compaction_content_digest":null,"tool_policy":{"contract_version":1,"config_digest":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"permissions_digest":null,"max_context_bytes":65536,"unguarded_rules_digest":null,"unguarded":"human_only","child_unguarded":"denied"}},"active_deadline_ms":1800000}"#;
const GLOBAL_RUNSTARTED: &str = r#"{"config":{"supervisor":null,"worker_policy":null,"agent_definitions":null,"roster":null,"provider":"openai","model":"gpt-5.4","effort":null,"custom_profile_provenance":null,"saved_api_account_id":null,"chatgpt_provenance":null,"output_token_bound":"provider_enforced","context_window_tokens":null,"policy":{"mode":"single"},"output_token_cap":4096,"provider_concurrency":1,"workspace_device":1,"workspace_inode":1,"user_instruction_digest":null,"instruction_sources":[{"directory":"","file":null,"digest":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7]}],"launch_directory":"","include_digests":[],"history_run_ids":[],"excluded_history_runs":0,"context_usage":null,"compaction_event_sequence":null,"compaction_content_digest":null,"tool_policy":null},"active_deadline_ms":1800000}"#;
const NESTED_RUNSTARTED: &str = r#"{"config":{"supervisor":null,"worker_policy":null,"agent_definitions":null,"roster":null,"provider":"openai","model":"gpt-5.4","effort":null,"custom_profile_provenance":null,"saved_api_account_id":null,"chatgpt_provenance":null,"output_token_bound":"provider_enforced","context_window_tokens":null,"policy":{"mode":"single"},"output_token_cap":4096,"provider_concurrency":1,"workspace_device":1,"workspace_inode":1,"instruction_sources":[{"directory":"","file":null,"digest":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7]}],"launch_directory":"","include_digests":[],"history_run_ids":[],"excluded_history_runs":0,"context_usage":null,"compaction_event_sequence":null,"compaction_content_digest":null,"tool_policy":null},"active_deadline_ms":1800000}"#;

// Synthetic test-only edge: a version-5 SessionStarted stored `name` where version 6 stores `title`.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StartedV5 {
    name: String,
    workspace_device: u64,
    workspace_inode: u64,
}
fn started_v5(value: Value) -> Option<Value> {
    let Value::Object(mut fields) = value else {
        return None;
    };
    let title = fields.remove("name")?;
    fields.insert("title".into(), title);
    (fields.len() == 3).then_some(Value::Object(fields))
}
fn started_v5_canonical(payload: &str) -> Option<String> {
    serde_json::to_string(&serde_json::from_str::<StartedV5>(payload).ok()?).ok()
}
const STARTED_STEP: [EventStep; 1] = [EventStep {
    kind: "SessionStarted",
    from: 5,
    transform: started_v5,
    canonical: started_v5_canonical,
}];
const REFUSING_STEP: [EventStep; 1] = [EventStep {
    kind: "SessionStarted",
    from: 5,
    transform: |_| None,
    canonical: started_v5_canonical,
}];
const STARTED_FIXTURE: EventFixture<'static> = (
    "SessionStarted",
    5,
    r#"{"name":"Session","workspace_device":1,"workspace_inode":2}"#,
    r#"{"title":"Session","workspace_device":1,"workspace_inode":2}"#,
);

fn started_floor() -> Vec<(&'static str, i64)> {
    EVENT_FLOOR
        .iter()
        .map(|&(kind, floor)| (kind, if kind == "SessionStarted" { 5 } else { floor }))
        .collect()
}

fn synthetic<'a>(floor: &'a [(&'a str, i64)], steps: &'a [EventStep]) -> Registry<'a> {
    Registry {
        event_floor: floor,
        event_steps: steps,
        ..REGISTRY
    }
}

fn fail_after(steps: Option<usize>) {
    FAIL_AT.with(|fail| fail.set(steps));
}

fn failed_compaction(run: RunId, covered_sequence: u64, digest: [u8; 32]) -> CompactionRecord {
    CompactionRecord {
        rate_limits: None,
        covered_run_id: run,
        covered_sequence,
        source_digest: digest,
        recent_files: Vec::new(),
        provider: "openai".into(),
        model: "test".into(),
        prompt_version: 1,
        schema_version: 1,
        compiler_version: 3,
        source_bytes: 1,
        source_tokens_estimate: 1,
        input_tokens: None,
        output_tokens: None,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        response_id: None,
        wire_provenance: None,
        chatgpt_provenance: None,
        output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
        status: crate::session::CompactionStatus::Failed {
            reason: crate::session::CompactionFailure::ProviderUnavailable,
        },
    }
}

struct History {
    root: StateRoot,
    sessions: [SessionId; 3],
}

fn forked_history(path: &Path) -> History {
    history(path, false, 0)
}

// A source with a finished Run (optionally Goal-compacted), a rewind and a compaction over it,
// `renames` idle renames, a child fork and a grandchild fork.
fn history(path: &Path, goal_compacted: bool, renames: usize) -> History {
    let (root, connection, source) = history_source(path, goal_compacted, renames);
    fork_history(root, connection, source)
}

fn history_source(
    path: &Path,
    goal_compacted: bool,
    renames: usize,
) -> (StateRoot, Connection, SessionId) {
    let root = StateRoot::admit(path).unwrap();
    let fixture = crate::session::worker_compacted_fixture(goal_compacted);
    let source = fixture.session;
    let mut connection = open_connection(&root, false).unwrap();
    let mut hot = None;
    for event in &fixture.events {
        let mut event = event.clone();
        // The fixture pins its digest over zero timestamps; the journal stamps real ones.
        if let Event::GoalContextCompacted { record, .. } = &mut event {
            let (events, _) = resolve_open_view(&connection, source, &mut 0).unwrap();
            record.source_digest =
                crate::session::prefix_digest(&events, record.covered_sequence).unwrap();
        }
        append(&mut connection, &mut hot, source, event).unwrap();
    }
    let view = |connection: &Connection| resolve_view(connection, source, 0, &mut 0).unwrap();
    let current = view(&connection).1.unwrap();
    let started =
        crate::session::rewind_started_fixture(&current, current.rewinds.checkpoints[0].clone());
    append(&mut connection, &mut hot, source, started).unwrap();
    let finished = crate::session::rewind_finished_fixture(
        &view(&connection).1.unwrap(),
        crate::session::rewind::RewindOutcome::Restored,
    );
    append(&mut connection, &mut hot, source, finished).unwrap();
    let (events, current) = view(&connection);
    let covered = current.unwrap().last_sequence;
    let record = failed_compaction(
        fixture.run,
        covered,
        crate::session::prefix_digest(&events, covered).unwrap(),
    );
    let compacted = Event::ContextCompacted {
        record,
        active_ms: None,
    };
    append(&mut connection, &mut hot, source, compacted).unwrap();
    let rename = Event::SessionRenamed { title: "x".into() };
    let transaction = connection.transaction().unwrap();
    for _ in 0..renames {
        transaction
            .execute(
                "INSERT INTO events (sequence, session_id, kind, event_version, payload, created_at_ms)
                 VALUES ((SELECT MAX(sequence) + 1 FROM events), ?1, ?2, ?3, ?4, 0)",
                rusqlite::params![
                    source.to_string(),
                    rename.kind(),
                    rename.version(),
                    rename.payload().unwrap()
                ],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    (root, connection, source)
}

fn fork_history(root: StateRoot, mut connection: Connection, source: SessionId) -> History {
    let child = fork_session(&mut connection, source, "Child".into()).unwrap();
    let grandchild = fork_session(&mut connection, child, "Grandchild".into()).unwrap();
    History {
        root,
        sessions: [source, child, grandchild],
    }
}

fn rewind_history(path: &Path, renames: usize) -> History {
    let (root, mut connection, source) = history_source(path, false, renames);
    let followup = crate::session::worker_compacted_fixture(false);
    let mut hot = None;
    for event in followup.events.iter().skip(1) {
        append(&mut connection, &mut hot, source, event.clone()).unwrap();
        if matches!(event, Event::AgentSpawned { agent_run_id, .. } if *agent_run_id == followup.primary)
        {
            break;
        }
    }
    for event in [
        Event::AgentFinished {
            run_id: followup.run,
            agent_run_id: followup.primary,
            disposition: crate::session::AgentDisposition::Cancelled,
            summary: None,
            result: None,
            worker: None,
        },
        Event::RunFinished {
            run_id: followup.run,
            disposition: crate::session::RunDisposition::Cancelled,
            failure: None,
            active_ms: 0,
        },
    ] {
        append(&mut connection, &mut hot, source, event).unwrap();
    }
    fork_history(root, connection, source)
}

fn rows(connection: &Connection) -> Vec<RawRow> {
    let mut statement = connection.prepare(ROWS_AFTER_SQL).unwrap();
    let mut rows: Vec<RawRow> = Vec::new();
    loop {
        let after = rows.last().map_or(0, |row| row.sequence);
        let chunk = statement
            .query_map([after], |row| Ok(raw_row(row).unwrap()))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        if chunk.is_empty() {
            return rows;
        }
        rows.extend(chunk);
    }
}

fn stored_prefix(row: &RawRow) -> PrefixRow<'_> {
    PrefixRow {
        sequence: u64::try_from(row.sequence).unwrap(),
        session_id: &row.session_id,
        run_id: row.run_id.as_deref(),
        agent_run_id: row.agent_run_id.as_deref(),
        kind: &row.kind,
        payload: &row.payload,
        created_at_ms: row.created_at_ms,
    }
}

fn decode_row(row: &RawRow) -> Event {
    let run = row.run_id.as_deref().map(|id| id.parse::<RunId>().unwrap());
    let agent_run = row
        .agent_run_id
        .as_deref()
        .map(|id| id.parse::<AgentRunId>().unwrap());
    Event::decode(&row.kind, row.version, &row.payload, run, agent_run).unwrap()
}

// Rewrites SessionStarted to the synthetic version 5 and re-pins every prefix reference over the
// stored bytes. With canonical text this is what a version-5 build wrote; noncanonical text pins
// digests that version-5 replay rejected.
fn downgrade_started(connection: &Connection, canonical: bool) {
    repin(connection, |row| {
        if row.kind == "SessionStarted" {
            let current: Value = serde_json::from_str(&row.payload).unwrap();
            let (name, device, inode) = (
                &current["title"],
                &current["workspace_device"],
                &current["workspace_inode"],
            );
            row.payload = if canonical {
                format!(
                    r#"{{"name":{name},"workspace_device":{device},"workspace_inode":{inode}}}"#
                )
            } else {
                format!(
                    r#"{{"workspace_inode":{inode},"name":{name},"workspace_device":{device}}}"#
                )
            };
            row.version = 5;
        }
    });
}

fn downgrade_run_provider(connection: &Connection) {
    repin(connection, |row| {
        if let Event::RunStarted { config, .. } = decode_row(row) {
            let tagged = serde_json::to_string(&config.provider).unwrap();
            let profile = serde_json::to_string(&config.provider.profile()).unwrap();
            row.payload = row.payload.replacen(&tagged, &profile, 1);
            row.version = 23;
        }
    });
}

fn downgrade_run_started(connection: &Connection) {
    repin(connection, |row| {
        if row.kind == "RunStarted" {
            let event = decode_row(row);
            let Event::RunStarted { config, .. } = &event else {
                unreachable!()
            };
            let tagged = serde_json::to_string(&config.provider).unwrap();
            let profile = serde_json::to_string(&config.provider.profile()).unwrap();
            row.payload = row.payload.replacen(&tagged, &profile, 1).replacen(
                r#""user_instruction_digest":null,"#,
                "",
                1,
            );
            row.version = 19;
        }
    });
}

fn repin(connection: &Connection, rewrite: impl Fn(&mut RawRow)) {
    let mut lanes = HashMap::<String, PrefixHasher>::new();
    let mut points = HashMap::<(String, u64), [u8; 32]>::new();
    for mut row in rows(connection) {
        rewrite(&mut row);
        let lane = lanes.entry(row.session_id.clone()).or_default();
        if matches!(
            row.kind.as_str(),
            "SessionForked"
                | "ContextCompacted"
                | "GoalContextCompacted"
                | "WorkspaceRewindStarted"
        ) {
            let mut event = decode_row(&row);
            match &mut event {
                Event::SessionForked {
                    source_session_id,
                    source_sequence,
                    prefix_digest,
                    ..
                } => *prefix_digest = points[&(source_session_id.to_string(), *source_sequence)],
                Event::ContextCompacted { record, .. }
                | Event::GoalContextCompacted { record, .. } => {
                    record.source_digest =
                        points[&(row.session_id.clone(), record.covered_sequence)];
                }
                Event::WorkspaceRewindStarted { record } => record.prefix_digest = lane.digest(),
                _ => unreachable!(),
            }
            row.payload = event.payload().unwrap();
        }
        connection
            .execute(
                "UPDATE events SET event_version=?1, payload=?2 WHERE sequence=?3",
                rusqlite::params![row.version, row.payload, row.sequence],
            )
            .unwrap();
        lane.update_row(&stored_prefix(&row)).unwrap();
        points.insert(
            (row.session_id.clone(), u64::try_from(row.sequence).unwrap()),
            lane.digest(),
        );
    }
}

fn downgrade_rewind_started(connection: &Connection) {
    let mut lanes = HashMap::<String, PrefixHasher>::new();
    let mut points = HashMap::<(String, u64), [u8; 32]>::new();
    for mut row in rows(connection) {
        let lane = lanes.entry(row.session_id.clone()).or_default();
        let mut event = decode_row(&row);
        match &mut event {
            Event::SessionForked {
                source_session_id,
                source_sequence,
                prefix_digest,
                ..
            } => *prefix_digest = points[&(source_session_id.to_string(), *source_sequence)],
            Event::ContextCompacted { record, .. } | Event::GoalContextCompacted { record, .. } => {
                record.source_digest = points[&(row.session_id.clone(), record.covered_sequence)];
            }
            Event::WorkspaceRewindStarted { record } => record.prefix_digest = lane.digest(),
            _ => {}
        }
        row.payload = event.payload().unwrap();
        if row.kind == "WorkspaceRewindStarted" {
            row.version = 1;
            row.payload = rewind_v1_payload(&row.payload);
        }
        connection
            .execute(
                "UPDATE events SET event_version=?1, payload=?2 WHERE sequence=?3",
                rusqlite::params![row.version, row.payload, row.sequence],
            )
            .unwrap();
        lane.update_row(&stored_prefix(&row)).unwrap();
        points.insert(
            (row.session_id.clone(), u64::try_from(row.sequence).unwrap()),
            lane.digest(),
        );
    }
}

fn rewind_v1_payload(current: &str) -> String {
    format!(
        "{},\"expires_at_ms\":60000}}",
        current.trim_end_matches('}')
    )
}

fn forge_rewind_v1_digest(connection: &Connection) {
    let row = first_of(connection, "WorkspaceRewindStarted");
    assert_eq!(row.version, 1);
    let mut value: Value = serde_json::from_str(&row.payload).unwrap();
    value["prefix_digest"][0] = serde_json::json!(value["prefix_digest"][0].as_u64().unwrap() ^ 1);
    let current = crate::session::rewind_started_v1_to_v2(value).unwrap();
    let event = Event::decode(
        "WorkspaceRewindStarted",
        2,
        &current.to_string(),
        None,
        None,
    )
    .unwrap();
    let payload = rewind_v1_payload(&event.payload().unwrap());
    connection
        .execute(
            "UPDATE events SET payload=?1 WHERE sequence=?2",
            rusqlite::params![payload, row.sequence],
        )
        .unwrap();
}

fn first_of(connection: &Connection, kind: &str) -> RawRow {
    rows(connection)
        .into_iter()
        .find(|row| row.kind == kind)
        .unwrap()
}

fn forge(connection: &Connection, kind: &str) {
    let row = first_of(connection, kind);
    let mut event = decode_row(&row);
    match &mut event {
        Event::SessionForked { prefix_digest, .. } => prefix_digest[0] ^= 1,
        Event::ContextCompacted { record, .. } | Event::GoalContextCompacted { record, .. } => {
            record.source_digest[0] ^= 1;
        }
        Event::WorkspaceRewindStarted { record } => record.prefix_digest[0] ^= 1,
        _ => unreachable!(),
    }
    connection
        .execute(
            "UPDATE events SET payload=?1 WHERE sequence=?2",
            rusqlite::params![event.payload().unwrap(), row.sequence],
        )
        .unwrap();
}

fn stored_rows(connection: &Connection) -> Vec<(i64, String, i64, String)> {
    rows(connection)
        .into_iter()
        .map(|row| (row.sequence, row.kind, row.version, row.payload))
        .collect()
}

fn fork_digests(connection: &Connection) -> Vec<String> {
    rows(connection)
        .into_iter()
        .filter(|row| row.kind == "SessionForked")
        .map(|row| {
            serde_json::from_str::<Value>(&row.payload).unwrap()["prefix_digest"].to_string()
        })
        .collect()
}

fn entries(path: &Path) -> Vec<String> {
    let mut names = match std::fs::read_dir(path) {
        Ok(entries) => entries
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

fn user_version(connection: &Connection) -> i64 {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

fn facts(path: &Path) -> Vec<(String, i64, i64, i64, String)> {
    let connection = Connection::open(path.join(DATABASE_FILE)).unwrap();
    connection
        .prepare("SELECT id, from_schema, to_schema, rewritten_events, backup FROM migrations")
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn replays(root: &StateRoot, sessions: &[SessionId]) {
    let connection = open_connection(root, false).unwrap();
    for session in sessions {
        let (_, view) = resolve_view(&connection, *session, 0, &mut 0).unwrap();
        assert!(view.is_some());
    }
}

#[test]
fn a_migratable_history_upgrades_in_a_validated_copy_with_recomputed_prefix_digests() {
    // The Goal compaction adds one more re-pinned digest.
    for (goal_compacted, rewritten) in [(false, 5), (true, 6)] {
        upgrades_with_recomputed_digests(goal_compacted, rewritten);
    }
}

fn upgrades_with_recomputed_digests(goal_compacted: bool, rewritten: i64) {
    let temp = tempfile::tempdir().unwrap();
    let built = temp.path().join("built");
    let path = temp.path().join("state");
    let History { sessions, .. } = history(&built, goal_compacted, 0);
    let writer = Connection::open(built.join(DATABASE_FILE)).unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    downgrade_started(&writer, true);
    let original = stored_rows(&writer);
    let old_digests = fork_digests(&writer);
    let root = StateRoot::admit(&path).unwrap();
    // A crash leaves the downgrade only in the uncheckpointed WAL.
    for name in [DATABASE_FILE, "events.sqlite3-wal"] {
        std::fs::copy(built.join(name), path.join(name)).unwrap();
        std::fs::set_permissions(path.join(name), std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    drop(writer);
    let floor = started_floor();

    assert!(
        root.upgrade_or_archive_history(&synthetic(&floor, &STARTED_STEP))
            .unwrap()
            .is_none(),
        "goal compacted: {goal_compacted}"
    );

    assert!(!path.join("archive").exists());
    for sidecar in &HISTORY_FILES[..3] {
        assert!(!path.join(sidecar).exists(), "{sidecar}");
    }
    let migrations = path.join(DIR);
    let names = entries(&migrations);
    assert_eq!(names.len(), 1, "only the backup remains: {names:?}");
    let backup = migrations.join(&names[0]);
    for (entry, mode) in [(&migrations, 0o700), (&backup, 0o600)] {
        assert_eq!(
            std::fs::metadata(entry).unwrap().permissions().mode() & 0o777,
            mode
        );
    }
    let backup = Connection::open_with_flags(&backup, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(
        stored_rows(&backup),
        original,
        "the backup holds every committed row"
    );
    drop(backup);
    let [fact] = facts(&path).try_into().unwrap();
    assert!(
        names[0].ends_with(&format!("-{}.sqlite3", fact.0)),
        "{names:?}"
    );
    assert_eq!(
        (fact.1, fact.2, fact.3, fact.4),
        (SCHEMA_VERSION, SCHEMA_VERSION, rewritten, names[0].clone()),
        "SessionStarted, every compaction, the rewind and two forks are rewritten"
    );
    replays(&root, &sessions);
    let upgraded = Connection::open(path.join(DATABASE_FILE)).unwrap();
    let new_digests = fork_digests(&upgraded);
    assert_eq!(new_digests.len(), 2);
    assert!(
        new_digests
            .iter()
            .zip(&old_digests)
            .all(|(new, old)| new != old),
        "every fork digest covers the rewritten prefix"
    );
    let started = first_of(&upgraded, "SessionStarted");
    assert_eq!(
        started.version,
        crate::test_support::payload_version("SessionStarted")
    );
    assert!(
        started.payload.starts_with(r#"{"title":"#),
        "{}",
        started.payload
    );
}

#[test]
fn run_started_migrations_preserve_identity_and_do_not_read_user_instructions() {
    for version in [19, 23] {
        migrate_run_started(version);
    }
}

fn migrate_run_started(version: i64) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let History { root, sessions } = history(&path, true, 0);
    std::fs::write(path.join("AGENTS.md"), "canary").unwrap();
    std::fs::set_permissions(
        path.join("AGENTS.md"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let connection = Connection::open(path.join(DATABASE_FILE)).unwrap();
    let current = stored_rows(&connection);
    let digests = fork_digests(&connection);
    if version == 19 {
        downgrade_run_started(&connection);
    } else {
        downgrade_run_provider(&connection);
    }
    let downgraded = stored_rows(&connection);
    assert_ne!(downgraded, current);
    assert_ne!(fork_digests(&connection), digests);
    drop(connection);

    assert!(
        root.upgrade_or_archive_history(&REGISTRY)
            .unwrap()
            .is_none()
    );

    assert!(!path.join("archive").exists());
    replays(&root, &sessions);
    let upgraded = Connection::open(path.join(DATABASE_FILE)).unwrap();
    assert_eq!(stored_rows(&upgraded), current);
    assert_eq!(fork_digests(&upgraded), digests);
    let started = first_of(&upgraded, "RunStarted");
    assert_eq!(
        started.version,
        crate::test_support::payload_version("RunStarted")
    );
    assert!(
        started
            .payload
            .contains(r#""user_instruction_digest":null"#)
    );
    assert!(!started.payload.contains("canary"));
}

#[test]
fn rewind_v1_file_migrates_and_cold_replays_all_bound_prefixes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let History { root, sessions } = rewind_history(&path, 0);
    let connection = Connection::open(path.join(DATABASE_FILE)).unwrap();
    let current_fork_digests = fork_digests(&connection);
    downgrade_rewind_started(&connection);
    let legacy_fork_digests = fork_digests(&connection);
    let source_rows = stored_rows(&connection);
    drop(connection);

    assert!(
        root.upgrade_or_archive_history(&REGISTRY)
            .unwrap()
            .is_none()
    );
    replays(&root, &sessions);

    let upgraded =
        Connection::open_with_flags(path.join(DATABASE_FILE), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let started = first_of(&upgraded, "WorkspaceRewindStarted");
    assert_eq!(started.version, 2);
    assert!(!started.payload.contains("expires_at_ms"));
    assert!(source_rows.iter().any(|row| {
        row.1 == "WorkspaceRewindStarted" && row.2 == 1 && row.3.contains("expires_at_ms")
    }));
    assert_eq!(
        Event::decode(&started.kind, started.version, &started.payload, None, None)
            .unwrap()
            .kind(),
        "WorkspaceRewindStarted"
    );
    let finished: Value =
        serde_json::from_str(&first_of(&upgraded, "WorkspaceRewindFinished").payload).unwrap();
    assert_eq!(finished["outcome"], "restored");
    assert_eq!(fork_digests(&upgraded), current_fork_digests);
    assert!(
        legacy_fork_digests
            .iter()
            .zip(&current_fork_digests)
            .all(|(legacy, current)| legacy != current)
    );
    drop(upgraded);
    let cold = open_connection(&root, true).unwrap();
    for session in sessions {
        let (_, view) = resolve_view(&cold, session, 0, &mut 0).unwrap();
        let view = view.unwrap();
        assert_eq!(
            view.rewinds.operations[0].outcome(),
            crate::session::rewind::RewindOutcome::Restored
        );
        assert_eq!(view.runs.len(), 2);
        assert_eq!(view.runs[1].status, crate::session::RunStatus::Cancelled);
    }
    drop(cold);
}

#[test]
fn rewind_v1_forged_prefix_archives_history_intact() {
    let temp = tempfile::tempdir().unwrap();
    let forged_path = temp.path().join("forged");
    let History { root: forged, .. } = rewind_history(&forged_path, 0);
    let connection = Connection::open(forged_path.join(DATABASE_FILE)).unwrap();
    downgrade_rewind_started(&connection);
    forge_rewind_v1_digest(&connection);
    drop(connection);
    let original = std::fs::read(forged_path.join(DATABASE_FILE)).unwrap();
    let archived = forged
        .upgrade_or_archive_history(&REGISTRY)
        .unwrap()
        .expect("forged historical prefix must archive");
    assert_eq!(
        std::fs::read(archived.path().join(DATABASE_FILE)).unwrap(),
        original
    );
    archived.acknowledge().unwrap();
}

#[test]
fn every_interrupted_migration_step_ends_original_or_upgraded_and_recovers() {
    // One fixed clock makes a retry reuse the same timestamp; the migration ID keeps names apart.
    CLOCK.with(|clock| clock.set(Some(SystemTime::UNIX_EPOCH)));
    let mut interrupted = 0;
    let mut cleaning_checked = false;
    loop {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let History { root, sessions } = forked_history(&path);
        let connection = Connection::open(path.join(DATABASE_FILE)).unwrap();
        downgrade_run_provider(&connection);
        restore_pre_check_schema(&connection, 4);
        drop(connection);
        let database = path.join(DATABASE_FILE);
        let original = std::fs::read(&database).unwrap();
        let original_rows = stored_rows(&Connection::open(&database).unwrap());
        seed_older_backup(&path);

        fail_after(Some(interrupted));
        let first = root.archive_incompatible_history();
        fail_after(None);
        match first {
            Ok(None) => break,
            Err(StoreError::Io(_)) => {}
            other => panic!("step {interrupted}: {other:?}"),
        }
        let current = std::fs::read(&database).unwrap();
        if current != original {
            assert_eq!(facts(&path).len(), 1, "step {interrupted}");
        }
        if let Some((_, marker)) = read_marker(&root).unwrap() {
            // Every changed pinned artifact fails closed before any cleanup.
            let staged = path.join(DIR).join(STAGED);
            let changed = match marker.phase {
                Phase::Preparing => vec![database.clone()],
                // After an upgrade the retained backup stays pinned; a rollback keeps nothing.
                Phase::Cleaning { upgraded, .. } => {
                    if upgraded {
                        cleaning_checked = true;
                        vec![path.join(DIR).join(&marker.backup)]
                    } else {
                        Vec::new()
                    }
                }
                Phase::Ready { .. } => {
                    let mut changed = vec![database.clone(), path.join(DIR).join(&marker.backup)];
                    changed.extend(staged.exists().then_some(staged));
                    changed
                }
            };
            for file in changed {
                let before = entries(&path.join(DIR));
                let saved = std::fs::read(&file).unwrap();
                std::fs::write(&file, b"changed").unwrap();
                assert!(
                    matches!(
                        root.archive_incompatible_history(),
                        Err(StoreError::MigrationConflict)
                    ),
                    "step {interrupted}: {file:?}"
                );
                assert_eq!(entries(&path.join(DIR)), before, "step {interrupted}");
                assert!(!path.join("archive").exists(), "step {interrupted}");
                std::fs::write(&file, saved).unwrap();
            }
            assert_eq!(
                std::fs::read(&database).unwrap(),
                current,
                "step {interrupted}"
            );
            if current != original {
                // A writable open recovers the pending upgrade under its own lease.
                let store = crate::Store::open(root.try_clone().unwrap()).unwrap();
                tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap()
                    .block_on(store.close())
                    .unwrap();
                assert!(!pending(&root).unwrap(), "step {interrupted}");
            }
        }

        // Recovery itself is interrupted at every step and restarts until it completes.
        let mut recovery = 0;
        loop {
            fail_after(Some(recovery));
            let resumed = root.archive_incompatible_history();
            fail_after(None);
            match resumed {
                Ok(None) => break,
                Err(StoreError::Io(_)) => assert!(
                    std::fs::read(&database).unwrap() == original || facts(&path).len() == 1,
                    "step {interrupted}.{recovery}"
                ),
                other => panic!("step {interrupted}.{recovery}: {other:?}"),
            }
            recovery += 1;
            assert!(
                recovery < 64,
                "step {interrupted}: recovery never completes"
            );
        }
        let [fact] = facts(&path).try_into().unwrap();
        assert_eq!((fact.1, fact.2), (4, SCHEMA_VERSION), "step {interrupted}");
        replays(&root, &sessions);
        let [backup] = entries(&path.join(DIR)).try_into().unwrap();
        assert_eq!(backup, fact.4, "step {interrupted}: only the latest backup");
        assert!(
            backup.starts_with("history-19700101T000000.000Z-"),
            "{backup}"
        );
        let backup = Connection::open(path.join(DIR).join(backup)).unwrap();
        assert_eq!(user_version(&backup), 4, "step {interrupted}");
        assert_eq!(stored_rows(&backup), original_rows, "step {interrupted}");
        interrupted += 1;
    }
    CLOCK.with(|clock| clock.set(None));
    assert!(interrupted >= 16, "every durability step was interrupted");
    assert!(
        cleaning_checked,
        "a backup changed after Cleaning was published fails closed"
    );
}

#[test]
fn a_later_migration_keeps_only_its_own_backup() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let History { root, sessions } = forked_history(&path);
    let database = Connection::open(path.join(DATABASE_FILE)).unwrap();
    restore_pre_check_schema(&database, 4);
    drop(database);
    assert!(root.archive_incompatible_history().unwrap().is_none());
    let [first] = entries(&path.join(DIR)).try_into().unwrap();
    downgrade_started(&Connection::open(path.join(DATABASE_FILE)).unwrap(), true);
    let floor = started_floor();

    assert!(
        root.upgrade_or_archive_history(&synthetic(&floor, &STARTED_STEP))
            .unwrap()
            .is_none()
    );

    let [backup] = entries(&path.join(DIR)).try_into().unwrap();
    assert_ne!(backup, first, "the older backup is replaced");
    replays(&root, &sessions);
}

fn seed_older_backup(path: &Path) {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(path.join(DIR))
        .unwrap();
    let older = path.join(DIR).join(format!(
        "history-19691231T000000.000Z-{}.sqlite3",
        uuid::Uuid::now_v7()
    ));
    std::fs::write(&older, b"older backup").unwrap();
    std::fs::set_permissions(&older, std::fs::Permissions::from_mode(0o600)).unwrap();
}

type Gate = (
    std::path::PathBuf,
    std::sync::mpsc::SyncSender<()>,
    std::sync::mpsc::Receiver<()>,
);
static LEASE_GATE: std::sync::Mutex<Option<Gate>> = std::sync::Mutex::new(None);

// Parks a gated Store open before it takes its history lease.
pub(super) fn before_lease(root: &StateRoot) {
    let gate = {
        let mut gate = LEASE_GATE.lock().unwrap();
        if gate.as_ref().is_some_and(|gate| gate.0 == root.path()) {
            gate.take()
        } else {
            None
        }
    };
    if let Some((_, entered, release)) = gate {
        entered.send(()).unwrap();
        release.recv().unwrap();
    }
}

#[test]
fn a_writable_open_racing_an_interrupted_migration_recovers_before_admission() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let History { root, sessions } = forked_history(&path);
    let database = path.join(DATABASE_FILE);
    restore_pre_check_schema(&Connection::open(&database).unwrap(), 4);
    let original = std::fs::read(&database).unwrap();
    let (entered_sender, entered) = std::sync::mpsc::sync_channel(0);
    let (release, release_receiver) = std::sync::mpsc::sync_channel(0);
    *LEASE_GATE.lock().unwrap() =
        Some((root.path().to_path_buf(), entered_sender, release_receiver));
    let opener = {
        let root = root.try_clone().unwrap();
        std::thread::spawn(move || crate::Store::open(root))
    };
    entered.recv().unwrap();

    // While the opener waits for its lease, another startup swaps in the upgrade and crashes.
    fail_after(Some(12));
    assert!(matches!(
        root.archive_incompatible_history(),
        Err(StoreError::Io(_))
    ));
    fail_after(None);
    assert!(std::fs::read(&database).unwrap() != original && pending(&root).unwrap());
    release.send(()).unwrap();
    let store = opener.join().unwrap().unwrap();

    assert!(
        !pending(&root).unwrap(),
        "recovery precedes writable admission"
    );
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(store.close())
        .unwrap();
    let [_fact] = facts(&path).try_into().unwrap();
    replays(&root, &sessions);
}

// A trigger that restores the honest fork row once row 1 is rewritten, after the first batch was read.
fn trigger_repairs_a_later_forgery(connection: &Connection) {
    downgrade_started(connection, true);
    let honest = first_of(connection, "SessionForked");
    assert!(honest.sequence > 256, "the forged row is in a later batch");
    forge(connection, "SessionForked");
    connection
        .execute_batch(&format!(
            "CREATE TRIGGER repair AFTER UPDATE ON events WHEN new.sequence = 1 BEGIN
             UPDATE events SET payload = '{}' WHERE sequence = {}; END;",
            honest.payload.replace('\'', "''"),
            honest.sequence
        ))
        .unwrap();
}

fn unsupported_tool_row(connection: &Connection, kind: &str, version: i64, payload: &str) {
    let run = first_of(connection, "RunStarted");
    connection.execute(
        "INSERT INTO events(session_id, run_id, agent_run_id, kind, event_version, payload, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
        rusqlite::params![run.session_id, run.run_id, run.agent_run_id, kind, version, payload],
    ).unwrap();
}

#[test]
fn unsupported_refused_or_forged_history_is_archived_intact() {
    let floor = started_floor();
    let (refusing, upgrading) = (
        synthetic(&floor, &REFUSING_STEP),
        synthetic(&floor, &STARTED_STEP),
    );
    type Case<'a> = (&'a str, &'a Registry<'a>, bool, usize, fn(&Connection));
    let cases: [Case<'_>; 11] = [
        ("ToolStarted v14", &REGISTRY, false, 0, |connection| {
            unsupported_tool_row(
                connection,
                "ToolStarted",
                14,
                include_str!("toolstarted-v14-archived.json"),
            );
        }),
        (
            "ToolStarted below floor",
            &REGISTRY,
            false,
            0,
            |connection| {
                unsupported_tool_row(
                    connection,
                    "ToolStarted",
                    REGISTRY.event_floor("ToolStarted").unwrap() - 1,
                    include_str!("toolstarted-without-expiry.json"),
                );
            },
        ),
        (
            "removed permission kind",
            &REGISTRY,
            false,
            0,
            |connection| {
                unsupported_tool_row(connection, "ToolPermissionDecided", 7, "{}");
            },
        ),
        (
            "schema below the floor",
            &REGISTRY,
            false,
            0,
            |connection| {
                connection
                    .execute_batch("DROP TABLE migrations; PRAGMA user_version=3;")
                    .unwrap();
            },
        ),
        ("refused step", &refusing, false, 0, |connection| {
            downgrade_started(connection, true);
        }),
        ("noncanonical pins", &upgrading, false, 0, |connection| {
            downgrade_started(connection, false);
        }),
        ("forged fork", &upgrading, false, 0, |connection| {
            downgrade_started(connection, true);
            forge(connection, "SessionForked");
        }),
        ("forged rewind", &upgrading, false, 0, |connection| {
            downgrade_started(connection, true);
            forge(connection, "WorkspaceRewindStarted");
        }),
        ("forged compaction", &upgrading, false, 0, |connection| {
            downgrade_started(connection, true);
            forge(connection, "ContextCompacted");
        }),
        (
            "forged Goal compaction",
            &upgrading,
            true,
            0,
            |connection| {
                downgrade_started(connection, true);
                forge(connection, "GoalContextCompacted");
            },
        ),
        (
            "trigger beyond the first batch",
            &upgrading,
            false,
            300,
            trigger_repairs_a_later_forgery,
        ),
    ];
    for (name, registry, goal_compacted, renames, prepare) in cases {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let History { root, .. } = history(&path, goal_compacted, renames);
        prepare(&Connection::open(path.join(DATABASE_FILE)).unwrap());
        let original = std::fs::read(path.join(DATABASE_FILE)).unwrap();

        let archived = root.upgrade_or_archive_history(registry).unwrap().unwrap();

        assert_eq!(
            std::fs::read(archived.path().join(DATABASE_FILE)).unwrap(),
            original,
            "{name}"
        );
        assert!(entries(&path.join(DIR)).is_empty(), "{name}");
        archived.acknowledge().unwrap();
        let fresh = open_connection(&root, false).unwrap();
        assert_eq!(stored_rows(&fresh), Vec::new(), "{name}");
    }
}

#[test]
fn replay_validation_bounds_aggregate_work_but_not_each_lineage() {
    let floor = started_floor();
    let registry = synthetic(&floor, &STARTED_STEP);
    for (limit, migrated) in [(MAX_VALIDATION_EVENTS, true), (MAX_SESSION_EVENTS, false)] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let History { root, sessions } = rewind_history(&path, 3_400);
        let connection = open_connection(&root, false).unwrap();
        let mut aggregate = 0;
        for session in sessions {
            let mut lineage = 0;
            resolve_view(&connection, session, 0, &mut lineage).unwrap();
            assert!(lineage <= MAX_SESSION_EVENTS);
            aggregate += lineage;
        }
        assert!(aggregate > MAX_SESSION_EVENTS);
        drop(connection);
        downgrade_started(&Connection::open(path.join(DATABASE_FILE)).unwrap(), true);
        let original = std::fs::read(path.join(DATABASE_FILE)).unwrap();
        VALIDATION_LIMIT.with(|bound| bound.set(limit));
        let result = root.upgrade_or_archive_history(&registry).unwrap();
        VALIDATION_LIMIT.with(|bound| bound.set(MAX_VALIDATION_EVENTS));
        if migrated {
            assert!(result.is_none());
            replays(&root, &sessions);
        } else {
            let archived = result.unwrap();
            assert_eq!(
                std::fs::read(archived.path().join(DATABASE_FILE)).unwrap(),
                original
            );
        }
    }
}

#[test]
fn unsupported_history_in_an_uncheckpointed_wal_is_archived_byte_for_byte() {
    for file in [DATABASE_FILE, checkpoints::FILES[0]] {
        let temp = tempfile::tempdir().unwrap();
        let built = temp.path().join("built");
        let path = temp.path().join("state");
        let History { root: source, .. } = forked_history(&built);
        checkpoint_fixture(&source);
        let writer = Connection::open(built.join(file)).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        if file == DATABASE_FILE {
            writer
                .execute_batch("UPDATE events SET event_version = 4 WHERE kind = 'SessionStarted'")
                .unwrap();
        } else {
            writer.pragma_update(None, "user_version", 0).unwrap();
        }
        let History { root, .. } = forked_history(&path);
        let mut copied = Vec::new();
        for name in [file.to_owned(), format!("{file}-wal")] {
            std::fs::copy(built.join(&name), path.join(&name)).unwrap();
            std::fs::set_permissions(path.join(&name), std::fs::Permissions::from_mode(0o600))
                .unwrap();
            copied.push((name.clone(), std::fs::read(path.join(&name)).unwrap()));
        }
        drop(writer);

        let archived = root.archive_incompatible_history().unwrap().unwrap();

        for (name, bytes) in copied {
            assert_eq!(
                std::fs::read(archived.path().join(&name)).unwrap(),
                bytes,
                "{name} is archived unchanged"
            );
        }
        assert!(!path.join(DIR).exists());
    }
}

// The guard: every version above its declared floor needs a registered step and fixture per edge.
fn uncovered(
    registry: &Registry<'_>,
    events: &[(&str, i64)],
    schema: i64,
    event_fixtures: &[EventFixture<'_>],
) -> Vec<String> {
    let mut problems = Vec::new();
    for &(kind, current) in events {
        let Some(floor) = registry.event_floor(kind) else {
            problems.push(format!("{kind} declares no migration floor"));
            continue;
        };
        let mut chained = None;
        for from in floor..current {
            let step = registry.event_step(kind, from);
            let fixture = event_fixtures
                .iter()
                .find(|fixture| fixture.0 == kind && fixture.1 == from);
            let (Some(step), Some(&(_, _, input, expected))) = (step, fixture) else {
                problems.push(format!(
                    "{kind} {from} -> {} has no step and fixture",
                    from + 1
                ));
                continue;
            };
            if (step.canonical)(input).as_deref() != Some(input) {
                problems.push(format!("{kind} {from} fixture is not canonical"));
            }
            let input: Value = serde_json::from_str(input).unwrap();
            let expected: Value = serde_json::from_str(expected).unwrap();
            if (step.transform)(input.clone()).as_ref() != Some(&expected) {
                problems.push(format!("{kind} {from} -> {} fixture mismatch", from + 1));
            }
            chained = Some(chained.unwrap_or(Some(input)).and_then(step.transform));
        }
        if let Some(value) = chained {
            let decodes = value.is_some_and(|value| {
                // A payload that pins its own actor (a Tool intent) decodes only under that scope.
                let pinned = intent_scope(&value);
                let scopes = [
                    (None, None),
                    (Some(RunId::new()), None),
                    (Some(RunId::new()), Some(AgentRunId::new())),
                    (pinned.map(|(run, _)| run), pinned.map(|(_, agent)| agent)),
                ];
                scopes.into_iter().any(|(run, agent)| {
                    Event::decode(kind, current, &value.to_string(), run, agent).is_ok()
                })
            });
            if !decodes {
                problems.push(format!("{kind} {floor} -> {current} does not decode"));
            }
        }
    }
    problems.extend(uncovered_schema(&registry.schema, schema));
    problems
}

fn uncovered_schema(format: &SchemaFormat, current: i64) -> Vec<String> {
    let mut problems = Vec::new();
    for from in format.floor..current {
        let (Some(step), Some(target)) = (format.step(from), format.expected_schema(from + 1))
        else {
            problems.push(format!(
                "schema {from} -> {} has no step and fixture",
                from + 1
            ));
            continue;
        };
        if schema_of(&format!("{}{}", step.source, step.sql)) != schema_of(target) {
            problems.push(format!("schema {from} -> {} fixture mismatch", from + 1));
        }
    }
    problems
}

fn intent_scope(value: &Value) -> Option<(RunId, AgentRunId)> {
    let intent = ["intent", "observation"]
        .into_iter()
        .find_map(|key| value.get(key))
        .map(|field| field.get("intent").unwrap_or(field))?;
    Some((
        serde_json::from_value(intent.get("run_id")?.clone()).ok()?,
        serde_json::from_value(intent.get("agent_run_id")?.clone()).ok()?,
    ))
}

fn schema_of(sql: &str) -> Vec<[String; 4]> {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch(sql).unwrap();
    schema_objects(&connection).unwrap()
}

fn assert_decodes_after_migration(kind: &str, from: i64, value: &Value) {
    let payload = value.to_string();
    let payload = REGISTRY
        .upgrade_payload(kind, from, &payload)
        .unwrap()
        .unwrap_or(payload);
    let (run, agent) = intent_scope(value).unwrap();
    Event::decode(
        kind,
        payload_version(kind).unwrap(),
        &payload,
        Some(run),
        Some(agent),
    )
    .unwrap();
}

#[test]
fn command_selection_migrates_committed_outputs_and_rejects_forged_old_counts() {
    for (kind, version, expiring, input, expected) in [
        (
            "ToolFinished",
            25,
            COMMAND_TOOLFINISHED,
            COMMAND_TOOLFINISHED_WITHOUT_EXPIRY,
            SELECTION_TOOLFINISHED,
        ),
        (
            "HookFinished",
            14,
            GLOB_HOOKFINISHED,
            include_str!("hookfinished-without-expiry.json"),
            SELECTION_HOOKFINISHED,
        ),
    ] {
        let step = REGISTRY.event_step(kind, version).unwrap();
        assert_eq!((step.canonical)(input).as_deref(), Some(input));
        let previous = REGISTRY.event_step(kind, version - 1).unwrap();
        assert_eq!((previous.canonical)(expiring).as_deref(), Some(expiring));
        let without_expiry = (previous.transform)(serde_json::from_str(expiring).unwrap()).unwrap();
        assert_eq!(
            without_expiry,
            serde_json::from_str::<Value>(input).unwrap()
        );
        let migrated = (step.transform)(without_expiry).unwrap();
        assert_eq!(migrated, serde_json::from_str::<Value>(expected).unwrap());
        assert_decodes_after_migration(kind, version + 1, &migrated);
        let observation = if kind == "HookFinished" {
            &migrated["observation"]
        } else {
            &migrated
        };
        let output: crate::tools::types::CommandResult =
            serde_json::from_str(observation["output"].as_str().unwrap()).unwrap();
        assert_eq!(output.selection, None);
        for selection in [
            Value::Null,
            serde_json::to_value(crate::tools::types::CommandSelectionSummary::default()).unwrap(),
        ] {
            let mut forged = migrated.clone();
            let observation = if kind == "HookFinished" {
                &mut forged["observation"]
            } else {
                &mut forged
            };
            let mut result: Value =
                serde_json::from_str(observation["output"].as_str().unwrap()).unwrap();
            result["selection"] = selection;
            observation["output"] = result.to_string().into();
            assert!((step.canonical)(&forged.to_string()).is_none());
            assert!((step.transform)(forged).is_none());
        }
    }
}

#[test]
fn command_selection_migration_refits_full_and_truncated_results_at_the_old_ceiling() {
    for (kind, version, input) in [
        ("ToolFinished", 25, COMMAND_TOOLFINISHED_WITHOUT_EXPIRY),
        (
            "HookFinished",
            14,
            include_str!("hookfinished-without-expiry.json"),
        ),
    ] {
        for shape in ["full", "balanced", "asymmetric"] {
            let mut value: Value = serde_json::from_str(input).unwrap();
            let observation = if kind == "HookFinished" {
                &mut value["observation"]
            } else {
                &mut value
            };
            let mut result: Value =
                serde_json::from_str(observation["output"].as_str().unwrap()).unwrap();
            let budget = observation["intent"]["limits"]["result_bytes"]
                .as_u64()
                .unwrap() as usize;
            let mut low = 0;
            let mut high = budget;
            let mut old = String::new();
            while low <= high {
                let count = low + (high - low) / 2;
                let (stdout, total) = if shape != "full" {
                    let head = "prefix".repeat(count);
                    let tail = "suffix".repeat(if shape == "asymmetric" { 1 } else { count });
                    let total = crate::tools::OutputTotal {
                        bytes: (head.len() + tail.len() + 1024) as u64,
                        lines: 1,
                    };
                    (
                        format!(
                            "{head}\n…[1024 bytes, 0 lines omitted; {} bytes, 1 lines total]…\n{tail}",
                            total.bytes
                        ),
                        total,
                    )
                } else {
                    let text = "line\"\n".repeat(count);
                    let total = crate::tools::OutputTotal {
                        bytes: text.len() as u64,
                        lines: text.lines().count() as u64,
                    };
                    (text, total)
                };
                assert!(total.valid_for(&stdout));
                result["stdout"] = stdout.into();
                result["stdout_total"] = serde_json::to_value(total).unwrap();
                let text = result.to_string();
                if text.len() <= budget {
                    old = text;
                    low = count + 1;
                } else {
                    high = count - 1;
                }
            }
            assert!(old.len() > budget - 16);
            let old_result: Value = serde_json::from_str(&old).unwrap();
            observation["output"] = old.into();
            let step = REGISTRY.event_step(kind, version).unwrap();
            let mut oversized = value.clone();
            let observation = if kind == "HookFinished" {
                &mut oversized["observation"]
            } else {
                &mut oversized
            };
            let mut output = old_result.clone();
            output["stdout"] = "x".repeat(budget).into();
            output["stdout_total"] = serde_json::json!({"bytes": budget, "lines": 1});
            observation["output"] = output.to_string().into();
            assert!((step.transform)(oversized).is_none());
            let migrated = (step.transform)(value).unwrap();
            let observation = if kind == "HookFinished" {
                &migrated["observation"]
            } else {
                &migrated
            };
            let output = observation["output"].as_str().unwrap();
            assert!(output.len() <= budget);
            let result: crate::tools::types::CommandResult = serde_json::from_str(output).unwrap();
            assert!(result.valid());
            assert_eq!(result.selection, None);
            let mut missing: Value = serde_json::from_str(output).unwrap();
            missing.as_object_mut().unwrap().remove("selection");
            assert!(serde_json::from_value::<crate::tools::types::CommandResult>(missing).is_err());
            for field in ["exit_code", "changeset", "stdout_total", "stderr_total"] {
                assert_eq!(
                    serde_json::to_value(&result).unwrap()[field],
                    old_result[field]
                );
            }
            assert!(result.stdout.contains("lines omitted;"));
            if shape == "asymmetric" {
                assert_eq!(result.stdout.split_once("]…\n").unwrap().1, "suffix");
            }
            assert_decodes_after_migration(kind, version + 1, &migrated);
        }
    }
}

#[test]
fn every_stored_version_above_its_floor_has_a_step_and_fixture() {
    use crate::journal::check::{CHECK_FORMATS, tests::fixture};
    let check_floors: Vec<_> = CHECK_FORMATS
        .iter()
        .map(|(kind, floor, _)| (*kind, *floor))
        .collect();
    let mut check_versions: Vec<_> = CHECK_FORMATS
        .iter()
        .map(|(kind, _, version)| (*kind, *version))
        .collect();
    let check_registry = Registry {
        schema: SchemaFormat {
            floor: SCHEMA_VERSION,
            current: SCHEMA_VERSION,
            schema: SCHEMA_SQL,
            steps: &[],
        },
        event_floor: &check_floors,
        event_steps: &[],
    };
    assert_eq!(
        uncovered(&check_registry, &check_versions, SCHEMA_VERSION, &[]),
        Vec::<String>::new()
    );
    for (kind, floor, version) in CHECK_FORMATS {
        let event = fixture()
            .1
            .into_iter()
            .find(|event| event.kind() == *kind)
            .expect("canonical check identity fixture");
        assert_eq!(*floor, 1);
        assert_eq!(event.version(), *version);
    }
    check_versions[0].1 += 1;
    assert_eq!(
        uncovered(&check_registry, &check_versions, SCHEMA_VERSION, &[]),
        ["ProviderCheckStarted 1 -> 2 has no step and fixture"]
    );
    assert_eq!(
        uncovered(
            &REGISTRY,
            &PAYLOAD_VERSIONS,
            SCHEMA_VERSION,
            &EVENT_FIXTURES
        ),
        Vec::<String>::new()
    );
    for kind in [
        "ToolFinished",
        "RunStarted",
        "GoalStarted",
        "GoalFinished",
        "GoalPlanContractRecorded",
        "AgentSpawned",
    ] {
        let (from, fixture) = EVENT_FIXTURES
            .iter()
            .filter(|(fixture_kind, _, _, _)| *fixture_kind == kind)
            .map(|(_, from, _, expected)| (*from, *expected))
            .max_by_key(|(from, _)| *from)
            .expect("canonical payload fixture");
        assert_eq!(from + 1, crate::session::payload_version(kind).unwrap());
        // Preserve the historical fixture while pairing its policy with a current Supervisor.
        let fixture = if kind == "RunStarted" {
            CURRENT_HOST_RUNSTARTED
        } else {
            fixture
        };
        let (run_id, agent_run_id) = match kind {
            "GoalStarted" | "GoalFinished" => (None, None),
            "RunStarted" | "AgentSpawned" => (
                Some(RunId::new()),
                (kind == "AgentSpawned").then(AgentRunId::new),
            ),
            "GoalPlanContractRecorded" => (Some(RunId::new()), None),
            _ => (
                Some("01a15ecf-a92a-7707-8af6-ce3d36320b67".parse().unwrap()),
                Some("01a15ecf-a92a-7707-8af6-ce3e0c00af83".parse().unwrap()),
            ),
        };
        let event = Event::decode(kind, from + 1, fixture, run_id, agent_run_id)
            .unwrap_or_else(|error| panic!("{kind} v{} fixture decode: {error:?}", from + 1));
        assert_eq!(event.payload().unwrap(), fixture, "{kind}");
    }
    assert_eq!(
        uncovered_schema(&CHECKPOINT_SCHEMA, checkpoints::SCHEMA_VERSION),
        Vec::<String>::new()
    );
    assert_eq!(
        uncovered_schema(&CHECKPOINT_SCHEMA, checkpoints::SCHEMA_VERSION + 1),
        ["schema 1 -> 2 has no step and fixture"]
    );
    let temp = tempfile::tempdir().unwrap();
    let fixture = Connection::open(temp.path().join("checkpoints.sqlite")).unwrap();
    fixture.execute_batch(CHECKPOINT_V1).unwrap();
    assert_eq!(CHECKPOINT_SCHEMA.floor, 1);
    assert_eq!(user_version(&fixture), CHECKPOINT_SCHEMA.floor);
    for step in CHECKPOINT_SCHEMA.steps {
        assert_eq!(step.from, user_version(&fixture));
        assert_eq!(schema_objects(&fixture).unwrap(), schema_of(step.source));
        fixture.execute_batch(step.sql).unwrap();
        fixture
            .pragma_update(None, "user_version", step.from + 1)
            .unwrap();
    }
    assert!(exact_schema(&fixture, &CHECKPOINT_SCHEMA, CHECKPOINT_SCHEMA.current).unwrap());
    assert_eq!(user_version(&fixture), CHECKPOINT_SCHEMA.current);
    let kind = "AgentSteeringNoteRecorded";
    assert_eq!(
        EVENT_FLOOR
            .iter()
            .find(|(name, _)| *name == kind)
            .map(|(_, version)| *version),
        Some(1)
    );
    let fixture = r#"{"goal_id":"01900000-0000-7000-8000-000000000001","text":"child note"}"#;
    let current = crate::Event::decode(
        kind,
        1,
        fixture,
        Some(RunId::new()),
        Some(AgentRunId::new()),
    )
    .unwrap();
    assert_eq!(current.payload().unwrap(), fixture);
    assert!(REGISTRY.event_step(kind, 0).is_none());
    // History older than the glob Tool cannot name it, so such a row is forged and refuses.
    let forged = GLOB_TOOLFINISHED.replace(
        r#"{"operation":"read","path":"src/lib.rs","offset":null,"limit":1024}"#,
        r#"{"operation":"glob","pattern":"*.rs","path":".","limit":100}"#,
    );
    assert_ne!(forged, GLOB_TOOLFINISHED);
    let step = REGISTRY.event_step("ToolFinished", 19).unwrap();
    assert!((step.canonical)(&forged).is_none());
    assert!((step.transform)(serde_json::from_str(&forged).unwrap()).is_none());
    // Version 14 may name glob but not the later Git writes.
    let git_step = REGISTRY.event_step("ToolFinished", 20).unwrap();
    assert!((git_step.canonical)(&forged).is_some());
    let forged_git = GLOB_TOOLFINISHED.replace(
        r#"{"operation":"read","path":"src/lib.rs","offset":null,"limit":1024}"#,
        r#"{"operation":"git_switch","branch":"topic"}"#,
    );
    assert_ne!(forged_git, GLOB_TOOLFINISHED);
    assert!((git_step.canonical)(&forged_git).is_none());
    assert!((git_step.transform)(serde_json::from_str(&forged_git).unwrap()).is_none());
    // A version-18 row that already pins nested sources is forged; one without instructions pins none.
    let step = REGISTRY.event_step("RunStarted", 18).unwrap();
    let forged = ROOT_RUNSTARTED.replace(
        r#""include_digests""#,
        r#""launch_directory":"","include_digests""#,
    );
    assert!((step.canonical)(&forged).is_none());
    assert!((step.transform)(serde_json::from_str(&forged).unwrap()).is_none());
    let step = REGISTRY.event_step("RunStarted", 19).unwrap();
    let forged = NESTED_RUNSTARTED.replace(
        r#""instruction_sources""#,
        r#""user_instruction_digest":null,"instruction_sources""#,
    );
    assert!((step.canonical)(&forged).is_none());
    assert!((step.transform)(serde_json::from_str(&forged).unwrap()).is_none());
    let step = REGISTRY.event_step("RunStarted", 18).unwrap();
    let upgraded = (step.transform)(serde_json::from_str(GLOB_RUNSTARTED).unwrap()).unwrap();
    assert_eq!(
        upgraded["config"]["instruction_sources"],
        serde_json::json!([])
    );
    let floor = started_floor();
    assert_eq!(
        uncovered(
            &synthetic(&floor, &STARTED_STEP),
            &[("SessionStarted", 6)],
            SCHEMA_VERSION,
            &[STARTED_FIXTURE]
        ),
        Vec::<String>::new(),
        "a registered step with its fixture covers its edge"
    );

    let mut bumped = PAYLOAD_VERSIONS.to_vec();
    bumped.iter_mut().for_each(|(kind, version)| {
        if *kind == "SessionRenamed" {
            *version += 1;
        }
    });
    bumped.push(("SyntheticKind", 1));
    assert_eq!(
        uncovered(&REGISTRY, &bumped, SCHEMA_VERSION + 1, &EVENT_FIXTURES),
        [
            "SessionRenamed 6 -> 7 has no step and fixture",
            "SyntheticKind declares no migration floor",
            "schema 7 -> 8 has no step and fixture",
        ]
    );
}

#[test]
fn rewind_started_migration_removes_only_the_old_expiry() {
    let step = REGISTRY
        .event_step("WorkspaceRewindStarted", 1)
        .expect("migration");
    assert_eq!(
        (step.canonical)(REWIND_STARTED_V1).as_deref(),
        Some(REWIND_STARTED_V1)
    );
    assert_eq!(
        (step.transform)(serde_json::from_str(REWIND_STARTED_V1).unwrap()),
        Some(serde_json::from_str(REWIND_STARTED_V2).unwrap())
    );
    let mut zero_expiry: Value = serde_json::from_str(REWIND_STARTED_V1).unwrap();
    zero_expiry["expires_at_ms"] = serde_json::json!(0);
    assert!((step.transform)(zero_expiry).is_none());
    assert!((step.canonical)(&REWIND_STARTED_V1.replace("60000", "0")).is_none());
}

fn checkpoint_fixture(root: &StateRoot) {
    create_private(&root.dir, checkpoints::FILES[0]).unwrap();
    Connection::open(root.path().join(checkpoints::FILES[0]))
        .unwrap()
        .execute_batch(CHECKPOINT_V1)
        .unwrap();
}

#[test]
fn checkpoint_identity_fixture_preserves_references_and_rewind_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let History { root, sessions } = forked_history(&path);
    checkpoint_fixture(&root);
    let original = std::fs::read(path.join(checkpoints::FILES[0])).unwrap();
    for _ in 0..2 {
        assert!(root.archive_incompatible_history().unwrap().is_none());
        let reopened = StateRoot::open_existing(&path).unwrap();
        let connection = open_source(&path.join(checkpoints::FILES[0]), true).unwrap();
        let references: Vec<(Vec<u8>, i64)> = connection
            .prepare("SELECT digest, len FROM refs ORDER BY tool_sequence")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(references.len(), 2);
        for (digest, len) in references {
            let bytes = checkpoints::load(&reopened, digest.try_into().unwrap(), &mut None)
                .unwrap()
                .unwrap();
            assert_eq!(bytes, b"original");
            assert_eq!(bytes.len() as i64, len);
        }
        replays(&reopened, &sessions);
        assert_eq!(
            std::fs::read(path.join(checkpoints::FILES[0])).unwrap(),
            original
        );
        assert!(!path.join(DIR).exists());
        assert!(!path.join("archive").exists());
    }
}

#[test]
fn checkpoint_below_floor_archives_history_intact_under_existing_locks() {
    let mut completed = false;
    for interruption in 0..64 {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let History { root, .. } = forked_history(&path);
        checkpoint_fixture(&root);
        Connection::open(path.join(checkpoints::FILES[0]))
            .unwrap()
            .pragma_update(None, "user_version", 0)
            .unwrap();
        let originals: Vec<_> = [DATABASE_FILE, checkpoints::FILES[0]]
            .into_iter()
            .map(|name| (name, std::fs::read(path.join(name)).unwrap()))
            .collect();
        if interruption == 0 {
            let lease = root.lock_history(false).unwrap();
            assert!(matches!(
                root.archive_incompatible_history(),
                Err(StoreError::HistoryBusy)
            ));
            drop(lease);
            let lock = checkpoints::lock(&root).unwrap();
            assert!(matches!(
                root.archive_incompatible_history(),
                Err(StoreError::CheckpointUnavailable)
            ));
            drop(lock);
        }
        fail_after(Some(interruption));
        let result = root.archive_incompatible_history();
        fail_after(None);
        let archived = match result {
            Ok(archived) => {
                completed = true;
                archived.unwrap()
            }
            Err(StoreError::Io(_)) => root.archive_incompatible_history().unwrap().unwrap(),
            Err(error) => panic!("step {interruption}: {error}"),
        };
        for (name, original) in originals {
            assert_eq!(std::fs::read(archived.path().join(name)).unwrap(), original);
        }
        assert!(!path.join(checkpoints::FILES[0]).exists());
        assert!(!path.join(DIR).exists());
        assert_eq!(
            root.archive_incompatible_history().unwrap().unwrap().path(),
            archived.path()
        );
        archived.acknowledge().unwrap();
        assert!(root.archive_incompatible_history().unwrap().is_none());
        assert!(stored_rows(&open_connection(&root, false).unwrap()).is_empty());
        if completed {
            assert!(interruption >= 9);
            break;
        }
    }
    assert!(
        completed,
        "archive must complete within the failpoint bound"
    );
}

fn hot_checkpoint_journal(
    temp: &std::path::Path,
    database: &std::path::Path,
    journal: &std::path::Path,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let scratch = temp.join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    let writer = Connection::open(database).unwrap();
    writer.execute_batch("PRAGMA cache_size=1; BEGIN;").unwrap();
    writer.execute_batch("CREATE TABLE junk(x BLOB);").unwrap();
    for _ in 0..300 {
        writer
            .execute("INSERT INTO junk VALUES (randomblob(4000))", [])
            .unwrap();
    }
    std::fs::copy(database, scratch.join("db")).unwrap();
    std::fs::copy(journal, scratch.join("journal")).unwrap();
    drop(writer);
    (scratch.join("db"), scratch.join("journal"))
}

fn install_hot_journal(
    (db, hot): &(std::path::PathBuf, std::path::PathBuf),
    database: &std::path::Path,
    journal: &std::path::Path,
) {
    std::fs::copy(db, database).unwrap();
    std::fs::copy(hot, journal).unwrap();
}

#[test]
fn checkpoint_hot_journal_recovers_before_eligibility() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let History { root, sessions } = forked_history(&path);
    checkpoint_fixture(&root);
    let database = path.join(checkpoints::FILES[0]);
    let journal_path = path.join(checkpoints::FILES[1]);
    let original = std::fs::read(&database).unwrap();
    let journal = std::fs::read(path.join(DATABASE_FILE)).unwrap();
    let hot = hot_checkpoint_journal(temp.path(), &database, &journal_path);
    install_hot_journal(&hot, &database, &journal_path);
    assert_ne!(std::fs::read(&database).unwrap(), original);
    assert!(!checkpoint_below_floor(&root).unwrap());
    assert!(!journal_path.exists());
    assert_eq!(std::fs::read(&database).unwrap(), original);
    assert!(root.archive_incompatible_history().unwrap().is_none());
    assert_eq!(std::fs::read(path.join(DATABASE_FILE)).unwrap(), journal);
    assert!(!path.join("archive").exists());
    replays(&root, &sessions);
}

#[test]
fn checkpoint_regular_journal_that_survives_recovery_refuses_untouched() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let History { root, sessions } = forked_history(&path);
    checkpoint_fixture(&root);
    let database = path.join(checkpoints::FILES[0]);
    let journal_path = path.join(checkpoints::FILES[1]);
    create_private(&root.dir, checkpoints::FILES[1]).unwrap();
    let database_bytes = std::fs::read(&database).unwrap();
    let journal = std::fs::read(path.join(DATABASE_FILE)).unwrap();
    assert!(matches!(
        checkpoint_below_floor(&root),
        Err(StoreError::InvalidStateDirectory)
    ));
    assert!(matches!(
        root.archive_incompatible_history(),
        Err(StoreError::InvalidStateDirectory)
    ));
    assert!(journal_path.symlink_metadata().unwrap().is_file());
    assert_eq!(std::fs::metadata(&journal_path).unwrap().len(), 0);
    assert_eq!(std::fs::read(&database).unwrap(), database_bytes);
    assert_eq!(std::fs::read(path.join(DATABASE_FILE)).unwrap(), journal);
    assert!(!path.join("archive").exists() && !path.join(DIR).exists());
    replays(&root, &sessions);
}

#[test]
fn checkpoint_journal_that_survives_recovery_refuses_untouched() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let History { root, sessions } = forked_history(&path);
    checkpoint_fixture(&root);
    let target = path.join("elsewhere");
    std::fs::write(&target, vec![0xA5; 512]).unwrap();
    std::os::unix::fs::symlink(&target, path.join(checkpoints::FILES[1])).unwrap();
    let original = std::fs::read(path.join(checkpoints::FILES[0])).unwrap();
    let journal = std::fs::read(path.join(DATABASE_FILE)).unwrap();
    assert!(checkpoint_below_floor(&root).is_err());
    assert!(root.archive_incompatible_history().is_err());
    assert!(
        path.join(checkpoints::FILES[1])
            .symlink_metadata()
            .unwrap()
            .is_symlink()
    );
    assert_eq!(std::fs::read(&target).unwrap(), vec![0xA5; 512]);
    assert_eq!(
        std::fs::read(path.join(checkpoints::FILES[0])).unwrap(),
        original
    );
    assert_eq!(std::fs::read(path.join(DATABASE_FILE)).unwrap(), journal);
    assert!(!path.join("archive").exists() && !path.join(DIR).exists());
    replays(&root, &sessions);
}

#[test]
fn orphan_checkpoint_journal_refuses_untouched() {
    for empty_database in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let History { root, sessions } = forked_history(&path);
        let database = path.join(checkpoints::FILES[0]);
        if empty_database {
            create_private(&root.dir, checkpoints::FILES[0]).unwrap();
        }
        create_private(&root.dir, checkpoints::FILES[1]).unwrap();
        std::fs::write(path.join(checkpoints::FILES[1]), b"orphan").unwrap();
        let journal = std::fs::read(path.join(DATABASE_FILE)).unwrap();
        assert!(matches!(
            root.archive_incompatible_history(),
            Err(StoreError::InvalidStateDirectory)
        ));
        assert_eq!(
            std::fs::read(path.join(checkpoints::FILES[1])).unwrap(),
            b"orphan"
        );
        assert_eq!(database.exists(), empty_database);
        assert_eq!(std::fs::read(path.join(DATABASE_FILE)).unwrap(), journal);
        assert!(!path.join("archive").exists() && !path.join(DIR).exists());
        replays(&root, &sessions);
    }
}

#[test]
fn checkpoint_recovery_preserves_newer_malformed_busy_and_unsafe_storage() {
    for case in ["newer", "malformed", "extra_trigger", "busy", "unsafe"] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let History { root, sessions } = forked_history(&path);
        checkpoint_fixture(&root);
        let connection = Connection::open(path.join(checkpoints::FILES[0])).unwrap();
        match case {
            "newer" => connection.pragma_update(None, "user_version", 2).unwrap(),
            "extra_trigger" => connection
                .execute_batch(
                    "CREATE TRIGGER forged AFTER INSERT ON refs BEGIN DELETE FROM blobs; END;",
                )
                .unwrap(),
            "busy" => connection.execute_batch("BEGIN EXCLUSIVE;").unwrap(),
            _ => {}
        }
        if case == "malformed" {
            std::fs::write(path.join(checkpoints::FILES[0]), b"malformed").unwrap();
        }
        if case == "unsafe" {
            std::os::unix::fs::symlink(
                path.join(checkpoints::FILES[0]),
                path.join(checkpoints::FILES[1]),
            )
            .unwrap();
        }
        let original = std::fs::read(path.join(checkpoints::FILES[0])).unwrap();
        let journal = std::fs::read(path.join(DATABASE_FILE)).unwrap();
        let result = root.archive_incompatible_history();
        match case {
            "busy" => assert!(
                matches!(result, Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(code, _)))
                if code.code == rusqlite::ErrorCode::DatabaseBusy)
            ),
            "unsafe" => assert!(result.is_err()),
            _ => assert!(result.unwrap().is_none()),
        }
        if matches!(case, "newer" | "extra_trigger") {
            assert!(
                matches!(
                    checkpoints::load(&root, [0; 32], &mut None),
                    Err(StoreError::CheckpointUnavailable)
                ),
                "{case}"
            );
        }
        assert_eq!(
            std::fs::read(path.join(checkpoints::FILES[0])).unwrap(),
            original,
            "{case}"
        );
        assert_eq!(
            std::fs::read(path.join(DATABASE_FILE)).unwrap(),
            journal,
            "{case}"
        );
        assert!(!path.join(DIR).exists(), "{case}");
        assert!(!path.join("archive").exists(), "{case}");
        replays(&root, &sessions);
    }
}

const SELECTION_HOOKFINISHED: &str = r#"{"boundary_active_ms":null,"observation":{"intent":{"target":{"kind":"workspace"},"id":"01a15ecf-a92b-736a-91a6-560ce9fc3701","run_id":"01a15ecf-a92a-7707-8af6-ce3d36320b67","agent_run_id":"01a15ecf-a92a-7707-8af6-ce3e0c00af83","policy_digest":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"enforcement_digest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"workspace_device":1,"workspace_inode":2,"call":{"operation":"command","program":"sh","args":["-c","exit 2"],"cwd":""},"limits":{"result_bytes":65536,"runtime_ms":60000,"cache_transfer_ms":0,"memory_bytes":536870912,"max_processes":64,"scratch_bytes":67108864,"project_bytes":67108864,"cache_requested_bytes":67108864,"cache_bytes":67108864,"cache_candidate_bytes":67108864,"network":"none","resource_class":"default","resource_profile":"linux_kernel"},"use_count":1},"disposition":"failed","enforcement_cause":null,"limit_cause":null,"output":"{\"exit_code\":2,\"stdout\":\"\",\"stderr\":\"blocked\",\"stdout_total\":{\"bytes\":0,\"lines\":0},\"stderr_total\":{\"bytes\":7,\"lines\":1},\"changeset\":{\"files\":[]},\"selection\":null}","attestation":{"guard":{"contract_version":1,"intent_digest":[141,97,53,247,53,49,168,154,40,85,182,218,142,191,39,167,241,62,24,231,138,149,79,234,129,164,230,179,135,109,205,218],"enforcement_digest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"limits":{"result_bytes":65536,"runtime_ms":60000,"cache_transfer_ms":0,"memory_bytes":536870912,"max_processes":64,"scratch_bytes":67108864,"project_bytes":67108864,"cache_requested_bytes":67108864,"cache_bytes":67108864,"cache_candidate_bytes":67108864,"network":"none","resource_class":"default","resource_profile":"linux_kernel"}}},"approval_cause":null,"cache_update":"not_applicable"},"result":{"restricted":{"decision":"deny","reason":"blocked"}}}"#;

const COMMAND_TOOLFINISHED_WITHOUT_EXPIRY: &str =
    include_str!("command-toolfinished-without-expiry.json");
const COMMAND_TOOLFINISHED: &str = r#"{"intent":{"target":{"kind":"workspace"},"id":"01a15ecf-a92b-736a-91a6-560ce9fc3701","run_id":"01a15ecf-a92a-7707-8af6-ce3d36320b67","agent_run_id":"01a15ecf-a92a-7707-8af6-ce3e0c00af83","policy_digest":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"enforcement_digest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"workspace_device":1,"workspace_inode":2,"call":{"operation":"command","program":"sh","args":["-c","exit 2"],"cwd":""},"limits":{"result_bytes":65536,"runtime_ms":60000,"cache_transfer_ms":0,"memory_bytes":536870912,"max_processes":64,"scratch_bytes":67108864,"project_bytes":67108864,"cache_requested_bytes":67108864,"cache_bytes":67108864,"cache_candidate_bytes":67108864,"network":"none","resource_class":"default","resource_profile":"linux_kernel"},"expires_at_ms":1,"use_count":1},"disposition":"failed","enforcement_cause":null,"limit_cause":null,"output":"{\"changeset\":{\"files\":[]},\"exit_code\":2,\"stderr\":\"blocked\",\"stderr_total\":{\"bytes\":7,\"lines\":1},\"stdout\":\"\",\"stdout_total\":{\"bytes\":0,\"lines\":0}}","attestation":{"guard":{"contract_version":1,"intent_digest":[131,11,117,251,8,22,13,66,136,215,191,13,209,208,209,113,109,80,241,218,188,193,7,40,115,63,160,61,172,44,137,133],"enforcement_digest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"limits":{"result_bytes":65536,"runtime_ms":60000,"cache_transfer_ms":0,"memory_bytes":536870912,"max_processes":64,"scratch_bytes":67108864,"project_bytes":67108864,"cache_requested_bytes":67108864,"cache_bytes":67108864,"cache_candidate_bytes":67108864,"network":"none","resource_class":"default","resource_profile":"linux_kernel"}}},"cache_update":"not_applicable","checkpoint":null}"#;
const SELECTION_TOOLFINISHED: &str = r#"{"intent":{"target":{"kind":"workspace"},"id":"01a15ecf-a92b-736a-91a6-560ce9fc3701","run_id":"01a15ecf-a92a-7707-8af6-ce3d36320b67","agent_run_id":"01a15ecf-a92a-7707-8af6-ce3e0c00af83","policy_digest":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"enforcement_digest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"workspace_device":1,"workspace_inode":2,"call":{"operation":"command","program":"sh","args":["-c","exit 2"],"cwd":""},"limits":{"result_bytes":65536,"runtime_ms":60000,"cache_transfer_ms":0,"memory_bytes":536870912,"max_processes":64,"scratch_bytes":67108864,"project_bytes":67108864,"cache_requested_bytes":67108864,"cache_bytes":67108864,"cache_candidate_bytes":67108864,"network":"none","resource_class":"default","resource_profile":"linux_kernel"},"use_count":1},"disposition":"failed","enforcement_cause":null,"limit_cause":null,"output":"{\"exit_code\":2,\"stdout\":\"\",\"stderr\":\"blocked\",\"stdout_total\":{\"bytes\":0,\"lines\":0},\"stderr_total\":{\"bytes\":7,\"lines\":1},\"changeset\":{\"files\":[]},\"selection\":null}","attestation":{"guard":{"contract_version":1,"intent_digest":[141,97,53,247,53,49,168,154,40,85,182,218,142,191,39,167,241,62,24,231,138,149,79,234,129,164,230,179,135,109,205,218],"enforcement_digest":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"limits":{"result_bytes":65536,"runtime_ms":60000,"cache_transfer_ms":0,"memory_bytes":536870912,"max_processes":64,"scratch_bytes":67108864,"project_bytes":67108864,"cache_requested_bytes":67108864,"cache_bytes":67108864,"cache_candidate_bytes":67108864,"network":"none","resource_class":"default","resource_profile":"linux_kernel"}}},"approval_cause":null,"cache_update":"not_applicable","checkpoint":null}"#;

#[test]
fn expiry_migration_rebinds_worker_receipts_and_preserves_interrupted_history() {
    for (name, fixture) in [
        ("worker", crate::session::worker_protocol_fixture()),
        ("compacted", crate::session::worker_compacted_fixture(true)),
        ("host", crate::session::host_protocol_fixture()),
        (
            "host-session",
            crate::session::session_host_protocol_fixture(),
        ),
    ] {
        for interrupted in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let mut connection = Connection::open(temp.path().join("history.sqlite3")).unwrap();
            connection.execute_batch(SCHEMA_SQL).unwrap();
            let mut pins = HashMap::new();
            let mut expected = Vec::new();
            for envelope in fixture.envelopes() {
                let event = &envelope.event;
                let mut value: Value = serde_json::from_str(&event.payload().unwrap()).unwrap();
                let intent = value
                    .get("intent")
                    .or_else(|| value.get("observation")?.get("intent"))
                    .cloned();
                let prior_human = value
                    .get("host")
                    .and_then(|host| host.get("source_intent"))
                    .and_then(|pin| serde_json::from_value::<[u8; 32]>(pin.clone()).ok());
                repin_intents(&mut value, &pins);
                if let Some(old) = prior_human.and_then(|pin| pins.get(&pin)) {
                    value["host"]["source_intent"] = serde_json::json!(old);
                }
                let source_version = match event {
                    Event::ToolFinished { .. } => 24,
                    Event::HookStarted { .. } => 8,
                    Event::HookFinished { .. } => 13,
                    _ => event.version(),
                };
                let expiring = source_version != event.version();
                if let Some(original) = intent.filter(|_| expiring) {
                    let current: crate::tools::EffectIntent =
                        serde_json::from_value(original).unwrap();
                    let target = if event.kind() == "HookFinished" {
                        &mut value["observation"]["intent"]
                    } else {
                        &mut value["intent"]
                    };
                    target["expires_at_ms"] = serde_json::json!(1);
                    let historical = crate::session::expiring_intent_text(target).unwrap();
                    let old = Sha256::digest(historical.as_bytes()).into();
                    pins.insert(current.digest(), old);
                    crate::session::replace_intent_pin(&mut value, &current.digest(), &old);
                }
                let observation = match event {
                    Event::ToolFinished { .. } => Some(&mut value),
                    Event::HookFinished { .. } => Some(&mut value["observation"]),
                    _ => None,
                };
                if let Some(observation) = observation {
                    observation
                        .as_object_mut()
                        .unwrap()
                        .remove("approval_cause");
                    if let Ok(mut result) =
                        serde_json::from_str::<Value>(observation["output"].as_str().unwrap())
                        && result.get("selection").is_some()
                    {
                        result.as_object_mut().unwrap().remove("selection");
                        observation["output"] = result.to_string().into();
                    }
                }
                let payload = if expiring {
                    crate::session::expiring_payload(event.kind(), &value.to_string()).unwrap()
                } else {
                    Event::decode(
                        event.kind(),
                        event.version(),
                        &value.to_string(),
                        envelope.run_id,
                        envelope.agent_run_id,
                    )
                    .unwrap()
                    .payload()
                    .unwrap()
                };
                connection.execute("INSERT INTO events (sequence,session_id,run_id,agent_run_id,kind,event_version,payload,created_at_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,0)", rusqlite::params![i64::try_from(envelope.sequence).unwrap(), envelope.session_id.to_string(), envelope.run_id.map(|id| id.to_string()), envelope.agent_run_id.map(|id| id.to_string()), event.kind(), source_version, payload]).unwrap();
                let mut expected_event = envelope.event;
                let observation = match &mut expected_event {
                    Event::ToolFinished { observation, .. } => Some(observation),
                    Event::HookFinished { outcome, .. } => Some(&mut outcome.observation),
                    _ => None,
                };
                if let Some(observation) = observation
                    && let Ok(mut result) = serde_json::from_str::<crate::tools::types::CommandResult>(
                        &observation.output,
                    )
                {
                    result.selection = None;
                    observation.output = serde_json::to_string(&result).unwrap();
                }
                expected.push(expected_event);
                if interrupted && expiring {
                    break;
                }
            }
            repin(&connection, |_| {});
            let transaction = connection.transaction().unwrap();
            assert!(rewrite_events(&transaction, &REGISTRY).unwrap() > 0);
            transaction.commit().unwrap();
            validate_all(&connection, MAX_VALIDATION_EVENTS).unwrap();
            drop(connection);
            let reopened = Connection::open(temp.path().join("history.sqlite3")).unwrap();
            let actual: Vec<_> = rows(&reopened)
                .into_iter()
                .map(|row| row.decode().unwrap().event)
                .collect();
            assert_eq!(actual, expected, "{name}, interrupted={interrupted}");
        }
    }
}

#[test]
fn diagnostic_schema_upgrade_preserves_sessions_and_discards_unproven_evidence() {
    for source_version in [4, 5] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        let History { root, sessions } = forked_history(&path);
        let connection = Connection::open(path.join(DATABASE_FILE)).unwrap();
        let original = stored_rows(&connection);
        let digests = fork_digests(&connection);
        restore_pre_check_schema(&connection, source_version);
        assert!(exact_schema(&connection, &REGISTRY.schema, source_version).unwrap());
        connection
            .execute(
                "INSERT INTO provider_evidence VALUES ('local', ?1, '[\"127.0.0.1:9321\"]', 1, 2)",
                [[1_u8; 32].as_slice()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO native_provider_evidence VALUES (?1, 1, 2)",
                [[2_u8; 32].as_slice()],
            )
            .unwrap();
        drop(connection);
        assert!(root.archive_incompatible_history().unwrap().is_none());
        let upgraded = Connection::open(path.join(DATABASE_FILE)).unwrap();
        assert!(exact_schema(&upgraded, &REGISTRY.schema, SCHEMA_VERSION).unwrap());
        assert_eq!(stored_rows(&upgraded), original);
        assert_eq!(fork_digests(&upgraded), digests);
        for table in [
            "provider_evidence",
            "native_provider_evidence",
            "provider_check_events",
        ] {
            assert_eq!(
                upgraded
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        drop(upgraded);
        replays(&root, &sessions);
    }
}

#[test]
fn anthropic_window_schema_upgrade_discards_old_evidence() {
    let bounds = format!(
        "BETWEEN {} AND {}",
        crate::provider::MIN_CONTEXT_WINDOW_TOKENS,
        crate::provider::MAX_CONTEXT_WINDOW_TOKENS,
    );
    assert!(SCHEMA_SQL.contains(&bounds));
    assert!(include_str!("schema_v7.sql").contains(&bounds));
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let History { root, sessions } = forked_history(&path);
    let connection = Connection::open(path.join(DATABASE_FILE)).unwrap();
    connection
        .execute_batch(
            "DROP TABLE native_provider_evidence;
        CREATE TABLE native_provider_evidence (
            fingerprint BLOB PRIMARY KEY CHECK(length(fingerprint) = 32),
            checked_at_ms INTEGER NOT NULL,
            expires_at_ms INTEGER NOT NULL,
            check_id TEXT NOT NULL
        ) STRICT;
        PRAGMA user_version=6;",
        )
        .unwrap();
    assert!(exact_schema(&connection, &REGISTRY.schema, 6).unwrap());
    connection
        .execute(
            "INSERT INTO native_provider_evidence VALUES (?1, 1, 2, '00000000-0000-7000-8000-000000000000')",
            [[2_u8; 32].as_slice()],
        )
        .unwrap();
    drop(connection);
    assert!(root.archive_incompatible_history().unwrap().is_none());
    let upgraded = Connection::open(path.join(DATABASE_FILE)).unwrap();
    assert!(exact_schema(&upgraded, &REGISTRY.schema, SCHEMA_VERSION).unwrap());
    assert_eq!(
        upgraded
            .query_row("SELECT COUNT(*) FROM native_provider_evidence", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        0
    );
    drop(upgraded);
    replays(&root, &sessions);
}

fn restore_pre_check_schema(connection: &Connection, source_version: i64) {
    connection.execute_batch("DROP TABLE provider_check_events; DROP TABLE provider_evidence; DROP TABLE native_provider_evidence;").unwrap();
    for statement in include_str!("schema_v5.sql").split(';').map(str::trim) {
        if statement.starts_with("CREATE TABLE provider_evidence ")
            || statement.starts_with("CREATE TABLE native_provider_evidence ")
        {
            connection.execute_batch(statement).unwrap();
        }
    }
    if source_version == 4 {
        connection.execute_batch("DROP TABLE migrations;").unwrap();
    }
    connection
        .pragma_update(None, "user_version", source_version)
        .unwrap();
    assert!(exact_schema(connection, &REGISTRY.schema, source_version).unwrap());
}

#[test]
fn pre_host_migrations_never_create_live_authority() {
    let mut run: Value = serde_json::from_str(HOST_RUNSTARTED).unwrap();
    assert!(crate::session::pre_host_payload("RunStarted", &run.to_string()).is_some());
    run["config"]["tool_policy"]["child_unguarded"] = serde_json::json!("human_only");
    let current = crate::session::run_started_v23_to_v24(run.clone()).unwrap();
    serde_json::from_value::<crate::session::RunConfig>(current["config"].clone()).unwrap();
    assert!(crate::session::pre_host_payload("RunStarted", &run.to_string()).is_none());
}

#[test]
fn pre_host_migrations_preserve_marker_words_in_data() {
    for marker in [
        "command_unguarded",
        "arany_command_unguarded",
        "exact_command_rule",
        "session_approval",
    ] {
        let mut tool: Value = serde_json::from_str(GLOB_TOOLFINISHED).unwrap();
        tool["intent"]["call"] =
            serde_json::json!({"operation":"search","path":"src","query":marker});
        let (run, agent) = intent_scope(&tool).unwrap();
        let payload = crate::session::expiring_payload("ToolFinished", &tool.to_string()).unwrap();
        assert_eq!(
            crate::session::pre_host_payload("ToolFinished", &payload),
            Some(payload)
        );
        let mut finished: Value = serde_json::from_str(GLOB_TOOLFINISHED).unwrap();
        finished["output"] = marker.into();
        let payload =
            crate::session::expiring_payload("ToolFinished", &finished.to_string()).unwrap();
        assert_eq!(
            crate::session::pre_host_payload("ToolFinished", &payload),
            Some(payload)
        );
        let mut provider: Value = serde_json::from_str(GLOB_PROVIDERCALLRECORDED).unwrap();
        provider["response_id"] = marker.into();
        let event = Event::decode(
            "ProviderCallRecorded",
            crate::test_support::payload_version("ProviderCallRecorded"),
            &provider.to_string(),
            Some(run),
            Some(agent),
        )
        .unwrap();
        let payload = event.payload().unwrap();
        assert_eq!(
            crate::session::pre_host_payload("ProviderCallRecorded", &payload),
            Some(payload)
        );
    }
}

#[test]
fn generic_host_migrations_preserve_preceding_typed_host_git_pins() {
    let oid = "1".repeat(40);
    let mut value: Value = serde_json::from_str(GLOB_TOOLFINISHED).unwrap();
    value["intent"]["call"] = serde_json::json!({"operation":"git_push", "remote":"https://github.com/o/r", "branch":"main"});
    value["intent"]["host"] =
        serde_json::json!({"source_oid":oid, "refspec":format!("{oid}:refs/heads/main")});
    value["intent"]["limits"]["network"] = "host".into();
    value["disposition"] = "denied".into();
    value["attestation"] = Value::Null;
    let (run, agent) = intent_scope(&value).unwrap();
    let payload = crate::session::expiring_payload("ToolFinished", &value.to_string()).unwrap();
    let step = REGISTRY.event_step("ToolFinished", 23).unwrap();
    assert_eq!((step.canonical)(&payload), Some(payload.clone()));
    let value: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!((step.transform)(value.clone()), Some(value.clone()));
    let mut current = value.clone();
    for from in 23..payload_version("ToolFinished").unwrap() {
        current = (REGISTRY.event_step("ToolFinished", from).unwrap().transform)(current).unwrap();
    }
    let event = Event::decode(
        "ToolFinished",
        crate::test_support::payload_version("ToolFinished"),
        &current.to_string(),
        Some(run),
        Some(agent),
    )
    .unwrap();
    assert_eq!(current["intent"]["host"], value["intent"]["host"]);
    assert_eq!(current["intent"]["limits"], value["intent"]["limits"]);
    assert!(current["intent"].get("expires_at_ms").is_none());
    assert_eq!(
        serde_json::from_str::<Value>(&event.payload().unwrap()).unwrap(),
        current
    );
}

#[test]
fn host_git_migrations_reject_new_authority_in_old_rows() {
    let step = REGISTRY.event_step("ToolFinished", 22).unwrap();
    let source: Value = serde_json::from_str(GLOB_TOOLFINISHED).unwrap();
    let mut multiedit = source.clone();
    multiedit["intent"]["call"] = serde_json::json!({"operation":"multiedit","file_path":"src/lib.rs","expected_digest":"0".repeat(64),"edits":[{"old_string":"before","new_string":"after","replace_all":false}]});
    assert!(
        (REGISTRY.event_step("ToolFinished", 21).unwrap().transform)(multiedit.clone()).is_none()
    );
    assert_eq!((step.transform)(multiedit.clone()), Some(multiedit.clone()));
    let canonical = (step.canonical)(&multiedit.to_string()).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&canonical).unwrap(),
        multiedit
    );
    for field in ["call", "host", "network"] {
        let mut forged = source.clone();
        match field {
            "call" => {
                forged["intent"]["call"] = serde_json::json!({"operation":"git_push", "remote":"https://github.com/o/r", "branch":"main"})
            }
            "host" => {
                forged["intent"]["host"] = serde_json::json!({"source_oid":"1".repeat(40),"refspec":format!("{}:refs/heads/main", "1".repeat(40))})
            }
            "network" => forged["intent"]["limits"]["network"] = serde_json::json!("host"),
            _ => unreachable!(),
        }
        assert!((step.canonical)(&forged.to_string()).is_none(), "{field}");
        assert!((step.transform)(forged).is_none(), "{field}");
    }
    assert_eq!(
        (step.canonical)(GLOB_TOOLFINISHED).unwrap(),
        GLOB_TOOLFINISHED
    );
    let mut host = source;
    host["intent"]["host"] = serde_json::json!({"source_oid":"1".repeat(40),"refspec":format!("{}:refs/heads/main", "1".repeat(40))});
    host["intent"]["call"] = serde_json::json!({"operation":"git_push","remote":"https://github.com/o/r","branch":"main"});
    host["intent"]["limits"]["network"] = serde_json::json!("host");
    let expiry = REGISTRY.event_step("ToolFinished", 24).unwrap();
    assert!((expiry.canonical)(&host.to_string()).is_some());
    let mut forged_cause = host.clone();
    forged_cause["approval_cause"] = serde_json::json!("declined");
    assert!((expiry.canonical)(&forged_cause.to_string()).is_none());
    assert!((expiry.transform)(forged_cause).is_none());
    let migrated = (expiry.transform)(host.clone()).unwrap();
    assert!(migrated["intent"].get("expires_at_ms").is_none());
    host["intent"]
        .as_object_mut()
        .unwrap()
        .remove("expires_at_ms");
    host["approval_cause"] = Value::Null;
    assert_eq!(
        migrated, host,
        "expiry removal preserves host facts and records unknown approval cause"
    );
    let removal = REGISTRY.event_step("ToolFinished", 26).unwrap();
    assert!((removal.canonical)(&migrated.to_string()).is_some());
    assert_eq!((removal.transform)(migrated.clone()), Some(migrated));
    let list_step = REGISTRY.event_step("ToolFinished", 27).unwrap();
    let no_expiry = include_str!("toolfinished-without-expiry.json");
    assert_eq!((list_step.canonical)(no_expiry).as_deref(), Some(no_expiry));
    assert_eq!(
        (list_step.transform)(serde_json::from_str(no_expiry).unwrap()),
        Some(serde_json::from_str(no_expiry).unwrap())
    );
    let mut list: Value =
        serde_json::from_str(include_str!("toolfinished-without-expiry.json")).unwrap();
    list["intent"]["call"] = serde_json::json!({
        "operation":"gh_pr_list",
        "repository":"github.com/o/r",
        "state":"open",
        "base":null,
        "head":null,
        "limit":30
    });
    list["intent"]["host"] = serde_json::json!({
        "source_oid":"gh-pr-list-v1",
        "refspec":"fixed-list"
    });
    list["intent"]["limits"]["network"] = serde_json::json!("host");
    assert!((list_step.canonical)(&list.to_string()).is_none());
    assert!((list_step.transform)(list).is_none());
}

#[test]
fn pr_view_identity_step_rejects_view_authority_in_older_payloads() {
    let step = REGISTRY.event_step("ToolFinished", 28).unwrap();
    let mut source: Value =
        serde_json::from_str(include_str!("toolfinished-without-expiry.json")).unwrap();
    assert!((step.canonical)(&source.to_string()).is_some());
    assert!((step.transform)(source.clone()).is_some());
    source["intent"]["call"] = serde_json::json!({
        "operation":"gh_pr_view",
        "repository":"github.com/o/r",
        "number":17
    });
    source["intent"]["host"] = serde_json::json!({
        "source_oid":"gh-pr-view-v1",
        "refspec":"fixed-view-v1"
    });
    source["intent"]["limits"]["network"] = serde_json::json!("host");
    assert!((step.canonical)(&source.to_string()).is_none());
    assert!((step.transform)(source).is_none());
}

#[test]
fn file_removal_identity_steps_reject_new_calls_and_capabilities_in_old_payloads() {
    for operation in ["delete", "move"] {
        let call = if operation == "delete" {
            serde_json::json!({"operation":operation,"path":"src/source","expected_digest":"0".repeat(64)})
        } else {
            serde_json::json!({"operation":operation,"source":"src/source","destination":"src/target","expected_digest":"0".repeat(64)})
        };
        let mut started: Value =
            serde_json::from_str(include_str!("toolfinished-without-expiry.json")).unwrap();
        started["intent"]["call"] = call;
        let step = REGISTRY.event_step("ToolFinished", 26).unwrap();
        assert!((step.canonical)(&started.to_string()).is_none());
        assert!((step.transform)(started.clone()).is_none());
        let list_step = REGISTRY.event_step("ToolFinished", 27).unwrap();
        assert!((list_step.canonical)(&started.to_string()).is_some());
        assert_eq!((list_step.transform)(started.clone()), Some(started));
        let mut run: Value = serde_json::from_str(HOST_RUNSTARTED).unwrap();
        run["config"]["worker_policy"] = serde_json::json!({
            "parent_grant":{"read_paths":["src"],"write_paths":["src"],"tools":[operation],"skills":[],"mcp_servers":[],"commands":[],"permission_mode":null},
            "concurrency":1,"max_turns":4,"max_tools":3,"data_bytes":1024
        });
        let step = REGISTRY.event_step("RunStarted", 22).unwrap();
        assert!((step.canonical)(&run.to_string()).is_none());
        assert!((step.transform)(run).is_none());
    }
}

#[test]
fn setup_identity_step_rejects_routing_in_a_source_version_without_it() {
    let step = REGISTRY.event_step("ProviderSetupEditStarted", 1).unwrap();
    assert_eq!(
        (step.canonical)(ROLE_SETUP_V1).as_deref(),
        Some(ROLE_SETUP_V1)
    );
    let mut forged: Value = serde_json::from_str(ROLE_SETUP_V1).unwrap();
    forged["target"] = serde_json::json!({"kind":"RoleRouting","identity":"roles"});
    assert!((step.canonical)(&forged.to_string()).is_none());
    assert!((step.transform)(forged).is_none());
}

#[test]
fn run_provider_migration_preserves_exact_names_and_canonical_bytes() {
    use crate::provider::{ProviderKind, ProviderRecord};
    let step = REGISTRY.event_step("RunStarted", 23).unwrap();
    for profile in ["openai", "anthropic", "chatgpt", "custom:exact-name"] {
        let record = ProviderRecord::parse(profile).unwrap();
        let mut source: Value = serde_json::from_str(GLOBAL_RUNSTARTED).unwrap();
        let config = &mut source["config"];
        config["provider"] = profile.into();
        match record.kind() {
            ProviderKind::OpenAi | ProviderKind::Anthropic => {
                config["saved_api_account_id"] = serde_json::json!(uuid::Uuid::now_v7());
            }
            ProviderKind::ChatGpt => {
                config["chatgpt_provenance"] = serde_json::json!({
                    "account_id": uuid::Uuid::now_v7(), "evidence_fingerprint": vec![7; 32],
                    "admission": "account_consent"
                });
                config["effort"] = "high".into();
                config["output_token_bound"] = "local_acceptance_only".into();
            }
            ProviderKind::Custom => {
                config["custom_profile_provenance"] = serde_json::json!({
                    "endpoint": "https://example.invalid/v1", "evidence_digest": vec![1; 32],
                    "capability_evidence_version": 2
                });
            }
        }
        let historical = (step.canonical)(&source.to_string()).unwrap();
        assert_eq!((step.canonical)(&historical).unwrap(), historical);
        let upgraded = (step.transform)(source.clone()).unwrap();
        source["config"]["provider"] = serde_json::to_value(&record).unwrap();
        assert_eq!(upgraded, source);
        let event = Event::decode(
            "RunStarted",
            25,
            &upgraded.to_string(),
            Some(RunId::new()),
            None,
        )
        .unwrap();
        migrate_provider_prefix(event.clone());
        let Event::RunStarted { config, .. } = event else {
            unreachable!()
        };
        assert_eq!(config.provider, record);
        assert_eq!(config.provider.profile(), profile);
    }
    let fixture = include_str!("runstarted-v24.json");
    let event = Event::decode("RunStarted", 25, fixture, Some(RunId::new()), None).unwrap();
    assert_eq!(event.payload().unwrap(), fixture);
    for profile in ["", "unknown", "custom:", "custom:Upper", "custom:bad/name"] {
        let source = GLOBAL_RUNSTARTED.replace(
            "\"provider\":\"openai\"",
            &format!("\"provider\":{profile:?}"),
        );
        assert!((step.canonical)(&source).is_none());
        assert!((step.transform)(serde_json::from_str(&source).unwrap()).is_none());
    }
    let duplicate = GLOBAL_RUNSTARTED.replace(
        "\"provider\":\"openai\"",
        "\"provider\":\"openai\",\"provider\":\"anthropic\"",
    );
    assert!((step.canonical)(&duplicate).is_none());
}

fn migrate_provider_prefix(mut started: Event) {
    let temp = tempfile::tempdir().unwrap();
    let root = StateRoot::admit(&temp.path().join("state")).unwrap();
    let fixture = crate::session::worker_protocol_fixture();
    let mut connection = open_connection(&root, false).unwrap();
    let mut hot = None;
    for event in fixture.events {
        if let Event::RunStarted {
            config: original,
            active_deadline_ms: deadline,
            ..
        } = event
        {
            let Event::RunStarted {
                run_id,
                config,
                active_deadline_ms,
            } = &mut started
            else {
                unreachable!()
            };
            *run_id = fixture.run;
            config.workspace_device = original.workspace_device;
            config.workspace_inode = original.workspace_inode;
            *active_deadline_ms = deadline;
            append(&mut connection, &mut hot, fixture.session, started).unwrap();
            break;
        }
        append(&mut connection, &mut hot, fixture.session, event).unwrap();
    }
    let original = stored_rows(&connection);
    downgrade_run_provider(&connection);
    drop(connection);
    assert!(
        root.upgrade_or_archive_history(&REGISTRY)
            .unwrap()
            .is_none()
    );
    let reopened = open_connection(&root, true).unwrap();
    assert_eq!(stored_rows(&reopened), original);
    let (_, view) = resolve_view(&reopened, fixture.session, 0, &mut 0).unwrap();
    assert!(view.unwrap().runs[0].config.is_some());
}

#[test]
fn setup_identity_step_rejects_roster_in_a_source_version_without_it() {
    for version in [1, 2] {
        let step = REGISTRY
            .event_step("ProviderSetupEditStarted", version)
            .unwrap();
        assert_eq!(
            (step.canonical)(ROLE_SETUP_V1).as_deref(),
            Some(ROLE_SETUP_V1)
        );
        let mut forged: Value = serde_json::from_str(ROLE_SETUP_V1).unwrap();
        forged["target"] = serde_json::json!({"kind":"Roster","identity":"roster"});
        assert!((step.canonical)(&forged.to_string()).is_none());
        assert!((step.transform)(forged).is_none());
    }
}

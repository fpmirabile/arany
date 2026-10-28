use super::types::*;
use crate::tools::catalog::call_schemas;
use serde_json::{Value, json};

#[test]
fn typed_tool_schema_and_receipts_have_exact_bounds() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("../../../tests/fixtures/tool-calls.json")).unwrap();
    assert_eq!(rows.len(), 15);
    let schema = jsonschema::options()
        .offline()
        .build(&json!({"anyOf": call_schemas()}))
        .unwrap();
    for row in &rows {
        assert!(schema.is_valid(row));
        let call: ToolCall = serde_json::from_value(row.clone()).unwrap();
        assert!(call.valid());
        let mut unknown = row.clone();
        unknown["network"] = json!(true);
        assert!(serde_json::from_value::<ToolCall>(unknown).is_err());
    }
    // Search options default when omitted or null and serialize omitted, so stored default intents keep their bytes.
    let decoded = |value: Value| serde_json::from_value::<ToolCall>(value).unwrap();
    let historical = json!({"operation":"search","path":"src","query":"fn main"});
    for equivalent in [
        rows[2].clone(),
        json!({"operation":"search","path":"src","query":"fn main","regex":false,"include":null,"case_sensitive":true,"offset":0,"limit":100}),
    ] {
        assert_eq!(decoded(equivalent), decoded(historical.clone()));
    }
    assert_eq!(
        serde_json::to_value(decoded(rows[2].clone())).unwrap(),
        historical
    );
    let custom = json!({"operation":"search","path":"src","query":"fn \\w+","regex":true,"include":"**/*.rs","case_sensitive":false,"offset":5,"limit":7});
    assert_eq!(
        serde_json::to_value(decoded(custom.clone())).unwrap(),
        custom
    );
    for keys in [vec!["offset"], vec!["limit"], vec!["offset", "limit"]] {
        let mut missing = rows[1].clone();
        for key in keys {
            missing.as_object_mut().unwrap().remove(key);
        }
        assert!(!schema.is_valid(&missing));
        let call = decoded(missing);
        assert!(call.valid());
        assert_eq!(serde_json::to_value(call).unwrap(), rows[1]);
    }
    for (index, key) in [(1, "path"), (4, "expected_digest"), (7, "resource")] {
        let mut missing = rows[index].clone();
        missing.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<ToolCall>(missing).is_err(),
            "fields without defaults are required"
        );
    }
    for call in [
        ToolCall::Read {
            path: "src/main.rs".into(),
            offset: None,
            limit: Some(2001),
        },
        ToolCall::Search {
            path: "src".into(),
            query: "x".repeat(513),
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: 100,
        },
        ToolCall::Write {
            path: "src/new".into(),
            expected_digest: None,
            content: "x".repeat(MAX_EDIT_CONTENT_BYTES + 1),
        },
        ToolCall::Write {
            path: "src/new".into(),
            expected_digest: None,
            content: format!("{}x", "é".repeat(MAX_EDIT_CONTENT_BYTES / 2)),
        },
        ToolCall::Edit {
            path: "src/new".into(),
            expected_digest: "0".repeat(64),
            old: "x".repeat(MAX_EDIT_CONTENT_BYTES / 2),
            new: "y".repeat(MAX_EDIT_CONTENT_BYTES / 2 + 1),
        },
        ToolCall::Command {
            program: "bash".into(),
            args: vec!["x".into(); 65],
            cwd: "".into(),
        },
        // Within its field limits, but over the serialized bound every non-content operation keeps.
        ToolCall::Command {
            program: "bash".into(),
            args: vec!["x".repeat(300); 64],
            cwd: "".into(),
        },
        ToolCall::Command {
            program: "bash".into(),
            args: vec!["nul\0argument".into()],
            cwd: "".into(),
        },
    ] {
        assert!(!call.valid());
    }
    // Bytes, not characters, and six-byte escapes plus a long escaped path still fit.
    let escaped_path = "\"".repeat(512);
    for call in [
        ToolCall::Write {
            path: "src/new".into(),
            expected_digest: None,
            content: "é".repeat(MAX_EDIT_CONTENT_BYTES / 2),
        },
        ToolCall::Write {
            path: escaped_path.clone(),
            expected_digest: Some("0".repeat(64)),
            content: "\u{1}".repeat(MAX_EDIT_CONTENT_BYTES),
        },
        ToolCall::Edit {
            path: escaped_path.clone(),
            expected_digest: "0".repeat(64),
            old: "\u{1}".repeat(MAX_EDIT_CONTENT_BYTES / 2),
            new: "\u{1}".repeat(MAX_EDIT_CONTENT_BYTES / 2),
        },
        ToolCall::MultiEdit {
            file_path: escaped_path,
            expected_digest: "0".repeat(64),
            edits: vec![LiteralEdit {
                old_string: "\u{1}".repeat(MAX_EDIT_CONTENT_BYTES / 2),
                new_string: "\u{2}".repeat(MAX_EDIT_CONTENT_BYTES / 2),
                replace_all: false,
            }],
        },
    ] {
        assert!(call.valid());
        let bytes = serde_json::to_vec(&call).unwrap().len();
        assert!(bytes > MAX_ORDINARY_TOOL_ARGUMENT_BYTES && bytes <= MAX_TOOL_ARGUMENT_BYTES);
    }
    let intent = EffectIntent {
        host: None,
        rewrite: Vec::new(),
        host_command: None,
        target: crate::session::worker::EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id: crate::session::RunId::new(),
        agent_run_id: crate::session::AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: ToolCall::GitStatus { path: "src".into() },
        limits: ToolLimits::default(),
        use_count: 1,
    };
    assert!(intent.valid());
    let mut limits = serde_json::to_value(&intent.limits).unwrap();
    let mut partial = limits.clone();
    partial.as_object_mut().unwrap().remove("cache_transfer_ms");
    assert!(serde_json::from_value::<ToolLimits>(partial).is_err());
    let mut native = intent.clone();
    native.limits.resource_profile = ToolResourceProfile::LinuxKernel;
    let linux_intent = native.digest();
    native.limits.resource_profile = ToolResourceProfile::MacosSupervised;
    for profile in [
        ToolResourceProfile::LinuxKernel,
        ToolResourceProfile::MacosSupervised,
    ] {
        limits["resource_profile"] = serde_json::to_value(profile).unwrap();
        let decoded: ToolLimits = serde_json::from_value(limits.clone()).unwrap();
        assert_eq!(decoded.resource_profile, profile);
        assert_eq!(serde_json::to_value(&decoded).unwrap(), limits);
    }
    assert!(native.valid());
    assert_ne!(
        native.digest(),
        linux_intent,
        "resource semantics bind the intent"
    );
    assert_eq!(
        serde_json::from_value::<EffectIntent>(serde_json::to_value(&native).unwrap()).unwrap(),
        native
    );
    let guard = GuardReceipt {
        contract_version: 1,
        intent_digest: intent.digest(),
        enforcement_digest: intent.enforcement_digest,
        limits: intent.limits.clone(),
    };
    let observation = ToolObservation {
        spill: None,
        enforcement_cause: None,
        limit_cause: None,
        intent: intent.clone(),
        disposition: ToolDisposition::Succeeded,
        output: "observed".into(),
        attestation: Some(Attestation::Guard(guard)),
        approval_cause: None,
        guard_stage: None,
        cache_update: crate::tools::CacheUpdate::NotApplicable,
    };
    assert!(observation.valid());
    for jobs in [0, 1, 2, intent.limits.max_processes + 1] {
        let mut build = intent.clone();
        build.call = ToolCall::Command {
            program: "cargo".into(),
            args: vec!["build".into()],
            cwd: String::new(),
        };
        build.limits.resource_class = ToolResourceClass::Build;
        build.limits.cache_bytes = 0;
        build.limits.cache_candidate_bytes = 0;
        let historical = build.digest();
        build.limits.build_jobs = jobs;
        assert_eq!(build.digest() == historical, jobs == 1);
        assert_eq!(
            build.valid(),
            (1..=build.limits.max_processes).contains(&jobs)
        );
        let decoded: EffectIntent =
            serde_json::from_slice(&serde_json::to_vec(&build).unwrap()).unwrap();
        assert_eq!(decoded, build);
        let mut receipt = GuardReceipt {
            contract_version: 1,
            intent_digest: build.digest(),
            enforcement_digest: build.enforcement_digest,
            limits: build.limits.clone(),
        };
        assert!(receipt.valid_for(&build));
        receipt.limits.build_jobs += 1;
        assert!(!receipt.valid_for(&build));
    }
    for cores in [0, 1, 8, u32::MAX] {
        let mut build = intent.clone();
        build.call = ToolCall::Command {
            program: "cargo".into(),
            args: vec!["build".into()],
            cwd: String::new(),
        };
        build.limits.resource_class = ToolResourceClass::Build;
        build.limits.cache_bytes = 0;
        build.limits.cache_candidate_bytes = 0;
        let historical = build.digest();
        build.limits.build_cpu_cores = cores;
        assert_eq!(build.digest() == historical, cores == 1);
        assert_eq!(build.valid(), cores > 0);
        let decoded: EffectIntent =
            serde_json::from_slice(&serde_json::to_vec(&build).unwrap()).unwrap();
        assert_eq!(decoded, build);
        let receipt = GuardReceipt {
            contract_version: 1,
            intent_digest: build.digest(),
            enforcement_digest: build.enforcement_digest,
            limits: build.limits.clone(),
        };
        assert!(receipt.valid_for(&build));
        build.limits.build_cpu_cores = if cores == 1 { 2 } else { 1 };
        assert!(!receipt.valid_for(&build));
    }
    let mut invalid_default = intent.clone();
    invalid_default.limits.build_cpu_cores = 2;
    assert!(!invalid_default.valid());
    let mut profile_mismatch = observation.clone();
    let Some(Attestation::Guard(receipt)) = &mut profile_mismatch.attestation else {
        unreachable!()
    };
    receipt.limits.resource_profile =
        if intent.limits.resource_profile == ToolResourceProfile::MacosSupervised {
            ToolResourceProfile::LinuxKernel
        } else {
            ToolResourceProfile::MacosSupervised
        };
    assert!(
        !profile_mismatch.valid(),
        "receipt cannot relabel resource guarantees"
    );
    let mut missing = observation.clone();
    missing.attestation = None;
    assert!(!missing.valid());
    let mut mismatched = observation.clone();
    let Some(Attestation::Guard(receipt)) = &mut mismatched.attestation else {
        unreachable!()
    };
    receipt.limits.max_processes += 1;
    assert!(!mismatched.valid());
    let mut command_intent = observation.intent.clone();
    command_intent.call = ToolCall::Command {
        program: "cargo".into(),
        args: vec!["build".into()],
        cwd: ".".into(),
    };
    let command_guard = GuardReceipt {
        contract_version: 1,
        intent_digest: command_intent.digest(),
        enforcement_digest: command_intent.enforcement_digest,
        limits: command_intent.limits.clone(),
    };
    let uncertain_cache = ToolObservation {
        spill: None,
        enforcement_cause: None,
        limit_cause: None,
        intent: command_intent,
        disposition: ToolDisposition::Uncertain,
        output: r#"{"selection":{"elapsed_ms":null,"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code":0,"stdout":"build complete","stderr":"","changeset":{"files":[]},"stdout_total":{"bytes":14,"lines":1},"stderr_total":{"bytes":0,"lines":0}}"#
            .into(),
        attestation: Some(Attestation::Guard(command_guard)),
        approval_cause: None,
        guard_stage: None,
        cache_update: CacheUpdate::Uncertain,
    };
    assert!(uncertain_cache.valid());
    assert!(uncertain_cache.stops_run());
    let mut worker_observation = uncertain_cache.clone();
    worker_observation.disposition = ToolDisposition::Succeeded;
    worker_observation.cache_update = CacheUpdate::NotAttempted;
    assert!(worker_observation.valid());
    assert!(!worker_observation.stops_run());
    worker_observation.cache_update = CacheUpdate::Failed;
    assert!(worker_observation.valid());
    let mut encoded = serde_json::to_value(&worker_observation).unwrap();
    encoded.as_object_mut().unwrap().remove("cache_update");
    assert!(serde_json::from_value::<ToolObservation>(encoded).is_err());
    let mut succeeded_uncertain_cache = uncertain_cache.clone();
    succeeded_uncertain_cache.disposition = ToolDisposition::Succeeded;
    assert!(succeeded_uncertain_cache.valid());
    assert!(succeeded_uncertain_cache.stops_run());
    assert_eq!(
        succeeded_uncertain_cache.disposition,
        ToolDisposition::Succeeded
    );
    let mut unattempted_error = uncertain_cache.clone();
    unattempted_error.disposition = ToolDisposition::Uncertain;
    unattempted_error.attestation = None;
    unattempted_error.cache_update = CacheUpdate::NotAttempted;
    assert!(unattempted_error.valid());
    assert!(unattempted_error.stops_run());
    let mut widened = intent.clone();
    widened.limits.runtime_ms = MAX_COMMAND_TIMEOUT_MS + 1;
    assert!(!widened.valid());
    let mut reusable = intent;
    reusable.use_count = 2;
    assert!(!reusable.valid());
    let errors = vec![
        super::ToolError::MissingConfiguration,
        super::ToolError::Configuration,
        super::ToolError::ProtectionUnavailable,
        super::ToolError::ChangedInput,
        super::ToolError::Path,
        super::ToolError::FilesystemDenied,
        super::ToolError::Limit,
        super::ToolError::Conflict,
        super::ToolError::Operation,
        super::ToolError::Uncertain,
        super::ToolError::GuardRejected(super::guard::GuardStage::ResourceAttestation),
        super::ToolError::GuardRejected(super::guard::GuardStage::ProcessCleanup),
        super::ToolError::GuardRejected(super::guard::GuardStage::LauncherReap),
        super::ToolError::GuardRejected(super::guard::GuardStage::OutputEof),
        super::ToolError::GuardRejected(super::guard::GuardStage::NamespaceBootstrap),
        super::ToolError::GuardRejected(super::guard::GuardStage::NativeManager),
        super::ToolError::GuardRejected(super::guard::GuardStage::ControlProtocol),
    ];
    for (stage, label) in [
        ("admission", "admission"),
        ("workspace", "pinned input"),
        ("capabilities", "native protection"),
        ("handshake", "handshake"),
        ("payload", "execution"),
        ("receipt", "receipt"),
        ("process_stop", "process cleanup"),
        ("launcher_reap", "launcher reap"),
        ("stderr_drain", "output EOF"),
        ("storage_cleanup", "storage cleanup"),
    ] {
        let error = super::ToolError::GuardBootstrap(
            serde_json::from_value(serde_json::json!({"stage": stage})).unwrap(),
        );
        let mut reserved = observation.intent.clone();
        reserved.limits.result_bytes = 128;
        let receipt =
            super::uncertain_observation(reserved, error, crate::tools::CacheUpdate::NotApplicable);
        assert_eq!(
            receipt.output,
            format!(
                "Guard {label} unconfirmed; effects may have occurred. Do not retry automatically."
            )
        );
        assert!(receipt.valid());
    }
    for (stage, label) in [
        (
            super::guard::GuardStage::ConnectionDeadline,
            "connection deadline",
        ),
        (
            super::guard::GuardStage::PreparationDeadline,
            "preparation deadline",
        ),
    ] {
        let mut reserved = observation.intent.clone();
        reserved.limits.result_bytes = 128;
        let failed =
            super::unstarted_observation(reserved.clone(), super::ToolError::GuardRejected(stage));
        assert_eq!(failed.disposition, ToolDisposition::Failed);
        assert!(failed.attestation.is_none());
        assert!(failed.valid() && failed.stops_run());
        assert_eq!(
            failed.output,
            format!("Guard {label} unavailable before dispatch; no operation was dispatched.")
        );
        let uncertain = super::uncertain_observation(
            reserved,
            super::ToolError::GuardRejected(stage),
            crate::tools::CacheUpdate::NotApplicable,
        );
        assert_eq!(uncertain.disposition, ToolDisposition::Uncertain);
        assert!(uncertain.attestation.is_none());
        assert!(uncertain.valid() && uncertain.stops_run());
        assert_eq!(
            uncertain.output,
            format!(
                "Guard {label} unconfirmed; effects may have occurred. Do not retry automatically."
            )
        );
    }
    for error in errors {
        let mut reserved = observation.intent.clone();
        reserved.limits.result_bytes = 128;
        let uncertain =
            super::uncertain_observation(reserved, error, crate::tools::CacheUpdate::NotApplicable);
        assert_eq!(uncertain.disposition, ToolDisposition::Uncertain);
        assert!(uncertain.attestation.is_none());
        assert!(uncertain.valid(), "minimum-budget uncertainty receipt");
        assert!(uncertain.output.ends_with("Do not retry automatically."));
    }
    let mut oversized = observation;
    oversized.output = "x".repeat(MAX_TOOL_RESULT_BYTES + 1);
    assert!(!oversized.valid());
}

#[test]
fn host_pin_requires_explicit_nullable_permissions_digest() {
    let pin = super::types::test_host_pin();
    let mut value = serde_json::to_value(&pin).unwrap();
    assert!(value["permissions_digest"].is_null());
    assert_eq!(
        serde_json::from_value::<super::types::HostCommandPin>(value.clone()).unwrap(),
        pin
    );
    value.as_object_mut().unwrap().remove("permissions_digest");
    assert!(serde_json::from_value::<super::types::HostCommandPin>(value).is_err());
}

#[test]
fn guard_stage_is_valid_only_on_unattested_guard_failures() {
    let fact = GuardStageFact {
        stage: FailureStage::SelectionTimedOut,
        check: None,
    };
    let observation = |call: ToolCall, disposition| {
        let intent = EffectIntent {
            host: None,
            rewrite: Vec::new(),
            host_command: None,
            target: crate::session::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: crate::session::RunId::new(),
            agent_run_id: crate::session::AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call,
            limits: ToolLimits::default(),
            use_count: 1,
        };
        ToolObservation {
            spill: None,
            intent,
            disposition,
            enforcement_cause: None,
            limit_cause: None,
            output: "x".into(),
            attestation: None,
            approval_cause: None,
            cache_update: CacheUpdate::NotApplicable,
            guard_stage: Some(fact),
        }
    };
    let command = ToolCall::Command {
        program: "sh".into(),
        args: vec![],
        cwd: String::new(),
    };
    assert!(observation(command.clone(), ToolDisposition::Failed).valid());
    assert!(!observation(command, ToolDisposition::Succeeded).valid());
    let read = ToolCall::Read {
        path: "a".into(),
        offset: None,
        limit: None,
    };
    assert!(!observation(read, ToolDisposition::Failed).valid());
}

use super::config::Config;
use super::skills::parse_json;
use super::types::*;
use serde_json::{Value, json};

fn config_value() -> Value {
    json!({"version":1,"workspace_paths":["src","Cargo.toml"],"write":false,"commands":[],"skills":[],"mcp":[]})
}

#[test]
fn tool_admission_corpus_is_closed_and_restrict_only() {
    let value = config_value();
    let config: Config = serde_json::from_value(value.clone()).unwrap();
    config.validate().unwrap();
    assert!(config.receipt().valid());
    for roots in [
        json!([]),
        json!(["src", "src/child"]),
        json!(["src", "src"]),
        json!(["."]),
        json!(["../outside"]),
        json!(["src/.env"]),
        json!(["/absolute"]),
    ] {
        let mut row = value.clone();
        row["workspace_paths"] = roots;
        assert!(
            serde_json::from_value::<Config>(row)
                .unwrap()
                .validate()
                .is_err()
        );
    }
    let duplicate = br#"{"version":1,"version":1,"workspace_paths":["src"],"write":false,"commands":[],"skills":[],"mcp":[]}"#;
    assert!(parse_json(duplicate, 64 * 1024).is_err());
    let mut unknown = value.clone();
    unknown["network"] = json!(true);
    assert!(serde_json::from_value::<Config>(unknown).is_err());
    for (path, expected) in [
        ("src/main.rs", true),
        ("src2/main.rs", false),
        ("src/../omitted", false),
        ("src/.git/config", false),
        ("src/.env.local", false),
        ("src/line\nbreak", false),
    ] {
        assert_eq!(
            config.allows(&ToolCall::Read {
                path: path.into(),
                offset: 0,
                limit: 1
            }),
            expected,
            "path case"
        );
    }
    assert!(!config.allows(&ToolCall::Write {
        path: "src/new".into(),
        expected_digest: None,
        content: "new".into()
    }));
    assert!(!config.allows(&ToolCall::Command {
        program: "unconfigured".into(),
        args: Vec::new(),
        cwd: "".into()
    }));
    assert!(!config.allows(&ToolCall::Skill {
        name: "unconfigured".into(),
        resource: None
    }));
    assert!(!config.allows(&ToolCall::McpList {
        server: "unconfigured".into()
    }));
    let mut duplicate_names = value;
    duplicate_names["mcp"] = json!([{"name":"example","program":{"name":"python","executable":"/usr/bin/python","sha256":"0".repeat(64),"interpreter":true,"inputs":[]},"args":[],"tools":["echo","echo"]}]);
    assert!(
        serde_json::from_value::<Config>(duplicate_names)
            .unwrap()
            .validate()
            .is_err()
    );
}

#[test]
fn typed_tool_schema_and_receipts_have_exact_bounds() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("../../tests/fixtures/tool-outcomes.json")).unwrap();
    assert_eq!(rows.len(), 10);
    let branch = outcome_branch();
    let schema = jsonschema::options().offline().build(&branch).unwrap();
    for row in &rows {
        assert!(schema.is_valid(&row["outcome"]));
        let call: ToolCall = serde_json::from_value(row["outcome"]["call"].clone()).unwrap();
        assert!(call.valid());
        let mut unknown = row["outcome"]["call"].clone();
        unknown["network"] = json!(true);
        assert!(serde_json::from_value::<ToolCall>(unknown).is_err());
    }
    for (index, key) in [(4, "expected_digest"), (7, "resource")] {
        let mut missing = rows[index]["outcome"]["call"].clone();
        missing.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<ToolCall>(missing).is_err(),
            "nullable fields are still required"
        );
    }
    for call in [
        ToolCall::Read {
            path: "src/main.rs".into(),
            offset: 0,
            limit: 4097,
        },
        ToolCall::Search {
            path: "src".into(),
            query: "x".repeat(513),
        },
        ToolCall::Write {
            path: "src/new".into(),
            expected_digest: None,
            content: "x".repeat(8193),
        },
        ToolCall::Command {
            program: "bash".into(),
            args: vec!["x".into(); 65],
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
    let intent = EffectIntent {
        id: uuid::Uuid::now_v7(),
        run_id: crate::session::RunId::new(),
        agent_run_id: crate::session::AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: ToolCall::List { path: "src".into() },
        limits: ToolLimits::default(),
        expires_at_ms: 1,
        use_count: 1,
    };
    assert!(intent.valid());
    let guard = GuardReceipt {
        contract_version: 1,
        intent_digest: intent.digest(),
        enforcement_digest: intent.enforcement_digest,
        limits: intent.limits.clone(),
    };
    let observation = ToolObservation {
        intent: intent.clone(),
        disposition: ToolDisposition::Succeeded,
        output: "observed".into(),
        guard: Some(guard),
    };
    assert!(observation.valid());
    let mut missing = observation.clone();
    missing.guard = None;
    assert!(!missing.valid());
    let mut mismatched = observation.clone();
    mismatched.guard.as_mut().unwrap().limits.max_processes += 1;
    assert!(!mismatched.valid());
    let mut widened = intent.clone();
    widened.limits.runtime_ms += 1;
    assert!(!widened.valid());
    let mut reusable = intent;
    reusable.use_count = 2;
    assert!(!reusable.valid());
    for error in [
        super::ToolError::MissingConfiguration,
        super::ToolError::Configuration,
        super::ToolError::ProtectionUnavailable,
        super::ToolError::ChangedInput,
        super::ToolError::Path,
        super::ToolError::Limit,
        super::ToolError::Conflict,
        super::ToolError::Operation,
        super::ToolError::Uncertain,
        super::ToolError::GuardRejected("admission"),
        super::ToolError::GuardRejected("Workspace identity"),
        super::ToolError::GuardRejected("kernel capability"),
        super::ToolError::GuardRejected("handshake"),
        super::ToolError::GuardRejected("payload"),
        super::ToolError::GuardRejected("receipt"),
        super::ToolError::GuardRejected("resource attestation"),
        super::ToolError::GuardRejected("process-unit cleanup"),
        super::ToolError::GuardRejected("launcher reap"),
        super::ToolError::GuardRejected("stderr EOF"),
        super::ToolError::GuardRejected("namespace bootstrap"),
        super::ToolError::GuardRejected("native manager"),
        super::ToolError::GuardRejected("bounded control protocol"),
        super::ToolError::GuardRejected("arbitrary text is not a durable diagnostic"),
    ] {
        let mut reserved = observation.intent.clone();
        reserved.limits.result_bytes = 128;
        let uncertain = super::uncertain_observation(reserved, error);
        assert_eq!(uncertain.disposition, ToolDisposition::Uncertain);
        assert!(uncertain.guard.is_none());
        assert!(uncertain.valid(), "minimum-budget uncertainty receipt");
        assert!(uncertain.output.ends_with("Do not retry automatically."));
        assert!(!uncertain.output.contains("arbitrary text"));
    }
    let mut oversized = observation;
    oversized.output = "x".repeat(MAX_TOOL_RESULT_BYTES + 1);
    assert!(!oversized.valid());
}

use super::{methods::*, rpc::*, *};
use serde_json::{Value, json};

#[test]
fn supervisor_event_projects_canonical_jsonl_and_validates_against_exported_schema() {
    let actor = arany::AgentRunId::new();
    let verdict = crate::presentation::supervisor_fixture(actor, 1).verdicts()[0]
        .1
        .clone();
    let run = arany::RunId::new();
    let session = arany::SessionId::new();
    let event = arany::Event::SupervisorVerdictRecorded {
        run_id: run,
        agent_run_id: actor,
        verdict,
    };
    let envelope = arany::EventEnvelope {
        sequence: 2,
        session_id: session,
        run_id: Some(run),
        agent_run_id: Some(actor),
        event,
        created_at_ms: 1,
    };
    let line = serde_json::to_value(EventLine::from(&envelope)).unwrap();
    assert_eq!(line["kind"], "SupervisorVerdictRecorded");
    assert_eq!(line["event_version"], envelope.event.version());
    assert_eq!(
        line["payload"],
        serde_json::from_str::<Value>(&envelope.event.payload().unwrap()).unwrap()
    );
    let schema = protocol_schema();
    validate(
        &schema,
        &schema["notifications"]["events/committed"]["params"],
        &json!({"threadId": session.to_string(), "event": line}),
    );
    validate(
        &schema,
        &schema["methods"]["events/subscribe"]["result"],
        &json!({"events": [line], "nextSince": null}),
    );
}

#[test]
fn child_steering_projects_as_an_actor_scoped_committed_event() {
    let (session, run, actor, goal) = (
        arany::SessionId::new(),
        arany::RunId::new(),
        arany::AgentRunId::new(),
        arany::GoalId::new(),
    );
    let envelope = arany::EventEnvelope {
        session_id: session,
        run_id: Some(run),
        agent_run_id: Some(actor),
        sequence: 7,
        created_at_ms: 0,
        event: arany::Event::AgentSteeringNoteRecorded {
            goal_id: goal,
            run_id: run,
            agent_run_id: actor,
            text: "note".into(),
        },
    };
    let line = serde_json::to_value(EventLine::from(&envelope)).unwrap();
    assert_eq!(line["kind"], "AgentSteeringNoteRecorded");
    assert_eq!(line["event_version"], envelope.event.version());
    assert_eq!(line["payload"], json!({"goal_id":goal,"text":"note"}));
    let schema = protocol_schema();
    validate(
        &schema,
        &schema["notifications"]["events/committed"]["params"],
        &json!({"threadId":session,"event":line}),
    );
}

#[test]
fn schema_snapshot_is_byte_stable() {
    let actual = format!(
        "{}\n",
        serde_json::to_string_pretty(&protocol_schema()).unwrap()
    );
    assert_eq!(
        actual,
        include_str!("../../tests/fixtures/serve/schema.json")
    );
}

#[test]
fn rewind_conformance_covers_closed_states_receipts_and_selection_bounds() {
    let schema = protocol_schema();
    let thread = arany::SessionId::new();
    let input = json!({"threadId":thread,"previewToken":"preview","restore":"code"});
    for (field, value) in [
        ("kept", json!(["a", "b"])),
        ("overwrite", json!(["a", "b"])),
    ] {
        let mut row = input.clone();
        row[field] = value;
        validate(&schema, &schema["methods"]["thread/rewind"]["params"], &row);
        assert!(serde_json::from_value::<ThreadRewindParams>(row).is_ok());
    }
    for paths in [vec!["a"; 65], vec!["b", "a"], vec!["a", "a"], vec![""]] {
        for field in ["kept", "overwrite"] {
            let mut row = input.clone();
            row[field] = json!(paths);
            assert!(serde_json::from_value::<ThreadRewindParams>(row).is_err());
        }
    }
    for field in ["kept", "overwrite"] {
        for (path, valid) in [
            ("x".repeat(512), true),
            ("x".repeat(513), false),
            ("é".repeat(256), true),
            ("é".repeat(257), false),
            ("x".repeat(4097), false),
        ] {
            let mut row = input.clone();
            row[field] = json!([path]);
            assert_eq!(
                serde_json::from_value::<ThreadRewindParams>(row).is_ok(),
                valid
            );
        }
    }
    for outcome in [
        "restored",
        "conflict",
        "denied",
        "failed",
        "cancelled",
        "uncertain",
    ] {
        let row = json!({"rewind":{"id":null,"outcome":outcome,"conflicts":["src/\u{1b}]52;bad\u{7}"],"conflictCount":1},"notices":[]});
        validate(&schema, &schema["methods"]["thread/rewind"]["result"], &row);
        let result: ThreadRewindResult = serde_json::from_value(row.clone()).unwrap();
        assert_eq!(serde_json::to_value(result).unwrap(), row);
    }
    assert!(CAPABILITIES.contains(&"thread_rewind_v2"));
}

#[test]
fn codec_round_trips_requests_notifications_responses_and_errors() {
    for value in [
        json!({"id": "client-1", "method": "initialize", "params": {}}),
        json!({"id": i64::MIN, "method": "thread/start"}),
        json!({"method": "initialized", "params": {}}),
        json!({"id": 42, "result": {"text": "line\nnext\u{001b}[31m"}}),
        json!({"id": "client-1", "result": null}),
        json!({"id": null, "error": {"code": -32700, "message": "Parse error"}}),
        json!({"id": 2, "error": {"code": -32600, "message": "Invalid request", "data": {"reason": "messageTooLarge"}}}),
    ] {
        for version in [false, true] {
            let mut value = value.clone();
            if version {
                value["jsonrpc"] = json!("2.0");
            }
            let decoded = decode_line(&serde_json::to_vec(&value).unwrap()).unwrap();
            let line = encode_line(&decoded).unwrap();
            assert_eq!(line.last(), Some(&b'\n'));
            assert_eq!(line.iter().filter(|byte| **byte == b'\n').count(), 1);
            value["jsonrpc"] = json!("2.0");
            assert_eq!(serde_json::from_slice::<Value>(&line).unwrap(), value);
            assert_eq!(decode_line(&line).unwrap(), decoded);
        }
    }
    for line in [
        "null",
        "[]",
        "1",
        "true",
        "{}",
        "{",
        "{\"method\":\"x\",\"jsonrpc\":\"1.0\"}",
        "{\"method\":\"x\",\"jsonrpc\":null}",
        "{\"method\":\"x\",\"id\":null}",
        "{\"method\":\"x\",\"id\":1.5}",
        "{\"method\":\"x\",\"id\":18446744073709551615}",
        "{\"method\":\"x\",\"result\":{}}",
        "{\"id\":1}",
        "{\"error\":{\"code\":-32700,\"message\":\"bad\"}}",
        "{\"method\":\"x\",\n\"id\":1}",
        "{}\n{}",
    ] {
        assert_eq!(
            decode_line(line.as_bytes()),
            Err(CodecError::Parse),
            "{line:?}"
        );
    }
    assert_eq!(decode_line(&[0xff]), Err(CodecError::Parse));
    assert_eq!(
        decode_line(&vec![b' '; MAX_RPC_LINE_BYTES + 1]),
        Err(CodecError::TooLarge)
    );
    let mut request = Request {
        jsonrpc: Version::V2,
        id: RequestId::Integer(1),
        method: "x".into(),
        params: Some(json!("")),
    };
    let overhead = encode_line(&request).unwrap().len();
    request.params = Some(json!("x".repeat(MAX_RPC_LINE_BYTES - overhead)));
    let line = encode_line(&request).unwrap();
    assert_eq!(line.len(), MAX_RPC_LINE_BYTES);
    assert!(decode_line(&line).is_ok());
    request.params = Some(json!("x".repeat(MAX_RPC_LINE_BYTES - overhead + 1)));
    assert_eq!(encode_line(&request), Err(CodecError::TooLarge));
}

#[test]
fn goal_stop_failure_schema_lists_every_serialized_run_failure() {
    let schema = protocol_schema();
    let failed = schema["$defs"]["GoalStop"]["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .find(|variant| variant["properties"]["reason"]["const"] == "failed")
        .unwrap();
    let listed = failed["properties"]["failure"]["enum"].as_array().unwrap();
    let failures = [
        arany::RunFailure::ProviderCall,
        arany::RunFailure::ProposalRefused,
        arany::RunFailure::BudgetExhausted,
        arany::RunFailure::ToolStopped,
        arany::RunFailure::DefinitionInvalid,
        arany::RunFailure::PlanAdmission,
        arany::RunFailure::ToolContextFull,
        arany::RunFailure::WorkersUnverified,
        arany::RunFailure::CriterionUnmet,
        arany::RunFailure::RefusalLimit,
    ];
    assert_eq!(listed.len(), failures.len());
    for failure in failures {
        assert!(
            listed.contains(&serde_json::to_value(failure).unwrap()),
            "{failure:?}"
        );
    }
    let goal = GoalSummary {
        status: GoalStatus::Failed,
        stopped: Some(GoalStop::GoalExhausted {
            limit: GoalLimit::Calls,
        }),
        budget_refusal: Some(arany::BudgetRefusal {
            dimension: arany::GoalLimit::Calls,
            cause: arany::BudgetRefusalCause::Exhausted,
            action: arany::BudgetAction::Tool,
            run_id: Some(arany::RunId::new()),
            agent_run_id: Some(arany::AgentRunId::new()),
            used: 3,
            limit: 3,
            requested: Some(1),
            required_minimum: Some(1),
            token_usage_unknown: false,
            charge_basis: arany::BudgetChargeBasis::Observed,
        }),
    };
    let value = serde_json::to_value(&goal).unwrap();
    validate(&schema, &json!({"$ref":"#/$defs/GoalSummary"}), &value);
    assert_eq!(
        serde_json::from_value::<GoalSummary>(value.clone()).unwrap(),
        goal
    );
    for (field, variants) in [
        (
            "dimension",
            serde_json::to_value([
                arany::GoalLimit::Calls,
                arany::GoalLimit::Tokens,
                arany::GoalLimit::ActiveTime,
                arany::GoalLimit::Runs,
            ])
            .unwrap(),
        ),
        (
            "cause",
            serde_json::to_value([
                arany::BudgetRefusalCause::Exhausted,
                arany::BudgetRefusalCause::UnknownUsage,
                arany::BudgetRefusalCause::InsufficientHeadroom,
            ])
            .unwrap(),
        ),
        (
            "action",
            serde_json::to_value([
                arany::BudgetAction::Provider,
                arany::BudgetAction::Tool,
                arany::BudgetAction::Delegation,
                arany::BudgetAction::Compaction,
                arany::BudgetAction::Run,
                arany::BudgetAction::Resume,
            ])
            .unwrap(),
        ),
        (
            "charge_basis",
            serde_json::to_value([
                arany::BudgetChargeBasis::Observed,
                arany::BudgetChargeBasis::Conservative,
            ])
            .unwrap(),
        ),
    ] {
        for variant in variants.as_array().unwrap() {
            let mut encoded = value.clone();
            encoded["budgetRefusal"][field] = variant.clone();
            validate(&schema, &json!({"$ref":"#/$defs/GoalSummary"}), &encoded);
            let decoded: GoalSummary = serde_json::from_value(encoded.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), encoded);
        }
    }
    for field in ["dimension", "cause", "action", "charge_basis"] {
        let mut unknown = value.clone();
        unknown["budgetRefusal"][field] = json!("unknown");
        assert!(serde_json::from_value::<GoalSummary>(unknown).is_err());
    }
}

#[test]
fn every_conformance_message_validates_against_the_schema() {
    let schema = protocol_schema();
    let validator = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .offline()
        .build(&schema)
        .unwrap();
    let thread_id = "019a0011-2233-7444-8555-667788990000";
    let run_id = "019a0011-2233-7444-8555-667788990001";
    let goal_id = "019a0011-2233-7444-8555-667788990002";
    let thread =
        json!({"id": thread_id, "title": "Example", "workspace": "/workspace", "createdAtMs": 1});
    let turn = json!({"id": run_id, "goalId": goal_id, "status": "inProgress", "items": []});
    for reason in [
        "actionBudgetExhausted",
        "tokenBudgetExhausted",
        "futureBudget",
    ] {
        let mut completed = turn.clone();
        completed["status"] = json!("failed");
        completed["stopReason"] = json!(reason);
        let valid = reason != "futureBudget";
        assert_eq!(
            serde_json::from_value::<Turn>(completed.clone()).is_ok(),
            valid
        );
        let mut part = schema.clone();
        part["$ref"] = json!("#/$defs/Turn");
        assert_eq!(
            jsonschema::validator_for(&part)
                .unwrap()
                .is_valid(&completed),
            valid
        );
    }
    for (stopped, valid) in [
        (json!({"reason": "goalExhausted", "limit": "calls"}), true),
        (json!({"reason": "goalExhausted", "limit": "runs"}), true),
        (
            json!({"reason": "goalExhausted", "limit": "activeTime"}),
            true,
        ),
        (json!({"reason": "goalExhausted", "limit": "tokens"}), true),
        (json!({"reason": "paused"}), true),
        (json!({"reason": "cancelled"}), true),
        (json!({"reason": "stoppedByHook"}), true),
        (json!({"reason": "uncertain"}), true),
        (json!({"reason": "interrupted"}), true),
        (
            json!({"reason": "notAdmitted", "refusal": "workspaceUnavailable"}),
            true,
        ),
        (json!({"reason": "notAdmitted"}), false),
        (
            json!({"reason": "notAdmitted", "refusal": "Workspace input is unavailable"}),
            false,
        ),
        (
            json!({"reason": "failed", "failure": "provider_call"}),
            true,
        ),
        (
            json!({"reason": "failed", "failure": "refusal_limit"}),
            true,
        ),
        (json!("Goal budget is exhausted"), false),
        (json!({"reason": "goalExhausted"}), false),
        (
            json!({"reason": "goalExhausted", "limit": "wallClock"}),
            false,
        ),
        (
            json!({"reason": "failed", "failure": "providerCall"}),
            false,
        ),
        (json!({"reason": "futureStop"}), false),
    ] {
        let mut completed = turn.clone();
        completed["status"] = json!("completed");
        completed["goal"] = json!({"status": "failed", "stopped": stopped});
        assert_eq!(
            serde_json::from_value::<Turn>(completed.clone()).is_ok(),
            valid,
            "{stopped}"
        );
        let mut part = schema.clone();
        part["$ref"] = json!("#/$defs/Turn");
        assert_eq!(
            jsonschema::validator_for(&part)
                .unwrap()
                .is_valid(&completed),
            valid,
            "{stopped}"
        );
    }
    for (index, (name, params, result)) in [
        ("initialize", json!({"clientInfo": {"name": "fixture", "version": "1"}, "capabilities": {"futureFlag": true}}), json!({"userAgent": "arany/0.1.0", "platformOs": "linux", "capabilities": CAPABILITIES})),
        ("thread/start", json!({"cwd": "/workspace", "provider": "openai", "model": "model", "effort": "high", "permissionMode": "acceptEdits", "includePaths": ["/workspace/file"]}), json!({"thread": thread})),
        ("thread/resume", json!({"threadId": thread_id, "cwd": "/workspace", "provider": "custom:local", "model": "model"}), json!({"thread": thread, "turns": [turn.clone()]})),
        ("turn/start", json!({"threadId": thread_id, "input": [{"type": "text", "text": "Hello"}, {"type": "image", "mediaType": "image/png", "data": "aW1hZ2U="}], "goal": {"autonomous": false}}), json!({"turn": turn})),
        ("thread/settings/update", json!({"threadId":thread_id,"permissionMode":"acceptEdits"}), json!({"permissionMode":"acceptEdits"})),
        ("thread/fork", json!({"threadId": thread_id, "cwd": "/workspace", "title": "Fork"}), json!({"thread": thread})),
        ("thread/rewind/preview", json!({"threadId":thread_id,"turnId":run_id}), json!({"checkpoint":{"kind":"run","id":run_id,"sequence":4},"restore":["both","conversation","code"],"files":[{"path":"src/a","state":"drifted","added":1,"removed":2}],"blockers":[],"nonRewindable":[{"sequence":8,"effect":"mcp"}],"previewToken":"preview"})),
        ("thread/rewind", json!({"threadId":thread_id,"previewToken":"preview","restore":"both","overwrite":["src/a"],"confirmOverwrite":true}), json!({"rewind":{"id":run_id,"outcome":"restored","conflicts":[],"conflictCount":0},"thread":thread,"conversation":{"threadId":thread_id,"prompt":"Selected prompt","imagesOmitted":false},"notices":["Restored 1 file to before Run 1"]})),
        ("thread/unsubscribe", json!({"threadId":thread_id}), json!({"status":"unsubscribed"})),
        ("thread/list", json!({"cwd":"/workspace","limit":100}), json!({"data":[{"id":thread_id,"title":"Example","cwd":"/workspace","createdAt":"2026-10-19T00:00:00Z","updatedAt":"2026-10-19T00:00:00Z"}],"nextCursor":null})),
        ("events/subscribe", json!({"threadId": thread_id, "since": 0}), json!({"events": [], "nextSince": null})),
        ("receipt/get", json!({"threadId": thread_id, "goalId": goal_id}), json!({"goal": {"id": goal_id, "status": "completed", "objective": "Hello", "budget": {"maxProviderCalls": 64, "maxTokens": null, "maxActiveMs": 1800000, "maxRuns": 16}, "usage": {"runs": 1, "providerCalls": 1, "activeMs": 1, "uncertainActiveMs": 0, "knownTokens": 0, "knownCachedTokens": 0, "tokenUsageUnknown": true}, "report": {"summary": "Done", "changes": [], "evidence": [], "unresolved": [], "cachedInputTokens": 0}}})),
        ("turn/interrupt", json!({"threadId": thread_id, "turnId": run_id}), json!({})),
        ("turn/steer", json!({"threadId": thread_id, "turnId": run_id, "input": [{"type": "text", "text": "Use the smaller fixture"}]}), json!({"turnId": run_id})),
        ("goal/pause", json!({"threadId": thread_id, "goalId": goal_id}), json!({})),
        ("goal/resume", json!({"threadId": thread_id, "goalId": goal_id, "input": [{"type": "text", "text": "Continue"}]}), json!({"turn": turn})),
    ].into_iter().enumerate() {
        let request = json!({"jsonrpc": "2.0", "id": index, "method": name, "params": params});
        assert!(validator.is_valid(&request), "request {name}");
        assert!(validator.is_valid(&json!({"jsonrpc": "2.0", "id": index, "result": result})), "response {name}");
        validate(&schema, &schema["methods"][name]["params"], &params);
        validate(&schema, &schema["methods"][name]["result"], &result);
        match name {
            "initialize" => {serde_json::from_value::<InitializeParams>(params).unwrap();}
            "thread/start" => {serde_json::from_value::<ThreadStartParams>(params).unwrap();}
            "thread/settings/update" => {serde_json::from_value::<ThreadSettingsUpdateParams>(params).unwrap();}
            "thread/resume" => {serde_json::from_value::<ThreadResumeParams>(params).unwrap();}
            "turn/start" => {serde_json::from_value::<TurnStartParams>(params).unwrap();}
            "turn/interrupt" => {serde_json::from_value::<TurnInterruptParams>(params).unwrap();}
            "turn/steer" => {serde_json::from_value::<TurnSteerParams>(params).unwrap();}
            "goal/pause" => {serde_json::from_value::<GoalPauseParams>(params).unwrap();}
            "goal/resume" => {serde_json::from_value::<GoalResumeParams>(params).unwrap();}
            "thread/fork" => {serde_json::from_value::<ThreadForkParams>(params).unwrap();}
            "thread/rewind/preview" => {serde_json::from_value::<ThreadRewindPreviewParams>(params).unwrap();}
            "thread/rewind" => {serde_json::from_value::<ThreadRewindParams>(params).unwrap();}
            "thread/unsubscribe" => {serde_json::from_value::<ThreadUnsubscribeParams>(params).unwrap();}
            "thread/list" => {serde_json::from_value::<ThreadListParams>(params).unwrap();}
            "events/subscribe" => {serde_json::from_value::<EventsSubscribeParams>(params).unwrap();}
            "receipt/get" => {serde_json::from_value::<ReceiptGetParams>(params).unwrap();}
            _ => unreachable!(),
        }
    }
    let items = [
        json!({"type": "userMessage", "id": "user", "text": "Hello", "images": [{"index": 0, "mediaType": "image/png", "bytes": 5}]}),
        json!({"type": "agentMessage", "id": "answer", "text": "Done", "phase": "final_answer"}),
        json!({"type": "commandExecution", "id": "tool", "command": "rustc", "args": ["--version"], "cwd": "/workspace", "status": "uncertain", "disposition": "uncertain", "attestation": "guard"}),
        json!({"type": "fileChange", "id": "file", "changes": [{"path": "/workspace/file", "kind": "update"}], "status": "declined", "disposition": "denied"}),
        json!({"type": "mcpToolCall", "id": "mcp", "server": "local", "tool": "read", "arguments": {}, "status": "completed", "disposition": "succeeded", "output": "ok"}),
        json!({"type": "toolCall", "id": "read", "call": {"type": "read", "path": "file"}, "status": "inProgress"}),
    ];
    for item in &items {
        validate(&schema, &json!({"$ref": "#/$defs/Item"}), item);
        let decoded: Item = serde_json::from_value(item.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), *item);
    }
    for (name, params) in [
        (
            "events/committed",
            json!({"threadId": thread_id, "event": {"sequence": 1, "session_id": thread_id, "run_id": null, "agent_run_id": null, "kind": "SessionStarted", "event_version": 2, "payload": {"title": "Example"}, "created_at_ms": 1}}),
        ),
        ("initialized", json!({})),
        ("thread/started", json!({"thread": thread})),
        ("turn/started", json!({"threadId": thread_id, "turn": turn})),
        (
            "turn/completed",
            json!({"threadId": thread_id, "turn": {"id": run_id, "goalId": goal_id, "status": "failed", "items": [], "error": {"message": "Unknown outcome"}, "goal": {"status": "failed"}}}),
        ),
        (
            "item/started",
            json!({"threadId": thread_id, "turnId": run_id, "item": items[5], "startedAtMs": 2}),
        ),
        (
            "item/completed",
            json!({"threadId": thread_id, "turnId": run_id, "item": items[2], "completedAtMs": 3}),
        ),
        (
            "events/resync",
            json!({"threadId": thread_id, "missed": 1, "lastSequence": 4}),
        ),
    ] {
        validate(&schema, &schema["notifications"][name]["params"], &params);
        let message = json!({"jsonrpc": "2.0", "method": name, "params": params});
        assert!(validator.is_valid(&message));
        assert!(
            encode_line(&decode_line(&serde_json::to_vec(&message).unwrap()).unwrap())
                .unwrap()
                .len()
                <= MAX_RPC_LINE_BYTES
        );
    }
    for name in [
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
        "item/tool/requestApproval",
    ] {
        let params = json!({"threadId": thread_id, "turnId": run_id, "itemId": "tool", "startedAtMs": 1, "intent": {}, "lines": ["Allow read?"]});
        validate(&schema, &schema["serverRequests"][name]["params"], &params);
        for decision in ["accept", "decline", "cancel"] {
            validate(
                &schema,
                &schema["serverRequests"][name]["result"],
                &json!({"decision": decision}),
            );
        }
    }
    for id in [Value::Null, json!(1), json!("client")] {
        let message = json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32600, "message": "Invalid request", "data": {"reason": "messageTooLarge"}}});
        assert!(validator.is_valid(&message));
    }
}

fn validate(document: &Value, reference: &Value, value: &Value) {
    let mut schema = reference.clone();
    schema["$schema"] = document["$schema"].clone();
    schema["$defs"] = document["$defs"].clone();
    let validator = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .offline()
        .build(&schema)
        .unwrap();
    let errors: Vec<_> = validator
        .iter_errors(value)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "{reference}: {errors:?}");
}

#[test]
fn protocol_params_reject_unknown_fields_unsupported_modes_and_input_overflow() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    let initialize = json!({"clientInfo": {"name": "fixture", "version": "1"}});
    for (key, value) in [
        ("name", "x".repeat(257)),
        ("version", "é".repeat(129)),
        ("title", "x".repeat(257)),
    ] {
        let mut params = initialize.clone();
        params["clientInfo"][key] = json!(value);
        assert!(serde_json::from_value::<InitializeParams>(params).is_err());
    }
    let mut params = initialize.clone();
    params["capabilities"] = json!({"optOutNotificationMethods": vec!["x"; 33]});
    assert!(serde_json::from_value::<InitializeParams>(params).is_err());
    let mut params = initialize;
    params["unknown"] = json!(true);
    assert!(serde_json::from_value::<InitializeParams>(params).is_err());
    for mode in ["plan", "dontAsk", "acceptForSession", "accept_edits"] {
        assert!(serde_json::from_value::<ThreadStartParams>(json!({"cwd": "/workspace", "provider": "openai", "model": "model", "permissionMode": mode})).is_err());
    }
    let mut params = json!({"threadId": "019a0011-2233-7444-8555-667788990000", "input": []});
    for input in [
        json!([]),
        json!([{"type": "text", "text": "x".repeat(65537)}]),
        json!([{"type": "text", "text": "é".repeat(32769)}]),
        json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]),
        json!(vec![
            json!({"type": "image", "mediaType": "image/png", "data": "eA=="});
            arany::MAX_MESSAGE_IMAGES + 1
        ]),
        json!(vec![
            json!({"type": "image", "mediaType": "image/png", "data": STANDARD.encode(vec![0; arany::MAX_IMAGE_BYTES / 2 + 1])});
            2
        ]),
        json!([{"type": "image", "mediaType": "image/jpeg", "data": "eA=="}]),
        json!([{"type": "image", "mediaType": "image/png", "data": "invalid"}]),
        json!([{"type": "image", "mediaType": "image/png", "data": "A".repeat(arany::MAX_IMAGE_BYTES.div_ceil(3) * 4 + 4)}]),
    ] {
        params["input"] = input;
        assert!(serde_json::from_value::<TurnStartParams>(params.clone()).is_err());
    }
    params["input"] = json!([{"type": "text", "text": "x".repeat(65536)}]);
    assert!(serde_json::from_value::<TurnStartParams>(params.clone()).is_ok());
    params["input"] = json!([
        {"type": "image", "mediaType": "image/png", "data": STANDARD.encode(vec![0; arany::MAX_IMAGE_BYTES / 2])},
        {"type": "image", "mediaType": "image/png", "data": STANDARD.encode(vec![0; arany::MAX_IMAGE_BYTES - arany::MAX_IMAGE_BYTES / 2])}
    ]);
    assert!(serde_json::from_value::<TurnStartParams>(params).is_ok());
    for decision in ["accept", "acceptForSession", "decline", "cancel"] {
        assert!(serde_json::from_value::<ApprovalResult>(json!({"decision":decision})).is_ok());
    }
    for decision in ["allowPrefixInFolder", "remember", "allow_always"] {
        assert!(serde_json::from_value::<ApprovalResult>(json!({"decision":decision})).is_err());
    }
    let capabilities: ClientCapabilities = serde_json::from_value(json!({
        "hostCommandSessionApproval":true,"unknown":true
    }))
    .unwrap();
    assert!(capabilities.host_command_session_approval);
    assert!(
        serde_json::from_value::<ClientCapabilities>(json!({"hostCommandSessionApproval":"true"}))
            .is_err()
    );
    let thread = "019a0011-2233-7444-8555-667788990000";
    for since in [json!(-1), json!(1.5), Value::Null, json!("1")] {
        assert!(
            serde_json::from_value::<EventsSubscribeParams>(
                json!({"threadId":thread,"since":since})
            )
            .is_err()
        );
    }
    assert!(
        serde_json::from_value::<EventsSubscribeParams>(
            json!({"threadId":thread,"since":u64::MAX})
        )
        .is_ok()
    );
    assert!(
        serde_json::from_value::<EventsSubscribeParams>(
            json!({"threadId":thread,"since":0,"extra":true})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<ReceiptGetParams>(
            json!({"threadId":thread,"goalId":thread,"path":"/private"})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<ThreadForkParams>(json!({"threadId":thread,"cwd":"relative"}))
            .is_err()
    );
}

#[test]
fn steering_and_resume_input_is_exactly_one_bounded_text() {
    let ids = json!({"threadId": "019a0011-2233-7444-8555-667788990000", "turnId": "019a0011-2233-7444-8555-667788990001", "goalId": "019a0011-2233-7444-8555-667788990002"});
    let text = |text: String| json!([{"type": "text", "text": text}]);
    let image = json!({"type": "image", "mediaType": "image/png", "data": "eA=="});
    let with = |name: &str, input: Value| match name {
        "steer" => serde_json::from_value::<TurnSteerParams>(
            json!({"threadId": ids["threadId"], "turnId": ids["turnId"], "input": input}),
        )
        .is_ok(),
        _ => serde_json::from_value::<GoalResumeParams>(
            json!({"threadId": ids["threadId"], "goalId": ids["goalId"], "input": input}),
        )
        .is_ok(),
    };
    let schema = protocol_schema();
    let accepts = |method: &str, ids: Value, input: &Value| {
        let mut params = ids;
        params["input"] = input.clone();
        let mut reference = schema["methods"][method]["params"].clone();
        reference["$schema"] = schema["$schema"].clone();
        reference["$defs"] = schema["$defs"].clone();
        jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .offline()
            .build(&reference)
            .unwrap()
            .is_valid(&params)
    };
    for (name, method) in [("steer", "turn/steer"), ("resume", "goal/resume")] {
        let wire_ids = if name == "steer" {
            json!({"threadId": ids["threadId"], "turnId": ids["turnId"]})
        } else {
            json!({"threadId": ids["threadId"], "goalId": ids["goalId"]})
        };
        assert!(accepts(method, wire_ids.clone(), &text("hello".into())));
        assert!(
            !accepts(method, wire_ids, &json!([image.clone()])),
            "{method}"
        );
        assert!(with(name, text("x".repeat(64 * 1024))), "{name}");
        for input in [
            json!([]),
            text("x".repeat(64 * 1024 + 1)),
            json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]),
            json!([image.clone()]),
            json!([{"type": "text", "text": "a"}, image.clone()]),
        ] {
            assert!(!with(name, input.clone()), "{name} {input}");
        }
    }
    assert!(
        serde_json::from_value::<TurnSteerParams>(
            json!({"threadId": ids["threadId"], "turnId": ids["turnId"]})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<GoalResumeParams>(
            json!({"threadId": ids["threadId"], "goalId": ids["goalId"]})
        )
        .is_ok()
    );
}

#[test]
fn event_kinds_const_matches_kind() {
    let mut kinds = EVENT_KINDS.to_vec();
    kinds.sort_unstable();
    kinds.dedup();
    assert_eq!(kinds.len(), EVENT_KINDS.len());
    let schema = protocol_schema();
    assert_eq!(
        schema["$defs"]["EventLine"]["properties"]["kind"]["enum"],
        json!(EVENT_KINDS)
    );
    let run_id = arany::RunId::new();
    for event in [
        arany::Event::SessionRenamed {
            title: "Example".into(),
        },
        arany::Event::MessageAccepted {
            skill: None,
            run_id,
            text: "Hello".into(),
            images: vec![],
        },
        arany::Event::MessageCommitted {
            run_id,
            text: "Done".into(),
        },
        arany::Event::RunFinished {
            run_id,
            disposition: arany::RunDisposition::Finished,
            failure: None,
            active_ms: 1,
        },
    ] {
        assert!(EVENT_KINDS.contains(&event.kind()));
    }
}

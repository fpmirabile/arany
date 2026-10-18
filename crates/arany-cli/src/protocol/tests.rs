use super::{methods::*, rpc::*, *};
use serde_json::{Value, json};

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
        decode_line(&vec![b' '; MAX_LINE_BYTES + 1]),
        Err(CodecError::TooLarge)
    );
    let mut request = Request {
        jsonrpc: Version::V2,
        id: RequestId::Integer(1),
        method: "x".into(),
        params: Some(json!("")),
    };
    let overhead = encode_line(&request).unwrap().len();
    request.params = Some(json!("x".repeat(MAX_LINE_BYTES - overhead)));
    let line = encode_line(&request).unwrap();
    assert_eq!(line.len(), MAX_LINE_BYTES);
    assert!(decode_line(&line).is_ok());
    request.params = Some(json!("x".repeat(MAX_LINE_BYTES - overhead + 1)));
    assert_eq!(encode_line(&request), Err(CodecError::TooLarge));
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
    for (index, (name, params, result)) in [
        ("initialize", json!({"clientInfo": {"name": "fixture", "version": "1"}, "capabilities": {"futureFlag": true}}), json!({"userAgent": "arany/0.1.0", "platformOs": "linux", "capabilities": CAPABILITIES})),
        ("thread/start", json!({"cwd": "/workspace", "provider": "openai", "model": "model", "effort": "high", "permissionMode": "acceptEdits", "includePaths": ["/workspace/file"]}), json!({"thread": thread})),
        ("thread/resume", json!({"threadId": thread_id, "cwd": "/workspace", "provider": "custom:local", "model": "model"}), json!({"thread": thread, "turns": [turn.clone()]})),
        ("turn/start", json!({"threadId": thread_id, "input": [{"type": "text", "text": "Hello"}, {"type": "image", "mediaType": "image/png", "data": "aW1hZ2U="}], "goal": {"autonomous": false}}), json!({"turn": turn})),
        ("turn/interrupt", json!({"threadId": thread_id, "turnId": run_id}), json!({})),
    ].into_iter().enumerate() {
        let request = json!({"jsonrpc": "2.0", "id": index, "method": name, "params": params});
        assert!(validator.is_valid(&request), "request {name}");
        assert!(validator.is_valid(&json!({"jsonrpc": "2.0", "id": index, "result": result})), "response {name}");
        validate(&schema, &schema["methods"][name]["params"], &params);
        validate(&schema, &schema["methods"][name]["result"], &result);
        match name {
            "initialize" => {serde_json::from_value::<InitializeParams>(params).unwrap();}
            "thread/start" => {serde_json::from_value::<ThreadStartParams>(params).unwrap();}
            "thread/resume" => {serde_json::from_value::<ThreadResumeParams>(params).unwrap();}
            "turn/start" => {serde_json::from_value::<TurnStartParams>(params).unwrap();}
            "turn/interrupt" => {serde_json::from_value::<TurnInterruptParams>(params).unwrap();}
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
                <= MAX_LINE_BYTES
        );
    }
    for name in [
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
        "item/tool/requestApproval",
    ] {
        let params = json!({"threadId": thread_id, "turnId": run_id, "itemId": "tool", "startedAtMs": 1, "expiresAtMs": 2, "intent": {"expires_at_ms": 2}, "lines": ["Allow read?"]});
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
    assert!(
        serde_json::from_value::<ApprovalResult>(json!({"decision": "acceptForSession"})).is_err()
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
            active_ms: 1,
        },
    ] {
        assert!(EVENT_KINDS.contains(&event.kind()));
    }
}

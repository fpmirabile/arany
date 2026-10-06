use super::*;
use crate::provider::{ChildResult, HistoryTurn};
use crate::session::{AgentRunId, RunId, SessionId};

fn run_request() -> ProviderRequest {
    ProviderRequest {
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        phase: AgentPhase::RootSynthesis,
        collaboration: crate::session::CollaborationPolicy::Auto {
            max_active_children: 3,
        },
        model: "claude-sonnet-5".into(),
        instructions: Some("workspace guidance canary".into()),
        objective: "objective canary".into(),
        images: Vec::new(),
        includes: vec!["include canary".into()],
        history: vec![HistoryTurn {
            user: "previous user canary".into(),
            assistant: "previous assistant canary".into(),
        }],
        context_summary: Some("derived summary canary".into()),
        child_results: vec![ChildResult {
            objective: "child objective canary".into(),
            summary: "child summary canary".into(),
            result: "child result canary".into(),
        }],
        max_output_tokens: 4096,
        tools: None,
    }
}

#[test]
fn outbound_messages_pin_schema_cap_and_disclosure_scope() {
    let request = run_request();
    let body = run_body(&request, Effort::Max);
    assert_eq!(body["model"], "claude-sonnet-5");
    assert!(
        body["system"]
            .as_str()
            .unwrap()
            .contains(crate::provider::COLLABORATION_INSTRUCTIONS)
    );
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["stream"], false);
    assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    assert_eq!(
        body["output_config"]["format"]["schema"],
        crate::provider::restrict_outcome_schema(outcome_schema(), &request)
    );
    assert_eq!(body["output_config"]["effort"], "max");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    let input: Value = serde_json::from_str(body["messages"][0]["content"].as_str().unwrap())
        .expect("typed user data");
    assert_eq!(input["phase"], "root_synthesis");
    assert_eq!(input["workspace_guidance"], "workspace guidance canary");
    assert_eq!(
        input["history"][0]["assistant"],
        "previous assistant canary"
    );
    assert_eq!(input["child_results"][0]["result"], "child result canary");
    assert!(!body["system"].as_str().unwrap().contains("canary"));
    assert!(input.get("run_id").is_none());
    assert!(input.get("agent_run_id").is_none());
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
    assert!(body.get("metadata").is_none());
    assert!(body.get("container").is_none());
    assert!(encode_request(&body).is_ok());
    for (collaboration, planning) in [
        (crate::session::CollaborationPolicy::Single, vec!["finish"]),
        (
            crate::session::CollaborationPolicy::Auto {
                max_active_children: 0,
            },
            vec!["finish"],
        ),
        (
            crate::session::CollaborationPolicy::Auto {
                max_active_children: 3,
            },
            vec!["finish", "delegate"],
        ),
        (
            crate::session::CollaborationPolicy::Team {
                max_active_children: 3,
            },
            vec!["delegate"],
        ),
    ] {
        for phase in [AgentPhase::RootPlan, AgentPhase::RootSynthesis] {
            let mut scoped = request.clone();
            scoped.phase = phase;
            scoped.collaboration = collaboration;
            let body = run_body(&scoped, Effort::Low);
            let input: Value =
                serde_json::from_str(body["messages"][0]["content"].as_str().unwrap()).unwrap();
            assert_eq!(
                input["collaboration"],
                serde_json::to_value(collaboration).unwrap()
            );
            let branches: Vec<_> =
                body["output_config"]["format"]["schema"]["properties"]["outcome"]["anyOf"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|branch| branch["properties"]["type"]["enum"][0].as_str().unwrap())
                    .collect();
            assert_eq!(
                branches,
                if phase == AgentPhase::RootPlan {
                    planning.clone()
                } else {
                    vec!["finish"]
                }
            );
        }
    }
    let mut child = request.clone();
    child.phase = AgentPhase::ChildWork;
    child.collaboration = crate::session::CollaborationPolicy::Single;
    assert_eq!(
        run_body(&child, Effort::Low)["output_config"]["format"]["schema"]["properties"]["outcome"]
            ["anyOf"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    {
        use crate::provider::{ImageAttachment, ImageOrigin, ProviderImage};
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
        let image = ImageAttachment::from_png(&STANDARD.decode(encoded).unwrap()).unwrap();
        let mut selected = request.clone();
        selected.images = vec![ProviderImage {
            origin: ImageOrigin::Objective,
            image,
        }];
        let multimodal = run_body(&selected, Effort::Low);
        assert_eq!(multimodal["messages"][0]["role"], "user");
        assert_eq!(
            multimodal["messages"][0]["content"],
            json!([
                {"type": "text", "text": "Image 1 (objective): PNG 1x1"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": encoded}},
                {"type": "text", "text": body["messages"][0]["content"]},
            ])
        );
        assert_eq!(multimodal["system"], body["system"]);
        assert_eq!(multimodal["max_tokens"], 4096);
        assert_eq!(multimodal["stream"], false);
        assert_eq!(
            multimodal["output_config"]["format"],
            body["output_config"]["format"]
        );
        assert_eq!(multimodal["output_config"]["effort"], "low");
        assert!(encode_request(&multimodal).is_ok());
    }
    for (model, effort) in [
        ("claude-fable-5-1", Effort::High),
        ("claude-opus-5-5", Effort::Medium),
        ("claude-sonnet-5-5", Effort::Max),
    ] {
        let mut selected = request.clone();
        selected.model = model.into();
        let selected_body = run_body(&selected, effort);
        assert_eq!(selected_body["model"], model);
        assert_eq!(selected_body["output_config"]["effort"], effort.as_str());
        assert_eq!(
            selected_body["output_config"]["format"],
            body["output_config"]["format"]
        );
        assert_eq!(selected_body["max_tokens"], 4096);
        assert!(encode_request(&selected_body).is_ok());
    }

    let compact = CompactionRequest {
        session_id: SessionId::new(),
        covered_run_id: request.run_id,
        model: request.model,
        previous_summary: Some("prior derived canary".into()),
        items: vec![
            CompactionItem::Completed(HistoryTurn {
                user: "done user canary".into(),
                assistant: "done answer canary".into(),
            }),
            CompactionItem::Unanswered {
                user: "open user canary".into(),
                status: UnansweredStatus::Interrupted,
            },
        ],
        max_output_tokens: 1024,
    };
    let compact_body = compaction_body(&compact, Effort::Low);
    let compact_input: Value =
        serde_json::from_str(compact_body["messages"][0]["content"].as_str().unwrap())
            .expect("typed compaction data");
    assert_eq!(compact_body["max_tokens"], 1024);
    assert_eq!(compact_body["output_config"]["effort"], "low");
    assert_eq!(compact_input["items"][1]["status"], "interrupted");
    assert!(compact_input.get("workspace_guidance").is_none());
    assert!(compact_input.get("session_id").is_none());
    assert_eq!(
        compact_body["output_config"]["format"]["schema"],
        summary_schema()
    );
}

fn response(stop_reason: &str, content: Value) -> Value {
    json!({
        "id": "msg_example",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-5",
        "stop_reason": stop_reason,
        "stop_details": null,
        "stop_sequence": null,
        "content": content,
        "usage": {"input_tokens": 12, "cache_creation_input_tokens": 3, "cache_read_input_tokens": 4, "output_tokens": 8}
    })
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

#[test]
fn tool_wire_uses_semantic_outcomes_and_separates_untrusted_catalog_data() {
    let mut request = run_request();
    request.phase = AgentPhase::RootPlan;
    request.tools = Some(crate::tools::ToolContext {
        catalog: "UNTRUSTED_TOOL_CATALOG_CANARY".into(),
        observations: Vec::new(),
    });
    let body = run_body(&request, Effort::Low);
    assert!(
        body["system"]
            .as_str()
            .unwrap()
            .contains(crate::provider::COLLABORATION_INSTRUCTIONS)
    );
    let input: Value =
        serde_json::from_str(body["messages"][0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(input["tools"]["catalog"], "UNTRUSTED_TOOL_CATALOG_CANARY");
    assert!(!body["system"].as_str().unwrap().contains("CANARY"));
    assert!(body.get("tools").is_none() && body.get("tool_choice").is_none());
    assert_eq!(body["max_tokens"], 4096);
    let branches = body["output_config"]["format"]["schema"]["properties"]["outcome"]["anyOf"]
        .as_array()
        .unwrap();
    assert_eq!(branches.len(), 3);
    assert_eq!(
        branches[2]["properties"]["call"]["anyOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|call| call["properties"]["operation"]["enum"][0].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["list", "read", "search"]
    );
    request.phase = AgentPhase::RootSynthesis;
    let synthesis = run_body(&request, Effort::Low);
    let branches = synthesis["output_config"]["format"]["schema"]["properties"]["outcome"]["anyOf"]
        .as_array()
        .unwrap();
    assert_eq!(branches.len(), 2);
    assert_eq!(
        branches[1]["properties"]["call"]["anyOf"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert!(
        !synthesis["system"]
            .as_str()
            .unwrap()
            .contains("synthesis must Finish"),
        "primary synthesis must be able to read and edit before finishing"
    );
    let rows: Vec<Value> = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/tool-outcomes.json"
    ))
    .unwrap();
    assert_eq!(rows.len(), 10);
    for row in rows {
        let fixture = response("end_turn", json!([text_block(&row.to_string())]));
        let decoded = decode_run(
            &serde_json::to_vec(&fixture).unwrap(),
            "claude-sonnet-5",
            4096,
        )
        .unwrap();
        assert!(
            matches!(decoded.outcome, ProviderOutcome::Tool(call) if serde_json::to_value(&call).unwrap() == row["outcome"]["call"])
        );
    }
    for call in [
        json!({"operation":"skill","name":"example"}),
        json!({"operation":"read","path":"../outside","offset":0,"limit":1}),
        json!({"operation":"command","program":"bash","args":[],"cwd":"","network":true}),
    ] {
        let text = json!({"outcome":{"type":"tool","call":call}}).to_string();
        let fixture = response("end_turn", json!([text_block(&text)]));
        assert!(
            decode_run(
                &serde_json::to_vec(&fixture).unwrap(),
                "claude-sonnet-5",
                4096
            )
            .is_err()
        );
    }
}

#[test]
fn inbound_messages_require_one_complete_structured_answer() {
    let finish = response(
        "end_turn",
        json!([
            {"type": "thinking", "thinking": "private", "signature": "sig"},
            {"type": "redacted_thinking", "data": "opaque"},
            text_block(r#"{"outcome":{"type":"finish","summary":"done","result":"answer"}}"#)
        ]),
    );
    let answer = decode_run(
        &serde_json::to_vec(&finish).unwrap(),
        "claude-sonnet-5",
        4096,
    )
    .expect("schema-constrained finish");
    assert_eq!(answer.response_id.as_deref(), Some("msg_example"));
    assert_eq!(answer.input_tokens, Some(19));
    assert_eq!(answer.output_tokens, Some(8));
    assert_eq!(
        answer.wire_provenance,
        Some(ProviderWireProvenance::MessagesEndTurnStorageUnspecified)
    );
    assert!(matches!(
        answer.outcome,
        ProviderOutcome::Finish(Finish { summary, result }) if summary == "done" && result == "answer"
    ));
    for model in ["claude-fable-5-1", "claude-opus-5-5", "claude-sonnet-5-5"] {
        let mut fixture = response(
            "end_turn",
            json!([text_block(
                r#"{"outcome":{"type":"finish","summary":"done","result":"answer"}}"#
            )]),
        );
        fixture["model"] = json!(model);
        let bytes = serde_json::to_vec(&fixture).unwrap();
        assert!(matches!(
            decode_run(&bytes, model, 4096),
            Ok(ProviderResponse {
                outcome: ProviderOutcome::Finish(_),
                ..
            })
        ));
        assert!(decode_run(&bytes, "claude-sonnet-5", 4096).is_err());
    }
    let mut uncached = finish.clone();
    uncached["usage"]
        .as_object_mut()
        .unwrap()
        .remove("cache_creation_input_tokens");
    uncached["usage"]
        .as_object_mut()
        .unwrap()
        .remove("cache_read_input_tokens");
    assert_eq!(
        decode_run(
            &serde_json::to_vec(&uncached).unwrap(),
            "claude-sonnet-5",
            4096
        )
        .expect("uncached usage")
        .input_tokens,
        Some(12)
    );

    let delegate = response(
        "end_turn",
        json!([text_block(
            r#"{"outcome":{"type":"delegate","children":["a","b"]}}"#
        )]),
    );
    let answer = decode_run(
        &serde_json::to_vec(&delegate).unwrap(),
        "claude-sonnet-5",
        4096,
    )
    .expect("schema-constrained delegation");
    assert!(matches!(
        answer.outcome,
        ProviderOutcome::Delegate(Delegate { children }) if children == ["a", "b"]
    ));

    let compact = response("end_turn", json!([text_block(r#"{"summary":"short"}"#)]));
    let summary = decode_compaction(
        &serde_json::to_vec(&compact).unwrap(),
        "claude-sonnet-5",
        1024,
    )
    .expect("schema-constrained compaction");
    assert_eq!(summary.summary, "short");
    for malformed in [json!({"type":"text"}), json!({"type":"text","text":null})] {
        for (compaction, valid) in [
            (
                false,
                r#"{"outcome":{"type":"finish","summary":"done","result":"answer"}}"#,
            ),
            (true, r#"{"summary":"short"}"#),
        ] {
            let fixture = response("end_turn", json!([malformed, text_block(valid)]));
            let bytes = serde_json::to_vec(&fixture).unwrap();
            if compaction {
                assert!(
                    decode_compaction(&bytes, "claude-sonnet-5", 1024).is_err(),
                    "malformed text precedes valid summary"
                );
            } else {
                assert!(
                    decode_run(&bytes, "claude-sonnet-5", 4096).is_err(),
                    "malformed text precedes valid outcome"
                );
            }
        }
    }
    assert_eq!(summary.output_tokens, Some(8));
    assert_eq!(
        summary.wire_provenance,
        Some(ProviderWireProvenance::MessagesEndTurnStorageUnspecified)
    );

    let bad = [
        response("max_tokens", json!([text_block("{}")])),
        response("refusal", json!([text_block("{}")])),
        response("tool_use", json!([text_block("{}")])),
        response("end_turn", json!([])),
        response("end_turn", json!([text_block("{}"), text_block("{}")])),
        response("end_turn", json!([{"type": "tool_use", "name": "x"}])),
        response("end_turn", json!([{"type": "citation", "text": "x"}])),
        response("end_turn", json!([text_block("{}"), {"type": "thinking"}])),
        response("end_turn", json!([{"type": "text"}])),
        response(
            "end_turn",
            json!([{"type": "text", "text": "{}", "citations": [{"type": "x"}]}]),
        ),
        response(
            "end_turn",
            json!([{"type": "text", "text": "{}", "unexpected": true}]),
        ),
    ];
    for (index, fixture) in bad.into_iter().enumerate() {
        assert!(
            decode_run(
                &serde_json::to_vec(&fixture).unwrap(),
                "claude-sonnet-5",
                4096
            )
            .is_err(),
            "bad content case {index}"
        );
    }
    let mut too_many_blocks = vec![json!({"type": "thinking"}); MAX_CONTENT_BLOCKS];
    too_many_blocks.push(text_block(
        r#"{"outcome":{"type":"finish","summary":"done","result":"answer"}}"#,
    ));
    assert!(
        decode_run(
            &serde_json::to_vec(&response("end_turn", json!(too_many_blocks))).unwrap(),
            "claude-sonnet-5",
            4096,
        )
        .is_err()
    );
    let drift = [
        ("model", json!("claude-sonnet-5-5")),
        ("role", json!("user")),
        ("type", json!("error")),
        ("stop_details", json!({"type": "refusal"})),
        ("stop_sequence", json!("done")),
        ("id", json!("")),
        ("usage", json!({"input_tokens": 12, "output_tokens": 4097})),
        (
            "usage",
            json!({"input_tokens": u32::MAX, "cache_read_input_tokens": 1, "output_tokens": 8}),
        ),
    ];
    for (index, (field, value)) in drift.into_iter().enumerate() {
        let mut fixture = finish.clone();
        fixture[field] = value;
        assert!(
            decode_run(
                &serde_json::to_vec(&fixture).unwrap(),
                "claude-sonnet-5",
                4096
            )
            .is_err(),
            "bad envelope case {index}"
        );
    }
    for text in [
        r#"{"outcome":{"type":"finish","summary":"x"}}"#,
        r#"{"outcome":{"type":"finish","summary":"x","result":"y","extra":1}}"#,
        r#"{"outcome":{"type":"delegate","children":[],"result":"x"}}"#,
        r#"{"outcome":{"type":"unknown"}}"#,
    ] {
        let fixture = response("end_turn", json!([text_block(text)]));
        assert!(
            decode_run(
                &serde_json::to_vec(&fixture).unwrap(),
                "claude-sonnet-5",
                4096
            )
            .is_err(),
            "invalid outcome shape"
        );
    }
    assert!(
        decode_compaction(
            &serde_json::to_vec(&response(
                "end_turn",
                json!([text_block(r#"{"summary":"ok","extra":1}"#)])
            ))
            .unwrap(),
            "claude-sonnet-5",
            1024,
        )
        .is_err()
    );
    assert!(encode_request(&json!({"oversize": "x".repeat(MAX_REQUEST_BYTES)})).is_err());
}

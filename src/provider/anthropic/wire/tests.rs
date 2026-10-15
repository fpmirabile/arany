use super::*;
use crate::provider::{AgentPhase, ChildResult, CompactionItem, HistoryTurn, UnansweredStatus};
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
        goal_budget: None,
        goal_plan_contract: None,
        child_results: vec![ChildResult {
            objective: "child objective canary".into(),
            summary: "child summary canary".into(),
            result: "child result canary".into(),
        }],
        max_output_tokens: 4096,
        tools: None,
    }
}

fn goal_plan_contract() -> crate::session::GoalPlanContract {
    crate::session::GoalPlanContract {
        mission: "Verify the requested Workspace update".into(),
        paths: vec!["src".into()],
        commands: vec!["bash".into()],
        skills: vec![crate::session::GoalSkillBinding {
            name: "review".into(),
            resource: Some("references/checklist.md".into()),
        }],
        mcp_tools: vec![crate::session::GoalMcpTool {
            server: "local".into(),
            tool: "inspect".into(),
        }],
        success_criterion: crate::session::GoalSuccessCriterion::WorkspaceFileContent {
            path: "src/main.rs".into(),
            content: "verified\n".into(),
        },
        budget: crate::session::GoalBudget::default(),
    }
}

#[test]
fn first_effect_requires_a_plan_and_bounded_read_only_prelude_is_available() {
    let mut request = run_request();
    request.collaboration = crate::session::CollaborationPolicy::Single;
    request.goal_budget = Some(crate::session::GoalBudget::default());
    request.tools = Some(crate::tools::ToolContext {
        catalog: json!({
            "write": true,
            "commands": [{"name":"bash"}],
            "skills": [{"name":"sample"}],
            "mcp_servers": [{"name":"local"}]
        })
        .to_string(),
        observations: Vec::new(),
    });
    for phase in [AgentPhase::RootPlan, AgentPhase::RootSynthesis] {
        request.phase = phase;
        let body = run_body(&request, Effort::Low);
        let branches = body["output_config"]["format"]["schema"]["properties"]["outcome"]["anyOf"]
            .as_array()
            .unwrap();
        let tool_branches: Vec<_> = branches
            .iter()
            .filter(|branch| branch["properties"]["type"]["enum"][0] == "tool")
            .collect();
        assert_eq!(tool_branches.len(), 2);
        for branch in tool_branches {
            let calls = branch["properties"]["call"]["anyOf"].as_array().unwrap();
            let prelude = calls.iter().all(|call| {
                matches!(
                    call["properties"]["operation"]["enum"][0].as_str(),
                    Some("list" | "read" | "search" | "skill" | "mcp_list")
                )
            });
            assert!(
                branch["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|field| field == "plan_contract")
            );
            let contract = &branch["properties"]["plan_contract"];
            let contract = if prelude {
                assert!(
                    contract["anyOf"]
                        .as_array()
                        .unwrap()
                        .contains(&json!({"type":"null"}))
                );
                &contract["anyOf"][0]
            } else {
                contract
            };
            assert!(
                contract["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("skills"))
            );
            assert_eq!(
                contract["properties"]["skills"]["items"]["required"],
                json!(["name", "resource"])
            );
            let eligible_calls = contract["properties"]["success_criterion"]["anyOf"][0]
                ["properties"]["call"]["anyOf"]
                .as_array()
                .unwrap()
                .iter()
                .map(|call| {
                    call["properties"]["operation"]["enum"][0]
                        .as_str()
                        .unwrap()
                })
                .collect::<Vec<_>>();
            assert_eq!(
                eligible_calls,
                vec!["mkdir", "write", "edit", "command", "skill", "mcp_call"]
            );
        }
        assert!(
            body["system"]
                .as_str()
                .unwrap()
                .contains("MCP catalog listing is the only process-backed prelude")
        );
    }
    request.phase = AgentPhase::RootSynthesis;
    request.goal_plan_contract = Some(goal_plan_contract());
    let continued = run_body(&request, Effort::Low);
    let branches = continued["output_config"]["format"]["schema"]["properties"]["outcome"]["anyOf"]
        .as_array()
        .unwrap();
    let tool = branches
        .iter()
        .find(|branch| branch["properties"]["type"]["enum"][0] == "tool")
        .unwrap();
    assert!(
        !tool["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field == "plan_contract")
    );
}

#[test]
fn structured_tool_outcome_preserves_the_sibling_goal_contract() {
    let call = crate::tools::ToolCall::Write {
        path: "src/new.rs".into(),
        expected_digest: None,
        content: "verified\n".into(),
    };
    let text = json!({
        "outcome": {
            "type": "tool",
            "call": call.clone(),
            "plan_contract": goal_plan_contract()
        }
    })
    .to_string();
    let decoded = decode_run(
        &serde_json::to_vec(&response("end_turn", json!([text_block(&text)]))).unwrap(),
        "claude-sonnet-5",
        4096,
    )
    .unwrap();
    assert!(matches!(
        decoded.outcome,
        ProviderOutcome::Tool {
            plan_contract: Some(_),
            ..
        }
    ));

    let text = json!({
        "outcome": {
            "type": "tool",
            "call": crate::tools::ToolCall::Read {
                path: "src/main.rs".into(),
                offset: 0,
                limit: 4096
            },
            "plan_contract": null
        }
    })
    .to_string();
    let decoded = decode_run(
        &serde_json::to_vec(&response("end_turn", json!([text_block(&text)]))).unwrap(),
        "claude-sonnet-5",
        4096,
    )
    .unwrap();
    assert!(matches!(
        decoded.outcome,
        ProviderOutcome::Tool {
            plan_contract: None,
            ..
        }
    ));
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
            matches!(decoded.outcome, ProviderOutcome::Tool { call, .. } if serde_json::to_value(&call).unwrap() == row["outcome"]["call"])
        );
    }
    for call in [
        json!(crate::tools::ToolCall::ApplyChangeset {
            source_intent_digest: [9; 32],
            changeset: crate::tools::CommandChangeset {
                files: vec![crate::tools::FileChange {
                    path: "generated".into(),
                    before: None,
                    after: Some(crate::tools::FileVersion::from_bytes(b"value", false)),
                }]
            },
        }),
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

#[test]
fn streamed_messages_require_closed_blocks_and_exclude_thinking_from_preview() {
    use crate::provider::{PreviewText, ResponsePreview};
    let outcome = serde_json::json!({"outcome":{"type":"finish","summary":"done","result":"line one\nline two 🚀"}}).to_string();
    let events = vec![
        serde_json::json!({"type":"message_start","message":{"id":"msg_stream","type":"message","role":"assistant","model":"model-1","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":12,"output_tokens":0}}}),
        serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"INTERNAL_CANARY"}}),
        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signature"}}),
        serde_json::json!({"type":"content_block_stop","index":0}),
        serde_json::json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        serde_json::json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":outcome}}),
        serde_json::json!({"type":"content_block_stop","index":1}),
        serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":8}}),
        serde_json::json!({"type":"message_stop"}),
    ];
    let wire = events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect::<String>();
    let (publisher, observed) = ResponsePreview::new();
    let mut decoder = MessageStream::new("synthetic-key", "model-1", publisher);
    let mut completed = None;
    for byte in wire.bytes() {
        if let Some(response) = decoder.feed(&[byte]).unwrap() {
            completed = Some(response);
        }
    }
    assert_eq!(
        **observed.borrow(),
        PreviewText {
            answer: "line one\nline two 🚀".into(),
            ..Default::default()
        }
    );
    let accepted = decode_run(&completed.unwrap(), "model-1", 32).unwrap();
    assert!(
        matches!(accepted.outcome, ProviderOutcome::Finish(Finish { result, .. }) if result == "line one\nline two 🚀")
    );
    for invalid in [
        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"unsafe"}}),
        serde_json::json!({"type":"message_stop"}),
    ] {
        let (publisher, _) = ResponsePreview::new();
        let mut decoder = MessageStream::new("key", "model-1", publisher);
        let wire = format!("data: {invalid}\n\n");
        assert!(decoder.feed(wire.as_bytes()).is_err());
    }
}

const SCHEMA_BOUND_KEYWORDS: [&str; 6] = [
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "minimum",
    "maximum",
];

fn assert_no_schema_bounds(value: &Value, at: &str) {
    match value {
        Value::Object(map) => {
            for keyword in SCHEMA_BOUND_KEYWORDS {
                assert!(!map.contains_key(keyword), "{at} carries {keyword}");
            }
            for (key, child) in map {
                assert_no_schema_bounds(child, &format!("{at}.{key}"));
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                assert_no_schema_bounds(child, &format!("{at}[{index}]"));
            }
        }
        _ => {}
    }
}

#[test]
fn provider_bound_schemas_carry_no_validation_keywords() {
    let mut request = run_request();
    request.collaboration = crate::session::CollaborationPolicy::Single;
    assert_no_schema_bounds(&run_body(&request, Effort::Low), "read_only");
    request.goal_budget = Some(crate::session::GoalBudget::default());
    request.tools = Some(crate::tools::ToolContext {
        catalog: json!({
            "write": true,
            "commands": [{"name":"bash"}],
            "skills": [{"name":"sample"}],
            "mcp_servers": [{"name":"local"}]
        })
        .to_string(),
        observations: Vec::new(),
    });
    for (label, phase) in [
        ("plan", AgentPhase::RootPlan),
        ("synthesis", AgentPhase::RootSynthesis),
    ] {
        request.phase = phase;
        assert_no_schema_bounds(&run_body(&request, Effort::Low), label);
    }
    request.goal_plan_contract = Some(goal_plan_contract());
    assert_no_schema_bounds(&run_body(&request, Effort::Low), "continued");
}

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
        model: "gpt-5.4".into(),
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
fn outbound_documents_pin_privacy_schema_and_scope() {
    let request = run_request();
    let body = run_body(&request);
    assert_eq!(body["model"], "gpt-5.4");
    assert_eq!(body["store"], false);
    assert_eq!(body["truncation"], "disabled");
    assert_eq!(body["max_output_tokens"], 4096);
    assert_eq!(body["tools"], json!([]));
    assert_eq!(body["tool_choice"], "none");
    assert_eq!(body["text"]["format"]["type"], "json_schema");
    assert_eq!(body["text"]["format"]["strict"], true);
    assert_eq!(body["text"]["format"]["schema"]["type"], "object");
    assert_eq!(
        body["text"]["format"]["schema"]["additionalProperties"],
        false
    );
    let input: Value = serde_json::from_str(body["input"].as_str().unwrap()).unwrap();
    assert_eq!(input["phase"], "root_synthesis");
    assert_eq!(input["workspace_guidance"], "workspace guidance canary");
    assert_eq!(
        input["history"][0]["assistant"],
        "previous assistant canary"
    );
    assert_eq!(input["child_results"][0]["result"], "child result canary");
    assert!(input.get("run_id").is_none());
    assert!(input.get("agent_run_id").is_none());
    assert!(body.get("previous_response_id").is_none());
    assert!(body.get("metadata").is_none());
    assert!(body.get("user").is_none());
    assert!(body.get("reasoning").is_none());
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
            let body = run_body(&scoped);
            let input: Value = serde_json::from_str(body["input"].as_str().unwrap()).unwrap();
            assert_eq!(
                input["collaboration"],
                serde_json::to_value(collaboration).unwrap()
            );
            let branches: Vec<_> =
                body["text"]["format"]["schema"]["properties"]["outcome"]["anyOf"]
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
        run_body(&child)["text"]["format"]["schema"]["properties"]["outcome"]["anyOf"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let native_body = run_body_with_effort(&request, Effort::Xhigh);
    assert_eq!(native_body["reasoning"]["effort"], "xhigh");
    assert_eq!(native_body["text"], body["text"]);
    {
        use crate::provider::{ImageAttachment, ImageOrigin, ProviderImage};
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
        let png = STANDARD.decode(encoded).expect("synthetic PNG");
        let image = ImageAttachment::from_png(&png).expect("bounded PNG container");
        assert_eq!(image.dimensions(), (1, 1));
        assert_eq!(image.byte_len(), png.len());
        assert_eq!(image.base64(), encoded);
        assert!(
            !format!("{image:?}").contains(encoded),
            "debug excludes image bytes"
        );
        let serialized = serde_json::to_value(&image).expect("image payload");
        assert_eq!(
            serialized,
            json!({"media_type": "image/png", "data": encoded})
        );
        assert_eq!(
            serde_json::from_value::<ImageAttachment>(serialized).unwrap(),
            image
        );
        for malformed in [
            json!({"media_type": "image/jpeg", "data": encoded}),
            json!({"media_type": "image/png", "data": "data:image/png;base64,forged"}),
            json!({"media_type": "image/png", "data": encoded, "url": "https://example.invalid/image"}),
        ] {
            assert!(
                serde_json::from_value::<ImageAttachment>(malformed).is_err(),
                "closed image payload"
            );
        }
        for invalid in [
            png[..png.len() - 1].to_vec(),
            {
                let mut corrupted = png.clone();
                corrupted[40] ^= 1;
                corrupted
            },
            {
                let mut trailing = png.clone();
                trailing.push(0);
                trailing
            },
            vec![0; crate::provider::MAX_IMAGE_BYTES + 1],
            b"/tmp/screenshot.png".to_vec(),
        ] {
            assert!(
                ImageAttachment::from_png(&invalid).is_err(),
                "complete bounded image admission"
            );
        }
        let chunk = |kind: &[u8; 4], data: &[u8]| {
            let mut output = (data.len() as u32).to_be_bytes().to_vec();
            output.extend(kind);
            output.extend(data);
            let mut crc = u32::MAX;
            for byte in kind.iter().chain(data) {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = if crc & 1 != 0 {
                        (crc >> 1) ^ 0xedb8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            output.extend((crc ^ u32::MAX).to_be_bytes());
            output
        };
        for (width, height, valid) in [
            (0_u32, 1_u32, false),
            (4097, 1, false),
            (4096, 4096, false),
            (2048, 2048, true),
        ] {
            let mut header = png[16..29].to_vec();
            header[..4].copy_from_slice(&width.to_be_bytes());
            header[4..8].copy_from_slice(&height.to_be_bytes());
            let mut candidate = png[..8].to_vec();
            candidate.extend(chunk(b"IHDR", &header));
            candidate.extend(&png[33..]);
            assert_eq!(
                ImageAttachment::from_png(&candidate).is_ok(),
                valid,
                "dimension/pixel admission, not raster decoding"
            );
        }
        let mut animated = png[..33].to_vec();
        animated.extend(chunk(b"acTL", &[0, 0, 0, 1, 0, 0, 0, 0]));
        animated.extend(&png[33..]);
        assert!(
            ImageAttachment::from_png(&animated).is_err(),
            "animation is not admitted"
        );
        let mut capped = png[..33].to_vec();
        capped.extend(chunk(
            b"tEXt",
            &vec![b'x'; crate::provider::MAX_IMAGE_BYTES - png.len() - 12],
        ));
        capped.extend(&png[33..]);
        assert_eq!(capped.len(), crate::provider::MAX_IMAGE_BYTES);
        let capped = ImageAttachment::from_png(&capped).expect("exact raw-image cap");
        assert_eq!(
            capped.base64().len(),
            crate::provider::MAX_IMAGE_BYTES.div_ceil(3) * 4
        );
        let mut selected = request.clone();
        selected.images = vec![
            ProviderImage {
                origin: ImageOrigin::History { turn_index: 0 },
                image: image.clone(),
            },
            ProviderImage {
                origin: ImageOrigin::Objective,
                image,
            },
        ];
        let multimodal = run_body_with_effort(&selected, Effort::Low);
        assert_eq!(
            multimodal["input"][0]["role"], "user",
            "images must be actual content, not JSON text"
        );
        assert_eq!(
            multimodal["input"][0]["content"][0],
            json!({"type": "input_text", "text": "Image 1 (history turn 1): PNG 1x1"})
        );
        assert_eq!(
            multimodal["input"][0]["content"][1],
            json!({"type": "input_image", "image_url": format!("data:image/png;base64,{encoded}"), "detail": "auto"})
        );
        assert_eq!(
            multimodal["input"][0]["content"][2],
            json!({"type": "input_text", "text": "Image 2 (objective): PNG 1x1"})
        );
        assert_eq!(
            multimodal["input"][0]["content"][3],
            multimodal["input"][0]["content"][1]
        );
        assert_eq!(
            multimodal["input"][0]["content"][4],
            json!({"type": "input_text", "text": body["input"]})
        );
        assert_eq!(multimodal["store"], false);
        assert_eq!(multimodal["max_output_tokens"], 4096);
        assert_eq!(multimodal["reasoning"]["effort"], "low");
        assert_eq!(multimodal["text"], body["text"]);
        assert!(encode_request(&multimodal).is_ok());
    }
    for (model, effort) in [
        ("gpt-6-astra", Effort::Low),
        ("gpt-6.1-sol", Effort::Max),
        ("gpt-6-luna", Effort::None),
    ] {
        let mut selected = request.clone();
        selected.model = model.into();
        let selected_body = run_body_with_effort(&selected, effort);
        assert_eq!(selected_body["model"], model);
        assert_eq!(selected_body["reasoning"]["effort"], effort.as_str());
        assert_eq!(selected_body["text"]["format"], body["text"]["format"]);
        assert_eq!(selected_body["max_output_tokens"], 4096);
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
    let compact_body = compaction_body(&compact);
    let compact_input: Value =
        serde_json::from_str(compact_body["input"].as_str().unwrap()).unwrap();
    assert_eq!(compact_body["store"], false);
    assert_eq!(compact_body["max_output_tokens"], 1024);
    assert_eq!(compact_input["items"][1]["status"], "interrupted");
    assert!(compact_input.get("workspace_guidance").is_none());
    assert!(compact_input.get("session_id").is_none());
    assert_eq!(compact_body["text"]["format"]["schema"], summary_schema());
    let native_compact = compaction_body_with_effort(&compact, Effort::Low);
    assert_eq!(native_compact["reasoning"]["effort"], "low");
}

fn response(status: &str, output: Value) -> Value {
    json!({
        "id": "resp_example",
        "status": status,
        "model": "gpt-5.4",
        "output": output,
        "usage": {"input_tokens": 12, "output_tokens": 8}
    })
}

fn message(text: &str) -> Value {
    json!({"type": "message", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": text}]})
}

#[test]
fn tool_wire_uses_semantic_outcomes_and_separates_untrusted_catalog_data() {
    let mut request = run_request();
    request.phase = AgentPhase::RootPlan;
    request.tools = Some(crate::tools::ToolContext {
        catalog: "UNTRUSTED_TOOL_CATALOG_CANARY".into(),
        observations: Vec::new(),
    });
    let body = run_body_with_effort(&request, Effort::Low);
    let input: Value = serde_json::from_str(body["input"].as_str().unwrap()).unwrap();
    assert_eq!(input["tools"]["catalog"], "UNTRUSTED_TOOL_CATALOG_CANARY");
    assert!(!body["instructions"].as_str().unwrap().contains("CANARY"));
    assert_eq!(body["tools"], json!([]));
    assert_eq!(body["tool_choice"], "none");
    assert_eq!(body["store"], false);
    assert_eq!(body["max_output_tokens"], 4096);
    let branches = body["text"]["format"]["schema"]["properties"]["outcome"]["anyOf"]
        .as_array()
        .unwrap();
    assert_eq!(branches.len(), 3);
    assert_eq!(branches[2], crate::tools::types::outcome_branch());
    let rows: Vec<Value> = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/tool-outcomes.json"
    ))
    .unwrap();
    assert_eq!(rows.len(), 10);
    for row in rows {
        let fixture = response("completed", json!([message(&row.to_string())]));
        let bytes = serde_json::to_vec(&fixture).unwrap();
        let decoded = decode_run(&bytes, "gpt-5.4").unwrap();
        assert!(
            matches!(decoded.outcome, ProviderOutcome::Tool(call) if serde_json::to_value(&call).unwrap() == row["outcome"]["call"])
        );
        let streamed = json!({"type":"response.completed","response":fixture});
        assert!(matches!(
            decode_streamed_run(&serde_json::to_vec(&streamed).unwrap(), "gpt-5.4")
                .unwrap()
                .outcome,
            ProviderOutcome::Tool(_)
        ));
    }
    for call in [
        json!({"operation":"write","path":"src/new","content":"missing required nullable"}),
        json!({"operation":"read","path":"../outside","offset":0,"limit":1}),
        json!({"operation":"command","program":"bash","args":[],"cwd":"","network":true}),
    ] {
        let text = json!({"outcome":{"type":"tool","call":call}}).to_string();
        let fixture = response("completed", json!([message(&text)]));
        assert!(decode_run(&serde_json::to_vec(&fixture).unwrap(), "gpt-5.4").is_err());
    }
}

#[test]
fn inbound_wire_corpus_requires_one_completed_structured_answer() {
    let finish = response(
        "completed",
        json!([
            {"type": "reasoning"},
            message(r#"{"outcome":{"type":"finish","summary":"done","result":"answer"}}"#)
        ]),
    );
    let parsed: WireResponse = serde_json::from_value(finish).unwrap();
    let (id, usage, text) = parsed.completed_text("gpt-5.4").unwrap();
    assert_eq!(id, "resp_example");
    assert_eq!(usage.unwrap().output_tokens, 8);
    let envelope: OutcomeEnvelope = serde_json::from_str(&text).unwrap();
    assert!(
        matches!(envelope.outcome, WireOutcome::Finish { summary, result } if summary == "done" && result == "answer")
    );
    let finish_response = response(
        "completed",
        json!([message(
            r#"{"outcome":{"type":"finish","summary":"done","result":"answer"}}"#
        )]),
    );
    let accepted = decode_run(&serde_json::to_vec(&finish_response).unwrap(), "gpt-5.4")
        .expect("completed Responses result");
    assert_eq!(
        accepted.wire_provenance,
        Some(ProviderWireProvenance::ResponsesCompletedStoreFalseRequested)
    );
    assert_eq!(accepted.input_tokens, Some(12));
    for compaction in [false, true] {
        let text = if compaction {
            r#"{"summary":"short"}"#
        } else {
            r#"{"outcome":{"type":"finish","summary":"done","result":"answer"}}"#
        };
        let mut final_message = message(text);
        final_message["phase"] = json!("final_answer");
        let mut commentary = message("Checking the supplied task.");
        commentary["phase"] = json!("commentary");
        let mut segmented = final_message.clone();
        segmented["content"] = json!([
            {"type":"output_text","text":&text[..10]},
            {"type":"output_text","text":&text[10..]}
        ]);
        let mut legacy_segmented = segmented.clone();
        legacy_segmented["phase"] = Value::Null;
        for output in [
            json!([final_message.clone()]),
            json!([commentary.clone(), final_message.clone()]),
            json!([commentary.clone(), commentary.clone(), segmented.clone()]),
            json!([legacy_segmented]),
        ] {
            let fixture = response("completed", output);
            let bytes = serde_json::to_vec(&fixture).unwrap();
            let streamed =
                serde_json::to_vec(&json!({"type":"response.completed","response":fixture}))
                    .unwrap();
            if compaction {
                assert_eq!(
                    decode_compaction(&bytes, "gpt-5.4").unwrap().summary,
                    "short"
                );
                assert_eq!(
                    decode_streamed_compaction(&streamed, "gpt-5.4")
                        .unwrap()
                        .summary,
                    "short"
                );
            } else {
                for accepted in [
                    decode_run(&bytes, "gpt-5.4").unwrap(),
                    decode_streamed_run(&streamed, "gpt-5.4").unwrap(),
                ] {
                    assert!(
                        matches!(accepted.outcome, ProviderOutcome::Finish(Finish { result, .. }) if result == "answer")
                    );
                }
            }
        }
        let mut unknown_phase = final_message.clone();
        unknown_phase["phase"] = json!("unknown");
        let mut malformed_commentary = commentary.clone();
        malformed_commentary["content"][0]
            .as_object_mut()
            .unwrap()
            .remove("text");
        let mut refusal = commentary.clone();
        refusal["content"] = json!([{"type":"refusal","refusal":"No."}]);
        let mut unphased = final_message.clone();
        unphased.as_object_mut().unwrap().remove("phase");
        for (output, expected) in [
            (json!([]), "MissingFinalMessage"),
            (json!([commentary.clone()]), "MissingFinalMessage"),
            (
                json!([final_message.clone(), final_message.clone()]),
                "DuplicateFinalMessage",
            ),
            (
                json!([unphased.clone(), final_message.clone()]),
                "AmbiguousFinalMessage",
            ),
            (
                json!([final_message.clone(), unphased.clone()]),
                "AmbiguousFinalMessage",
            ),
            (json!([unphased.clone(), unphased]), "AmbiguousFinalMessage"),
            (json!([final_message.clone(), commentary]), "LateCommentary"),
            (json!([unknown_phase]), "Phase"),
            (
                json!([malformed_commentary, final_message.clone()]),
                "Content",
            ),
            (json!([refusal, final_message]), "Content"),
        ] {
            let fixture = response("completed", output);
            let bytes = serde_json::to_vec(&fixture).unwrap();
            let streamed =
                serde_json::to_vec(&json!({"type":"response.completed","response":fixture}))
                    .unwrap();
            if compaction {
                assert!(decode_compaction(&bytes, "gpt-5.4").is_err());
                assert_eq!(
                    format!(
                        "{:?}",
                        decode_streamed_compaction(&streamed, "gpt-5.4").unwrap_err()
                    ),
                    expected,
                );
            } else {
                assert!(decode_run(&bytes, "gpt-5.4").is_err());
                assert_eq!(
                    format!(
                        "{:?}",
                        decode_streamed_run(&streamed, "gpt-5.4").unwrap_err()
                    ),
                    expected,
                );
            }
        }
    }
    let compact_response = response("completed", json!([message(r#"{"summary":"short"}"#)]));
    let compact = decode_compaction(&serde_json::to_vec(&compact_response).unwrap(), "gpt-5.4")
        .expect("completed Responses compaction");
    assert_eq!(compact.summary, "short");
    for malformed in [
        json!({"type":"output_text"}),
        json!({"type":"output_text","text":null}),
    ] {
        for (compaction, valid) in [
            (
                false,
                r#"{"outcome":{"type":"finish","summary":"done","result":"answer"}}"#,
            ),
            (true, r#"{"summary":"short"}"#),
        ] {
            let fixture = response(
                "completed",
                json!([
                    {"type":"message","role":"assistant","status":"completed","content":[malformed]},
                    message(valid)
                ]),
            );
            let bytes = serde_json::to_vec(&fixture).unwrap();
            let streamed =
                serde_json::to_vec(&json!({"type":"response.completed","response":fixture}))
                    .unwrap();
            if compaction {
                assert!(
                    decode_compaction(&bytes, "gpt-5.4").is_err(),
                    "malformed text precedes valid summary"
                );
                assert!(decode_streamed_compaction(&streamed, "gpt-5.4").is_err());
            } else {
                assert!(
                    decode_run(&bytes, "gpt-5.4").is_err(),
                    "malformed text precedes valid outcome"
                );
                assert!(decode_streamed_run(&streamed, "gpt-5.4").is_err());
            }
        }
    }
    assert_eq!(
        compact.wire_provenance,
        Some(ProviderWireProvenance::ResponsesCompletedStoreFalseRequested)
    );
    for model in ["gpt-6-astra", "gpt-6.1-sol", "gpt-6-luna"] {
        let mut fixture = response(
            "completed",
            json!([message(
                r#"{"outcome":{"type":"finish","summary":"done","result":"answer"}}"#
            )]),
        );
        fixture["model"] = json!(model);
        let bytes = serde_json::to_vec(&fixture).unwrap();
        assert!(matches!(
            decode_run(&bytes, model),
            Ok(ProviderResponse {
                outcome: ProviderOutcome::Finish(_),
                ..
            })
        ));
        assert!(decode_run(&bytes, "gpt-5.4").is_err());
    }

    let delegate: OutcomeEnvelope =
        serde_json::from_str(r#"{"outcome":{"type":"delegate","children":["a","b"]}}"#).unwrap();
    assert!(
        matches!(delegate.outcome, WireOutcome::Delegate { children } if children == ["a", "b"])
    );

    let bad_outputs = [
        response("incomplete", json!([message("{}")])),
        response("completed", json!([message("{}"), message("{}")])),
        response(
            "completed",
            json!([{"type": "message", "role": "assistant", "content": [{"type": "refusal", "refusal": "no"}]}]),
        ),
        response("completed", json!([{"type": "function_call", "name": "x"}])),
        response("completed", json!([])),
    ];
    for (index, fixture) in bad_outputs.into_iter().enumerate() {
        let parsed: WireResponse = serde_json::from_value(fixture).unwrap();
        assert!(
            parsed.completed_text("gpt-5.4").is_err(),
            "bad wire case {index}"
        );
    }
    for field in ["error", "incomplete_details"] {
        for value in [
            json!({"reason":"incomplete"}),
            json!(false),
            json!("invalid"),
        ] {
            let mut fixture = response(
                "completed",
                json!([message(
                    r#"{"outcome":{"type":"finish","summary":"done","result":"answer"}}"#
                )]),
            );
            fixture[field] = value;
            let streamed =
                serde_json::to_vec(&json!({"type":"response.completed","response":fixture}))
                    .unwrap();
            assert_eq!(
                decode_streamed_run(&streamed, "gpt-5.4").unwrap_err(),
                ResponseError::Status,
                "contradictory completed response {field}"
            );
        }
    }
    for text in [
        r#"{"outcome":{"type":"finish","summary":"x"}}"#,
        r#"{"outcome":{"type":"finish","summary":"x","result":"y","extra":1}}"#,
        r#"{"outcome":{"type":"delegate","children":[],"result":"x"}}"#,
        r#"{"outcome":{"type":"unknown"}}"#,
    ] {
        assert!(
            serde_json::from_str::<OutcomeEnvelope>(text).is_err(),
            "bad outcome shape"
        );
    }
    assert!(serde_json::from_str::<SummaryEnvelope>(r#"{"summary":"ok","extra":1}"#).is_err());
    assert!(encode_request(&json!({"oversize": "x".repeat(MAX_REQUEST_BYTES)})).is_err());
}

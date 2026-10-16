use super::*;
use crate::provider::test_support::{
    catalog, goal_plan_contract, observed_step, primary_request, read_only_request,
    researcher_request, tool_context,
};
use crate::provider::{
    AgentPhase, CompactionItem, Delegate, Finish, HistoryTurn, UnansweredStatus, render,
};
use crate::session::{CollaborationPolicy, RunId, SessionId};

const KEY: &str = "synthetic-key";
const MODEL: &str = "gpt-5.4";

fn run_body_for(request: &ProviderRequest, effort: Option<Effort>) -> Value {
    let conversation = render::conversation(request).unwrap();
    match effort {
        Some(effort) => run_body_with_effort(request, &conversation, effort),
        None => run_body(request, &conversation),
    }
    .unwrap()
}

fn body(request: &ProviderRequest) -> Value {
    run_body_for(request, None)
}

fn scope<'a>(request: &'a ProviderRequest) -> RunScope<'a> {
    RunScope::new(request, &render::conversation(request).unwrap(), KEY)
}

fn decode(request: &ProviderRequest, fixture: &Value) -> Result<ProviderResponse, ProviderError> {
    decode_run(&serde_json::to_vec(fixture).unwrap(), &scope(request))
}

fn decode_streamed(
    request: &ProviderRequest,
    fixture: &Value,
) -> Result<ProviderResponse, ResponseError> {
    let event = json!({"type": "response.completed", "response": fixture});
    decode_streamed_run(&serde_json::to_vec(&event).unwrap(), &scope(request))
}

fn response(status: &str, output: Value) -> Value {
    json!({
        "id": "resp_example",
        "status": status,
        "model": MODEL,
        "output": output,
        "usage": {"input_tokens": 12, "output_tokens": 8}
    })
}

fn message(text: &str) -> Value {
    json!({"type": "message", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": text}]})
}

fn function_call(name: &str, arguments: &Value) -> Value {
    function_call_with("call_test", "fc_test", name, arguments)
}

fn function_call_with(call_id: &str, id: &str, name: &str, arguments: &Value) -> Value {
    json!({"type":"function_call","id":id,"call_id":call_id,"name":name,"arguments":arguments.to_string(),"status":"completed"})
}

fn reasoning() -> Value {
    json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "CIPHERTEXT=="})
}

fn finish_text() -> &'static str {
    r#"{"outcome":{"type":"finish","summary":"done","result":"answer"}}"#
}

fn read_arguments() -> Value {
    json!({"path": "README.md", "offset": 0, "limit": 4096, "plan_contract": null})
}

fn function_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|function| function["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn outbound_documents_pin_privacy_schema_and_scope() {
    let request = read_only_request(MODEL);
    let rendered = body(&request);
    assert_eq!(rendered["prompt_cache_key"], request.session_id.to_string());
    let mut next_run = request.clone();
    next_run.run_id = RunId::new();
    next_run.agent_run_id = crate::session::AgentRunId::new();
    assert_eq!(
        body(&next_run)["prompt_cache_key"],
        rendered["prompt_cache_key"]
    );
    next_run.session_id = SessionId::new();
    assert_ne!(
        body(&next_run)["prompt_cache_key"],
        rendered["prompt_cache_key"]
    );
    assert_eq!(rendered["model"], MODEL);
    assert_eq!(rendered["store"], false);
    assert_eq!(rendered["truncation"], "disabled");
    assert_eq!(rendered["max_output_tokens"], 4096);
    assert_eq!(rendered["tools"], json!([]));
    assert_eq!(rendered["tool_choice"], "none");
    assert!(rendered.get("include").is_none() && rendered.get("parallel_tool_calls").is_none());
    assert_eq!(rendered["text"]["format"]["type"], "json_schema");
    assert_eq!(rendered["text"]["format"]["strict"], true);
    assert_eq!(rendered["text"]["format"]["schema"]["type"], "object");
    assert_eq!(
        rendered["text"]["format"]["schema"]["additionalProperties"],
        false
    );
    let instructions = rendered["instructions"].as_str().unwrap();
    assert!(instructions.starts_with(crate::provider::prompts::READ_ONLY_INSTRUCTIONS));
    assert!(instructions.contains("Pinned collaboration policy"));
    assert!(!instructions.contains("canary"));
    let input = rendered["input"].as_array().unwrap();
    assert!(input.iter().all(|item| item["role"] == "user"));
    let text = rendered["input"].to_string();
    for canary in [
        "workspace guidance canary",
        "include canary",
        "previous user canary",
        "previous assistant canary",
        "derived summary canary",
        "child result canary",
        "objective canary",
    ] {
        assert!(text.contains(canary), "{canary}");
    }
    assert!(!text.contains("run_id") && !text.contains("agent_run_id"));
    for absent in ["previous_response_id", "metadata", "user", "reasoning"] {
        assert!(rendered.get(absent).is_none(), "{absent}");
    }
    assert!(encode_request(&rendered).is_ok());

    for (collaboration, planning) in [
        (CollaborationPolicy::Single, vec!["finish"]),
        (
            CollaborationPolicy::Auto {
                max_active_children: 0,
            },
            vec!["finish"],
        ),
        (
            CollaborationPolicy::Auto {
                max_active_children: 3,
            },
            vec!["finish", "delegate"],
        ),
        (
            CollaborationPolicy::Team {
                max_active_children: 3,
            },
            vec!["delegate"],
        ),
    ] {
        for phase in [AgentPhase::RootPlan, AgentPhase::RootSynthesis] {
            let mut scoped = request.clone();
            scoped.phase = phase;
            scoped.collaboration = collaboration;
            let scoped_body = body(&scoped);
            let branches: Vec<_> = scoped_body["text"]["format"]["schema"]["properties"]["outcome"]
                ["anyOf"]
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
    child.collaboration = CollaborationPolicy::Single;
    assert_eq!(
        body(&child)["text"]["format"]["schema"]["properties"]["outcome"]["anyOf"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let native_body = run_body_for(&request, Some(Effort::Xhigh));
    assert_eq!(native_body["reasoning"]["effort"], "xhigh");
    assert_eq!(native_body["text"], rendered["text"]);
    for (model, effort) in [
        ("gpt-6-astra", Effort::Low),
        ("gpt-6.1-sol", Effort::Max),
        ("gpt-6-luna", Effort::None),
    ] {
        let mut selected = request.clone();
        selected.model = model.into();
        let selected_body = run_body_for(&selected, Some(effort));
        assert_eq!(selected_body["model"], model);
        assert_eq!(selected_body["reasoning"]["effort"], effort.as_str());
        assert_eq!(selected_body["text"]["format"], rendered["text"]["format"]);
        assert_eq!(selected_body["max_output_tokens"], 4096);
        assert!(encode_request(&selected_body).is_ok());
    }

    let compact = CompactionRequest {
        session_id: request.session_id,
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
    assert_eq!(
        compact_body["prompt_cache_key"],
        rendered["prompt_cache_key"]
    );
    assert_eq!(
        compact_body["prompt_cache_key"],
        compact.session_id.to_string()
    );
    assert_eq!(compact_body["max_output_tokens"], 1024);
    assert_eq!(compact_input["items"][1]["status"], "interrupted");
    assert!(compact_input.get("workspace_guidance").is_none());
    assert!(compact_input.get("session_id").is_none());
    assert_eq!(compact_body["text"]["format"]["schema"], summary_schema());
    let native_compact = compaction_body_with_effort(&compact, Effort::Low);
    assert_eq!(native_compact["reasoning"]["effort"], "low");
}

#[test]
fn image_attachments_admit_only_bounded_png_and_render_as_labelled_content() {
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

    let mut selected = read_only_request(MODEL);
    selected.input.images = vec![
        ProviderImage {
            origin: ImageOrigin::History { turn_index: 0 },
            image: image.clone(),
        },
        ProviderImage {
            origin: ImageOrigin::Objective,
            image,
        },
    ];
    let multimodal = run_body_for(&selected, Some(Effort::Low));
    let parts = multimodal["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["content"].as_array())
        .find(|parts| {
            parts
                .iter()
                .any(|part| part["text"] == "Image 1 (history turn 1): PNG 1x1")
        })
        .expect("image message");
    let at = parts
        .iter()
        .position(|part| part["text"] == "Image 1 (history turn 1): PNG 1x1")
        .unwrap();
    assert_eq!(
        parts[at + 1],
        json!({"type": "input_image", "image_url": format!("data:image/png;base64,{encoded}"), "detail": "auto"})
    );
    assert_eq!(
        parts[at + 2],
        json!({"type": "input_text", "text": "Image 2 (objective): PNG 1x1"})
    );
    assert_eq!(parts[at + 3], parts[at + 1]);
    assert_eq!(multimodal["store"], false);
    assert_eq!(multimodal["reasoning"]["effort"], "low");
    assert!(encode_request(&multimodal).is_ok());
}

#[test]
fn outbound_requests_encrypted_reasoning_include_and_replays_items_adjacent_and_unchanged() {
    let mut request = primary_request(MODEL);
    let call = function_call("arany_read", &read_arguments());
    let reply = response("completed", json!([reasoning(), call.clone()]));
    let step = observed_step(decode(&request, &reply).unwrap(), "{\"text\":\"ok\"}");
    request.input.tools.as_mut().unwrap().steps.push(step);
    let rendered = body(&request);
    assert_eq!(rendered["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(rendered["store"], false);
    assert_eq!(rendered["tool_choice"]["type"], "allowed_tools");
    assert_eq!(rendered["tool_choice"]["mode"], "required");
    assert_eq!(rendered["parallel_tool_calls"], false);
    let input = rendered["input"].as_array().unwrap();
    let at = input
        .iter()
        .position(|item| item["type"] == "reasoning")
        .expect("reasoning item replays");
    assert_eq!(input[at], reasoning(), "ciphertext survives untouched");
    assert_eq!(
        input[at + 1],
        call,
        "the call sits right behind its reasoning"
    );
    assert_eq!(input[at + 2]["type"], "function_call_output");
    assert_eq!(input[at + 2]["call_id"], "call_test");
    let encoded = String::from_utf8(encode_request(&rendered).unwrap()).unwrap();
    let reasoning_text = serde_json::to_string(&reasoning()).unwrap();
    let call_text = serde_json::to_string(&call).unwrap();
    assert!(
        encoded.contains(&format!("{reasoning_text},{call_text}")),
        "the raw items are adjacent in the request bytes"
    );
    assert_eq!(rendered["tools"].as_array().unwrap().len(), 14);
}

#[test]
fn outbound_input_items_replay_turns_and_function_outputs_once() {
    let mut request = primary_request(MODEL);
    let reply = response(
        "completed",
        json!([function_call("arany_read", &read_arguments())]),
    );
    let step = observed_step(
        decode(&request, &reply).unwrap(),
        "{\"text\":\"RESULT_CANARY\"}",
    );
    request.input.tools.as_mut().unwrap().steps.push(step);
    let rendered = body(&request);
    let input = rendered["input"].as_array().unwrap();
    assert_eq!(
        input.len(),
        4,
        "current message, call, output, trailing facts"
    );
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(input[2]["type"], "function_call_output");
    let output: Value = serde_json::from_str(input[2]["output"].as_str().unwrap()).unwrap();
    assert!(
        output.get("call").is_none(),
        "the replayed turn carries the call"
    );
    assert_eq!(output["workspace_effect"], "none");
    assert_eq!(input[3]["role"], "user");
    assert!(
        input[3]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("goal_context_facts")
    );
    let encoded = encode_request(&rendered).unwrap();
    assert_eq!(
        encoded
            .windows(b"RESULT_CANARY".len())
            .filter(|part| *part == b"RESULT_CANARY")
            .count(),
        1,
        "each observation appears once"
    );
    assert!(
        !rendered["instructions"]
            .as_str()
            .unwrap()
            .contains("RESULT_CANARY")
    );
}

#[test]
fn delegate_function_call_is_answered_by_a_fixed_output_before_the_child_results() {
    let mut request = primary_request(MODEL);
    request.collaboration = CollaborationPolicy::Auto {
        max_active_children: 2,
    };
    let reply = response(
        "completed",
        json!([function_call(
            "arany_delegate",
            &json!({"children": ["inspect"]})
        )]),
    );
    let delegated = decode(&request, &reply).unwrap();
    assert!(matches!(delegated.outcome, ProviderOutcome::Delegate(_)));
    request.phase = AgentPhase::RootSynthesis;
    request.input.child_results = vec![crate::provider::ChildResult {
        objective: "inspect".into(),
        summary: "s".into(),
        result: "child canary".into(),
    }];
    request
        .input
        .tools
        .as_mut()
        .unwrap()
        .steps
        .push(crate::tools::ToolStep {
            turn: delegated.turn,
            requests: Vec::new(),
            observations: Vec::new(),
            facts: None,
            steering_notes: Vec::new(),
            calls_remaining: None,
        });
    let input = body(&request)["input"].as_array().unwrap().clone();
    assert_eq!(input[1]["type"], "function_call");
    assert_eq!(
        input[2],
        json!({"type": "function_call_output", "call_id": "call_test", "output": DELEGATED_OUTPUT})
    );
    assert!(
        input[3]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("child canary")
    );
    let wrong_route = crate::provider::ProviderTurn::scripted();
    let mut foreign = request.clone();
    foreign.input.tools.as_mut().unwrap().steps[0].turn = wrong_route;
    let conversation = render::conversation(&foreign).unwrap();
    assert!(
        run_body(&foreign, &conversation).is_err(),
        "a turn replays only on its own route"
    );
}

#[test]
fn reconcile_keeps_raw_output_items_and_checks_the_typed_view() {
    let call = function_call("arany_read", &read_arguments());
    let done: Vec<(WireItem, Value)> = [reasoning(), call.clone()]
        .into_iter()
        .map(|raw| (serde_json::from_value(raw.clone()).unwrap(), raw))
        .collect();
    let mut completed =
        json!({"type": "response.completed", "response": response("completed", json!([]))});
    completed["response"]["id"] = json!("resp_1");
    let bytes = serde_json::to_vec(&completed).unwrap();
    let reconciled = reconcile_streamed_output(&bytes, "resp_1", done.clone()).unwrap();
    let value: Value = serde_json::from_slice(&reconciled).unwrap();
    assert_eq!(
        value["response"]["output"],
        json!([reasoning(), call]),
        "ciphertext is not dropped"
    );
    assert!(matches!(
        reconcile_streamed_output(&bytes, "resp_2", done.clone()),
        Err(ResponseError::Identifier)
    ));
    let mut explicit = completed.clone();
    explicit["response"]["output"] = json!([reasoning(), call.clone()]);
    assert!(
        reconcile_streamed_output(
            &serde_json::to_vec(&explicit).unwrap(),
            "resp_1",
            done.clone()
        )
        .is_ok()
    );
    explicit["response"]["output"] = json!([reasoning()]);
    assert!(matches!(
        reconcile_streamed_output(&serde_json::to_vec(&explicit).unwrap(), "resp_1", done),
        Err(ResponseError::Message)
    ));
}

#[test]
fn openai_usage_maps_cached_tokens() {
    let request = read_only_request(MODEL);
    let mut finish = response("completed", json!([message(finish_text())]));
    finish["usage"] = json!({
        "input_tokens": 12,
        "output_tokens": 8,
        "input_tokens_details": {"cached_tokens": 5}
    });
    let cached = decode(&request, &finish).expect("cached Responses result");
    assert_eq!(cached.usage.input_tokens, Some(12));
    assert_eq!(cached.usage.cache_read_input_tokens, Some(5));
    assert_eq!(cached.usage.cache_creation_input_tokens, None);
    assert!(cached.usage.cache_consistent());
    assert_eq!(
        decode_streamed(&request, &finish).unwrap().usage,
        cached.usage
    );
    for invalid in [
        json!(-1),
        json!(1.5),
        json!("5"),
        json!(u64::from(u32::MAX) + 1),
    ] {
        let mut malformed = finish.clone();
        malformed["usage"]["input_tokens_details"]["cached_tokens"] = invalid;
        assert!(decode(&request, &malformed).is_err());
        assert!(decode_streamed(&request, &malformed).is_err());
    }
    finish["usage"] = json!({"input_tokens": 12, "output_tokens": 8});
    let uncached = decode(&request, &finish).expect("usage without cache details");
    assert_eq!(uncached.usage.cache_read_input_tokens, None);
}

#[test]
fn native_functions_are_strict_catalog_scoped_and_followed_by_the_outcomes() {
    let mut request = primary_request(MODEL);
    request.input.tools = Some(tool_context(
        json!({"write":false,"commands":[],"skills":[],"mcp_servers":[]}),
    ));
    let rendered = run_body_for(&request, Some(Effort::Low));
    assert_eq!(rendered["tool_choice"]["type"], "allowed_tools");
    assert_eq!(rendered["tool_choice"]["mode"], "required");
    assert_eq!(rendered["parallel_tool_calls"], false);
    assert_eq!(rendered["store"], false);
    assert_eq!(rendered["max_output_tokens"], 4096);
    assert!(rendered.get("text").is_none());
    assert_eq!(
        function_names(&rendered),
        ["arany_list", "arany_read", "arany_search", "arany_finish"]
    );
    assert!(
        rendered["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|function| function["strict"] == true)
    );
    let instructions = rendered["instructions"].as_str().unwrap();
    assert!(instructions.starts_with(crate::provider::prompts::PRIMARY_INSTRUCTIONS));
    assert!(!instructions.contains("canary"));
    request.collaboration = CollaborationPolicy::Auto {
        max_active_children: 2,
    };
    assert_eq!(
        function_names(&body(&request)),
        [
            "arany_list",
            "arany_read",
            "arany_search",
            "arany_finish",
            "arany_delegate"
        ]
    );
    for policy in [
        CollaborationPolicy::Single,
        CollaborationPolicy::Auto {
            max_active_children: 0,
        },
        CollaborationPolicy::Auto {
            max_active_children: 2,
        },
        CollaborationPolicy::Team {
            max_active_children: 2,
        },
    ] {
        request.collaboration = policy;
        request.phase = AgentPhase::RootPlan;
        let planning = body(&request);
        request.phase = AgentPhase::RootSynthesis;
        let synthesis = body(&request);
        for field in ["tools", "instructions", "prompt_cache_key"] {
            assert_eq!(
                serde_json::to_vec(&planning[field]).unwrap(),
                serde_json::to_vec(&synthesis[field]).unwrap(),
                "{policy:?}: {field}"
            );
        }
        assert_eq!(planning["prompt_cache_key"], request.session_id.to_string());
        let allowed = |body: &Value, name: &str| {
            body["tool_choice"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == name)
        };
        assert_eq!(
            allowed(&planning, "arany_finish"),
            !matches!(policy, CollaborationPolicy::Team { .. })
        );
        assert_eq!(
            allowed(&planning, "arany_delegate"),
            policy.max_children() > 0
        );
        assert!(allowed(&synthesis, "arany_finish"));
        assert!(!allowed(&synthesis, "arany_delegate"));
        let delegate = response(
            "completed",
            json!([function_call(
                "arany_delegate",
                &json!({"children": ["inspect"]})
            )]),
        );
        assert!(
            decode(&request, &delegate).is_err(),
            "synthesis cannot delegate from the cached catalog"
        );
        request.phase = AgentPhase::RootPlan;
        let finish = response(
            "completed",
            json!([function_call(
                "arany_finish",
                &json!({"summary": "done", "result": "answer"})
            )]),
        );
        assert_eq!(
            decode(&request, &finish).is_ok(),
            !matches!(policy, CollaborationPolicy::Team { .. })
        );
    }

    let rows: Vec<Value> =
        serde_json::from_str(include_str!("../../../../tests/fixtures/tool-calls.json")).unwrap();
    assert_eq!(rows.len(), 10);
    for (catalog, accepted) in [
        (
            json!({"write":true,"commands":[],"skills":[],"mcp_servers":[]}),
            6,
        ),
        (
            json!({"write":false,"commands":[],"skills":[],"mcp_servers":[]}),
            3,
        ),
        (
            json!({"write":true,"commands":[{"name":"bash"}],"skills":[{"name":"example"}],"mcp_servers":[{"name":"example"}]}),
            10,
        ),
        (
            json!({"write":false,"commands":[{"name":""}],"skills":[{"name":"../bad"}],"mcp_servers":[{"name":""}]}),
            3,
        ),
    ] {
        request.input.tools = Some(tool_context(catalog));
        let rendered = body(&request);
        let valid = |row: &Value| {
            let mut arguments = row.clone();
            let operation = arguments
                .as_object_mut()
                .unwrap()
                .remove("operation")
                .unwrap();
            arguments["plan_contract"] = Value::Null;
            let name = format!("arany_{}", operation.as_str().unwrap());
            rendered["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|function| function["name"] == name)
                .is_some_and(|function| {
                    jsonschema::options()
                        .offline()
                        .build(&function["parameters"])
                        .unwrap()
                        .is_valid(&arguments)
                })
        };
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(
                valid(row),
                index < accepted,
                "catalog Tool availability: {index}"
            );
            if index >= 6 {
                let mut unnamed = row.clone();
                let field = match index {
                    6 => "program",
                    7 => "name",
                    _ => "server",
                };
                unnamed[field] = json!("");
                assert!(
                    !valid(&unnamed),
                    "unconfigured/empty Tool name must not be offered"
                );
            }
        }
    }
}

#[test]
fn inbound_function_calls_decode_one_native_request_or_outcome() {
    let mut request = primary_request(MODEL);
    request.collaboration = CollaborationPolicy::Auto {
        max_active_children: 2,
    };
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("../../../../tests/fixtures/tool-calls.json")).unwrap();
    for row in rows {
        let mut arguments = row.clone();
        let operation = arguments
            .as_object_mut()
            .unwrap()
            .remove("operation")
            .unwrap();
        arguments["plan_contract"] = Value::Null;
        let fixture = response(
            "completed",
            json!([
                reasoning(),
                function_call(
                    &format!("arany_{}", operation.as_str().unwrap()),
                    &arguments
                )
            ]),
        );
        for decoded in [
            decode(&request, &fixture).unwrap(),
            decode_streamed(&request, &fixture).unwrap(),
        ] {
            let ProviderOutcome::Tools(requests) = decoded.outcome else {
                panic!("native Tool request");
            };
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].id.as_str(), "call_test");
            assert!(requests[0].plan_contract.is_none());
            assert_eq!(serde_json::to_value(&requests[0].call).unwrap(), row);
            assert_eq!(
                decoded.turn.items("openai").unwrap().len(),
                2,
                "reasoning stays in the turn"
            );
        }
    }
    let contract = serde_json::to_value(goal_plan_contract()).unwrap();
    let mut with_contract =
        json!({"path": "src/new.rs", "expected_digest": null, "content": "verified\n"});
    with_contract["plan_contract"] = contract.clone();
    let planned = decode(
        &request,
        &response(
            "completed",
            json!([function_call("arany_write", &with_contract)]),
        ),
    )
    .unwrap();
    assert!(matches!(
        planned.outcome,
        ProviderOutcome::Tools(requests) if serde_json::to_value(requests[0].plan_contract.as_deref().unwrap()).unwrap() == contract
    ));

    for (name, arguments) in [
        ("arany_finish", json!({"summary":"done","result":"applied"})),
        (
            "arany_finish",
            json!({"summary":"done","result":"\"".repeat(12 * 1024)}),
        ),
        (
            "arany_delegate",
            json!({"children":["review supplied data"]}),
        ),
    ] {
        let fixture = response("completed", json!([function_call(name, &arguments)]));
        let decoded = decode(&request, &fixture).unwrap();
        assert!(!matches!(decoded.outcome, ProviderOutcome::Tools(_)));
        assert!(decode_streamed(&request, &fixture).is_ok());
        assert!(
            decode(&read_only_request(MODEL), &fixture).is_err(),
            "read-only encoding must not accept functions"
        );
    }
    let read = function_call("arany_read", &read_arguments());
    for (case, output) in [
        ("no output", json!([])),
        (
            "two calls",
            json!([
                read.clone(),
                function_call_with("call_2", "fc_2", "arany_read", &read_arguments())
            ]),
        ),
        (
            "call and message",
            json!([read.clone(), message("ignored final answer")]),
        ),
        (
            "unknown name",
            json!([function_call("arany_unknown", &json!({}))]),
        ),
        (
            "read bounds",
            json!([function_call(
                "arany_read",
                &json!({"path":"README.md","offset":0,"limit":4097,"plan_contract":null})
            )]),
        ),
        (
            "path escape",
            json!([function_call(
                "arany_read",
                &json!({"path":"../outside","offset":0,"limit":4096,"plan_contract":null})
            )]),
        ),
        (
            "apply_changeset",
            json!([function_call("arany_apply_changeset", &json!({}))]),
        ),
        (
            "operation injected",
            json!([function_call(
                "arany_read",
                &json!({"operation":"edit","path":"README.md","offset":0,"limit":4096,"plan_contract":null})
            )]),
        ),
        (
            "duplicate keys",
            json!([function_call_raw(
                "arany_read",
                "{\"path\":\"one\",\"path\":\"two\",\"offset\":0,\"limit\":4096,\"plan_contract\":null}"
            )]),
        ),
        (
            "non-object arguments",
            json!([function_call_raw("arany_read", "[]")]),
        ),
        (
            "unoffered outcome",
            json!([function_call("arany_delegate", &json!({"children":["x"]}))]),
        ),
    ] {
        let mut scoped = request.clone();
        if case == "unoffered outcome" {
            scoped.collaboration = CollaborationPolicy::Single;
        }
        assert!(
            decode(&scoped, &response("completed", output.clone())).is_err(),
            "{case}"
        );
    }
    for (field, value) in [
        ("status", json!("in_progress")),
        ("call_id", json!("")),
        ("id", Value::Null),
    ] {
        let mut bad = read.clone();
        bad[field] = value;
        assert!(
            decode(&request, &response("completed", json!([bad]))).is_err(),
            "{field}"
        );
    }
}

fn function_call_raw(name: &str, arguments: &str) -> Value {
    json!({"type":"function_call","id":"fc_test","call_id":"call_test","name":name,"arguments":arguments,"status":"completed"})
}

#[test]
fn rejected_native_calls_report_their_diagnostic_stage() {
    use crate::diagnostics::SubscriptionFailureStage as Stage;
    let request = primary_request(MODEL);
    for (call, expected) in [
        (
            json!({"operation":"read","path":"README.md","offset":0,"limit":4097}),
            Stage::OutcomeReadBounds,
        ),
        (
            json!({"operation":"edit","path":"README.md","expected_digest":"unavailable","old":"old","new":"new"}),
            Stage::OutcomeToolDigest,
        ),
        (
            json!({"operation":"edit","path":"README.md","expected_digest":"0".repeat(64),"old":"","new":"date"}),
            Stage::OutcomeToolEmptyEdit,
        ),
        (
            json!({"operation":"read","path":"./README.md","offset":0,"limit":4096}),
            Stage::OutcomeToolPath,
        ),
        (
            json!({"operation":"edit","path":"README.md","expected_digest":"0".repeat(64),"old":"x".repeat(4096),"new":"x".repeat(4097)}),
            Stage::OutcomeToolTextSize,
        ),
        (
            json!({"operation":"command","program":"/usr/bin/date","args":[],"cwd":""}),
            Stage::OutcomeToolProgram,
        ),
        (
            json!({"operation":"write","path":"README.md","expected_digest":"0".repeat(64),"content":"x".repeat(16385)}),
            Stage::OutcomeToolArgumentSize,
        ),
    ] {
        let mut arguments = call.clone();
        let operation = arguments
            .as_object_mut()
            .unwrap()
            .remove("operation")
            .unwrap();
        arguments["plan_contract"] = Value::Null;
        let fixture = response(
            "completed",
            json!([function_call(
                &format!("arany_{}", operation.as_str().unwrap()),
                &arguments
            )]),
        );
        assert_eq!(
            decode_streamed(&request, &fixture).unwrap_err(),
            ResponseError::Outcome
        );
        assert!(matches!(
            ResponseError::Outcome.provider_error(),
            ProviderError::InvalidOutcomeContract
        ));
        let bytes =
            serde_json::to_vec(&json!({"type":"response.completed","response":fixture})).unwrap();
        assert_eq!(outcome_failure_stage(&bytes, MODEL), expected);
        let (stage, shape) = outcome_failure_diagnostic(&bytes, MODEL);
        assert_eq!(stage, expected);
        if expected != Stage::OutcomeToolArgumentSize {
            let shape = format!(
                "{:?}",
                shape.expect("rejected typed Tool keeps numeric shape")
            );
            assert!(shape.contains(&format!(
                "operation: {:?}",
                call["operation"].as_str().unwrap()
            )));
            assert!(
                !shape.contains("README.md")
                    && !shape.contains("/usr/bin/date")
                    && !shape.contains("unavailable")
            );
        }
    }
    for (text, expected) in [
        ("Pending read result", Stage::OutcomeEncoding),
        (
            r#"{"outcome":{"type":"tool","call":{"operation":"read","path":"README.md","limit":4096}}}"#,
            Stage::OutcomeFields,
        ),
    ] {
        let fixture = json!({"type":"response.completed","response":response("completed", json!([message(text)]))});
        let bytes = serde_json::to_vec(&fixture).unwrap();
        assert_eq!(
            decode_streamed(&read_only_request(MODEL), &fixture["response"]).unwrap_err(),
            ResponseError::Outcome
        );
        assert_eq!(outcome_failure_stage(&bytes, MODEL), expected);
    }
}

#[test]
fn inbound_wire_corpus_requires_one_completed_structured_answer() {
    let request = read_only_request(MODEL);
    let finish = response(
        "completed",
        json!([{"type": "reasoning"}, message(finish_text())]),
    );
    let parsed: WireResponse = serde_json::from_value(finish).unwrap();
    let (id, usage, text) = parsed.completed_text(MODEL).unwrap();
    assert_eq!(id, "resp_example");
    assert_eq!(usage.unwrap().output_tokens, 8);
    assert!(matches!(
        structured_outcome(&text).unwrap(),
        ProviderOutcome::Finish(Finish { summary, result }) if summary == "done" && result == "answer"
    ));
    let finish_response = response("completed", json!([message(finish_text())]));
    let accepted = decode(&request, &finish_response).expect("completed Responses result");
    assert_eq!(
        accepted.wire_provenance,
        Some(ProviderWireProvenance::ResponsesCompletedStoreFalseRequested)
    );
    assert_eq!(accepted.usage.input_tokens, Some(12));
    assert_eq!(accepted.turn.items("openai").unwrap().len(), 1);
    for compaction in [false, true] {
        let text = if compaction {
            r#"{"summary":"short"}"#
        } else {
            finish_text()
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
                assert_eq!(decode_compaction(&bytes, MODEL).unwrap().summary, "short");
                assert_eq!(
                    decode_streamed_compaction(&streamed, MODEL)
                        .unwrap()
                        .summary,
                    "short"
                );
            } else {
                for accepted in [
                    decode(&request, &fixture).unwrap(),
                    decode_streamed(&request, &fixture).unwrap(),
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
                assert!(decode_compaction(&bytes, MODEL).is_err());
                assert_eq!(
                    format!(
                        "{:?}",
                        decode_streamed_compaction(&streamed, MODEL).unwrap_err()
                    ),
                    expected,
                );
            } else {
                assert!(decode(&request, &fixture).is_err());
                assert_eq!(
                    format!("{:?}", decode_streamed(&request, &fixture).unwrap_err()),
                    expected,
                );
            }
        }
    }
    let compact_response = response("completed", json!([message(r#"{"summary":"short"}"#)]));
    let compact = decode_compaction(&serde_json::to_vec(&compact_response).unwrap(), MODEL)
        .expect("completed Responses compaction");
    assert_eq!(compact.summary, "short");
    for malformed in [
        json!({"type":"output_text"}),
        json!({"type":"output_text","text":null}),
    ] {
        for (compaction, valid) in [(false, finish_text()), (true, r#"{"summary":"short"}"#)] {
            let fixture = response(
                "completed",
                json!([
                    {"type":"message","role":"assistant","status":"completed","content":[malformed]},
                    message(valid)
                ]),
            );
            let bytes = serde_json::to_vec(&fixture).unwrap();
            if compaction {
                assert!(
                    decode_compaction(&bytes, MODEL).is_err(),
                    "malformed text precedes valid summary"
                );
            } else {
                assert!(
                    decode(&request, &fixture).is_err(),
                    "malformed text precedes valid outcome"
                );
                assert!(decode_streamed(&request, &fixture).is_err());
            }
        }
    }
    assert_eq!(
        compact.wire_provenance,
        Some(ProviderWireProvenance::ResponsesCompletedStoreFalseRequested)
    );
    for model in ["gpt-6-astra", "gpt-6.1-sol", "gpt-6-luna"] {
        let mut fixture = response("completed", json!([message(finish_text())]));
        fixture["model"] = json!(model);
        let mut scoped = request.clone();
        scoped.model = model.into();
        assert!(matches!(
            decode(&scoped, &fixture),
            Ok(ProviderResponse {
                outcome: ProviderOutcome::Finish(_),
                ..
            })
        ));
        assert!(decode(&request, &fixture).is_err());
    }
    assert!(matches!(
        structured_outcome(r#"{"outcome":{"type":"delegate","children":["a","b"]}}"#).unwrap(),
        ProviderOutcome::Delegate(Delegate { children }) if children == ["a", "b"]
    ));
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
            parsed.completed_text(MODEL).is_err(),
            "bad wire case {index}"
        );
    }
    for field in ["error", "incomplete_details"] {
        for value in [
            json!({"reason":"incomplete"}),
            json!(false),
            json!("invalid"),
        ] {
            let mut fixture = response("completed", json!([message(finish_text())]));
            fixture[field] = value;
            assert_eq!(
                decode_streamed(&request, &fixture).unwrap_err(),
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
        r#"{"outcome":{"type":"tool","call":{"operation":"list","path":"."}}}"#,
    ] {
        assert!(structured_outcome(text).is_err(), "bad outcome shape");
    }
    assert!(serde_json::from_str::<SummaryEnvelope>(r#"{"summary":"ok","extra":1}"#).is_err());
    assert!(encode_request(&json!({"oversize": "x".repeat(MAX_REQUEST_BYTES)})).is_err());
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
    assert_no_schema_bounds(
        &run_body_for(&read_only_request(MODEL), Some(Effort::Low)),
        "read_only",
    );
    let mut request = primary_request(MODEL);
    for (label, phase) in [
        ("plan", AgentPhase::RootPlan),
        ("synthesis", AgentPhase::RootSynthesis),
    ] {
        request.phase = phase;
        assert_no_schema_bounds(&run_body_for(&request, Some(Effort::Low)), label);
    }
    request.input.goal_plan_contract = Some(goal_plan_contract());
    assert_no_schema_bounds(&run_body_for(&request, Some(Effort::Low)), "continued");
}

#[test]
fn steering_notes_are_user_input_data_never_instructions() {
    let mut request = read_only_request(MODEL);
    request.input.steering_notes = vec!["steering canary".into()];
    let rendered = body(&request);
    let instructions = rendered["instructions"].as_str().unwrap();
    assert!(instructions.contains("Steering notes are messages"));
    assert!(!instructions.contains("steering canary"));
    assert!(rendered["input"].to_string().contains("steering canary"));
    let plain = body(&read_only_request(MODEL));
    assert!(
        !plain["input"]
            .to_string()
            .contains("Steering notes the user sent")
    );
}

#[test]
fn child_research_request_offers_only_read_only_functions_and_finish() {
    let request = researcher_request(MODEL);
    let rendered = body(&request);
    assert_eq!(rendered["tool_choice"]["type"], "allowed_tools");
    assert_eq!(rendered["tool_choice"]["mode"], "required");
    assert_eq!(rendered["parallel_tool_calls"], false);
    assert!(
        rendered["instructions"]
            .as_str()
            .unwrap()
            .starts_with(crate::provider::prompts::RESEARCH_INSTRUCTIONS)
    );
    assert_eq!(
        function_names(&rendered),
        [
            "arany_list",
            "arany_read",
            "arany_search",
            "arany_skill",
            "arany_mcp_list",
            "arany_finish"
        ]
    );
    assert!(
        rendered["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|function| {
                function["parameters"]["properties"]
                    .get("plan_contract")
                    .is_none()
            })
    );
    let read = response(
        "completed",
        json!([function_call(
            "arany_read",
            &json!({"path": "src/lib.rs", "offset": 0, "limit": 1024})
        )]),
    );
    assert!(matches!(
        decode(&request, &read).unwrap().outcome,
        ProviderOutcome::Tools(requests) if requests[0].plan_contract.is_none()
    ));
    let mut spent = catalog();
    spent["tool_calls_remaining"] = json!(0);
    let mut exhausted = request.clone();
    exhausted.input.tools = Some(tool_context(spent));
    let spent_body = body(&exhausted);
    assert_eq!(function_names(&spent_body), function_names(&rendered));
    assert_eq!(spent_body["tools"], rendered["tools"]);
    assert_eq!(spent_body["instructions"], rendered["instructions"]);
    assert_eq!(spent_body["prompt_cache_key"], rendered["prompt_cache_key"]);
    assert!(decode(&exhausted, &read).is_err());
    assert_eq!(
        spent_body["tool_choice"],
        json!({"type": "allowed_tools", "mode": "required", "tools": [{"type": "function", "name": "arany_finish"}]})
    );
    assert_eq!(rendered["tool_choice"]["type"], "allowed_tools");
    assert_eq!(rendered["tool_choice"]["mode"], "required");
}

#[test]
fn git_reads_encode_as_strict_native_functions() {
    use crate::tools::ToolCall;
    let mut request = primary_request(MODEL);
    let git_catalog = |git: Value| {
        tool_context(
            json!({"write": false, "commands": [], "skills": [], "mcp_servers": [], "git": git}),
        )
    };
    request.input.tools = Some(git_catalog(
        json!({"operations": ["git_status", "git_diff", "git_log"]}),
    ));
    let rendered = run_body_for(&request, Some(Effort::Low));
    let functions = rendered["tools"].as_array().unwrap();
    for name in ["arany_git_status", "arany_git_diff", "arany_git_log"] {
        let function = functions
            .iter()
            .find(|function| function["name"] == name)
            .unwrap_or_else(|| panic!("{name}"));
        assert_eq!(function["strict"], true);
        assert!(!function["description"].as_str().unwrap().is_empty());
        assert!(
            function["parameters"]["properties"]["plan_contract"]["anyOf"]
                .as_array()
                .unwrap()
                .contains(&json!({"type": "null"}))
        );
    }
    for (call, name) in [
        (ToolCall::GitStatus { path: ".".into() }, "arany_git_status"),
        (
            ToolCall::GitDiff {
                path: "src".into(),
                base: Some("HEAD~1".into()),
                head: Some("HEAD".into()),
                staged: false,
            },
            "arany_git_diff",
        ),
        (
            ToolCall::GitLog {
                path: ".".into(),
                revision: None,
                max_count: 5,
            },
            "arany_git_log",
        ),
    ] {
        let mut arguments = serde_json::to_value(&call).unwrap();
        arguments.as_object_mut().unwrap().remove("operation");
        arguments["plan_contract"] = Value::Null;
        let decoded = decode(
            &request,
            &response("completed", json!([function_call(name, &arguments)])),
        )
        .unwrap();
        let ProviderOutcome::Tools(requests) = decoded.outcome else {
            panic!("{name} did not decode as a Tool request");
        };
        assert_eq!(requests[0].call, call);
        assert!(requests[0].plan_contract.is_none());
    }
    for catalog in [json!("unavailable"), Value::Null] {
        request.input.tools = Some(git_catalog(catalog));
        let rendered = run_body_for(&request, Some(Effort::Low));
        assert!(
            !function_names(&rendered)
                .iter()
                .any(|name| name.starts_with("arany_git_"))
        );
    }
}

fn compaction_bound_request(
    model: &str,
    body_of: fn(&CompactionRequest) -> Value,
) -> (CompactionRequest, Value) {
    let request = |user: String| CompactionRequest {
        session_id: SessionId::new(),
        covered_run_id: RunId::new(),
        model: model.into(),
        previous_summary: None,
        items: vec![CompactionItem::Completed(HistoryTurn {
            user,
            assistant: String::new(),
        })],
        max_output_tokens: crate::session::MAX_COMPACTION_OUTPUT_TOKENS,
    };
    let base = crate::provider::compaction_input_bytes(&request(String::new())).expect("input");
    let slack = crate::session::MAX_COMPACTION_INPUT_BYTES - base;
    // Each quote serializes to two input bytes and four request bytes: the worst expansion.
    let at_bound = request(format!(
        "{}{}",
        "\"".repeat(slack / 2),
        "a".repeat(slack % 2)
    ));
    assert_eq!(
        crate::provider::compaction_input_bytes(&at_bound).expect("input"),
        crate::session::MAX_COMPACTION_INPUT_BYTES
    );
    let body = body_of(&at_bound);
    (at_bound, body)
}

#[test]
fn compaction_envelope_and_bound_fit_the_request_ceiling() {
    let empty = CompactionRequest {
        session_id: SessionId::new(),
        covered_run_id: RunId::new(),
        model: "gpt-5.4".into(),
        previous_summary: None,
        items: Vec::new(),
        max_output_tokens: crate::session::MAX_COMPACTION_OUTPUT_TOKENS,
    };
    let envelope = serde_json::to_vec(&compaction_body_with_effort(
        &empty,
        crate::provider::Effort::High,
    ))
    .expect("body")
    .len();
    assert!(
        envelope <= crate::provider::COMPACTION_REQUEST_ENVELOPE_BYTES,
        "envelope {envelope} bytes"
    );
    let (_, body) = compaction_bound_request("gpt-5.4", |request| {
        compaction_body_with_effort(request, crate::provider::Effort::High)
    });
    let bytes = encode_request(&body).expect("a compaction request at the bound is sent");
    assert!(bytes.len() <= crate::provider::MAX_PROVIDER_REQUEST_BYTES);
}

#[test]
fn tool_surface_matches_the_checked_in_snapshot() {
    let rendered: serde_json::Map<String, Value> =
        crate::provider::test_support::tool_surface_requests(MODEL)
            .into_iter()
            .map(|(label, request)| (label.to_owned(), body(&request)["tools"].clone()))
            .collect();
    let rendered = serde_json::to_string_pretty(&rendered).unwrap() + "\n";
    // Regenerate with ARANY_UPDATE_SNAPSHOTS=1, then review the diff.
    if std::env::var_os("ARANY_UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/provider/snapshots/openai_tools.json"
            ),
            &rendered,
        )
        .unwrap();
    }
    assert!(
        rendered == include_str!("../../snapshots/openai_tools.json"),
        "{rendered}"
    );
}

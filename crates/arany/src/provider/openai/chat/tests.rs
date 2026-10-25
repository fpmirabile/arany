use super::*;
use crate::provider::conversation::{Block, Message, Role};
use crate::provider::{Effort, PromptRole, RunInput, render, test_support::read_only_request};

const MODEL: &str = "chat-model";
const KEY: &str = "synthetic-chat-key";

fn request() -> ProviderRequest {
    read_only_request(MODEL)
}

fn answer_reply() -> Value {
    json!({
        "id":"chatcmpl_safe",
        "model":MODEL,
        "choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"safe answer"}}],
        "usage":{"prompt_tokens":12,"completion_tokens":8,"total_tokens":20}
    })
}

#[test]
fn request_body_uses_the_non_streaming_chat_contract() {
    let mut request = request();
    request.phase = crate::provider::AgentPhase::RootPlan;
    request.collaboration = crate::CollaborationPolicy::Team {
        max_active_children: 1,
    };
    request.input = RunInput::new(PromptRole::ReadOnly, "snapshot prompt".into());
    let conversation = render::conversation(&request).unwrap();
    let prompt = include_str!("../../prompts/read_only.txt");
    use sha2::Digest;
    let prompt_digest: String = sha2::Sha256::digest(prompt.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        prompt_digest,
        "e698d8bb1000a637f926a1b7f27a1b2c5ae35631dd0fac32ce3c8bbbef5cf1e2"
    );
    let schema = json!({
        "type":"object",
        "properties":{
            "children":{"type":"array","items":{"type":"object","properties":{
                "objective":{"type":"string"},
                "profile":{"type":["string","null"],"enum":[null],"description":"Eligible admitted profile, or null for the pinned child default."}
            },"required":["objective","profile"],"additionalProperties":false}},
            "agent":{"type":["string","null"],"description":"Admitted agent definition name, or null. An editor assignment selects its own exact admitted definition."}
        },"required":["children","agent"],"additionalProperties":false
    });
    let expected_messages = json!([
        {"role":"system","content":format!("{prompt}\n\nPinned collaboration policy: {}.", serde_json::to_string(&request.collaboration).unwrap())},
        {"role":"user","content":"snapshot prompt"}
    ]);
    let mut expected = json!({
        "model":"chat-model",
        "messages":expected_messages,
        "max_completion_tokens":4096,
        "stream":false,
        "parallel_tool_calls":false,
        "tools":[{"type":"function","function":{"name":"arany_delegate","description":"Delegate a bounded team of researchers and offered editor assignments in the next settled wave. The primary integrates private edits and verifies the criterion before completing.","parameters":schema,"strict":true}}],
        "tool_choice":"auto"
    });
    for (effort, reasoning_effort) in [(None, None), (Some(Effort::High), Some("high"))] {
        if let Some(reasoning_effort) = reasoning_effort {
            expected["reasoning_effort"] = json!(reasoning_effort);
        }
        let body = run_body(&request, &conversation, effort).unwrap();
        assert_eq!(
            serde_json::to_vec(&body).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
        assert!(
            body["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("You are Arany's read-only assistant")
        );
    }
}

#[test]
fn answer_decoder_requires_one_exactly_metered_choice() {
    let request = request();
    let conversation = render::conversation(&request).unwrap();
    let offered = Vec::new();
    let schema = conversation.outcome.clone();
    let decode = |value: &Value| {
        decode_run(
            &serde_json::to_vec(value).unwrap(),
            &request,
            KEY,
            &offered,
            &schema,
        )
    };
    let accepted = decode(&answer_reply()).unwrap();
    assert_eq!(accepted.response_id.as_deref(), Some("chatcmpl_safe"));
    assert_eq!(accepted.usage.input_tokens, Some(12));
    assert_eq!(accepted.usage.output_tokens, Some(8));
    assert_eq!(
        accepted.wire_provenance,
        Some(ProviderWireProvenance::ChatCompletionsStopStorageUnspecified)
    );

    let mutations: [fn(&mut Value); 5] = [
        |value| {
            let first = value["choices"][0].clone();
            value["choices"].as_array_mut().unwrap().push(first);
        },
        |value: &mut Value| value["choices"][0]["finish_reason"] = json!("length"),
        |value: &mut Value| {
            value.as_object_mut().unwrap().remove("usage");
        },
        |value: &mut Value| value["usage"]["total_tokens"] = json!(21),
        |value: &mut Value| value["model"] = json!("other-model"),
    ];
    for (index, mutate) in mutations.into_iter().enumerate() {
        let mut rejected = answer_reply();
        mutate(&mut rejected);
        let error = decode(&rejected).unwrap_err();
        if index == 0 {
            assert!(matches!(error, ProviderError::InvalidResponseContract));
        } else {
            assert!(error.failure_reason().is_some());
        }
    }
}

#[test]
fn decoder_rejects_selected_key_reflection_and_sse_envelopes() {
    let request = request();
    let conversation = render::conversation(&request).unwrap();
    let decode = |bytes: &[u8]| decode_run(bytes, &request, KEY, &[], &conversation.outcome);
    let reflected = answer_reply().to_string().replace("safe answer", KEY);
    assert!(decode(reflected.as_bytes()).is_err());
    let sse = format!("data: {}\n\n", answer_reply());
    assert!(decode(sse.as_bytes()).is_err());
}

#[test]
fn one_offered_delegate_call_decodes_as_a_typed_outcome() {
    let mut request = request();
    request.phase = crate::provider::AgentPhase::RootPlan;
    request.input = RunInput::new(PromptRole::ReadOnly, "delegate objective".into());
    request.collaboration = crate::CollaborationPolicy::Team {
        max_active_children: 1,
    };
    let conversation = render::conversation(&request).unwrap();
    let body = run_body(&request, &conversation, None).unwrap();
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["tools"][0]["function"]["name"], "arany_delegate");
    let arguments = json!({
        "children":[{"objective":"Inspect the plan","profile":null}],
        "agent":null
    })
    .to_string();
    let reply = json!({
        "id":"chatcmpl_delegate",
        "model":MODEL,
        "choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_delegate","type":"function","function":{"name":"arany_delegate","arguments":arguments}}]}}],
        "usage":{"prompt_tokens":14,"completion_tokens":5,"total_tokens":19}
    });
    let response = decode_run(
        &serde_json::to_vec(&reply).unwrap(),
        &request,
        KEY,
        &[],
        &conversation.outcome,
    )
    .unwrap();
    assert!(matches!(response.outcome, ProviderOutcome::Delegate(_)));

    let mut replay = conversation;
    replay.messages.push(Message {
        role: Role::Assistant,
        blocks: vec![Block::AssistantTurn(response.turn)],
    });
    replay.messages.push(Message {
        role: Role::User,
        blocks: vec![Block::ToolResult {
            id: ToolUseId::new("call_delegate").unwrap(),
            output: json!({"summary":"child result"}),
            is_error: false,
        }],
    });
    let replay_body = run_body(&request, &replay, None).unwrap();
    assert_eq!(
        replay_body["messages"][2],
        json!({"role":"assistant","content":null,"tool_calls":[{"id":"call_delegate","type":"function","function":{"name":"arany_delegate","arguments":arguments}}]})
    );
    assert_eq!(
        replay_body["messages"][3],
        json!({"role":"tool","tool_call_id":"call_delegate","content":"{\"summary\":\"child result\"}"})
    );
}

#[test]
fn tool_review_uses_strict_json_schema_and_decodes_a_typed_verdict() {
    let mut request = request();
    request.phase = crate::provider::AgentPhase::ToolReview;
    request.collaboration = crate::CollaborationPolicy::Single;
    request.input = RunInput::new(PromptRole::ToolReview, "review objective".into());
    let conversation = render::conversation(&request).unwrap();
    let body = run_body(&request, &conversation, None).unwrap();
    assert_eq!(body["response_format"]["type"], "json_schema");
    assert_eq!(body["response_format"]["json_schema"]["strict"], true);
    assert_eq!(
        body["response_format"]["json_schema"]["schema"],
        conversation.outcome
    );
    assert!(body.get("tools").is_none());

    let mut reply = answer_reply();
    reply["choices"][0]["message"]["content"] = json!(r#"{"verdict":"approve"}"#);
    let response = decode_run(
        &serde_json::to_vec(&reply).unwrap(),
        &request,
        KEY,
        &[],
        &conversation.outcome,
    )
    .unwrap();
    assert_eq!(
        response.outcome,
        ProviderOutcome::Review(crate::provider::ReviewVerdict::Approve)
    );
}

use super::*;
use crate::provider::conversation::{Block, Message, Role};
use crate::provider::{Effort, PromptRole, RunInput, render, test_support::read_only_request};

const MODEL: &str = "chat-model";
const KEY: &str = "synthetic-chat-key";

type RejectionCase = (&'static str, fn(&mut Value));

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

fn delegate_request() -> ProviderRequest {
    let mut request = request();
    request.phase = crate::provider::AgentPhase::RootPlan;
    request.input = RunInput::new(PromptRole::ReadOnly, "delegate objective".into());
    request.collaboration = crate::CollaborationPolicy::Team {
        max_active_children: 1,
    };
    request
}

fn delegate_reply() -> Value {
    let mut reply = answer_reply();
    reply["choices"][0] = json!({
        "finish_reason":"tool_calls",
        "message":{"role":"assistant","content":null,"tool_calls":[{
            "id":"call_delegate","type":"function","function":{
                "name":"arany_delegate",
                "arguments":json!({"children":[{"objective":"Inspect the plan","profile":null}],"agent":null}).to_string()
            }
        }]}
    });
    reply
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
    let body = run_body(&request, &conversation, None).unwrap();
    for field in ["tools", "tool_choice", "parallel_tool_calls"] {
        assert!(body.get(field).is_none());
    }
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
    let request = delegate_request();
    let conversation = render::conversation(&request).unwrap();
    let body = run_body(&request, &conversation, None).unwrap();
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["tools"][0]["function"]["name"], "arany_delegate");
    let reply = delegate_reply();
    let arguments = reply["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
        .as_str()
        .unwrap();
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
fn call_shapes_reject_multiple_mixed_and_unoffered_calls() {
    let request = delegate_request();
    let conversation = render::conversation(&request).unwrap();
    let cases: [RejectionCase; 4] = [
        ("multiple calls", |value| {
            let call = value["choices"][0]["message"]["tool_calls"][0].clone();
            value["choices"][0]["message"]["tool_calls"]
                .as_array_mut()
                .unwrap()
                .push(call);
        }),
        ("mixed answer and call", |value| {
            value["choices"][0]["finish_reason"] = json!("stop");
            value["choices"][0]["message"]["content"] = json!("answer");
        }),
        ("non-null content with call", |value| {
            value["choices"][0]["message"]["content"] = json!("");
        }),
        ("unoffered function", |value| {
            value["choices"][0]["message"]["tool_calls"][0]["function"]["name"] =
                json!("arany_read");
        }),
    ];
    for (case, mutate) in cases {
        let mut reply = delegate_reply();
        mutate(&mut reply);
        assert!(
            decode_run(
                reply.to_string().as_bytes(),
                &request,
                KEY,
                &[],
                &conversation.outcome
            )
            .is_err(),
            "{case}"
        );
    }
}

#[test]
fn function_selection_and_arguments_keep_the_outcome_contract_reason() {
    let request = delegate_request();
    let conversation = render::conversation(&request).unwrap();
    for (name, arguments) in [
        ("arany_read", "{}"),
        ("external_delegate", "{}"),
        ("arany_delegate", "{"),
        ("arany_delegate", "[]"),
        ("arany_delegate", r#"{"children":"invalid","agent":null}"#),
    ] {
        let mut reply = delegate_reply();
        reply["choices"][0]["message"]["tool_calls"][0]["function"] =
            json!({"name":name,"arguments":arguments});
        assert!(matches!(
            decode_run(
                reply.to_string().as_bytes(),
                &request,
                KEY,
                &[],
                &conversation.outcome
            ),
            Err(ProviderError::InvalidOutcomeContract)
        ));
    }
}

#[test]
fn compaction_requires_bounded_string_content_without_calls_or_refusals() {
    let limit = crate::session::MAX_COMPACTION_SUMMARY_BYTES;
    let summary = format!("{}.", " ".repeat(limit - 1));
    let mut accepted = answer_reply();
    accepted["choices"][0]["message"]["content"] = json!(summary);
    assert_eq!(
        decode_compaction(accepted.to_string().as_bytes(), MODEL, 128)
            .unwrap()
            .summary,
        summary
    );

    let cases: [RejectionCase; 6] = [
        ("whitespace", |value| {
            value["choices"][0]["message"]["content"] = json!(" \n\t")
        }),
        ("overflow", |value| {
            value["choices"][0]["message"]["content"] =
                json!(".".repeat(crate::session::MAX_COMPACTION_SUMMARY_BYTES + 1))
        }),
        ("tool call", |value| *value = delegate_reply()),
        ("refusal", |value| {
            value["choices"][0]["message"]["refusal"] = json!("refused")
        }),
        ("content array", |value| {
            value["choices"][0]["message"]["content"] = json!([{"type":"text","text":"summary"}])
        }),
        ("length stop", |value| {
            value["choices"][0]["finish_reason"] = json!("length")
        }),
    ];
    for (case, mutate) in cases {
        let mut reply = answer_reply();
        mutate(&mut reply);
        assert!(
            decode_compaction(reply.to_string().as_bytes(), MODEL, 128).is_err(),
            "{case}"
        );
    }
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
    for field in ["tools", "tool_choice", "parallel_tool_calls"] {
        assert!(body.get(field).is_none());
    }

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
    assert!(
        decode_run(
            delegate_reply().to_string().as_bytes(),
            &request,
            KEY,
            &[],
            &conversation.outcome
        )
        .is_err()
    );
}

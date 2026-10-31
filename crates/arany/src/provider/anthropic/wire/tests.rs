use super::*;
use crate::provider::DEFAULT_OUTPUT_TOKEN_CAP;
use crate::provider::test_support::{
    assert_no_schema_bounds, catalog, compaction_bound_request, goal_plan_contract, observed_step,
    primary_request, read_only_request, researcher_request, tool_context,
};
use crate::provider::{AgentPhase, Answer, Delegate, HistoryTurn, UnansweredStatus, render};
use crate::session::{CollaborationPolicy, RunId, SessionId};

const KEY: &str = "synthetic-key";
const MODEL: &str = "claude-sonnet-5";

fn body(request: &ProviderRequest, effort: Effort) -> Value {
    let conversation = render::conversation(request).unwrap();
    run_body(request, &conversation, effort).unwrap()
}

fn decode(request: &ProviderRequest, fixture: &Value) -> Result<ProviderResponse, ProviderError> {
    decode_bytes(request, &serde_json::to_vec(fixture).unwrap())
}

fn decode_bytes(
    request: &ProviderRequest,
    bytes: &[u8],
) -> Result<ProviderResponse, ProviderError> {
    let conversation = render::conversation(request).unwrap();
    decode_run(
        bytes,
        &RunScope {
            model: &request.model,
            output_cap: request.max_output_tokens,
            secret: KEY,
            offered: &conversation
                .tools
                .clone()
                .unwrap_or_default()
                .into_iter()
                .chain(outcome_specs(&conversation.outcome))
                .collect::<Vec<_>>(),
            review: request.input.role == crate::provider::PromptRole::ToolReview,
        },
    )
}

fn response(stop_reason: &str, content: Value) -> Value {
    json!({
        "id": "msg_example",
        "type": "message",
        "role": "assistant",
        "model": MODEL,
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

fn answer_text() -> &'static str {
    r#"answer"#
}

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!({"type": "tool_use", "id": id, "name": name, "input": input})
}

fn read_input() -> Value {
    json!({"path": "src/main.rs", "offset": null, "limit": null})
}

fn tool_names(body: &Value) -> Vec<&str> {
    body["tools"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect()
}

fn outcome_types(body: &Value) -> Vec<&str> {
    tool_names(body)
        .into_iter()
        .filter(|name| *name == "arany_delegate")
        .collect()
}

#[test]
fn outbound_conversation_renders_system_tools_and_native_tool_results() {
    let mut request = primary_request(MODEL);
    let thinking = json!({"type": "thinking", "thinking": "private", "signature": "sig"});
    let reply = response(
        "tool_use",
        json!([
            thinking,
            text_block("looking"),
            tool_use("toolu_1", "arany_read", read_input())
        ]),
    );
    let step = observed_step(decode(&request, &reply).unwrap(), "{\"text\":\"fn main\"}");
    let replayed = step.turn.clone();
    request.input.tools.as_mut().unwrap().steps.push(step);
    let body = body(&request, Effort::Low);
    assert_eq!(body["model"], MODEL);
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["stream"], false);
    let system = body["system"].as_array().unwrap();
    assert_eq!(system.len(), 5);
    assert!(system.iter().all(|block| block["type"] == "text"));
    assert_eq!(
        system[0]["text"],
        crate::provider::EffectivePrompts::default().text(crate::provider::PromptFile::Primary)
    );
    assert_eq!(
        system[1]["text"],
        crate::provider::prompts::PRIMARY_AUTHORITY
    );
    assert_eq!(
        system[2]["text"],
        crate::provider::prompts::PLAN_INSTRUCTIONS
    );
    assert!(!body["system"].to_string().contains("canary"));
    assert_eq!(
        tool_names(&body),
        [
            "arany_list",
            "arany_read",
            "arany_search",
            "arany_git_status",
            "arany_git_diff",
            "arany_git_log",
            "arany_skill",
            "arany_mcp_list",
            "arany_mkdir",
            "arany_write",
            "arany_edit",
            "arany_command",
            "arany_mcp_call",
            "arany_command_unguarded",
            "arany_glob",
            "arany_git_commit",
            "arany_git_branch",
            "arany_git_switch",
            "arany_multiedit",
            "arany_delete",
            "arany_move",
            "arany_plan"
        ]
    );
    assert!(body["tools"].as_array().unwrap().iter().all(|tool| {
        tool.get("strict").is_none()
            && tool["input_schema"]["additionalProperties"] == false
            && tool["input_schema"]["properties"]
                .get("plan_contract")
                .is_none()
    }));
    let host = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "arany_command_unguarded")
        .unwrap();
    assert_eq!(
        host["input_schema"]["properties"]["prefix_rule"]["type"],
        json!(["array", "null"])
    );
    assert!(
        host["input_schema"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("prefix_rule"))
    );
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(
        messages
            .iter()
            .map(|message| message["role"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["user", "assistant", "user"]
    );
    assert_eq!(
        messages[1]["content"],
        serde_json::Value::Array(replayed.items("anthropic").unwrap()),
        "the assistant turn replays exactly, thinking included"
    );
    assert_eq!(messages[1]["content"][0], thinking);
    let results = messages[2]["content"].as_array().unwrap();
    assert_eq!(results[0]["type"], "tool_result");
    assert_eq!(results[0]["tool_use_id"], "toolu_1");
    assert_eq!(results[0]["is_error"], false);
    let result: Value = serde_json::from_str(results[0]["content"].as_str().unwrap()).unwrap();
    assert!(
        result.get("call").is_none(),
        "the replayed turn carries the call"
    );
    assert!(result.get("workspace_effect").is_none() && result.get("cache_update").is_none());
    assert!(
        results[1]["text"]
            .as_str()
            .unwrap()
            .contains("goal_context_facts")
    );
    assert!(encode_request(&body).is_ok());
    assert_eq!(body.to_string().matches("\"cache_control\":").count(), 3);
    let marker = json!({"type": "ephemeral"});
    assert_eq!(
        body["tools"].as_array().unwrap().last().unwrap()["cache_control"],
        marker
    );
    assert_eq!(system.last().unwrap()["cache_control"], marker);
    assert_eq!(results.last().unwrap()["cache_control"], marker);
    assert!(
        messages[0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|block| block.get("cache_control").is_none())
    );
}

#[test]
fn outbound_tool_choice_is_auto_with_parallel() {
    let tool_body = body(&primary_request(MODEL), Effort::Low);
    assert_eq!(tool_body["tool_choice"], json!({"type": "auto"}));
    assert_eq!(tool_body["output_config"]["effort"], "low");
    assert_eq!(outcome_types(&tool_body), [] as [&str; 0]);
    let mut editor = researcher_request(MODEL);
    editor.input.role = crate::provider::PromptRole::Editor;
    assert_eq!(
        body(&editor, Effort::Low)["tool_choice"],
        json!({"type": "auto", "disable_parallel_tool_use": true})
    );
    let plain = body(&read_only_request(MODEL), Effort::Low);
    for field in ["tools", "tool_choice"] {
        assert!(plain.get(field).is_none(), "{field}");
    }
    assert_eq!(plain.to_string().matches("\"cache_control\":").count(), 1);
}

#[test]
fn final_call_keeps_the_tool_list_and_offers_only_an_answer() {
    let mut request = primary_request(MODEL);
    let open = body(&request, Effort::Low);
    request.input.final_call = true;
    let final_call = body(&request, Effort::Low);
    assert_eq!(final_call["tools"], open["tools"]);
    assert_eq!(final_call["system"], open["system"]);
    assert_eq!(final_call["tool_choice"], json!({"type": "none"}));
    assert!(
        final_call["messages"]
            .to_string()
            .contains("harness_final_call")
    );
    let mut researcher = researcher_request(MODEL);
    researcher.input.final_call = true;
    assert!(render::conversation(&researcher).is_err());
}

#[test]
fn multiple_tool_blocks_preserve_order_and_pair_results_in_one_user_message() {
    let cap = usize::from(crate::session::MAX_STEP_TOOL_CALLS);
    for mut request in [primary_request(MODEL), researcher_request(MODEL)] {
        for count in [2, cap, cap + 1, MAX_CONTENT_BLOCKS] {
            request.input.tools.as_mut().unwrap().steps.clear();
            let content: Vec<_> = (0..count)
                .map(|index| tool_use(&format!("toolu_{index}"), "arany_read", read_input()))
                .collect();
            let reply = decode(&request, &response("tool_use", json!(content))).unwrap();
            assert_eq!(reply.usage.input_tokens, Some(19));
            let step = observed_step(reply, "{\"text\":\"bounded result\"}");
            assert_eq!(step.requests.len(), count);
            assert_eq!(step.turn.items(ADAPTER).unwrap(), content);
            if count > cap {
                continue;
            }
            request.input.tools.as_mut().unwrap().steps.push(step);
            let body = body(&request, Effort::Low);
            let messages = body["messages"].as_array().unwrap();
            let user = messages.last().unwrap();
            assert_eq!(user["role"], "user");
            let results: Vec<_> = user["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|block| block["type"] == "tool_result")
                .collect();
            assert_eq!(results.len(), count);
            for (index, result) in results.iter().enumerate() {
                assert_eq!(result["tool_use_id"], format!("toolu_{index}"));
                assert_eq!(result["is_error"], false);
            }
        }
        let blocks: Vec<_> = (0..=MAX_CONTENT_BLOCKS)
            .map(|index| tool_use(&format!("toolu_{index}"), "arany_read", read_input()))
            .collect();
        assert!(decode(&request, &response("tool_use", json!(blocks))).is_err());
    }
}

struct ResearchBatchProvider {
    identity: crate::provider::ProviderIdentity,
    fixtures: std::sync::Mutex<std::collections::VecDeque<Value>>,
    requests: std::sync::Mutex<Vec<ProviderRequest>>,
}

#[async_trait::async_trait]
impl crate::provider::Provider for ResearchBatchProvider {
    fn identity(&self) -> &crate::provider::ProviderIdentity {
        &self.identity
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        let fixture = self
            .fixtures
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted reply");
        decode(&request, &fixture)
    }
}

#[tokio::test]
async fn researcher_over_cap_refusals_commit_before_feedback_and_bound_continuations() {
    for attempts in [1, 3, 8] {
        let (provider, events, view) = run_research_batch_case(attempts).await;
        assert_research_batch_result(attempts, &provider, &events, &view);
    }
}

async fn run_research_batch_case(
    attempts: usize,
) -> (
    std::sync::Arc<ResearchBatchProvider>,
    Vec<crate::EventEnvelope>,
    crate::SessionView,
) {
    use crate::test_support::{engine, goals, scenario};
    use crate::{EngineConfig, RunControls, StateRoot, ToolGrant};
    use std::{os::unix::fs::PermissionsExt, sync::Arc};
    let temp = tempfile::tempdir().unwrap();
    let workspace = goals::facts_workspace(temp.path(), "bounded facts");
    let state = temp.path().join("state");
    let root = StateRoot::admit(&state).unwrap();
    let config = state.join("tools.json");
    std::fs::write(
        &config,
        br#"{"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#,
    )
    .unwrap();
    std::fs::set_permissions(config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let engine = engine::open(
        root,
        EngineConfig {
            guard_executable: Some(std::env::current_exe().unwrap()),
            ..Default::default()
        },
    )
    .unwrap();
    let provider = Arc::new(research_batch_provider(attempts));
    let mut input = scenario::request(&workspace, "Delegate bounded research");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    let outcome = engine::run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    engine.close().await.unwrap();
    let (events, view) = goals::replayed(&state, outcome.session_id).await;
    assert_eq!(view.runs[0], outcome.run);
    (provider, events, view)
}

fn assert_research_batch_result(
    attempts: usize,
    provider: &ResearchBatchProvider,
    events: &[crate::EventEnvelope],
    view: &crate::SessionView,
) {
    use crate::RunStatus;
    assert_eq!(
        view.runs[0].status,
        if attempts == 1 {
            RunStatus::Finished
        } else {
            RunStatus::Failed
        }
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, crate::Event::ToolStarted { .. }))
    );
    let child = &view.runs[0].agents[1];
    assert_eq!(
        child
            .provider_calls
            .iter()
            .filter(|call| call.refusal.is_some())
            .count(),
        attempts
    );
    for call in child.provider_calls.iter().take(attempts) {
        assert_eq!(call.disposition, crate::ProviderCallDisposition::Rejected);
        assert_eq!(
            call.refusal.as_ref().unwrap().cause,
            crate::ProposalRefusalCause::InvalidArguments
        );
        assert_eq!(call.requested_tools, 0);
        assert_eq!(call.input_tokens, Some(19));
    }
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), if attempts == 1 { 4 } else { attempts + 1 });
    assert_eq!(provider.fixtures.lock().unwrap().len(), 0);
    for request in requests
        .iter()
        .skip(2)
        .filter(|request| request.phase == AgentPhase::ChildWork)
    {
        assert_researcher_refusal_feedback(request);
    }
    if attempts == 1 {
        let refusal = events
            .iter()
            .find(|event| {
                matches!(&event.event,
                    crate::Event::ProviderCallRecorded { agent_run_id, record, .. }
                        if *agent_run_id == child.id && record.refusal.is_some()
                )
            })
            .unwrap();
        let reservation = events.iter().find(|event| event.sequence > refusal.sequence
            && matches!(event.event, crate::Event::GoalProviderCallReserved { agent_run_id, .. } if agent_run_id == child.id)).unwrap();
        assert!(refusal.sequence < reservation.sequence);
    }
}

fn research_batch_provider(attempts: usize) -> ResearchBatchProvider {
    let cap = usize::from(crate::session::MAX_STEP_TOOL_CALLS);
    let mut fixtures = vec![response(
        "tool_use",
        json!([tool_use(
            "toolu_delegate",
            "arany_delegate",
            json!({
                "children": [{"objective": "Read the facts", "profile": null}], "agent": null
            })
        )]),
    )];
    for attempt in 0..attempts {
        let (name, input) = if attempts == 8 && attempt % 2 == 1 {
            ("arany_list", json!({"path": "notes"}))
        } else {
            ("arany_read", json!({"path": "notes/facts.txt"}))
        };
        let blocks: Vec<_> = (0..=cap)
            .map(|index| tool_use(&format!("toolu_{index}"), name, input.clone()))
            .collect();
        fixtures.push(response("tool_use", json!(blocks)));
    }
    if attempts == 1 {
        fixtures.extend([
            response("end_turn", json!([text_block("Research complete")])),
            response("end_turn", json!([text_block("Facts collected")])),
        ]);
    }
    for (index, fixture) in fixtures.iter_mut().enumerate() {
        fixture["id"] = json!(format!("msg_{index}"));
    }
    ResearchBatchProvider {
        identity: crate::provider::ProviderIdentity::new(
            ADAPTER,
            MODEL,
            crate::provider::Provenance::native(),
        ),
        fixtures: std::sync::Mutex::new(fixtures.into()),
        requests: std::sync::Mutex::new(Vec::new()),
    }
}

fn assert_researcher_refusal_feedback(request: &ProviderRequest) {
    let cap = usize::from(crate::session::MAX_STEP_TOOL_CALLS);
    let body = body(request, Effort::Low);
    let content = body["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_array()
        .unwrap();
    let paired: Vec<_> = content
        .iter()
        .filter(|block| block["type"] == "tool_result")
        .collect();
    assert_eq!(paired.len(), cap + 1);
    for (index, block) in paired.iter().enumerate() {
        assert_eq!(block["tool_use_id"], format!("toolu_{index}"));
        assert_eq!(block["is_error"], true);
        let feedback: Value = serde_json::from_str(block["content"].as_str().unwrap()).unwrap();
        assert_eq!(feedback["dispatched"], false);
        assert_eq!(feedback["effect"], "none");
        assert_eq!(feedback["error"]["cause"], "invalid_arguments");
        assert!(
            feedback["error"]["remediation"]
                .as_str()
                .unwrap()
                .contains(&format!("1 to {cap} Tool calls"))
        );
    }
}

#[test]
fn inbound_tool_use_stop_yields_one_request_and_keeps_text_and_thinking_in_turn() {
    let request = primary_request(MODEL);
    let content = json!([
        {"type": "thinking", "thinking": "private", "signature": "sig"},
        {"type": "redacted_thinking", "data": "opaque"},
        text_block("Inspecting the file."),
        tool_use("toolu_1", "arany_read", read_input())
    ]);
    let answer = decode(&request, &response("tool_use", content.clone())).unwrap();
    let ProviderOutcome::Tools(requests) = &answer.outcome else {
        panic!("one Tool request");
    };
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].id.as_str(), "toolu_1");

    assert_eq!(
        serde_json::to_value(&requests[0].call).unwrap(),
        json!({"operation": "read", "path": "src/main.rs", "offset": null, "limit": null})
    );
    let omitted = decode(
        &request,
        &response(
            "tool_use",
            json!([tool_use(
                "toolu_omitted",
                "arany_read",
                json!({"path": "src/main.rs"})
            )]),
        ),
    )
    .unwrap();
    let ProviderOutcome::Tools(omitted_calls) = omitted.outcome else {
        panic!("Read call without paging fields")
    };
    assert_eq!(
        serde_json::to_value(&omitted_calls[0].call).unwrap(),
        json!({"operation": "read", "path": "src/main.rs", "offset": null, "limit": null})
    );
    assert_eq!(
        answer.turn.items("anthropic").unwrap(),
        content.as_array().unwrap().clone(),
        "text and thinking stay in the replayed turn"
    );
    for (offset, limit, accepted) in [
        (json!(null), json!(null), true),
        (json!(1), json!(2000), true),
        (json!(2001), json!(1), true),
        (json!(0), json!(1), false),
        (json!(1), json!(0), false),
        (json!(1), json!(2001), false),
    ] {
        let input = json!({"path":"src/main.rs","offset":offset,"limit":limit});
        let fixture = response(
            "tool_use",
            json!([tool_use("toolu_1", "arany_read", input.clone())]),
        );
        let decoded = decode(&request, &fixture).expect("typed range reaches Engine validation");
        let ProviderOutcome::Tools(calls) = decoded.outcome else {
            panic!("Read call")
        };
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call.valid(), accepted, "{input}");
        assert_eq!(
            serde_json::to_value(&calls[0].call).unwrap(),
            json!({"operation":"read", "path":"src/main.rs", "offset":offset, "limit":limit})
        );
    }
    assert_eq!(answer.response_id.as_deref(), Some("msg_example"));
    assert_eq!(answer.usage.input_tokens, Some(19));
    for model in [
        "deepseek-flash",
        "vendor.other/exact-1",
        "kimi-k3",
        "k3-256k",
        "glm-5.3",
        "glm-5.3-flash",
    ] {
        for signature in [None, Some(""), Some("opaque-signature")] {
            let request = primary_request(model);
            let mut fixture = response("tool_use", content.clone());
            fixture["model"] = json!(model);
            match signature {
                Some(value) => fixture["content"][0]["signature"] = json!(value),
                None => {
                    fixture["content"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("signature");
                }
            }
            let accepted = decode(&request, &fixture).unwrap();
            assert_eq!(
                accepted.turn.items("anthropic").unwrap(),
                fixture["content"].as_array().unwrap().clone()
            );
            fixture["model"] = json!("claude-sonnet-5");
            assert!(decode(&request, &fixture).is_err());
        }
    }
}

#[test]
fn streamed_messages_report_closed_failure_stages() {
    use crate::provider::diagnostics::SubscriptionFailureStage as Stage;
    for (wire, expected) in [
        ("data: {\n\n".to_owned(), Stage::EventJson),
        (
            sse(&[json!({"type":"message_start","message":{}})]),
            Stage::ResponseMessage,
        ),
        (
            sse(&[
                message_start(),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text"}}),
            ]),
            Stage::ResponseContent,
        ),
        (
            sse(&[json!({"type":"error","error":{"type":"overloaded_error"}})]),
            Stage::RemoteFailure,
        ),
    ] {
        let (preview, _) = crate::provider::ResponsePreview::new();
        let mut decoder = MessageStream::new(KEY, "model-1", preview, DEFAULT_OUTPUT_TOKEN_CAP);
        assert!(decoder.feed(wire.as_bytes()).is_err());
        assert_eq!(decoder.stage(), expected);
        assert_eq!(decoder.counts().bytes, wire.len());
        assert!(decoder.counts().events > 0);
    }
}

#[test]
fn tool_use_stop_records_its_own_wire_provenance() {
    let request = primary_request(MODEL);
    let tool = decode(
        &request,
        &response(
            "tool_use",
            json!([tool_use("toolu_1", "arany_list", json!({"path": "src"}))]),
        ),
    )
    .unwrap();
    assert_eq!(
        tool.wire_provenance,
        Some(ProviderWireProvenance::MessagesToolUseStorageUnspecified)
    );
    assert!(tool.wire_provenance.unwrap().valid_for(
        tool.response_id.as_deref(),
        tool.usage.input_tokens,
        tool.usage.output_tokens
    ));
    let finish = decode(
        &request,
        &response("end_turn", json!([text_block(answer_text())])),
    )
    .unwrap();
    assert_eq!(
        finish.wire_provenance,
        Some(ProviderWireProvenance::MessagesEndTurnStorageUnspecified)
    );
    assert!(
        !ProviderWireProvenance::MessagesToolUseStorageUnspecified.valid_for(
            Some("id"),
            None,
            Some(1)
        )
    );
}

#[test]
fn plan_is_a_separate_optional_function() {
    let request = primary_request(MODEL);
    let contract = serde_json::to_value(goal_plan_contract()).unwrap();
    let answer = decode(
        &request,
        &response(
            "tool_use",
            json!([tool_use("toolu_1", "arany_plan", contract.clone())]),
        ),
    )
    .unwrap();
    assert!(
        matches!(answer.outcome, ProviderOutcome::Plan(plan) if serde_json::to_value(&plan.contract).unwrap() == contract)
    );
    for value in [Value::Null, contract] {
        let legacy = json!({"path":"src/new.rs", "expected_digest":null, "content":"x", "plan_contract":value});
        assert!(
            decode(
                &request,
                &response(
                    "tool_use",
                    json!([tool_use("toolu_old", "arany_write", legacy)])
                )
            )
            .is_err()
        );
    }
    let prelude = decode(
        &request,
        &response(
            "tool_use",
            json!([tool_use("toolu_2", "arany_read", read_input())]),
        ),
    )
    .unwrap();
    assert!(matches!(&prelude.outcome, ProviderOutcome::Tools(_)));
    for bad in [
        json!({"path": "src/new.rs", "content": "x"}),
        json!({"path": "src/new.rs", "expected_digest": null, "content": "x", "plan_contract": {"mission": "x"}}),
        json!({"operation": "write", "path": "a", "expected_digest": null, "content": "x"}),
        json!({"path": "a", "expected_digest": null, "content": "x", "network": true}),
    ] {
        assert!(
            decode(
                &request,
                &response(
                    "tool_use",
                    json!([tool_use("toolu_3", "arany_write", bad.clone())])
                )
            )
            .is_err(),
            "{bad}"
        );
    }
}

#[test]
fn inbound_rejects_max_tokens_refusal_and_mixed_outcomes() {
    let mut request = primary_request(MODEL);
    request.collaboration = CollaborationPolicy::Auto {
        max_active_children: 3,
    };
    let one = tool_use("toolu_1", "arany_read", read_input());
    let two = tool_use("toolu_2", "arany_read", read_input());
    let delegate = tool_use(
        "toolu_delegate",
        "arany_delegate",
        json!({
            "children": [{"objective": "Inspect", "profile": null}], "agent": null
        }),
    );
    for (case, fixture) in [
        ("max_tokens", response("max_tokens", json!([one.clone()]))),
        ("refusal", response("refusal", json!([text_block("no")]))),
        (
            "stop_sequence",
            response("stop_sequence", json!([text_block("x")])),
        ),
        (
            "pause_turn",
            response("pause_turn", json!([text_block("x")])),
        ),
        (
            "tool_use without a block",
            response("tool_use", json!([text_block("x")])),
        ),
        (
            "end_turn with a tool_use",
            response("end_turn", json!([text_block(answer_text()), one.clone()])),
        ),
        (
            "tool_use with only thinking",
            response(
                "tool_use",
                json!([{"type": "thinking", "thinking": "t", "signature": "s"}]),
            ),
        ),
        (
            "plan mixed with read",
            response(
                "tool_use",
                json!([
                    tool_use("toolu_plan", "arany_plan", json!(goal_plan_contract())),
                    one.clone()
                ]),
            ),
        ),
        (
            "read mixed with plan",
            response(
                "tool_use",
                json!([
                    one.clone(),
                    tool_use("toolu_plan", "arany_plan", json!(goal_plan_contract()))
                ]),
            ),
        ),
        (
            "two plans",
            response(
                "tool_use",
                json!([
                    tool_use("toolu_plan", "arany_plan", json!(goal_plan_contract())),
                    tool_use(
                        "toolu_other_plan",
                        "arany_plan",
                        json!(goal_plan_contract())
                    )
                ]),
            ),
        ),
        (
            "text between calls",
            response(
                "tool_use",
                json!([one.clone(), text_block("Inspecting"), two]),
            ),
        ),
        (
            "delegate mixed with read",
            response("tool_use", json!([delegate.clone(), one.clone()])),
        ),
        (
            "read mixed with delegate",
            response("tool_use", json!([one.clone(), delegate])),
        ),
        (
            "malformed later input",
            response(
                "tool_use",
                json!([
                    one.clone(),
                    tool_use("toolu_bad", "arany_read", json!({"path": 42}))
                ]),
            ),
        ),
        (
            "duplicate ids",
            response("tool_use", json!([one.clone(), one.clone()])),
        ),
        (
            "unknown name",
            response(
                "tool_use",
                json!([tool_use("toolu_1", "arany_unknown", json!({}))]),
            ),
        ),
        (
            "unprefixed name",
            response(
                "tool_use",
                json!([tool_use("toolu_1", "read", read_input())]),
            ),
        ),
        (
            "apply_changeset",
            response(
                "tool_use",
                json!([tool_use("toolu_1", "arany_apply_changeset", json!({}))]),
            ),
        ),
        (
            "empty id",
            response(
                "tool_use",
                json!([tool_use("", "arany_read", read_input())]),
            ),
        ),
        (
            "non-object input",
            response(
                "tool_use",
                json!([tool_use("toolu_1", "arany_read", json!([]))]),
            ),
        ),
        (
            "tool_use without id",
            response(
                "tool_use",
                json!([{"type": "tool_use", "name": "arany_read", "input": {}}]),
            ),
        ),
        (
            "unexpected block field",
            response(
                "tool_use",
                json!([{"type": "tool_use", "id": "t", "name": "arany_read", "input": {}, "caller": {}}]),
            ),
        ),
    ] {
        assert!(decode(&request, &fixture).is_err(), "{case}");
    }
    let mut over_cap = response("tool_use", json!([one]));
    over_cap["usage"] = json!({"input_tokens": 12, "output_tokens": 4097});
    assert!(decode(&request, &over_cap).is_err());
    let read_only = read_only_request(MODEL);
    assert!(
        decode(
            &read_only,
            &response(
                "tool_use",
                json!([tool_use("toolu_1", "arany_read", read_input())])
            )
        )
        .is_err(),
        "a Tool that was not offered does not exist"
    );
    let ceiling = crate::tools::types::MAX_EDIT_CONTENT_BYTES;
    for (name, input, accepted) in [
        (
            "arany_write",
            json!({"path": "a", "expected_digest": null, "content": "é".repeat(ceiling / 2)}),
            true,
        ),
        (
            "arany_edit",
            json!({"path": "a", "expected_digest": "0".repeat(64), "old": "x".repeat(ceiling / 2), "new": "é".repeat(ceiling / 4)}),
            true,
        ),
        (
            "arany_write",
            json!({"path": "a", "expected_digest": null, "content": "x".repeat(ceiling + 1)}),
            false,
        ),
        (
            "arany_edit",
            json!({"path": "a", "expected_digest": "0".repeat(64), "old": "x".repeat(ceiling / 2), "new": "x".repeat(ceiling / 2 + 1)}),
            false,
        ),
    ] {
        let decoded = decode(
            &request,
            &response("tool_use", json!([tool_use("toolu_1", name, input)])),
        );
        let decoded = decoded.expect("typed content reaches Engine validation");
        let ProviderOutcome::Tools(calls) = decoded.outcome else {
            panic!("write or edit")
        };
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call.valid(), accepted, "{name} valid={accepted}");
    }
}

#[test]
fn custom_messages_optional_effort_preserves_text_compaction() {
    let request = read_only_request("deepseek-flash");
    let conversation = render::conversation(&request).unwrap();
    let omitted = run_body_with_optional_effort(&request, &conversation, None).unwrap();
    assert!(omitted.get("output_config").is_none());
    assert!(omitted.get("thinking").is_none());
    assert_eq!(omitted["model"], "deepseek-flash");
    let compact = CompactionRequest {
        session_id: request.session_id,
        covered_run_id: request.run_id,
        model: request.model.clone(),
        previous_summary: None,
        items: vec![],
        max_output_tokens: 1024,
        prompts: Default::default(),
    };
    let strict = compaction_body_with_optional_effort(&compact, None);
    assert!(strict["output_config"].get("effort").is_none());
    assert!(strict["output_config"].get("format").is_none());
    assert!(strict.get("thinking").is_none());
    for effort in Effort::ALL {
        assert_eq!(
            run_body_with_optional_effort(&request, &conversation, Some(effort)).unwrap(),
            run_body(&request, &conversation, effort).unwrap()
        );
        assert_eq!(
            compaction_body_with_optional_effort(&compact, Some(effort)),
            compaction_body(&compact, effort)
        );
        assert_eq!(
            compaction_body_with_optional_effort(&compact, Some(effort))["output_config"]["effort"],
            effort.as_str()
        );
    }
    let mut review = request;
    review.phase = AgentPhase::ToolReview;
    review.input.role = crate::provider::PromptRole::ToolReview;
    let conversation = render::conversation(&review).unwrap();
    let strict = run_body_with_optional_effort(&review, &conversation, None).unwrap();
    assert_eq!(
        strict["output_config"]["format"]["schema"],
        conversation.outcome
    );
    assert!(strict["output_config"].get("effort").is_none());
}

#[test]
fn anthropic_usage_reports_cache_read_and_creation_separately() {
    let answer = decode(
        &read_only_request(MODEL),
        &response("end_turn", json!([text_block(answer_text())])),
    )
    .expect("cached finish");
    assert_eq!(answer.usage.input_tokens, Some(19));
    assert_eq!(answer.usage.cache_read_input_tokens, Some(4));
    assert_eq!(answer.usage.cache_creation_input_tokens, Some(3));
    assert_eq!(answer.usage.output_tokens, Some(8));
    assert!(answer.usage.cache_consistent());
    let mut detailed = response("end_turn", json!([text_block(answer_text())]));
    detailed["usage"]["output_tokens_details"] = json!({"thinking_tokens": 5});
    let usage = decode(&read_only_request(MODEL), &detailed).unwrap().usage;
    assert_eq!(usage.output_tokens, Some(8));
    assert_eq!(usage.input_tokens, Some(12 + 3 + 4));
    let mut missing = response("end_turn", json!([text_block(answer_text())]));
    missing["usage"] = json!({"input_tokens":12,"output_tokens":8});
    let usage = decode(&read_only_request(MODEL), &missing).unwrap().usage;
    assert_eq!(usage.cache_read_input_tokens, None);
    assert_eq!(usage.cache_creation_input_tokens, None);
    assert_eq!(usage.input_tokens, Some(12));
    assert_eq!(usage.output_tokens, Some(8));
    for (field, other) in [
        ("cache_read_input_tokens", "cache_creation_input_tokens"),
        ("cache_creation_input_tokens", "cache_read_input_tokens"),
    ] {
        let mut partial = missing.clone();
        partial["usage"][field] = json!(0);
        let usage = decode(&read_only_request(MODEL), &partial).unwrap().usage;
        assert_eq!(usage.input_tokens, Some(12));
        assert_eq!(
            (
                usage.cache_read_input_tokens,
                usage.cache_creation_input_tokens
            ),
            if field == "cache_read_input_tokens" {
                (Some(0), None)
            } else {
                (None, Some(0))
            }
        );
        assert!(partial["usage"].get(other).is_none());
    }
    for model in [MODEL, "kimi-k3"] {
        let request = read_only_request(model);
        for flat in [None, Some(3)] {
            let mut fixture = response("end_turn", json!([text_block(answer_text())]));
            fixture["model"] = json!(model);
            let usage = fixture["usage"].as_object_mut().unwrap();
            usage.remove("cache_creation_input_tokens");
            if let Some(flat) = flat {
                usage.insert("cache_creation_input_tokens".into(), json!(flat));
            }
            usage.insert(
                "cache_creation".into(),
                json!({"ephemeral_5m_input_tokens":1,"ephemeral_1h_input_tokens":2}),
            );
            let expected = decode(&request, &fixture).unwrap().usage;
            assert_eq!(expected.input_tokens, Some(19));
            assert_eq!(expected.cache_creation_input_tokens, Some(3));
            assert_eq!(expected.cache_read_input_tokens, Some(4));
            assert_eq!(expected.output_tokens, Some(8));
            let (publisher, _) = crate::provider::ResponsePreview::new();
            let streamed = run_stream_for(
                &render::semantic::messages_stream(&fixture),
                model,
                publisher,
            )
            .unwrap()
            .unwrap();
            let streamed: Value = serde_json::from_slice(&streamed).unwrap();
            assert_eq!(decode(&request, &streamed).unwrap().usage, expected);
            fixture["content"] = json!([text_block(r#"short"#)]);
            assert_eq!(
                decode_compaction(&serde_json::to_vec(&fixture).unwrap(), model, 1024)
                    .unwrap()
                    .usage,
                expected
            );
        }
    }
    for flat in [None, Some(3)] {
        for nested in [
            json!({}),
            json!({"ephemeral_5m_input_tokens":1}),
            json!({"ephemeral_1h_input_tokens":2}),
            json!({"ephemeral_5m_input_tokens":1,"ephemeral_1h_input_tokens":2,"unexpected":0}),
            json!({"ephemeral_5m_input_tokens":u32::MAX,"ephemeral_1h_input_tokens":1}),
            json!({"ephemeral_5m_input_tokens":u32::MAX,"ephemeral_1h_input_tokens":0}),
            json!(false),
            json!([]),
        ]
        .into_iter()
        .chain(
            ["ephemeral_5m_input_tokens", "ephemeral_1h_input_tokens"]
                .into_iter()
                .flat_map(|field| {
                    [
                        json!(-1),
                        json!(1.5),
                        json!("2"),
                        json!(null),
                        json!(u64::from(u32::MAX) + 1),
                    ]
                    .into_iter()
                    .map(move |value| {
                        let mut nested =
                            json!({"ephemeral_5m_input_tokens":1,"ephemeral_1h_input_tokens":2});
                        nested[field] = value;
                        nested
                    })
                }),
        ) {
            let mut fixture = response("end_turn", json!([text_block(answer_text())]));
            fixture["usage"]
                .as_object_mut()
                .unwrap()
                .remove("cache_creation_input_tokens");
            if let Some(flat) = flat {
                fixture["usage"]["cache_creation_input_tokens"] = json!(flat);
            }
            fixture["usage"]["cache_creation"] = nested;
            assert!(matches!(
                decode(&read_only_request(MODEL), &fixture),
                Err(ProviderError::InvalidOutcome)
            ));
        }
    }
    let mut inconsistent = response("end_turn", json!([text_block(answer_text())]));
    inconsistent["usage"]["cache_creation"] =
        json!({"ephemeral_5m_input_tokens":3,"ephemeral_1h_input_tokens":1});
    assert!(matches!(
        decode(&read_only_request(MODEL), &inconsistent),
        Err(ProviderError::InvalidOutcome)
    ));
    let mut zero = response("end_turn", json!([text_block(answer_text())]));
    zero["usage"]
        .as_object_mut()
        .unwrap()
        .remove("cache_creation_input_tokens");
    zero["usage"]["cache_creation"] =
        json!({"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0});
    let usage = decode(&read_only_request(MODEL), &zero).unwrap().usage;
    assert_eq!(usage.cache_creation_input_tokens, Some(0));
    assert_eq!(usage.input_tokens, Some(16));
    for field in ["cache_read_input_tokens", "cache_creation_input_tokens"] {
        for invalid in [
            json!(-1),
            json!(1.5),
            json!("4"),
            json!(u64::from(u32::MAX) + 1),
            json!(u32::MAX),
        ] {
            let mut malformed = response("end_turn", json!([text_block(answer_text())]));
            malformed["usage"][field] = invalid;
            assert!(
                decode(&read_only_request(MODEL), &malformed).is_err(),
                "{field}"
            );
        }
    }
}

#[test]
fn inbound_end_turn_answers_in_text_and_delegation_is_a_tool() {
    let mut request = read_only_request(MODEL);
    let finish = response(
        "end_turn",
        json!([
            {"type": "thinking", "thinking": "private", "signature": "sig"},
            {"type": "redacted_thinking", "data": "opaque"},
            text_block(answer_text())
        ]),
    );
    let answer = decode(&request, &finish).expect("text compaction finish");
    assert_eq!(answer.response_id.as_deref(), Some("msg_example"));
    assert_eq!(answer.usage.input_tokens, Some(19));
    assert!(matches!(
        answer.outcome,
        ProviderOutcome::Answer(Answer { text: result }) if result == "answer"
    ));
    assert_eq!(
        answer.turn.items("anthropic").unwrap().len(),
        3,
        "the whole content array is the replayed turn"
    );
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
        decode(&request, &uncached).unwrap().usage.input_tokens,
        Some(12)
    );

    request.collaboration = CollaborationPolicy::Auto {
        max_active_children: 3,
    };
    request.phase = AgentPhase::RootPlan;
    let delegate = response(
        "tool_use",
        json!([tool_use(
            "delegate_1",
            "arany_delegate",
            json!({"children":[{"objective":"a","profile":null},{"objective":"b","profile":null}], "agent":null})
        )]),
    );
    assert!(matches!(
        decode(&request, &delegate).unwrap().outcome,
        ProviderOutcome::Delegate(Delegate { children, agent: None }) if children.iter().map(|child| child.objective.as_str()).collect::<Vec<_>>() == ["a", "b"]
    ));

    let compact = response("end_turn", json!([text_block(r#"short"#)]));
    let summary = decode_compaction(&serde_json::to_vec(&compact).unwrap(), MODEL, 1024)
        .expect("complete text compaction");
    assert_eq!(summary.summary, "short");
    assert_eq!(summary.usage.output_tokens, Some(8));
    assert_eq!(
        summary.wire_provenance,
        Some(ProviderWireProvenance::MessagesEndTurnStorageUnspecified)
    );
    assert!(
        decode_compaction(
            &serde_json::to_vec(&response("tool_use", json!([text_block(r#"short"#)]))).unwrap(),
            MODEL,
            1024
        )
        .is_err(),
        "compaction never accepts a Tool stop"
    );
    for malformed in [json!({"type":"text"}), json!({"type":"text","text":null})] {
        for (compaction, valid) in [(false, answer_text()), (true, r#"short"#)] {
            let fixture = response("end_turn", json!([malformed, text_block(valid)]));
            let accepted = if compaction {
                decode_compaction(&serde_json::to_vec(&fixture).unwrap(), MODEL, 1024).is_ok()
            } else {
                decode(&request, &fixture).is_ok()
            };
            assert!(!accepted, "malformed text precedes a valid answer");
        }
    }
    let bad = [
        response("end_turn", json!([])),
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
            decode(&request, &fixture).is_err(),
            "bad content case {index}"
        );
    }
    let mut too_many_blocks = vec![json!({"type": "thinking"}); MAX_CONTENT_BLOCKS];
    too_many_blocks.push(text_block(answer_text()));
    assert!(decode(&request, &response("end_turn", json!(too_many_blocks))).is_err());
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
            decode(&request, &fixture).is_err(),
            "bad envelope case {index}"
        );
    }
    let joined = response("end_turn", json!([text_block("hola"), text_block("! 🚀")]));
    assert!(
        matches!(decode(&request, &joined).unwrap().outcome, ProviderOutcome::Answer(Answer { text }) if text == "hola! 🚀")
    );

    for (text, accepted) in [
        (String::new(), false),
        (" \n\t".into(), false),
        (
            "x".repeat(crate::session::MAX_COMPACTION_SUMMARY_BYTES),
            true,
        ),
        (
            "é".repeat(crate::session::MAX_COMPACTION_SUMMARY_BYTES / 2),
            true,
        ),
        (
            "x".repeat(crate::session::MAX_COMPACTION_SUMMARY_BYTES + 1),
            false,
        ),
        (
            "é".repeat(crate::session::MAX_COMPACTION_SUMMARY_BYTES / 2 + 1),
            false,
        ),
        ("  summary\n".into(), true),
        (r#"{"summary":"ok","extra":1}"#.into(), true),
    ] {
        let fixture = response("end_turn", json!([text_block(&text)]));
        let decoded = decode_compaction(&serde_json::to_vec(&fixture).unwrap(), MODEL, 1024);
        assert_eq!(decoded.is_ok(), accepted);
        if let Ok(decoded) = decoded {
            assert_eq!(decoded.summary, text);
        }
    }
    let split = response("end_turn", json!([text_block("one"), text_block("two")]));
    assert!(decode_compaction(&serde_json::to_vec(&split).unwrap(), MODEL, 1024).is_err());
    assert!(
        encode_request(&json!({
            "oversize": "x".repeat(crate::provider::MAX_PROVIDER_REQUEST_BYTES)
        }))
        .is_err()
    );
}

#[test]
fn outbound_messages_pin_schema_cap_and_disclosure_scope() {
    for (phase, role, model) in [
        (
            AgentPhase::RootPlan,
            crate::provider::PromptRole::ReadOnly,
            "primary-model",
        ),
        (
            AgentPhase::ChildWork,
            crate::provider::PromptRole::ReadOnly,
            "child-model",
        ),
        (
            AgentPhase::ToolReview,
            crate::provider::PromptRole::ToolReview,
            "review-model",
        ),
    ] {
        let mut role_request = read_only_request(model);
        role_request.phase = phase;
        role_request.collaboration = CollaborationPolicy::Single;
        role_request.input.role = role;
        if phase != AgentPhase::RootPlan {
            role_request.input.goal_context_facts = None;
        }
        assert_eq!(body(&role_request, Effort::Low)["model"], model);
        if role == crate::provider::PromptRole::ToolReview {
            assert_eq!(
                body(&role_request, Effort::Low)["output_config"]["format"]["type"],
                "json_schema"
            );
            role_request.model = MODEL.into();
            let valid = response("end_turn", json!([text_block(r#"{"verdict":"approve"}"#)]));
            assert!(matches!(
                decode(&role_request, &valid).unwrap().outcome,
                ProviderOutcome::Review(crate::provider::ReviewVerdict::Approve)
            ));
            for text in [
                "approve",
                r#"{"verdict":"allow"}"#,
                r#"{"verdict":"approve","extra":true}"#,
            ] {
                assert!(
                    decode(
                        &role_request,
                        &response("end_turn", json!([text_block(text)]))
                    )
                    .is_err()
                );
            }
        }
    }
    let request = read_only_request(MODEL);
    let rendered = body(&request, Effort::Max);
    assert_eq!(rendered["model"], MODEL);
    assert_eq!(rendered["max_tokens"], 4096);
    assert_eq!(rendered["stream"], false);
    assert!(rendered["output_config"].get("format").is_none());
    assert_eq!(rendered["output_config"]["effort"], "max");
    assert!(!rendered["system"].to_string().contains("canary"));
    let messages = rendered["messages"].as_array().unwrap();
    assert!(messages.iter().all(|message| message["role"] == "user"));
    let text = rendered["messages"].to_string();
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
    for absent in ["metadata", "container"] {
        assert!(rendered.get(absent).is_none(), "{absent}");
    }
    assert!(encode_request(&rendered).is_ok());
    for (collaboration, planning) in [
        (CollaborationPolicy::Single, vec![]),
        (
            CollaborationPolicy::Auto {
                max_active_children: 0,
            },
            vec![],
        ),
        (
            CollaborationPolicy::Auto {
                max_active_children: 3,
            },
            vec!["arany_delegate"],
        ),
        (
            CollaborationPolicy::Team {
                max_active_children: 3,
            },
            vec!["arany_delegate"],
        ),
    ] {
        for phase in [AgentPhase::RootPlan, AgentPhase::RootSynthesis] {
            let mut scoped = request.clone();
            scoped.phase = phase;
            scoped.collaboration = collaboration;
            let scoped_body = body(&scoped, Effort::Low);
            assert_eq!(
                scoped_body["system"][1]["text"],
                format!(
                    "Pinned collaboration policy: {}.",
                    serde_json::to_string(&collaboration).unwrap()
                )
            );
            assert_eq!(
                outcome_types(&scoped_body),
                if phase == AgentPhase::RootPlan {
                    planning.clone()
                } else {
                    vec![]
                }
            );
        }
    }
    let mut child = request.clone();
    child.phase = AgentPhase::ChildWork;
    child.collaboration = CollaborationPolicy::Single;
    let child_body = body(&child, Effort::Low);
    assert_eq!(outcome_types(&child_body), [] as [&str; 0]);
    assert_eq!(child_body["system"].as_array().unwrap().len(), 1);
    {
        use crate::provider::{ImageOrigin, ProviderImage, test_image};
        let image = test_image();
        let mut selected = request.clone();
        selected.input.images = vec![ProviderImage {
            origin: ImageOrigin::Objective,
            image: image.clone(),
        }];
        let multimodal = body(&selected, Effort::Low);
        let current = multimodal["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| {
                message["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|block| block["type"] == "image")
            });
        let content = current.expect("image message")["content"]
            .as_array()
            .unwrap();
        let at = content
            .iter()
            .position(|block| block["type"] == "image")
            .unwrap();
        assert_eq!(
            content[at - 1],
            json!({"type": "text", "text": "Image 1 (objective): PNG 1x1"})
        );
        assert_eq!(
            content[at],
            json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": image.base64()}})
        );
        assert!(encode_request(&multimodal).is_ok());
    }
    for (model, effort) in [
        ("claude-fable-5-1", Effort::High),
        ("claude-opus-5-5", Effort::Medium),
        ("claude-sonnet-5-5", Effort::Max),
    ] {
        let mut selected = request.clone();
        selected.model = model.into();
        let selected_body = body(&selected, effort);
        assert_eq!(selected_body["model"], model);
        assert_eq!(selected_body["output_config"]["effort"], effort.as_str());
        assert_eq!(
            selected_body["output_config"]["format"],
            rendered["output_config"]["format"]
        );
        assert_eq!(selected_body["max_tokens"], 4096);
    }

    let compact = CompactionRequest {
        session_id: SessionId::new(),
        covered_run_id: request.run_id,
        model: request.model,
        previous_summary: Some("prior derived canary".into()),
        items: vec![
            crate::provider::CompactionItem::Completed(HistoryTurn {
                user: "done user canary".into(),
                assistant: "done answer canary".into(),
            }),
            crate::provider::CompactionItem::Unanswered {
                user: "open user canary".into(),
                status: UnansweredStatus::Interrupted,
            },
        ],
        max_output_tokens: 1024,
        prompts: Default::default(),
    };
    let compact_body = compaction_body(&compact, Effort::Low);
    let compact_input: Value = serde_json::from_str(
        compact_body["messages"][0]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .expect("typed compaction data");
    assert_eq!(
        compact_body
            .to_string()
            .matches("\"cache_control\":")
            .count(),
        1
    );
    assert!(
        compact_body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none()
    );
    assert_eq!(compact_body["max_tokens"], 1024);
    assert_eq!(compact_body["output_config"]["effort"], "low");
    assert_eq!(compact_input["items"][1]["status"], "interrupted");
    assert!(compact_input.get("workspace_guidance").is_none());
    assert!(compact_input.get("session_id").is_none());
    assert!(compact_body["output_config"].get("format").is_none());
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect()
}

fn run_stream(
    wire: &str,
    preview: crate::provider::ResponsePreview,
) -> Result<Option<Vec<u8>>, ProviderError> {
    run_stream_for(wire, "model-1", preview)
}

fn run_stream_for(
    wire: &str,
    model: &str,
    preview: crate::provider::ResponsePreview,
) -> Result<Option<Vec<u8>>, ProviderError> {
    let mut decoder = MessageStream::new(KEY, model, preview, DEFAULT_OUTPUT_TOKEN_CAP);
    let mut completed = None;
    for byte in wire.bytes() {
        if let Some(response) = decoder.feed(&[byte])? {
            completed = Some(response);
        }
    }
    Ok(completed)
}

fn message_start() -> Value {
    json!({"type":"message_start","message":{"id":"msg_stream","type":"message","role":"assistant","model":"model-1","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":12,"cache_read_input_tokens":4,"cache_creation_input_tokens":3,"output_tokens":0}}})
}

#[test]
fn a_clean_end_after_a_framed_event_is_a_retryable_cut_and_a_non_stream_is_not() {
    for (label, wire, cut) in [
        ("after message_start", sse(&[message_start()]), true),
        ("no framed event", "not an event stream".to_owned(), false),
        (
            "unterminated frame",
            "data: {\"type\":\"message_start\"".to_owned(),
            false,
        ),
    ] {
        let (preview, _) = crate::provider::ResponsePreview::new();
        let mut decoder = MessageStream::new(KEY, "model-1", preview, DEFAULT_OUTPUT_TOKEN_CAP);
        assert!(decoder.feed(wire.as_bytes()).unwrap().is_none(), "{label}");
        let error = decoder.end_error();
        assert_eq!(
            error.retry_cause().map(|(cause, _)| cause),
            cut.then_some(crate::provider::RetryCause::StreamCut),
            "{label}"
        );
        assert_eq!(
            error.failure_reason(),
            Some(crate::ProviderFailureReason::StreamProtocol),
            "{label}"
        );
    }
}

#[test]
fn decoded_messages_report_closed_failure_stages() {
    use crate::provider::diagnostics::SubscriptionFailureStage as Stage;
    let request = primary_request(MODEL);
    let conversation = render::conversation(&request).unwrap();
    let offered: Vec<_> = conversation
        .tools
        .clone()
        .unwrap_or_default()
        .into_iter()
        .chain(outcome_specs(&conversation.outcome))
        .collect();
    let scope = RunScope {
        model: &request.model,
        output_cap: request.max_output_tokens,
        secret: KEY,
        offered: &offered,
        review: false,
    };
    let encode = |value: Value| serde_json::to_vec(&value).unwrap();
    let mut wrong_model = response("end_turn", json!([text_block("answer")]));
    wrong_model["model"] = json!("other-model");
    let mut invalid_usage = response("end_turn", json!([text_block("answer")]));
    invalid_usage["usage"]["output_tokens"] = json!(u64::from(request.max_output_tokens) + 1);
    let mut invalid_status = response("unknown_stop", json!([text_block("answer")]));
    invalid_status["stop_sequence"] = json!("sequence");
    let mut invalid_id = response("end_turn", json!([text_block("answer")]));
    invalid_id["id"] = json!("");
    let malformed_second_tool = response(
        "tool_use",
        json!([
            tool_use("toolu_read", "arany_read", json!({"path":"src/main.rs"})),
            tool_use(
                "toolu_write",
                "arany_write",
                json!({"path":"src/new.rs","content":"x"})
            )
        ]),
    );
    let malformed_second_tool = encode(malformed_second_tool);
    let invalid_typed_tool = encode(response(
        "tool_use",
        json!([tool_use(
            "",
            "arany_read",
            json!({"path":"src/main.rs","offset":7,"limit":3})
        )]),
    ));
    let cases = [
        (b"{".to_vec(), Stage::ResponseEnvelope),
        (encode(wrong_model), Stage::ResponseModel),
        (encode(invalid_usage), Stage::UsageContract),
        (encode(invalid_status), Stage::ResponseStatus),
        (encode(invalid_id), Stage::ResponseIdentifier),
        (
            encode(response("end_turn", json!([]))),
            Stage::ResponseContent,
        ),
        (
            encode(response(
                "end_turn",
                json!([{"type":"text","text":"answer","unknown":"field"}]),
            )),
            Stage::ResponseEnvelope,
        ),
        (invalid_typed_tool.clone(), Stage::OutcomeToolArguments),
        (
            encode(response(
                "tool_use",
                json!([
                    tool_use("toolu_one", "arany_read", json!({"path":"src/main.rs"})),
                    tool_use("toolu_one", "arany_read", json!({"path":"src/lib.rs"}))
                ]),
            )),
            Stage::OutcomeToolArguments,
        ),
        (malformed_second_tool.clone(), Stage::OutcomeToolArguments),
    ];
    for (bytes, expected) in cases {
        assert_eq!(decode_run_failure_stage(&bytes, &scope), expected);
    }
    for (bytes, expected) in [
        (&malformed_second_tool, "operation: \"write\""),
        (&invalid_typed_tool, "read_offset: Some(7)"),
        (&invalid_typed_tool, "read_limit: Some(3)"),
        (&invalid_typed_tool, "path_bytes: Some(11)"),
    ] {
        let shape = decode_run_failure_tool_shape(bytes, &scope);
        assert!(format!("{:?}", shape.unwrap()).contains(expected));
    }
}

#[test]
fn completed_stream_rejection_logs_decoder_record_with_received_counters() {
    if crate::test_process::isolate_test() {
        return;
    }
    use crate::provider::diagnostics::SubscriptionFailureStage as Stage;
    let temp = tempfile::tempdir().expect("diagnostic state");
    let root =
        crate::store::StateRoot::admit(&temp.path().join("state")).expect("private state root");
    crate::diagnostics::enable_test_diagnostics(&root);

    let request = primary_request(MODEL);
    let conversation = render::conversation(&request).unwrap();
    let offered: Vec<_> = conversation
        .tools
        .clone()
        .unwrap_or_default()
        .into_iter()
        .chain(outcome_specs(&conversation.outcome))
        .collect();
    let scope = RunScope {
        model: &request.model,
        output_cap: request.max_output_tokens,
        secret: KEY,
        offered: &offered,
        review: false,
    };
    let (bytes, counts, wire_bytes) = malformed_completed_write_stream();
    assert_eq!(counts.events, 6);
    assert_eq!(counts.bytes, wire_bytes);
    assert_ne!(counts.bytes, bytes.len());
    assert_eq!(counts.http_status, Some(200));
    assert!(decode_run_with_counts(&bytes, &scope, counts).is_err());

    let log =
        std::fs::read_to_string(root.path().join("development.log")).expect("decoder diagnostic");
    assert!(log.contains(&format!(
        "messages stage={:?} bytes={wire_bytes} events=6 http_status=Some(200)",
        Stage::OutcomeToolArguments
    )));
    assert!(log.contains("operation: \"write\""));
}

fn malformed_completed_write_stream() -> (Vec<u8>, crate::provider::diagnostics::StreamCounts, usize)
{
    let mut start = message_start();
    start["message"]["model"] = json!(MODEL);
    let streamed = sse(&[
        start,
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_write","name":"arany_write","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"src/new.rs\",\"content\":\"x\"}"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":8}}),
        json!({"type":"message_stop"}),
    ]);
    let (preview, _) = crate::provider::ResponsePreview::new();
    let mut decoder = MessageStream::new(KEY, MODEL, preview, DEFAULT_OUTPUT_TOKEN_CAP);
    decoder.set_http_status(200);
    let mut reply = None;
    for chunk in streamed.as_bytes().chunks(17) {
        if let Some(bytes) = decoder.feed(chunk).unwrap() {
            reply = Some(bytes);
        }
    }
    (
        reply.expect("completed Messages stream"),
        decoder.counts(),
        streamed.len(),
    )
}

#[test]
fn overloaded_stream_after_tool_input_never_allows_fallback() {
    let (preview, _) = crate::provider::ResponsePreview::new();
    let wire = sse(&[
        message_start(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tool-1","name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
        json!({"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}),
    ]);
    let error = run_stream(&wire, preview.clone()).unwrap_err();
    assert!(!preview.has_output());
    assert!(!error.allows_fallback());
}

#[test]
fn streamed_messages_require_closed_blocks_and_exclude_thinking_from_preview() {
    use crate::provider::{PreviewText, ResponsePreview};
    let outcome = "line one\nline two 🚀";
    for model in ["model-1", "kimi-k3", "k3-256k", "glm-5.3", "glm-5.3-flash"] {
        for signature in [None, Some(""), Some("signature")] {
            let mut start = message_start();
            start["message"]["model"] = json!(model);
            let mut events = vec![
                start,
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"INTERNAL_CANARY"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":outcome}}),
                json!({"type":"content_block_stop","index":1}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":8}}),
                json!({"type":"message_stop"}),
            ];
            if let Some(signature) = signature {
                let middle = signature.len() / 2;
                for part in [&signature[..middle], &signature[middle..]]
                    .into_iter()
                    .rev()
                {
                    events.insert(3, json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":part}}));
                }
            }
            let wire = sse(&events);
            let (publisher, observed) = ResponsePreview::new();
            let completed = run_stream_for(&wire, model, publisher).unwrap().unwrap();
            assert_eq!(
                **observed.borrow(),
                PreviewText {
                    answer: "line one\nline two 🚀".into(),
                    ..Default::default()
                }
            );
            let mut request = read_only_request(model);
            request.max_output_tokens = 32;
            let conversation = render::conversation(&request).unwrap();
            let accepted = decode_run(
                &completed,
                &RunScope {
                    model,
                    output_cap: 32,
                    secret: KEY,
                    offered: &conversation
                        .tools
                        .clone()
                        .unwrap_or_default()
                        .into_iter()
                        .chain(outcome_specs(&conversation.outcome))
                        .collect::<Vec<_>>(),
                    review: request.input.role == crate::provider::PromptRole::ToolReview,
                },
            )
            .unwrap();
            assert!(
                matches!(accepted.outcome, ProviderOutcome::Answer(Answer { text: result }) if result == "line one\nline two 🚀")
            );
            assert_eq!(accepted.usage.input_tokens, Some(19));
            assert_eq!(accepted.usage.cache_read_input_tokens, Some(4));
            assert_eq!(accepted.usage.cache_creation_input_tokens, Some(3));
            let thinking = &accepted.turn.items("anthropic").unwrap()[0];
            assert_eq!(thinking["thinking"], "INTERNAL_CANARY");
            assert_eq!(thinking.get("signature").and_then(Value::as_str), signature);
        }
    }
    for invalid in [
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"unsafe"}}),
        json!({"type":"message_stop"}),
    ] {
        let (publisher, _) = ResponsePreview::new();
        let mut decoder = MessageStream::new("key", "model-1", publisher, DEFAULT_OUTPUT_TOKEN_CAP);
        let wire = format!("data: {invalid}\n\n");
        assert!(decoder.feed(wire.as_bytes()).is_err());
    }
}

#[test]
fn streamed_multi_call_inputs_are_isolated_and_replayed_in_model_order() {
    let mut events = vec![message_start()];
    let cap = usize::from(crate::session::MAX_STEP_TOOL_CALLS);
    for index in 0..cap {
        events.push(
            json!({"type":"content_block_start","index":index,"content_block":{
                "type":"tool_use","id":format!("toolu_{index}"),"name":"arany_read","input":{}
            }}),
        );
        for fragment in [
            format!("{{\"path\":\"src/file{index}.rs\","),
            "\"offset\":null,\"limit\":null}".into(),
        ] {
            events.push(json!({"type":"content_block_delta","index":index,"delta":{
                "type":"input_json_delta","partial_json":fragment
            }}));
        }
        events.push(json!({"type":"content_block_stop","index":index}));
    }
    events.extend([
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":8}}),
        json!({"type":"message_stop"}),
    ]);
    let (preview, observed) = crate::provider::ResponsePreview::new();
    let assembled = run_stream(&sse(&events), preview).unwrap().unwrap();
    assert!(observed.borrow().answer.is_empty());
    let reply = decode_streamed_tool(&assembled, 4096).unwrap();
    let ProviderOutcome::Tools(requests) = &reply.outcome else {
        panic!("multiple Tool requests");
    };
    assert_eq!(requests.len(), cap);
    let turn = reply.turn.items(ADAPTER).unwrap();
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(request.id.as_str(), format!("toolu_{index}"));
        assert_eq!(
            request.call.path_argument().unwrap().0,
            format!("src/file{index}.rs")
        );
        assert_eq!(turn[index]["id"], request.id.as_str());
        assert_eq!(turn[index]["input"]["path"], format!("src/file{index}.rs"));
    }
}

#[test]
fn streamed_tool_use_input_json_deltas_assemble_byte_stably_and_are_not_previewed() {
    use crate::provider::ResponsePreview;
    let fragments = [
        "{\"path\":\"src/",
        "main.rs\",\"offset\":null,",
        "\"limit\":null}",
    ];
    let mut events = vec![
        message_start(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"reading"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"arany_read","input":{}}}),
    ];
    events.extend(fragments.iter().map(|fragment| {
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":fragment}})
    }));
    events.extend([
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":8}}),
        json!({"type":"message_stop"}),
    ]);
    for model in ["model-1", "kimi-k3", "k3-256k", "glm-5.3", "glm-5.3-flash"] {
        events[0]["message"]["model"] = json!(model);
        let wire = sse(&events);
        let (publisher, observed) = ResponsePreview::new();
        let first = run_stream_for(&wire, model, publisher).unwrap().unwrap();
        let (publisher, _) = ResponsePreview::new();
        let second = run_stream_for(&wire, model, publisher).unwrap().unwrap();
        assert_eq!(
            first, second,
            "assembly is byte-stable for one event sequence"
        );
        assert!(
            !observed.borrow().answer.contains("main.rs"),
            "tool input is never previewed"
        );
        let request = primary_request(model);
        let conversation = render::conversation(&request).unwrap();
        let scope = RunScope {
            model,
            output_cap: 4096,
            secret: KEY,
            offered: conversation.tools.as_deref().unwrap(),
            review: false,
        };
        let accepted = decode_run(&first, &scope).unwrap();
        let ProviderOutcome::Tools(requests) = accepted.outcome else {
            panic!("one Tool request");
        };
        assert!(
            matches!(&requests[0].call, crate::tools::ToolCall::Read { path, limit: None, .. } if path == "src/main.rs")
        );
        assert_eq!(accepted.turn.items("anthropic").unwrap().len(), 2);
    }
    for invalid in [
        vec![
            json!({"type":"content_block_start","index":u64::MAX,"content_block":{"type":"text","text":""}}),
        ],
        vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":u64::MAX,"delta":{"type":"text_delta","text":"unsafe"}}),
        ],
        vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_stop","index":u64::MAX}),
        ],
        vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
        ],
        vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"arany_read","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
            json!({"type":"content_block_stop","index":0}),
        ],
        vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"arany_read","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"[]"}}),
            json!({"type":"content_block_stop","index":0}),
        ],
        vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"arany_read","input":{"path":"x"}}}),
        ],
        vec![
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"arany_read","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"x".repeat(crate::tools::types::MAX_ORDINARY_TOOL_ARGUMENT_BYTES + 1)}}),
        ],
    ] {
        let mut events = vec![message_start()];
        events.extend(invalid);
        let (publisher, _) = ResponsePreview::new();
        assert!(matches!(
            run_stream(&sse(&events), publisher),
            Err(ProviderError::InvalidStream)
        ));
    }
    let escaped = r#"{"path":"\u0073ynthetic-key"}"#;
    let events = [
        message_start(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"arany_read","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":escaped}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":8}}),
        json!({"type":"message_stop"}),
    ];
    let (publisher, _) = ResponsePreview::new();
    assert!(
        matches!(
            run_stream(&sse(&events), publisher),
            Err(ProviderError::InvalidOutcome)
        ),
        "an escaped credential in assembled tool input is rejected"
    );
}

fn streamed_tool_use(name: &str, arguments: &str) -> Result<Option<Vec<u8>>, ProviderError> {
    let mut events = vec![
        message_start(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":name,"input":{}}}),
    ];
    events.extend(arguments.as_bytes().chunks(4096).map(|fragment| {
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":std::str::from_utf8(fragment).unwrap()}})
    }));
    events.extend([
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":8}}),
        json!({"type":"message_stop"}),
    ]);
    let (publisher, _) = crate::provider::ResponsePreview::new();
    run_stream(&sse(&events), publisher)
}

fn decode_streamed_tool(
    assembled: &[u8],
    max_output_tokens: u32,
) -> Result<ProviderResponse, ProviderError> {
    let mut request = primary_request("model-1");
    request.max_output_tokens = max_output_tokens;
    let conversation = render::conversation(&request).unwrap();
    decode_run(
        assembled,
        &RunScope {
            model: "model-1",
            output_cap: max_output_tokens,
            secret: KEY,
            offered: conversation.tools.as_deref().unwrap(),
            review: false,
        },
    )
}

#[test]
fn native_tool_arguments_are_bounded_as_raw_text_before_parsing() {
    let mut request = primary_request(MODEL);
    request.max_output_tokens = 128_000;
    for (operation, input) in [
        ("read", read_input()),
        (
            "write",
            json!({"path":"src/new.rs","expected_digest":null,"content":"x"}),
        ),
        (
            "edit",
            json!({"path":"src/main.rs","expected_digest":"0".repeat(64),"old":"x","new":"y"}),
        ),
        (
            "multiedit",
            json!({"file_path":"src/main.rs","expected_digest":"0".repeat(64),"edits":[{"old_string":"x","new_string":"y"}]}),
        ),
    ] {
        let name = format!("arany_{operation}");
        let limit = argument_limit(operation);
        let text = input.to_string();
        for (len, accepted) in [(limit, true), (limit + 1, false)] {
            let arguments = format!(
                "{{{}{rest}",
                " ".repeat(len - text.len()),
                rest = &text[1..]
            );
            assert_eq!(arguments.len(), len);
            let fixture = response("tool_use", json!([tool_use("toolu_1", &name, json!({}))]))
                .to_string()
                .replace("\"input\":{}", &format!("\"input\":{arguments}"));
            let case = format!("{operation} len={len}");
            assert_eq!(
                decode_bytes(&request, fixture.as_bytes()).is_ok(),
                accepted,
                "{case}"
            );
        }
    }
}

#[test]
fn multiedit_defaults_and_content_bounds_hold_in_messages_and_streams() {
    let mut request = primary_request(MODEL);
    request.max_output_tokens = 128_000;
    for (old, new, replace_all, accepted) in [
        ("a".into(), "b".into(), None, true),
        ("a".into(), "b".into(), Some(json!(null)), true),
        ("a".into(), "b".into(), Some(json!(false)), true),
        ("a".into(), "b".into(), Some(json!(true)), true),
        ("\u{1}".repeat(32768), "\u{2}".repeat(32768), None, true),
        ("a".into(), "b".repeat(65536), None, false),
        ("a".into(), "b".into(), Some(json!("false")), false),
    ] {
        let mut edit = json!({"old_string":old,"new_string":new});
        if let Some(option) = replace_all.clone() {
            edit["replace_all"] = option;
        }
        let input =
            json!({"file_path":"src/main.rs","expected_digest":"0".repeat(64),"edits":[edit]});
        let fixture = response(
            "tool_use",
            json!([tool_use("toolu_1", "arany_multiedit", input.clone())]),
        );
        let well_typed = replace_all != Some(json!("false"));
        assert_eq!(decode(&request, &fixture).is_ok(), well_typed);
        let streamed = streamed_tool_use("arany_multiedit", &input.to_string())
            .unwrap()
            .unwrap();
        let decoded = decode_streamed_tool(&streamed, 128_000);
        assert_eq!(decoded.is_ok(), well_typed);
        if well_typed {
            let ProviderOutcome::Tools(calls) = decoded.unwrap().outcome else {
                panic!("MultiEdit call")
            };
            assert_eq!(calls[0].call.valid(), accepted);
            assert!(
                matches!(&calls[0].call, crate::ToolCall::MultiEdit { edits, .. } if edits[0].replace_all == (replace_all == Some(json!(true))))
            );
        }
        for unavailable in [read_only_request(MODEL), researcher_request(MODEL)] {
            assert!(decode(&unavailable, &fixture).is_err());
        }
    }
    let input = json!({"file_path":"src/main.rs","expected_digest":"0".repeat(64),"edits":[{"old_string":"a","new_string":"b"}]});
    let text = input.to_string();
    for len in [400 * 1024, 400 * 1024 + 1] {
        let raw = format!("{text}{}", " ".repeat(len - text.len()));
        let streamed = streamed_tool_use("arany_multiedit", &raw);
        assert_eq!(streamed.is_ok(), len == 400 * 1024);
        if let Ok(Some(bytes)) = streamed {
            assert!(decode_streamed_tool(&bytes, 128_000).is_ok());
        }
    }
}

#[test]
fn streamed_ordinary_arguments_are_bounded_as_raw_text_before_parsing() {
    let limit = crate::tools::types::MAX_ORDINARY_TOOL_ARGUMENT_BYTES;
    for path in [r#""src/main.rs""#, r#""\u0073rc/main.rs""#] {
        let body = format!(r#""path":{path},"offset":null,"limit":null}}"#);
        for (len, accepted) in [(limit, true), (limit + 1, false)] {
            let arguments = format!("{{{}{body}", " ".repeat(len - 1 - body.len()));
            assert_eq!(arguments.len(), len);
            let streamed = streamed_tool_use("arany_read", &arguments);
            let case = format!("{path} len={len}");
            if accepted {
                let assembled = streamed.unwrap().unwrap();
                assert!(
                    matches!(
                        decode_streamed_tool(&assembled, 4096).unwrap().outcome,
                        ProviderOutcome::Tools(_)
                    ),
                    "{case}"
                );
            } else {
                assert!(
                    matches!(streamed, Err(ProviderError::InvalidStream)),
                    "{case}"
                );
            }
        }
    }
}

#[test]
fn streamed_writes_at_the_content_ceiling_assemble_and_decode() {
    let ceiling = crate::tools::types::MAX_EDIT_CONTENT_BYTES;
    // Escape-heavy input needs a turn allowance above the 4096-token fixture's 96 KiB.
    for (path, content, max_output_tokens) in [
        (
            "src/new.rs".to_owned(),
            "x".repeat(ceiling),
            crate::provider::MAX_OUTPUT_TOKEN_CAP,
        ),
        ("\"".repeat(512), "\u{1}".repeat(ceiling), 128_000),
    ] {
        let mut request = primary_request("model-1");
        request.max_output_tokens = max_output_tokens;
        assert_eq!(
            body(&request, Effort::Medium)["max_tokens"],
            max_output_tokens
        );
        let arguments =
            json!({"path": path, "expected_digest": null, "content": content}).to_string();
        let assembled = streamed_tool_use("arany_write", &arguments)
            .unwrap()
            .unwrap();
        let mut assembled: Value = serde_json::from_slice(&assembled).unwrap();
        assembled["usage"]["output_tokens"] = json!(max_output_tokens);
        let assembled = serde_json::to_vec(&assembled).unwrap();
        let ProviderOutcome::Tools(requests) = decode_streamed_tool(&assembled, max_output_tokens)
            .unwrap()
            .outcome
        else {
            panic!("one Tool request");
        };
        assert!(
            matches!(&requests[0].call, crate::tools::ToolCall::Write { content: decoded, .. } if *decoded == content)
        );
    }
}

#[test]
fn streamed_delegate_accepts_the_bounded_assignment_envelope_beyond_tool_argument_bytes() {
    let arguments = json!({"agent":null, "children":vec![json!({"objective":"x".repeat(2048),"profile":null});8]}).to_string();
    assert!(arguments.len() > crate::tools::types::MAX_ORDINARY_TOOL_ARGUMENT_BYTES);
    let wire = sse(&[
        message_start(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"delegate_1","name":"arany_delegate","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":arguments}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":8}}),
        json!({"type":"message_stop"}),
    ]);
    let (preview, observed) = crate::provider::ResponsePreview::new();
    let response = run_stream(&wire, preview).unwrap().unwrap();
    assert!(observed.borrow().answer.is_empty());
    let mut request = primary_request("model-1");
    request.collaboration = CollaborationPolicy::Team {
        max_active_children: 8,
    };
    let offered = outcome_specs(&render::conversation(&request).unwrap().outcome);
    let accepted = decode_run(
        &response,
        &RunScope {
            model: "model-1",
            output_cap: 4096,
            secret: KEY,
            offered: &offered,
            review: false,
        },
    )
    .unwrap();
    assert!(
        matches!(accepted.outcome, ProviderOutcome::Delegate(Delegate { children, .. }) if children.len() == 8)
    );
}

#[test]
fn provider_bound_schemas_carry_no_validation_keywords() {
    assert_no_schema_bounds(&body(&read_only_request(MODEL), Effort::Low), "read_only");
    let mut request = primary_request(MODEL);
    for (label, phase) in [
        ("plan", AgentPhase::RootPlan),
        ("synthesis", AgentPhase::RootSynthesis),
    ] {
        request.phase = phase;
        assert_no_schema_bounds(&body(&request, Effort::Low), label);
    }
    request.input.goal_plan_contract = Some(goal_plan_contract());
    assert_no_schema_bounds(&body(&request, Effort::Low), "continued");
}

#[test]
fn steering_notes_are_user_input_data_never_system_instructions() {
    let mut request = read_only_request(MODEL);
    request.input.steering_notes = vec!["steering canary".into()];
    let rendered = body(&request, Effort::Max);
    assert!(!rendered["system"].to_string().contains("steering canary"));
    assert!(rendered["messages"].to_string().contains("steering canary"));
    assert!(
        rendered["system"]
            .to_string()
            .contains("Steering notes are messages")
    );
    let plain = body(&read_only_request(MODEL), Effort::Max);
    assert!(
        !plain["messages"]
            .to_string()
            .contains("Steering notes the user sent")
    );
    request.phase = AgentPhase::ChildWork;
    request.collaboration = crate::session::CollaborationPolicy::Single;
    request.input.role = crate::provider::PromptRole::ReadOnly;
    request.input.tools = None;
    assert!(request.validate_scope().is_ok());
    let child = body(&request, Effort::Max);
    assert!(!child["system"].to_string().contains("steering canary"));
    assert!(child["messages"].to_string().contains("steering canary"));
    request.phase = AgentPhase::ToolReview;
    request.input.role = crate::provider::PromptRole::ToolReview;
    assert!(request.validate_scope().is_err());
}

#[test]
fn child_research_request_offers_only_read_only_tools_and_text_at_exhaustion() {
    let request = researcher_request(MODEL);
    let rendered = body(&request, Effort::Low);
    assert_eq!(
        rendered["system"][0]["text"],
        crate::provider::EffectivePrompts::default().text(crate::provider::PromptFile::Researcher)
    );
    assert_eq!(
        rendered["system"][1]["text"],
        crate::provider::prompts::RESEARCH_AUTHORITY
    );
    assert_eq!(
        rendered["system"][2]["text"],
        format!(
            "Tool calls per step: at most {}. Answer at most {} bytes.",
            crate::session::MAX_STEP_TOOL_CALLS,
            crate::session::MAX_CHILD_RESULT_BYTES
        )
    );
    assert_eq!(rendered["system"].as_array().unwrap().len(), 3);
    assert_eq!(
        tool_names(&rendered),
        [
            "arany_list",
            "arany_read",
            "arany_search",
            "arany_skill",
            "arany_mcp_list",
            "arany_glob"
        ]
    );
    assert!(rendered["tools"].as_array().unwrap().iter().all(|tool| {
        tool["input_schema"]["properties"]
            .get("plan_contract")
            .is_none()
    }));
    assert_eq!(outcome_types(&rendered), [] as [&str; 0]);
    let mut exhausted = request.clone();
    let mut spent = catalog();
    spent["tool_calls_remaining"] = json!(0);
    exhausted.input.tools = Some(tool_context(spent));
    let spent_body = body(&exhausted, Effort::Low);
    assert_eq!(tool_names(&spent_body), tool_names(&rendered));
    assert_eq!(spent_body["tool_choice"], json!({"type": "none"}));
    assert_eq!(
        spent_body.to_string().matches("\"cache_control\":").count(),
        2
    );
    assert_eq!(spent_body["system"], rendered["system"]);
    assert_eq!(spent_body["tools"], rendered["tools"]);
    assert_eq!(
        spent_body["messages"].as_array().unwrap().len(),
        rendered["messages"].as_array().unwrap().len()
    );
    assert!(
        !spent_body["messages"]
            .to_string()
            .contains("No Tool calls remain")
    );
    assert_eq!(rendered["tool_choice"]["type"], "auto");
    assert_eq!(outcome_types(&spent_body), [] as [&str; 0]);
}

#[test]
fn git_reads_join_the_native_tools_when_cataloged() {
    let mut request = primary_request(MODEL);
    let with_catalog = |request: &mut ProviderRequest, git: Value| {
        request.input.tools = Some(tool_context(
            json!({"write": false, "commands": [], "skills": [], "mcp_servers": [], "git": git}),
        ));
        body(request, Effort::Low)
    };
    let cataloged = with_catalog(
        &mut request,
        json!({"operations": ["git_status", "git_diff", "git_log"]}),
    );
    assert_eq!(
        tool_names(&cataloged),
        [
            "arany_list",
            "arany_read",
            "arany_search",
            "arany_git_status",
            "arany_git_diff",
            "arany_git_log",
            "arany_glob",
            "arany_plan"
        ]
    );
    let plain = with_catalog(&mut request, json!("unavailable"));
    assert_eq!(
        tool_names(&plain),
        [
            "arany_list",
            "arany_read",
            "arany_search",
            "arany_glob",
            "arany_plan"
        ]
    );
}

#[test]
fn full_catalog_uses_non_strict_tools_without_a_run_text_schema() {
    let mut request = primary_request(MODEL);
    request.collaboration = CollaborationPolicy::Auto {
        max_active_children: 3,
    };
    let rendered = body(&request, Effort::Low);
    assert!(
        rendered["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool.get("strict").is_none()),
        "the plan-contract unions keep Tool definitions non-strict"
    );
    assert!(rendered["output_config"].get("format").is_none());
}

fn thinking_tool_reply(id: &str) -> Value {
    response(
        "tool_use",
        json!([
            {"type": "thinking", "thinking": "private", "signature": format!("sig-{id}")},
            tool_use(id, "arany_read", read_input())
        ]),
    )
}

fn assert_extends(earlier: &Value, later: &Value) {
    assert_eq!(earlier["system"], later["system"]);
    assert_eq!(earlier["tools"], later["tools"]);
    for body in [earlier, later] {
        let markers = if body["tool_choice"]["type"] == "none" {
            2
        } else {
            3
        };
        assert_eq!(
            body.to_string().matches("\"cache_control\":").count(),
            markers
        );
    }
    let mut earlier = earlier["messages"].as_array().unwrap().clone();
    earlier.last_mut().unwrap()["content"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("cache_control");
    let later = later["messages"].as_array().unwrap();
    assert!(later.len() > earlier.len());
    assert_eq!(earlier[..], later[..earlier.len()]);
}

#[test]
fn researcher_requests_extend_each_other_across_calls() {
    let mut request = researcher_request(MODEL);
    let mut bodies = vec![body(&request, Effort::Low)];
    for (index, remaining) in [(1, 3), (2, 2), (3, 1), (4, 0)] {
        let reply = thinking_tool_reply(&format!("toolu_{index}"));
        let mut step = observed_step(decode(&request, &reply).unwrap(), "{}");
        step.facts = None;
        step.calls_remaining = Some(remaining);
        request.input.tools.as_mut().unwrap().steps.push(step);
        bodies.push(body(&request, Effort::Low));
    }
    for pair in bodies.windows(2) {
        assert_extends(&pair[0], &pair[1]);
    }
    assert_eq!(bodies[3]["tool_choice"], json!({"type":"auto"}));
    assert_eq!(bodies[4]["tool_choice"], json!({"type":"none"}));
    assert!(
        bodies[3]["messages"]
            .to_string()
            .contains("tool_calls_remaining")
    );
}

#[test]
fn post_compaction_requests_drop_signed_thinking_and_then_extend() {
    let mut request = primary_request(MODEL);
    for index in 1..=3 {
        let reply = thinking_tool_reply(&format!("toolu_{index}"));
        let step = observed_step(decode(&request, &reply).unwrap(), "{}");
        request.input.tools.as_mut().unwrap().steps.push(step);
    }
    // Compaction replaces the first step with a summary and unsigns the kept turns.
    let tools = request.input.tools.as_mut().unwrap();
    tools.steps.remove(0);
    tools.history_summary = Some("summary of the first step".into());
    for step in &mut tools.steps {
        step.turn.without_thinking();
    }
    let compacted = body(&request, Effort::Low);
    assert!(!compacted["messages"].to_string().contains("thinking"));
    let reply = thinking_tool_reply("toolu_4");
    let step = observed_step(decode(&request, &reply).unwrap(), "{}");
    request.input.tools.as_mut().unwrap().steps.push(step);
    assert_extends(&compacted, &body(&request, Effort::Low));
}

#[test]
fn a_quoted_preamble_before_tool_use_does_not_break_the_stream() {
    use crate::provider::ResponsePreview;
    let preamble = r#"I will read "src/main.rs" and {not json} first"#;
    let events = [
        message_start(),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":preamble}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"arany_read","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a\",\"offset\":null,\"limit\":10}"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":8}}),
        json!({"type":"message_stop"}),
    ];
    let (publisher, observed) = ResponsePreview::new();
    let completed = run_stream(&sse(&events), publisher).unwrap().unwrap();
    assert!(observed.borrow().answer.is_empty());
    let request = primary_request("model-1");
    let conversation = render::conversation(&request).unwrap();
    let scope = RunScope {
        model: "model-1",
        output_cap: 4096,
        secret: KEY,
        offered: conversation.tools.as_deref().unwrap(),
        review: false,
    };
    assert!(decode_run(&completed, &scope).is_ok());
    let leaked = r#"I hold synthetic-key here"#;
    let mut secret_events = events.to_vec();
    secret_events[2] =
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":leaked}});
    let (publisher, _) = ResponsePreview::new();
    assert!(
        run_stream(&sse(&secret_events), publisher).is_err(),
        "the credential check stays fatal"
    );
}

#[test]
fn compaction_envelope_and_bound_fit_the_request_ceiling() {
    let empty = CompactionRequest {
        session_id: SessionId::new(),
        covered_run_id: RunId::new(),
        model: "claude-opus-4-5".into(),
        previous_summary: None,
        items: Vec::new(),
        max_output_tokens: crate::session::MAX_COMPACTION_OUTPUT_TOKENS,
        // Control bytes are the worst JSON escaping for the largest admitted override.
        prompts: crate::provider::EffectivePrompts::default().with_override(
            crate::provider::PromptFile::Compaction,
            &"\u{1}".repeat(crate::provider::MAX_COMPACTION_PROMPT_OVERRIDE_BYTES),
        ),
    };
    let envelope = serde_json::to_vec(&compaction_body(&empty, crate::provider::Effort::High))
        .expect("body")
        .len();
    assert!(
        envelope <= crate::provider::COMPACTION_REQUEST_ENVELOPE_BYTES,
        "envelope {envelope} bytes"
    );
    let (_, body) = compaction_bound_request("claude-opus-4-5", |request| {
        compaction_body(request, crate::provider::Effort::High)
    });
    let bytes = encode_request(&body).expect("a compaction request at the bound is sent");
    assert!(bytes.len() <= crate::provider::MAX_PROVIDER_REQUEST_BYTES);
}

#[test]
fn run_request_ceiling_tracks_admitted_window_and_keeps_default_bound() {
    let mut run = primary_request(MODEL);
    run.input.objective = "x".repeat(520 * 1024);
    let request = body(&run, Effort::None);
    let window_ceiling = crate::provider::request_ceiling(Some(200_000));
    assert!(encode_request_within(&request, window_ceiling).is_ok());
    assert!(encode_request_within(&request, crate::provider::MAX_PROVIDER_REQUEST_BYTES).is_err());

    let oversized = json!({"input": "x".repeat(window_ceiling)});
    assert!(encode_request_within(&oversized, window_ceiling).is_err());
    assert!(encode_request_within(&request, crate::provider::request_ceiling(None)).is_err());
}

#[test]
fn tool_surface_matches_the_checked_in_snapshot() {
    let mut rendered: serde_json::Map<String, Value> =
        crate::provider::test_support::tool_surface_requests(MODEL)
            .into_iter()
            .map(|(label, request)| {
                (
                    label.to_owned(),
                    body(&request, Effort::High)["tools"].clone(),
                )
            })
            .collect();
    let mut assignment = primary_request(MODEL);
    assignment.collaboration = CollaborationPolicy::Auto {
        max_active_children: 3,
    };
    assignment.input.assignment_profiles = vec!["inspect".into(), "main".into()];
    rendered.insert(
        "assignment_outcome".into(),
        crate::provider::run_outcome_schema(&assignment),
    );
    rendered.insert(
        "tool_choice".into(),
        body(&assignment, Effort::High)["tool_choice"].clone(),
    );
    let rendered = serde_json::to_string_pretty(&rendered).unwrap() + "\n";
    // Regenerate with ARANY_UPDATE_SNAPSHOTS=1, then review the diff.
    if std::env::var_os("ARANY_UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/provider/snapshots/anthropic_tools.json"
            ),
            &rendered,
        )
        .unwrap();
    }
    assert!(
        rendered == include_str!("../../snapshots/anthropic_tools.json"),
        "{rendered}"
    );
}

#[test]
fn assignment_objects_pin_eligible_names_and_reject_malformed_or_reflected_fields() {
    let mut request = read_only_request(MODEL);
    request.phase = AgentPhase::RootPlan;
    request.input.assignment_profiles = vec!["inspect".into()];
    let rendered = body(&request, Effort::High);
    let delegate = &rendered["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "arany_delegate")
        .unwrap()["input_schema"];
    assert_eq!(
        delegate["properties"]["children"]["items"]["properties"]["profile"]["enum"],
        json!(["inspect", null])
    );
    for (child, valid) in crate::provider::test_support::assignment_shapes() {
        let outcome = json!({"outcome": {"type": "delegate", "children": [child]}});
        let fixture = response(
            "tool_use",
            crate::provider::render::semantic::messages_content(&outcome),
        );
        assert_eq!(decode(&request, &fixture).is_ok(), valid);
    }
    for field in ["objective", "profile"] {
        let mut child = json!({"objective": "Inspect", "profile": "inspect"});
        child[field] = json!(KEY);
        let outcome = json!({"outcome": {"type": "delegate", "children": [child]}});
        assert!(
            decode(
                &request,
                &response(
                    "tool_use",
                    crate::provider::render::semantic::messages_content(&outcome)
                )
            )
            .is_err()
        );
    }
}

#[test]
fn delegate_tool_use_is_answered_by_a_fixed_result_before_the_child_results() {
    for model in [
        "deepseek-flash",
        "kimi-k3",
        "k3-256k",
        "glm-5.3",
        "glm-5.3-flash",
    ] {
        for signature in [None, Some(""), Some("unchanged")] {
            let mut request = primary_request(model);
            request.collaboration = CollaborationPolicy::Auto {
                max_active_children: 2,
            };
            let mut reply = response(
                "tool_use",
                json!([tool_use(
                    "delegate_1",
                    "arany_delegate",
                    json!({"children":[{"objective":"inspect","profile":null}], "agent":null})
                )]),
            );
            reply["model"] = json!(model);
            let mut thinking = json!({"type":"thinking","thinking":"opaque"});
            if let Some(signature) = signature {
                thinking["signature"] = json!(signature);
            }
            reply["content"]
                .as_array_mut()
                .unwrap()
                .insert(0, thinking.clone());
            let delegated = decode(&request, &reply).unwrap();
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
                    narrowed_observations: Default::default(),
                    refusal_index: None,
                    refusal: None,
                    plan: None,
                    turn: delegated.turn,
                    requests: Vec::new(),
                    observations: Vec::new(),
                    facts: None,
                    steering_notes: Vec::new(),
                    calls_remaining: None,
                    cleared: false,
                    hook_feedback: Default::default(),
                });
            let messages = body(&request, Effort::Low)["messages"]
                .as_array()
                .unwrap()
                .clone();
            let call = messages
                .iter()
                .position(|message| {
                    message["content"].as_array().is_some_and(|blocks| {
                        blocks.iter().any(|block| block["type"] == "tool_use")
                    })
                })
                .expect("replayed delegate");
            assert_eq!(messages[call]["content"][0], thinking);
            assert_eq!(
                messages[call + 1]["content"][0],
                json!({"type": "tool_result", "tool_use_id": "delegate_1", "content": DELEGATED_OUTPUT})
            );
            assert!(
                messages[call + 1]["content"][1]["text"]
                    .as_str()
                    .unwrap()
                    .contains("child canary")
            );
        }
    }
}

#[test]
fn answer_text_streams_and_is_withdrawn_when_a_call_follows() {
    use crate::provider::ResponsePreview;
    for (stop_reason, block, answer, commentary) in [
        ("end_turn", None, "Hola there", Vec::<String>::new()),
        (
            "tool_use",
            Some(json!({"type":"tool_use","id":"toolu_1","name":"arany_read","input":{}})),
            "",
            vec!["Hola there".to_owned()],
        ),
    ] {
        let mut events = vec![
            message_start(),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hola "}}),
        ];
        let (publisher, observed) = ResponsePreview::new();
        let mut decoder = MessageStream::new(KEY, "model-1", publisher, DEFAULT_OUTPUT_TOKEN_CAP);
        decoder.feed(sse(&events).as_bytes()).unwrap();
        assert_eq!(observed.borrow().answer, "Hola ");
        assert!(observed.borrow().tentative);
        events = vec![
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"there"}}),
            json!({"type":"content_block_stop","index":0}),
        ];
        if let Some(block) = block {
            events.push(json!({"type":"content_block_start","index":1,"content_block":block}));
            events.push(json!({"type":"content_block_stop","index":1}));
        }
        events.push(json!({"type":"message_delta","delta":{"stop_reason":stop_reason,"stop_sequence":null},"usage":{"output_tokens":8}}));
        events.push(json!({"type":"message_stop"}));
        decoder.feed(sse(&events).as_bytes()).unwrap();
        assert_eq!(observed.borrow().answer, answer);
        assert_eq!(observed.borrow().commentary, commentary);
    }
}

#[test]
fn token_sized_write_deltas_cover_the_content_ceiling_at_the_real_cap() {
    let cap = crate::provider::MAX_OUTPUT_TOKEN_CAP;
    let content = "x".repeat(crate::tools::types::MAX_EDIT_CONTENT_BYTES);
    let arguments =
        json!({"path":"src/new.rs","expected_digest":null,"content":content}).to_string();
    let mut events = vec![
        message_start(),
        json!({"type":"content_block_start","index":0,"content_block":{
            "type":"tool_use","id":"tool_write","name":"arany_write","input":{}}}),
    ];
    events.extend(arguments.as_bytes().chunks(4).map(|fragment| {
        json!({"type":"content_block_delta","index":0,"delta":{
            "type":"input_json_delta","partial_json":std::str::from_utf8(fragment).unwrap()}})
    }));
    events.extend([
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},
            "usage":{"output_tokens":arguments.len().div_ceil(4)}}),
        json!({"type":"message_stop"}),
    ]);
    let (publisher, observed) = crate::provider::ResponsePreview::new();
    let mut decoder = MessageStream::new(KEY, "model-1", publisher, cap);
    let mut completed = None;
    for event in events {
        completed = decoder.feed(sse(&[event]).as_bytes()).unwrap();
    }
    assert!(observed.borrow().answer.is_empty());
    let decoded = decode_streamed_tool(&completed.unwrap(), cap).unwrap();
    let ProviderOutcome::Tools(calls) = decoded.outcome else {
        panic!("one Write call");
    };
    assert_eq!(calls.len(), 1);
    assert!(matches!(&calls[0].call,
        crate::tools::ToolCall::Write { content: actual, .. } if *actual == content));
}

#[test]
fn declared_stream_bounds_fail_closed_at_the_framing_limit_stage() {
    for cap in [
        crate::provider::SMALL_OUTPUT_CAP,
        crate::provider::MAX_OUTPUT_TOKEN_CAP,
    ] {
        let (publisher, _) = crate::provider::ResponsePreview::new();
        let mut decoder = MessageStream::new(KEY, "model-1", publisher.clone(), cap);
        let frame = b"data: {\"type\":\"ping\"}\n\n";
        for _ in 0..crate::provider::max_stream_events(cap) {
            assert!(decoder.feed(frame).unwrap().is_none());
        }
        let error = decoder.feed(frame).unwrap_err();
        assert_eq!(
            decoder.framer.stage,
            crate::provider::streaming::FramingStage::Limit
        );
        assert_eq!(
            error.failure_reason(),
            Some(crate::ProviderFailureReason::StreamProtocol)
        );
        assert!(error.retry_cause().is_none());
        let mut decoder = MessageStream::new(KEY, "model-1", publisher, cap);
        let comment = format!(":{}\n", " ".repeat(MAX_RESPONSE_BYTES - 2));
        let bound = crate::provider::max_stream_bytes(cap);
        for _ in 0..bound / comment.len() {
            assert!(decoder.feed(comment.as_bytes()).unwrap().is_none());
        }
        assert!(
            decoder
                .feed(&comment.as_bytes()[..bound % comment.len()])
                .unwrap()
                .is_none()
        );
        let error = decoder.feed(b":").unwrap_err();
        assert_eq!(
            decoder.framer.stage,
            crate::provider::streaming::FramingStage::Limit
        );
        assert_eq!(
            error.failure_reason(),
            Some(crate::ProviderFailureReason::StreamProtocol)
        );
        assert!(error.retry_cause().is_none());
    }
}

use super::*;
use crate::{
    provider::{
        AgentPhase, CompactionItem, HistoryTurn, ProviderFailureClass, ProviderOutcome,
        UnansweredStatus,
    },
    session::{AgentRunId, RunId, SessionId},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const TOKEN: &str = "synthetic-subscription-token";

#[test]
fn checked_provider_declares_local_bound_and_exact_account_provenance() {
    let provenance = ChatGptProvenance {
        account_id: uuid::Uuid::now_v7(),
        evidence_fingerprint: [7; 32],
        admission: crate::provider::ChatGptAdmission::AccountConsent,
    };
    let provider = ChatGptProvider::from_checked_account(
        "model-a".into(),
        Effort::High,
        TOKEN.into(),
        provenance.clone(),
    )
    .expect("admitted synthetic account");
    assert_eq!(provider.context_window_tokens(), None);
    let provider = provider.with_context_window(Some(2_000_001));
    assert_eq!(provider.context_window_tokens(), None);
    let provider = provider.with_context_window(Some(272_000));
    assert_eq!(provider.context_window_tokens(), Some(272_000));
    assert_eq!(provider.identity().profile, "chatgpt");
    assert_eq!(provider.identity().model, "model-a");
    assert_eq!(provider.identity().effort, Some(Effort::High));
    assert_eq!(provider.identity().limits.concurrency, 1);
    assert_eq!(provider.identity().chatgpt_provenance(), Some(&provenance));
    assert_eq!(
        provider.identity().limits.deadline,
        std::time::Duration::from_secs(600)
    );
    assert_eq!(
        provider.identity().limits.output_token_bound,
        OutputTokenBound::LocalAcceptanceOnly
    );
    assert!(
        ChatGptProvider::from_checked_account(
            "model-a".into(),
            Effort::High,
            TOKEN.into(),
            ChatGptProvenance {
                evidence_fingerprint: [0; 32],
                ..provenance
            },
        )
        .is_err()
    );
}

fn request() -> ProviderRequest {
    ProviderRequest {
        session_id: crate::session::SessionId::new(),
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        phase: AgentPhase::RootPlan,
        collaboration: crate::session::CollaborationPolicy::Single,
        model: "model-a".into(),
        max_output_tokens: 64,
        input: RunInput {
            ..RunInput::new(PromptRole::ReadOnly, "Synthetic objective".into())
        },
    }
}

fn body_for(request: &ProviderRequest, effort: Effort) -> Result<Vec<u8>, ProviderError> {
    subscription_body(
        request,
        &render::conversation(request)?,
        effort,
        crate::provider::MAX_PROVIDER_REQUEST_BYTES,
    )
}

fn scope_for(request: &ProviderRequest) -> wire::RunScope<'_> {
    wire::RunScope::new(request, &render::conversation(request).unwrap(), TOKEN)
}

fn compaction_request() -> CompactionRequest {
    CompactionRequest {
        session_id: SessionId::new(),
        covered_run_id: RunId::new(),
        model: "model-a".into(),
        previous_summary: Some("Earlier summary".into()),
        items: vec![
            CompactionItem::Completed(HistoryTurn {
                user: "Completed objective".into(),
                assistant: "Completed result".into(),
            }),
            CompactionItem::Unanswered {
                user: "Unanswered objective".into(),
                status: UnansweredStatus::Failed,
            },
        ],
        max_output_tokens: 64,
    }
}

fn completed(output_tokens: Option<u32>) -> Vec<u8> {
    completed_with_text(output_tokens, r#"done"#)
}

fn completed_with_text(output_tokens: Option<u32>, text: &str) -> Vec<u8> {
    completed_with_id("resp_synthetic", output_tokens, text)
}

fn completed_with_id(id: &str, output_tokens: Option<u32>, text: &str) -> Vec<u8> {
    let mut response = json!({
        "id": id,
        "status": "completed",
        "model": "model-a",
        "output": [{
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{
                "type": "output_text",
                "text": text
            }]
        }],
        "usage": {"input_tokens": 8, "output_tokens": 12}
    });
    match output_tokens {
        Some(count) => response["usage"]["output_tokens"] = json!(count),
        None => {
            response.as_object_mut().unwrap().remove("usage");
        }
    }
    if let Ok(value) = serde_json::from_str::<Value>(text)
        && value["outcome"]["type"] == "delegate"
    {
        response["output"] = crate::provider::render::semantic::responses_output(&value);
    }
    format!(
        "event: response.created\r\ndata: {{\"type\":\"response.created\"}}\r\n\r\nevent: response.completed\r\ndata: {}\r\n\r\n",
        json!({"type": "response.completed", "response": response})
    ).into_bytes()
}

#[test]
fn subscription_body_is_streaming_and_omits_unsupported_remote_cap() {
    let body = body_for(&request(), Effort::Medium).expect("bounded body");
    let value: Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(value["model"], "model-a");
    assert_eq!(value["store"], false);
    assert_eq!(value["stream"], true);
    assert_eq!(value["reasoning"]["effort"], "medium");
    assert_eq!(value["input"][0]["role"], "user");
    assert_eq!(
        value["input"][0]["content"][0]["text"],
        "Synthetic objective"
    );
    assert!(value.get("text").is_none());
    for forbidden in [
        "max_output_tokens",
        "truncation",
        "previous_response_id",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
    ] {
        assert!(value.get(forbidden).is_none(), "{forbidden}");
    }
    let mut coding = request();
    coding.input.role = PromptRole::Primary;
    coding.input.goal_budget = Some(crate::session::GoalBudget::default());
    coding.input.tools = Some(crate::tools::ToolContext {
        invoked_skill: None,
        stop_bytes: 0,
        catalog: json!({"write":true,"commands":[],"skills":[],"mcp_servers":[]}).to_string(),
        history_summary: None,
        steps: Vec::new(),
    });
    let coding: Value =
        serde_json::from_slice(&body_for(&coding, Effort::Medium).unwrap()).unwrap();
    assert_eq!(coding["tool_choice"], "auto");
    assert_eq!(coding["parallel_tool_calls"], false);
    assert!(coding.get("text").is_none());
    assert!(
        coding["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "arany_edit" && tool["strict"] == true)
    );
    for forbidden in ["max_output_tokens", "truncation", "previous_response_id"] {
        assert!(coding.get(forbidden).is_none());
    }
    {
        use crate::provider::{ImageAttachment, ImageOrigin, ProviderImage};
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
        let image = ImageAttachment::from_png(&STANDARD.decode(encoded).unwrap()).unwrap();
        let mut selected = request();
        selected.input.images = vec![ProviderImage {
            origin: ImageOrigin::Objective,
            image,
        }];
        let body = body_for(&selected, Effort::Low).expect("streaming image input");
        let multimodal: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            multimodal["input"],
            json!([{"role": "user", "content": [
                {"type": "input_text", "text": "Image 1 (objective): PNG 1x1"},
                {"type": "input_image", "image_url": format!("data:image/png;base64,{encoded}"), "detail": "auto"},
                {"type": "input_text", "text": "Synthetic objective"},
            ]}])
        );
        assert_eq!(multimodal["store"], false);
        assert_eq!(multimodal["stream"], true);
        assert_eq!(multimodal["reasoning"]["effort"], "low");
        assert_eq!(multimodal["text"], value["text"]);
        for forbidden in ["max_output_tokens", "truncation", "previous_response_id"] {
            assert!(multimodal.get(forbidden).is_none(), "{forbidden}");
        }
        selected.input.images[0].origin = ImageOrigin::History { turn_index: 0 };
        selected.input.history.push(crate::provider::HistoryTurn {
            user: "prior objective".into(),
            assistant: "prior result".into(),
        });
        assert!(
            body_for(&selected, Effort::Low).is_ok(),
            "declared history origin"
        );
        selected.phase = AgentPhase::ChildWork;
        assert!(
            matches!(
                body_for(&selected, Effort::Low),
                Err(ProviderError::Rejected)
            ),
            "children cannot receive historical pixels"
        );
        selected.phase = AgentPhase::RootPlan;
        selected.input.history.clear();
        assert!(
            matches!(
                body_for(&selected, Effort::Low),
                Err(ProviderError::Rejected)
            ),
            "missing historical origin rejected before transport"
        );
        selected.input.images[0].origin = ImageOrigin::Objective;
        selected.input.images = vec![selected.input.images[0].clone(); 5];
        assert!(
            matches!(
                body_for(&selected, Effort::Low),
                Err(ProviderError::Rejected)
            ),
            "image count is bounded"
        );
    }
}

#[test]
fn subscription_compaction_body_keeps_only_bounded_history_data() {
    let body = subscription_compaction_body(&compaction_request(), Effort::Medium)
        .expect("bounded compaction body");
    let value: Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(value["model"], "model-a");
    assert_eq!(value["store"], false);
    assert_eq!(value["stream"], true);
    assert_eq!(value["reasoning"]["effort"], "medium");
    assert_eq!(value["input"][0]["role"], "user");
    let input = value["input"][0]["content"].as_str().expect("input text");
    assert!(input.contains("Earlier summary"));
    assert!(input.contains("Completed objective"));
    assert!(input.contains("Unanswered objective"));
    assert!(!input.contains("workspace_guidance"));
    assert_eq!(value["text"]["format"]["name"], "arany_compaction");
    for forbidden in ["max_output_tokens", "truncation", "previous_response_id"] {
        assert!(value.get(forbidden).is_none(), "{forbidden}");
    }
}

#[test]
fn only_a_complete_bounded_terminal_event_yields_an_outcome() {
    {
        let full = completed(Some(12));
        let data = String::from_utf8(full).unwrap();
        let data = data
            .lines()
            .find_map(|line| {
                line.strip_prefix("data: ")
                    .filter(|data| data.contains("response.completed"))
            })
            .unwrap();
        let mut terminal: Value = serde_json::from_str(data).unwrap();
        let mut item = terminal["response"]["output"][0].clone();
        item["id"] = json!("msg_synthetic");
        item["phase"] = json!("final_answer");
        terminal["response"]["output"] = json!([]);
        let created = json!({"type":"response.created", "response":{"id":"resp_synthetic"}});
        let done = json!({"type":"response.output_item.done", "output_index":0, "item":item});
        let reasoning = json!({"type":"response.output_item.done", "output_index":0,
            "item":{"id":"rs_synthetic", "type":"reasoning", "summary":[]}});
        let prefix = format!("data: {created}\n\ndata: {done}\n\n");
        for (operation, text) in [
            ("Run", r#"done"#),
            ("compaction", r#"{"summary":"Condensed"}"#),
        ] {
            for populated in [false, true] {
                let mut done = done.clone();
                done["output_index"] = json!(1);
                done["item"]["content"][0]["text"] = json!(text);
                let mut terminal = terminal.clone();
                if populated {
                    terminal["response"]["output"] = json!([reasoning["item"], done["item"]]);
                }
                let mut decoder = StreamDecoder::new(TOKEN);
                let prefix = format!("data: {created}\n\ndata: {reasoning}\n\ndata: {done}\n\n");
                assert!(
                    decoder.feed(prefix.as_bytes()).unwrap().is_none(),
                    "{operation}: done alone is not success"
                );
                let closing = format!("data: {terminal}\n\n");
                let mut body = None;
                for byte in closing.as_bytes().chunks(1) {
                    if let Some(completed) = decoder.feed(byte).unwrap() {
                        body = Some(completed);
                    }
                }
                let body = body.unwrap();
                if operation == "Run" {
                    wire::decode_streamed_run(&body, &scope_for(&request()))
                        .expect("complete streamed Run");
                } else {
                    wire::decode_streamed_compaction(&body, "model-a")
                        .expect("complete streamed compaction");
                }
            }
        }
        let mut rejected = Vec::new();
        let mut changed = terminal.clone();
        changed["response"]["id"] = json!("resp_other");
        rejected.push((
            "response identity drift",
            format!("{prefix}data: {changed}\n\n"),
        ));
        changed = terminal.clone();
        changed["response"]["output"] = json!([done["item"]]);
        changed["response"]["output"][0]["content"][0]["text"] = json!("conflicting text");
        rejected.push((
            "conflicting terminal output",
            format!("{prefix}data: {changed}\n\n"),
        ));
        rejected.push(("duplicate done index", format!("{prefix}data: {done}\n\n")));
        rejected.push((
            "done without response identity",
            format!("data: {done}\n\n"),
        ));
        for (label, pointer, value) in [
            ("gapped index", "/output_index", json!(1)),
            ("negative index", "/output_index", json!(-1)),
            ("maximal index", "/output_index", json!(u64::MAX)),
            ("unfinished message", "/item/status", json!("in_progress")),
            ("missing item identity", "/item/id", Value::Null),
        ] {
            let mut changed = done.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            rejected.push((label, format!("data: {created}\n\ndata: {changed}\n\n")));
        }
        let mut repeated_id = done.clone();
        repeated_id["output_index"] = json!(1);
        rejected.push((
            "repeated item identity",
            format!("{prefix}data: {repeated_id}\n\n"),
        ));
        for (label, stream) in rejected {
            assert!(
                StreamDecoder::new(TOKEN).feed(stream.as_bytes()).is_err(),
                "{label}"
            );
        }
        for kind in ["response.failed", "response.incomplete", "error"] {
            let stream = format!("{prefix}data: {}\n\n", json!({"type":kind}));
            assert!(
                StreamDecoder::new(TOKEN).feed(stream.as_bytes()).is_err(),
                "{kind} after done"
            );
        }
        for (label, pointer, value) in [
            ("commentary only", "/item/phase", json!("commentary")),
            ("refusal", "/item/content/0/type", json!("refusal")),
        ] {
            let mut changed = done.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            let stream = format!("data: {created}\n\ndata: {changed}\n\ndata: {terminal}\n\n");
            let body = StreamDecoder::new(TOKEN)
                .feed(stream.as_bytes())
                .unwrap()
                .unwrap();
            assert!(
                wire::decode_streamed_run(&body, &scope_for(&request())).is_err(),
                "{label}"
            );
            assert!(
                wire::decode_streamed_compaction(&body, "model-a").is_err(),
                "{label}"
            );
        }
        for (label, pointer, value) in [
            ("failed terminal", "/response/status", json!("failed")),
            ("wrong model", "/response/model", json!("model-b")),
        ] {
            let mut changed = terminal.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            let stream = format!("{prefix}data: {changed}\n\n");
            let body = StreamDecoder::new(TOKEN)
                .feed(stream.as_bytes())
                .unwrap()
                .unwrap();
            assert!(
                wire::decode_streamed_run(&body, &scope_for(&request())).is_err(),
                "{label}"
            );
            assert!(
                wire::decode_streamed_compaction(&body, "model-a").is_err(),
                "{label}"
            );
        }
        let reflected = prefix.replace("\"done\"", &format!("\"{TOKEN}\""));
        assert!(
            StreamDecoder::new(TOKEN)
                .feed(reflected.as_bytes())
                .is_err()
        );
        let duplicate_item = format!(
            "data: {created}\n\ndata: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"id\":\"one\",\"id\":\"two\",\"type\":\"message\",\"status\":\"completed\"}}}}\n\n"
        );
        assert!(
            StreamDecoder::new(TOKEN)
                .feed(duplicate_item.as_bytes())
                .is_err()
        );
    }
    let mut decoder = StreamDecoder::new(TOKEN);
    let stream = completed(Some(12));
    let split = stream.len() / 3;
    assert!(decoder.feed(&stream[..split]).unwrap().is_none());
    assert!(decoder.feed(&stream[split..split * 2]).unwrap().is_none());
    let body = decoder
        .feed(&stream[split * 2..])
        .unwrap()
        .expect("complete response");
    let decoded = wire::decode_streamed_run(&body, &scope_for(&request())).expect("strict outcome");
    assert!(matches!(decoded.outcome, ProviderOutcome::Answer(_)));
    assert_eq!(decoded.usage.output_tokens, Some(12));
    for (name, ending, named) in [
        ("LF", "\n", true),
        ("CRLF", "\r\n", true),
        ("CR", "\r", true),
        ("data-only LF", "\n", false),
        ("data-only CRLF", "\r\n", false),
    ] {
        let stream = String::from_utf8(completed(Some(12)))
            .expect("synthetic UTF-8 stream")
            .replace("\r\n", "\n")
            .replace('\n', ending);
        let stream = if named {
            stream
        } else {
            stream
                .split_inclusive(ending)
                .filter(|line| !line.starts_with("event:"))
                .collect::<String>()
        };
        let mut decoder = StreamDecoder::new(TOKEN);
        let mut terminal = None;
        for byte in stream.as_bytes().chunks(1) {
            if let Some(completed) = decoder.feed(byte).expect(name) {
                terminal = Some(completed);
                break;
            }
        }
        assert_eq!(terminal.as_deref(), Some(body.as_slice()), "{name}");
    }
    let duplicate_field = br#"{"type":"response.completed","response":{"id":"resp_synthetic","status":"failed","status":"completed","model":"model-a","output":[]}}"#;
    assert!(wire::decode_streamed_run(duplicate_field, &scope_for(&request())).is_err());

    for (label, bytes) in [
        ("failed", b"event: response.failed\ndata: {\"type\":\"response.failed\"}\n\n".as_slice()),
        ("incomplete", b"event: response.incomplete\ndata: {\"type\":\"response.incomplete\"}\n\n".as_slice()),
        ("error", b"event: error\ndata: {\"type\":\"error\"}\n\n".as_slice()),
        ("type drift", b"event: response.completed\ndata: {\"type\":\"response.failed\"}\n\n".as_slice()),
        ("malformed", b"event: response.completed\ndata: {broken}\n\n".as_slice()),
        ("missing data-only type", b"data: {\"response\":{}}\n\n".as_slice()),
        ("null data-only type", b"data: {\"type\":null}\n\n".as_slice()),
        ("non-string data-only type", b"data: {\"type\":12}\n\n".as_slice()),
        ("empty data-only type", b"data: {\"type\":\"\"}\n\n".as_slice()),
        ("unsafe data-only type", b"data: {\"type\":\"response.\\u001bcompleted\"}\n\n".as_slice()),
        ("duplicate event field", b"event: response.completed\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n".as_slice()),
        ("reflected token", b"event: response.created\ndata: {\"type\":\"response.created\",\"echo\":\"\\u0073ynthetic-subscription-token\"}\n\n".as_slice()),
    ] {
        let mut decoder = StreamDecoder::new(TOKEN);
        assert!(decoder.feed(bytes).is_err(), "{label}");
        let stage = match label {
            "failed" | "error" => Stage::RemoteFailure,
            "incomplete" => Stage::IncompleteResponse,
            "malformed" => Stage::EventJson,
            "duplicate event field" => Stage::EventFraming,
            "reflected token" => Stage::CredentialReflection,
            _ => Stage::EventType,
        };
        assert_eq!(decoder.stage, stage, "{label}");
    }
    let mut failed = StreamDecoder::new(TOKEN);
    let failure = failed
        .feed(b"event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"subscription_sharing_usage_unavailable\"}}}\n\n")
        .expect_err("remote stream failure");
    assert_eq!(
        failure.to_string(),
        "Provider stream failed: subscription_sharing_usage_unavailable"
    );
    assert_eq!(failure.failure_class(), ProviderFailureClass::Unavailable);
    let mut event_error = StreamDecoder::new(TOKEN);
    let failure = event_error
        .feed(b"event: error\ndata: {\"type\":\"error\",\"code\":\"subscription_sharing_usage_limit_exceeded\"}\n\n")
        .expect_err("structured error event");
    assert!(matches!(
        failure,
        ProviderError::RemoteStreamCode(code)
            if code == "subscription_sharing_usage_limit_exceeded"
    ));
    for (label, stream) in [
        (
            "control in code",
            b"event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"bad\\u001b[31m\"}}}\n\n"
                .to_vec(),
        ),
        (
            "oversized code",
            format!(
                "event: response.failed\ndata: {{\"type\":\"response.failed\",\"response\":{{\"error\":{{\"code\":\"{}\"}}}}}}\n\n",
                "x".repeat(129)
            )
            .into_bytes(),
        ),
    ] {
        let failure = StreamDecoder::new(TOKEN)
            .feed(&stream)
            .expect_err(label);
        assert!(matches!(failure, ProviderError::Rejected), "{label}");
    }
    let reflected = StreamDecoder::new(TOKEN)
        .feed(b"event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"\\u0073ynthetic-subscription-token\"}}}\n\n")
        .expect_err("reflected token in code");
    assert!(matches!(reflected, ProviderError::InvalidOutcome));
    assert!(!reflected.to_string().contains(TOKEN));
    assert!(
        StreamDecoder::new(TOKEN)
            .feed(&vec![b'x'; MAX_RESPONSE_BYTES + 1])
            .is_err()
    );
    assert!(
        StreamDecoder::new(TOKEN)
            .feed(b"event: response.created\ndata: {\"type\":\"response.created\"}\n\n")
            .unwrap()
            .is_none()
    );
}

#[test]
fn created_event_echoing_a_large_tool_catalog_is_admitted_within_the_byte_bound() {
    let tools: Vec<_> = (0..40)
        .map(|index| {
            let properties: serde_json::Map<String, serde_json::Value> = (0..40)
                .map(|field| (format!("field_{field}"), json!({"type":"string","description":"x"})))
                .collect();
            json!({"type":"function","name":format!("tool_{index}"),"parameters":{"type":"object","properties":properties}})
        })
        .collect();
    let created =
        json!({"type":"response.created","response":{"id":"resp_synthetic","tools":tools}});
    let bytes = format!("event: response.created\ndata: {created}\n\n").into_bytes();
    assert!(bytes.len() < 1024 * 1024);
    let mut decoder = StreamDecoder::new(TOKEN);
    decoder
        .feed(&bytes)
        .expect("echoed catalog within the byte bound");

    let flood = format!(
        "event: response.created\ndata: {{\"type\":\"response.created\",\"x\":[{}]}}\n\n",
        vec!["0"; 300_000].join(",")
    );
    let mut decoder = StreamDecoder::new(TOKEN);
    assert!(decoder.feed(flood.as_bytes()).is_err());
    assert_eq!(decoder.stage, Stage::EventJson);
}

// A response.created event echoes the whole request; half-cap headroom makes catalog growth fail here before it fails users.
#[test]
fn created_echo_of_the_largest_catalog_request_stays_within_half_the_stream_json_caps() {
    fn nodes(value: &serde_json::Value) -> usize {
        1 + match value {
            serde_json::Value::Array(items) => items.iter().map(nodes).sum(),
            serde_json::Value::Object(fields) => fields.values().map(nodes).sum(),
            _ => 0,
        }
    }
    let catalog = json!({
        "write": true,
        "commands": (0..8).map(|index| json!({"name": format!("{index}{}", "c".repeat(63))})).collect::<Vec<_>>(),
        "skills": (0..crate::tools::MAX_SKILLS).map(|index| json!({"name": format!("{index:02}{}", "s".repeat(62)), "description": "d".repeat(100)})).collect::<Vec<_>>(),
        "mcp_servers": (0..4).map(|index| json!({"name": format!("{index}{}", "m".repeat(63)), "tools": ["echo"]})).collect::<Vec<_>>(),
        "git": {"operations": ["git_status", "git_diff", "git_log"]},
        "tool_calls_remaining": 4,
    });
    assert!(catalog.to_string().len() <= crate::tools::types::MAX_TOOL_CATALOG_BYTES);
    let mut request = crate::provider::test_support::primary_request("model-a");
    request.input.tools = Some(crate::provider::test_support::tool_context(catalog));
    let conversation = render::conversation(&request).unwrap();
    let chatgpt: serde_json::Value =
        serde_json::from_slice(&body_for(&request, Effort::Medium).unwrap()).unwrap();
    let api = crate::provider::openai::wire::run_body(&request, &conversation).unwrap();
    for (label, echoed) in [("chatgpt", chatgpt), ("openai api", api)] {
        let mut echoed = echoed;
        echoed["id"] = json!("resp_synthetic");
        let created = json!({"type":"response.created","response":echoed});
        let data = created.to_string();
        let count = nodes(&created);
        assert!(
            data.len() < crate::provider::MAX_RESPONSE_BYTES / 2,
            "{label} {} bytes",
            data.len()
        );
        assert!(
            count < crate::provider::MAX_RESPONSE_JSON_NODES / 2,
            "{label} {count} nodes"
        );
        let mut decoder = StreamDecoder::new(TOKEN);
        decoder
            .feed(format!("event: response.created\ndata: {data}\n\n").as_bytes())
            .unwrap_or_else(|error| panic!("{label}: {error}"));
    }
}

async fn serve(reply: Vec<u8>) -> (Url, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let endpoint = Url::parse(&format!(
        "http://{}/v1/responses",
        listener.local_addr().unwrap()
    ))
    .expect("test endpoint");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("request");
        let request = read_request(&mut socket).await;
        socket.write_all(&reply).await.expect("reply");
        request
    });
    (endpoint, server)
}

async fn read_request(socket: &mut TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let count = socket.read(&mut buffer).await.expect("read");
        if count == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..count]);
        if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .expect("body length");
            if request.len() >= header_end + 4 + length {
                break;
            }
        }
        assert!(request.len() <= 512 * 1024 + 8192);
    }
    String::from_utf8(request).expect("request text")
}

async fn serve_sequence(replies: Vec<Vec<u8>>) -> (Url, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let endpoint = Url::parse(&format!(
        "http://{}/v1/responses",
        listener.local_addr().unwrap()
    ))
    .expect("test endpoint");
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for reply in replies {
            let (mut socket, _) = listener.accept().await.expect("request");
            requests.push(read_request(&mut socket).await);
            socket.write_all(&reply).await.expect("reply");
        }
        requests
    });
    (endpoint, server)
}

fn event_reply(stream: Vec<u8>) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        stream.len()
    )
    .into_bytes()
    .into_iter()
    .chain(stream)
    .collect()
}

fn client() -> Client {
    Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .retry(reqwest::retry::never())
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .expect("client")
}

#[tokio::test]
async fn streaming_transport_accepts_only_final_usage_within_local_cap() {
    let mut coding = request();
    coding.input.role = PromptRole::Primary;
    coding.input.tools = Some(crate::tools::ToolContext {
        invoked_skill: None,
        stop_bytes: 0,
        catalog: json!({"write":true,"commands":[],"skills":[],"mcp_servers":[]}).to_string(),
        history_summary: None,
        steps: Vec::new(),
    });
    let response = json!({"id":"resp_function","status":"completed","model":"model-a","output":[{"type":"reasoning","id":"rs_function","summary":[],"encrypted_content":"CIPHERTEXT=="},{"type":"function_call","id":"fc_function","call_id":"call_function","name":"arany_read","arguments":json!({"path":"README.md","offset":null,"limit":null}).to_string(),"status":"completed"}],"usage":{"input_tokens":12,"output_tokens":12,"input_tokens_details":{"cached_tokens":7}}});
    let stream = format!(
        "data: {}\n\n",
        json!({"type":"response.completed","response":response})
    );
    let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{stream}", stream.len()).into_bytes();
    let (endpoint, server) = serve(reply).await;
    let decoded = invoke_at(&client(), endpoint, TOKEN, &coding, Effort::Medium)
        .await
        .unwrap();
    assert!(matches!(
        &decoded.outcome,
        ProviderOutcome::Tools(requests) if matches!(
            requests[..],
            [crate::tools::ToolRequest {
                call: crate::tools::ToolCall::Read { offset: None, limit: None, .. },
                    ..
            }]
        )
    ));
    assert_eq!(decoded.turn.items("openai").unwrap().len(), 2);
    assert_eq!(decoded.usage.cache_read_input_tokens, Some(7));
    let wire_request = server.await.unwrap();
    let body: Value = serde_json::from_str(wire_request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["parallel_tool_calls"], false);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    let stream = String::from_utf8(completed(Some(12)))
        .unwrap()
        .split_inclusive('\n')
        .filter(|line| !line.starts_with("event:"))
        .collect::<String>()
        .into_bytes();
    for (label, content_type) in [
        ("SSE", "Content-Type: text/event-stream\r\n"),
        (
            "parameterized SSE",
            "Content-Type: Text/Event-Stream; charset=utf-8\r\n",
        ),
        ("absent media label", ""),
        (
            "misleading media label",
            "Content-Type: application/json\r\n",
        ),
    ] {
        let reply = format!(
            "HTTP/1.1 200 OK\r\n{content_type}Content-Length: {}\r\nConnection: close\r\n\r\n",
            stream.len()
        )
        .into_bytes()
        .into_iter()
        .chain(stream.clone())
        .collect();
        let (endpoint, server) = serve(reply).await;
        let response = invoke_at(&client(), endpoint, TOKEN, &request(), Effort::Medium)
            .await
            .expect(label);
        assert_eq!(response.usage.output_tokens, Some(12), "{label}");
        assert!(
            matches!(response.outcome, ProviderOutcome::Answer(ref value) if value.text == "done")
        );
        let wire_request = server.await.expect("server");
        assert!(wire_request.starts_with("POST /v1/responses HTTP/1.1\r\n"));
        assert!(wire_request.contains("authorization: Bearer synthetic-subscription-token\r\n"));
        assert!(wire_request.contains("accept: text/event-stream\r\n"));
        let document: Value =
            serde_json::from_str(wire_request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(document["store"], false);
        assert_eq!(document["stream"], true);
        assert!(document.get("max_output_tokens").is_none());
    }

    for (label, stream, reason) in [
        (
            "local token cap",
            completed(Some(65)),
            crate::ProviderFailureReason::LocalOutputLimit,
        ),
        (
            "zero output usage",
            completed(Some(0)),
            crate::ProviderFailureReason::ResponseContract,
        ),
        (
            "missing usage",
            completed(None),
            crate::ProviderFailureReason::ResponseContract,
        ),
        (
            "wrong model",
            String::from_utf8(completed(Some(12)))
                .unwrap()
                .replace("model-a", "model-b")
                .into_bytes(),
            crate::ProviderFailureReason::ResponseContract,
        ),
        (
            "interrupted",
            b"event: response.created\ndata: {\"type\":\"response.created\"}\n\n".to_vec(),
            crate::ProviderFailureReason::StreamProtocol,
        ),
    ] {
        for content_type in [
            "Content-Type: text/event-stream\r\n",
            "",
            "Content-Type: application/json\r\n",
        ] {
            let reply = format!(
                "HTTP/1.1 200 OK\r\n{content_type}Content-Length: {}\r\nConnection: close\r\n\r\n",
                stream.len()
            )
            .into_bytes()
            .into_iter()
            .chain(stream.clone())
            .collect();
            let (endpoint, server) = serve(reply).await;
            let error = invoke_at(&client(), endpoint, TOKEN, &request(), Effort::Medium)
                .await
                .expect_err(label);
            assert_eq!(
                error.failure_class(),
                ProviderFailureClass::InvalidOutcome,
                "{label}"
            );
            assert_eq!(error.failure_reason(), Some(reason), "{label}");
            // Only a well-formed stream that ends early is a cut retried on the same route.
            assert_eq!(
                error.retry_cause().map(|(cause, _)| cause),
                (label == "interrupted").then_some(crate::provider::RetryCause::StreamCut),
                "{label}"
            );
            assert!(!error.to_string().contains(TOKEN));
            server.await.expect("server");
        }
    }
}

fn with_output_details(stream: Vec<u8>, details: &Value) -> Vec<u8> {
    String::from_utf8(stream)
        .unwrap()
        .split_inclusive('\n')
        .map(|line| match line.strip_prefix("data: ") {
            Some(data) if data.contains("response.completed") => {
                let mut event: Value = serde_json::from_str(data.trim_end()).unwrap();
                event["response"]["usage"]["output_tokens_details"] = details.clone();
                format!("data: {event}\r\n")
            }
            _ => line.to_owned(),
        })
        .collect::<String>()
        .into_bytes()
}

#[tokio::test]
async fn the_local_cap_counts_visible_output_and_keeps_the_reasoning_total_for_charging() {
    use crate::ProviderFailureReason::{LocalOutputLimit, ResponseContract};
    // Both requests cap visible output at 64 tokens.
    for (label, total, details, expected) in [
        (
            "reasoning-heavy",
            4_000,
            json!({"reasoning_tokens": 3_990}),
            None,
        ),
        (
            "visible output over the cap",
            4_000,
            json!({"reasoning_tokens": 3_900}),
            Some(LocalOutputLimit),
        ),
        (
            "absent details count all output",
            65,
            Value::Null,
            Some(LocalOutputLimit),
        ),
        (
            "reasoning above the total",
            12,
            json!({"reasoning_tokens": 13}),
            Some(ResponseContract),
        ),
        (
            "details without a count",
            12,
            json!({}),
            Some(ResponseContract),
        ),
        (
            "total above the reporting ceiling",
            MAX_REPORTED_OUTPUT_TOKENS + 1,
            json!({"reasoning_tokens": MAX_REPORTED_OUTPUT_TOKENS}),
            Some(ResponseContract),
        ),
    ] {
        for compaction in [false, true] {
            let text = if compaction {
                r#"{"summary":"Condensed"}"#
            } else {
                "done"
            };
            let stream = with_output_details(completed_with_text(Some(total), text), &details);
            let (endpoint, server) = serve(event_reply(stream)).await;
            let result = if compaction {
                compact_at(
                    &client(),
                    endpoint,
                    TOKEN,
                    &compaction_request(),
                    Effort::Medium,
                )
                .await
                .map(|response| response.usage)
            } else {
                invoke_at(&client(), endpoint, TOKEN, &request(), Effort::Medium)
                    .await
                    .map(|response| response.usage)
            };
            match expected {
                None => assert_eq!(
                    result.expect(label).output_tokens,
                    Some(total),
                    "{label}, compaction {compaction}"
                ),
                Some(reason) => {
                    let error = result.expect_err(label);
                    assert_eq!(
                        error.failure_reason(),
                        Some(reason),
                        "{label}, compaction {compaction}"
                    );
                    assert_eq!(error.retry_cause(), None, "{label}");
                }
            }
            server.await.expect("server");
        }
    }
}

struct ScriptedChunks(std::collections::VecDeque<(Duration, Vec<u8>)>);

impl ChunkSource for ScriptedChunks {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, Stage> {
        let Some((gap, chunk)) = self.0.pop_front() else {
            return Ok(None);
        };
        tokio::time::sleep(gap).await;
        Ok(Some(chunk))
    }
}

#[tokio::test(start_paused = true)]
async fn only_received_events_restart_the_stream_idle_window() {
    let stream = completed(Some(12));
    let split = stream.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let (created, terminal) = (stream[..split].to_vec(), stream[split..].to_vec());
    let progress =
        b"event: response.in_progress\r\ndata: {\"type\":\"response.in_progress\"}\r\n\r\n"
            .to_vec();
    let secs = Duration::from_secs;
    let steady = [
        (secs(0), created.clone()),
        (secs(100), progress.clone()),
        (secs(299), progress),
        (secs(150), terminal.clone()),
    ];
    let stalled = [(secs(0), created.clone()), (secs(301), terminal.clone())];
    // A comment and a partial frame are bytes, not events, so the window keeps running.
    let trickled = [
        (secs(0), created.clone()),
        (secs(200), b": keep-alive\r\n\r\n".to_vec()),
        (secs(50), terminal[..8].to_vec()),
        (secs(100), terminal[8..].to_vec()),
    ];
    for (label, script, elapsed, completes) in [
        ("slow but steady", &steady[..], secs(549), true),
        ("idle gap", &stalled[..], secs(300), false),
        ("bytes without an event", &trickled[..], secs(300), false),
    ] {
        let started = Instant::now();
        let mut decoder = StreamDecoder::new(TOKEN);
        let mut source = ScriptedChunks(script.iter().cloned().collect());
        let result = read_events(&mut decoder, &mut source).await;
        assert_eq!(started.elapsed(), elapsed, "{label}");
        if completes {
            assert!(result.is_ok(), "{label}");
            continue;
        }
        let error = result.expect_err(label);
        assert!(matches!(error, ProviderError::PartialTransport), "{label}");
        assert_eq!(
            error.retry_cause().map(|(cause, _)| cause),
            Some(crate::provider::RetryCause::StreamCut),
            "{label}"
        );
    }
}

#[tokio::test]
async fn compaction_stream_requires_complete_bounded_summary() {
    let stream = completed_with_text(Some(12), r#"{"summary":"Condensed"}"#);
    let reply = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        stream.len()
    )
    .into_bytes()
    .into_iter()
    .chain(stream)
    .collect();
    let (endpoint, server) = serve(reply).await;
    let response = compact_at(
        &client(),
        endpoint,
        TOKEN,
        &compaction_request(),
        Effort::Medium,
    )
    .await
    .expect("complete summary");
    assert_eq!(response.summary, "Condensed");
    assert_eq!(response.usage.output_tokens, Some(12));
    let request = server.await.expect("server");
    let document: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(document["text"]["format"]["name"], "arany_compaction");
    assert!(document.get("max_output_tokens").is_none());

    for (label, stream) in [
        (
            "local token cap",
            completed_with_text(Some(65), r#"{"summary":"Condensed"}"#),
        ),
        (
            "missing usage",
            completed_with_text(None, r#"{"summary":"Condensed"}"#),
        ),
        (
            "escaped token reflection",
            completed_with_text(
                Some(12),
                r#"{"summary":"\u0073ynthetic-subscription-token"}"#,
            ),
        ),
        (
            "interrupted",
            b"event: response.created\ndata: {\"type\":\"response.created\"}\n\n".to_vec(),
        ),
        (
            "failed",
            b"event: response.failed\ndata: {\"type\":\"response.failed\"}\n\n".to_vec(),
        ),
        (
            "incomplete",
            b"event: response.incomplete\ndata: {\"type\":\"response.incomplete\"}\n\n".to_vec(),
        ),
    ] {
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            stream.len()
        )
        .into_bytes()
        .into_iter()
        .chain(stream)
        .collect();
        let (endpoint, server) = serve(reply).await;
        assert!(
            compact_at(
                &client(),
                endpoint,
                TOKEN,
                &compaction_request(),
                Effort::Medium,
            )
            .await
            .is_err(),
            "{label}"
        );
        server.await.expect("server");
    }
}

#[tokio::test]
async fn transport_rejects_non_event_and_encoded_replies() {
    let response = json!({
        "id": "resp_synthetic",
        "status": "completed",
        "model": "model-a",
        "output": [{
            "type": "message", "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": r#"done"#}]
        }],
        "usage": {"input_tokens": 8, "output_tokens": 12}
    })
    .to_string()
    .into_bytes();
    for (label, extra, body) in [
        (
            "non-streamed completed JSON",
            "Content-Type: application/json\r\n",
            response.clone(),
        ),
        (
            "JSON mislabeled as SSE",
            "Content-Type: text/event-stream\r\n",
            response,
        ),
        (
            "HTML",
            "Content-Type: text/html\r\n",
            b"<!doctype html><html><body>Upstream page</body></html>".to_vec(),
        ),
        ("plain text", "", b"not a response stream".to_vec()),
        (
            "compressed",
            "Content-Type: text/event-stream\r\nContent-Encoding: gzip\r\n",
            completed(Some(12)),
        ),
        (
            "declared overflow",
            "Content-Type: text/event-stream\r\nContent-Length: 1048577\r\n",
            completed(Some(12)),
        ),
    ] {
        let reply = format!("HTTP/1.1 200 OK\r\n{extra}Connection: close\r\n\r\n")
            .into_bytes()
            .into_iter()
            .chain(body)
            .collect();
        let (endpoint, server) = serve(reply).await;
        let error = invoke_at(&client(), endpoint, TOKEN, &request(), Effort::Medium)
            .await
            .expect_err(label);
        assert_eq!(
            error.failure_reason(),
            Some(crate::ProviderFailureReason::StreamProtocol),
            "{label}"
        );
        assert!(
            error.retry_cause().is_none(),
            "a malformed reply never retries: {label}"
        );
        server.await.expect("server");
    }
}

#[tokio::test]
async fn stream_failure_code_reaches_run_and_compaction_without_partial_result() {
    let stream = b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\nevent: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"subscription_sharing_usage_limit_exceeded\",\"message\":\"private diagnostic\"}}}\n\n";
    let (endpoint, server) = serve(event_reply(stream.to_vec())).await;
    let error = invoke_at(&client(), endpoint, TOKEN, &request(), Effort::Medium)
        .await
        .expect_err("failed Run stream");
    assert!(
        matches!(&error, ProviderError::RemoteStreamCode(code) if code == "subscription_sharing_usage_limit_exceeded")
    );
    assert!(!error.to_string().contains("partial"));
    assert!(!error.to_string().contains("private diagnostic"));
    server.await.expect("Run server");

    let (endpoint, server) = serve(event_reply(stream.to_vec())).await;
    let error = compact_at(
        &client(),
        endpoint,
        TOKEN,
        &compaction_request(),
        Effort::Medium,
    )
    .await
    .expect_err("failed compaction stream");
    assert!(
        matches!(&error, ProviderError::RemoteStreamCode(code) if code == "subscription_sharing_usage_limit_exceeded")
    );
    assert!(!error.to_string().contains("partial"));
    server.await.expect("compaction server");
}

#[tokio::test]
async fn direct_route_http_failures_are_classified_without_exposing_body() {
    for (status, expected) in [
        (400, ProviderFailureClass::Rejected),
        (401, ProviderFailureClass::Rejected),
        (403, ProviderFailureClass::Rejected),
        (429, ProviderFailureClass::Rejected),
        (503, ProviderFailureClass::Unavailable),
        (302, ProviderFailureClass::InvalidOutcome),
    ] {
        let body = format!("{{\"detail\":\"{TOKEN}\"}}");
        let location = if status == 302 {
            "Location: https://example.invalid/v1/responses\r\n"
        } else {
            ""
        };
        let reply = format!(
            "HTTP/1.1 {status} Failure\r\n{location}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        let (endpoint, server) = serve(reply).await;
        let actual = invoke_at(&client(), endpoint, TOKEN, &request(), Effort::Medium)
            .await
            .expect_err("non-success direct route");
        // The bounded 429 body names no quota, so it is a retryable rate limit.
        assert!(match &actual {
            ProviderError::RateLimited(None) => status == 429,
            ProviderError::RemoteServerError(actual_status, None) => *actual_status == status,
            ProviderError::RemoteHttp(actual_status) => *actual_status == status && status != 429,
            _ => false,
        });
        assert_eq!(actual.failure_class(), expected, "HTTP {status}");
        assert_eq!(
            actual.to_string(),
            format!("Provider returned HTTP {status}")
        );
        server.await.expect("server");
    }
}

#[tokio::test]
async fn direct_route_429_honors_retry_after_and_never_retries_a_quota_body() {
    for (body, quota) in [
        (r#"{"error":{"type":"usage_limit_reached"}}"#, true),
        (r#"{"error":{"code":"rate_limit_exceeded"}}"#, false),
    ] {
        let reply = format!(
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 30\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        let (endpoint, server) = serve(reply).await;
        let actual = invoke_at(&client(), endpoint, TOKEN, &request(), Effort::Medium)
            .await
            .expect_err("429");
        if quota {
            assert!(matches!(actual, ProviderError::QuotaExceeded));
            assert!(actual.retry_cause().is_none());
        } else {
            assert!(matches!(
                actual,
                ProviderError::RateLimited(Some(after)) if after == std::time::Duration::from_secs(30)
            ));
        }
        assert_eq!(actual.to_string(), "Provider returned HTTP 429");
        server.await.expect("server");
    }
}

#[tokio::test]
async fn model_probe_requires_distinct_direct_delegate_and_compaction_outcomes() {
    let direct = event_reply(completed_with_id("resp_direct", Some(12), r#"done"#));
    let delegate = event_reply(completed_with_id(
        "resp_delegate",
        Some(12),
        r#"{"outcome":{"type":"delegate","children":[{"objective":"Explain two","profile":null}]}}"#,
    ));
    let compact = event_reply(completed_with_id(
        "resp_compact",
        Some(12),
        r#"{"summary":"Condensed"}"#,
    ));
    let (endpoint, server) =
        serve_sequence(vec![direct.clone(), delegate.clone(), compact.clone()]).await;
    probe_model_at(&client(), endpoint, TOKEN, "model-a", Effort::Medium)
        .await
        .expect("three strict synthetic calls");
    let requests = server.await.expect("server");
    assert_eq!(requests.len(), 3);
    for (index, request) in requests.iter().enumerate() {
        assert!(request.contains("authorization: Bearer synthetic-subscription-token\r\n"));
        let body: Value = serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap())
            .expect("request document");
        assert_eq!(body["model"], "model-a");
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body.get("max_output_tokens").is_none());
        assert!(!request.contains("private-workspace-canary"));
        if index == 2 {
            assert_eq!(body["text"]["format"]["name"], "arany_compaction");
        } else {
            assert!(body.get("text").is_none());
        }
    }

    let wrong_direct = event_reply(completed_with_id(
        "resp_direct",
        Some(12),
        r#"{"outcome":{"type":"delegate","children":[{"objective":"Wrong phase","profile":null}]}}"#,
    ));
    let repeated_delegate = event_reply(completed_with_id(
        "resp_direct",
        Some(12),
        r#"{"outcome":{"type":"delegate","children":[{"objective":"Explain two","profile":null}]}}"#,
    ));
    let wrong_delegate = event_reply(completed_with_id("resp_delegate", Some(12), r#"done"#));
    let blank_compact = event_reply(completed_with_id(
        "resp_compact",
        Some(12),
        r#"{"summary":""}"#,
    ));
    let repeated_compact = event_reply(completed_with_id(
        "resp_direct",
        Some(12),
        r#"{"summary":"Condensed"}"#,
    ));
    let over_cap = event_reply(completed_with_id(
        "resp_direct",
        Some(PROBE_OUTPUT_CAP + 1),
        r#"done"#,
    ));
    for (label, replies) in [
        ("wrong direct", vec![wrong_direct]),
        (
            "repeated response ID",
            vec![direct.clone(), repeated_delegate],
        ),
        ("wrong delegate", vec![direct.clone(), wrong_delegate]),
        (
            "blank compaction",
            vec![direct.clone(), delegate.clone(), blank_compact],
        ),
        (
            "repeated compaction ID",
            vec![direct.clone(), delegate, repeated_compact],
        ),
        ("local output cap", vec![over_cap]),
    ] {
        let expected_calls = replies.len();
        let (endpoint, server) = serve_sequence(replies).await;
        let error = probe_model_at(&client(), endpoint, TOKEN, "model-a", Effort::Medium)
            .await
            .expect_err(label);
        assert!(
            if label == "local output cap" {
                matches!(error, ProviderError::LocalOutputLimit)
            } else {
                matches!(
                    error,
                    ProviderError::InvalidOutcome | ProviderError::InvalidOutcomeContract
                )
            },
            "{label}"
        );
        assert_eq!(
            server.await.expect("server").len(),
            expected_calls,
            "{label}"
        );
    }

    let endpoint = Url::parse("http://127.0.0.1:9/v1/responses").unwrap();
    for (token, model) in [("", "model-a"), (TOKEN, "bad model")] {
        assert!(matches!(
            probe_model_at(&client(), endpoint.clone(), token, model, Effort::Medium).await,
            Err(ProviderError::Rejected)
        ));
    }
}

#[test]
fn attached_stream_preview_is_in_place_and_never_exposes_wire_arguments() {
    let outcome = r#"hello
🚀"#;
    let final_item = json!({"id":"msg_final","type":"message","role":"assistant","status":"completed","phase":"final_answer","content":[{"type":"output_text","text":outcome}]});
    let commentary = json!({"id":"msg_comment","type":"message","role":"assistant","status":"completed","phase":"commentary","content":[{"type":"output_text","text":"Public progress"}]});
    let mut events = vec![
        json!({"type":"response.created","response":{"id":"resp_preview","model":"model-a"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_comment","type":"message","role":"assistant","phase":"commentary","status":"in_progress","content":[]}}),
        json!({"type":"response.output_item.done","output_index":0,"item":commentary}),
        json!({"type":"response.output_item.added","output_index":1,"item":{"id":"msg_final","type":"message","role":"assistant","phase":"final_answer","status":"in_progress","content":[]}}),
    ];
    for character in outcome.chars() {
        events.push(json!({"type":"response.output_text.delta","output_index":1,"content_index":0,"item_id":"msg_final","delta":character.to_string()}));
    }
    events.push(json!({"type":"response.output_item.done","output_index":1,"item":final_item}));
    events.push(json!({"type":"response.completed","response":{"id":"resp_preview","model":"model-a","status":"completed","output":[commentary, final_item],"usage":{"input_tokens":8,"output_tokens":12}}}));
    let (publisher, observed) = ResponsePreview::new();
    let mut decoder = StreamDecoder::with_preview(TOKEN, "model-a", Some(publisher));
    let mut last = String::new();
    let mut accepted = None;
    for event in &events {
        let bytes = format!("data: {event}\n\n");
        accepted = decoder.feed(bytes.as_bytes()).unwrap().or(accepted);
        let current = observed.borrow().answer.clone();
        assert!(current.starts_with(&last));
        assert!(!current.contains("outcome"));
        last = current;
    }
    assert_eq!(last, "hello\n🚀");
    assert_eq!(observed.borrow().commentary, vec!["Public progress"]);
    assert!(wire::decode_streamed_run(&accepted.unwrap(), &scope_for(&request())).is_ok());
    for event in [
        json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"unknown","delta":"unsafe"}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg","type":"message","role":"assistant","phase":"reasoning","content":[]}}),
        json!({"type":"response.output_item.added","output_index":u64::MAX,"item":{"id":"msg","type":"message","role":"assistant","phase":"final_answer","content":[]}}),
    ] {
        let (publisher, observed) = ResponsePreview::new();
        let mut decoder = StreamDecoder::with_preview(TOKEN, "model-a", Some(publisher));
        assert!(
            decoder
                .feed(format!("data: {event}\n\n").as_bytes())
                .is_err()
        );
        assert!(observed.borrow().answer.is_empty());
    }
}

#[test]
fn subscription_accept_rejects_cached_tokens_above_input() {
    let usage = Usage {
        input_tokens: Some(12),
        output_tokens: Some(8),
        cache_read_input_tokens: Some(13),
        cache_creation_input_tokens: None,
    };
    assert!(accept_result(false, usage, 0, 4096, StreamCounts::default()).is_err());
}

#[test]
fn chatgpt_replay_drops_item_ids_and_keeps_ciphertext() {
    let mut coding = request();
    coding.input.role = PromptRole::Primary;
    coding.input.tools = Some(crate::tools::ToolContext {
        invoked_skill: None,
        stop_bytes: 0,
        catalog: json!({"write":true,"commands":[],"skills":[],"mcp_servers":[]}).to_string(),
        history_summary: None,
        steps: Vec::new(),
    });
    let reply = json!({"id":"resp_1","status":"completed","model":"model-a","output":[
        {"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"CIPHERTEXT=="},
        {"type":"function_call","id":"fc_1","call_id":"call_1","name":"arany_read",
         "arguments":json!({"path":"README.md","offset":null,"limit":10}).to_string(),"status":"completed"}
    ],"usage":{"input_tokens":1,"output_tokens":1}});
    let conversation = render::conversation(&coding).unwrap();
    for (offset, limit, accepted) in [
        (json!(null), json!(null), true),
        (json!(1), json!(2000), true),
        (json!(2001), json!(1), true),
        (json!(0), json!(1), false),
        (json!(1), json!(0), false),
        (json!(1), json!(2001), false),
    ] {
        let mut fixture = reply.clone();
        fixture["output"][1]["arguments"] =
            json!(json!({"path":"README.md","offset":offset,"limit":limit}).to_string());
        assert_eq!(
            wire::decode_run(
                &serde_json::to_vec(&fixture).unwrap(),
                &wire::RunScope::new(&coding, &conversation, TOKEN)
            )
            .is_ok(),
            accepted
        );
    }
    let decoded = wire::decode_run(
        &serde_json::to_vec(&reply).unwrap(),
        &wire::RunScope::new(&coding, &conversation, TOKEN),
    )
    .unwrap();
    coding
        .input
        .tools
        .as_mut()
        .unwrap()
        .steps
        .push(crate::tools::ToolStep {
            plan: None,
            turn: decoded.turn,
            requests: match decoded.outcome {
                ProviderOutcome::Tools(requests) => requests,
                _ => panic!("Tool request"),
            },
            observations: vec![],
            facts: None,
            steering_notes: Vec::new(),
            calls_remaining: None,
            cleared: false,
            hook_feedback: Default::default(),
        });
    coding.input.tools.as_mut().unwrap().steps[0]
        .observations
        .push(crate::provider::test_support::observation(
            crate::tools::ToolCall::Read {
                path: "README.md".into(),
                offset: None,
                limit: Some(10),
            },
            "{}",
        ));
    let value: Value = serde_json::from_slice(&body_for(&coding, Effort::Low).unwrap()).unwrap();
    let items = value["input"].as_array().unwrap();
    let reasoning = items
        .iter()
        .find(|item| item["type"] == "reasoning")
        .unwrap();
    let call = items
        .iter()
        .find(|item| item["type"] == "function_call")
        .unwrap();
    assert!(reasoning.get("id").is_none() && call.get("id").is_none());
    assert_eq!(reasoning["encrypted_content"], "CIPHERTEXT==");
    assert_eq!(call["call_id"], "call_1");
}

#[test]
fn chatgpt_uses_auto_with_phase_specific_functions() {
    let mut researcher = crate::provider::test_support::researcher_request("model-a");
    let active: Value =
        serde_json::from_slice(&body_for(&researcher, Effort::Low).unwrap()).unwrap();
    let mut spent = crate::provider::test_support::catalog();
    spent["tool_calls_remaining"] = json!(0);
    researcher.input.tools = Some(crate::provider::test_support::tool_context(spent));
    let value: Value =
        serde_json::from_slice(&body_for(&researcher, Effort::Low).unwrap()).unwrap();
    for field in ["instructions", "prompt_cache_key"] {
        assert_eq!(value[field], active[field], "{field}");
    }
    assert_eq!(value["prompt_cache_key"], researcher.session_id.to_string());
    assert_eq!(value["tools"], active["tools"]);
    assert_eq!(value["tool_choice"], "none");
    assert_eq!(value["parallel_tool_calls"], false);
    let names = |body: &Value| {
        body["tools"]
            .as_array()
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert!(names(&active).contains(&"arany_read".into()));
    let mut primary = crate::provider::test_support::primary_request("model-a");
    for policy in [
        crate::session::CollaborationPolicy::Auto {
            max_active_children: 2,
        },
        crate::session::CollaborationPolicy::Team {
            max_active_children: 2,
        },
    ] {
        primary.collaboration = policy;
        primary.phase = AgentPhase::RootPlan;
        let planning: Value =
            serde_json::from_slice(&body_for(&primary, Effort::Low).unwrap()).unwrap();
        primary.phase = AgentPhase::RootSynthesis;
        let synthesis: Value =
            serde_json::from_slice(&body_for(&primary, Effort::Low).unwrap()).unwrap();
        for field in ["instructions", "prompt_cache_key"] {
            assert_eq!(planning[field], synthesis[field], "{field}");
        }
        for body in [&planning, &synthesis] {
            assert_eq!(body["tool_choice"], "auto");
            assert_eq!(body["prompt_cache_key"], primary.session_id.to_string());
        }
        assert!(names(&planning).contains(&"arany_delegate".into()));
        assert!(!names(&synthesis).contains(&"arany_delegate".into()));
        assert_ne!(planning["tools"], synthesis["tools"]);
    }
    let mut next_run = primary.clone();
    next_run.run_id = RunId::new();
    let next: Value = serde_json::from_slice(&body_for(&next_run, Effort::Low).unwrap()).unwrap();
    assert_eq!(next["prompt_cache_key"], primary.session_id.to_string());
}

#[test]
fn optional_plan_is_separate_from_effect_schemas_and_has_a_correlated_result() {
    let mut request = crate::provider::test_support::primary_request("model-a");
    let body: serde_json::Value =
        serde_json::from_slice(&body_for(&request, Effort::Low).unwrap()).unwrap();
    let functions = body["tools"].as_array().unwrap();
    assert!(
        functions
            .iter()
            .any(|function| function["name"] == "arany_plan")
    );
    assert!(functions.iter().all(|function| {
        function["parameters"]["properties"]
            .get("plan_contract")
            .is_none()
    }));
    let contract = crate::provider::test_support::goal_plan_contract();
    let reply = json!({"id":"resp_plan", "status":"completed", "model":"model-a", "output":[
        {"type":"function_call", "id":"fc_plan", "call_id":"plan_1", "name":"arany_plan", "arguments":serde_json::to_string(&contract).unwrap(), "status":"completed"}
    ], "usage":{"input_tokens":8,"output_tokens":4}});
    let conversation = render::conversation(&request).unwrap();
    let decoded = wire::decode_run(
        &serde_json::to_vec(&reply).unwrap(),
        &wire::RunScope::new(&request, &conversation, TOKEN),
    )
    .unwrap();
    let ProviderOutcome::Plan(plan) = decoded.outcome else {
        panic!("optional plan");
    };
    assert_eq!(plan.contract, contract);
    request
        .input
        .tools
        .as_mut()
        .unwrap()
        .steps
        .push(crate::tools::ToolStep {
            plan: Some(*plan),
            turn: decoded.turn,
            requests: Vec::new(),
            observations: Vec::new(),
            facts: None,
            steering_notes: Vec::new(),
            calls_remaining: None,
            cleared: false,
            hook_feedback: Default::default(),
        });
    let continuation: serde_json::Value =
        serde_json::from_slice(&body_for(&request, Effort::Low).unwrap()).unwrap();
    let result = continuation["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap();
    assert_eq!(result["call_id"], "plan_1");
    let output: serde_json::Value =
        serde_json::from_str(result["output"].as_str().unwrap()).unwrap();
    assert_eq!(output["status"], "plan_recorded");
    assert_eq!(output["scope"]["paths"], json!(["src"]));
}

#[test]
fn answer_text_streams_before_completion_and_is_withdrawn_when_a_call_follows() {
    let call = json!({"id":"fc_1","type":"function_call","call_id":"call_1","name":"arany_read","arguments":"{}","status":"completed"});
    for (with_call, answer, commentary) in [
        (false, "hello", Vec::<String>::new()),
        (true, "", vec!["hello".to_owned()]),
    ] {
        let message = json!({"id":"msg_1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hello"}]});
        let mut events = vec![
            json!({"type":"response.created","response":{"id":"resp_1","model":"model-a"}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","status":"in_progress","content":[]}}),
            json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg_1","delta":"hello"}),
        ];
        let (publisher, observed) = ResponsePreview::new();
        let mut decoder = StreamDecoder::with_preview(TOKEN, "model-a", Some(publisher));
        for event in &events {
            decoder
                .feed(format!("data: {event}\n\n").as_bytes())
                .unwrap();
        }
        assert_eq!(observed.borrow().answer, "hello");
        events = vec![json!({"type":"response.output_item.done","output_index":0,"item":message})];
        let mut output = vec![message.clone()];
        if with_call {
            events.push(json!({"type":"response.output_item.added","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"arany_read","arguments":"","status":"in_progress"}}));
            events.push(json!({"type":"response.output_item.done","output_index":1,"item":call}));
            output.push(call.clone());
        }
        events.push(json!({"type":"response.completed","response":{"id":"resp_1","model":"model-a","status":"completed","output":output,"usage":{"input_tokens":8,"output_tokens":12}}}));
        for event in &events {
            decoder
                .feed(format!("data: {event}\n\n").as_bytes())
                .unwrap();
        }
        assert_eq!(observed.borrow().answer, answer);
        assert_eq!(observed.borrow().commentary, commentary);
    }
}

#[test]
fn oversized_streamed_answer_leaves_refusal_to_the_engine_instead_of_failing_the_call() {
    let text = "x".repeat(crate::session::MAX_DISPLAY_TEXT_BYTES + 1);
    let message = json!({"id":"msg_1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text}]});
    let events = [
        json!({"type":"response.created","response":{"id":"resp_1","model":"model-a"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","status":"in_progress","content":[]}}),
        json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg_1","delta":text}),
        json!({"type":"response.output_item.done","output_index":0,"item":message}),
        json!({"type":"response.completed","response":{"id":"resp_1","model":"model-a","status":"completed","output":[message],"usage":{"input_tokens":8,"output_tokens":12}}}),
    ];
    let (publisher, observed) = ResponsePreview::new();
    let mut decoder = StreamDecoder::with_preview(TOKEN, "model-a", Some(publisher));
    for event in &events {
        decoder
            .feed(format!("data: {event}\n\n").as_bytes())
            .unwrap();
    }
    assert!(observed.borrow().answer.len() <= crate::session::MAX_DISPLAY_TEXT_BYTES);
}

#[test]
fn streamed_answer_is_tentative_unless_the_item_is_final_answer() {
    for (phase, tentative) in [(None, true), (Some("final_answer"), false)] {
        let mut added = json!({"id":"msg_1","type":"message","role":"assistant","status":"in_progress","content":[]});
        if let Some(phase) = phase {
            added["phase"] = json!(phase);
        }
        let events = [
            json!({"type":"response.created","response":{"id":"resp_1","model":"model-a"}}),
            json!({"type":"response.output_item.added","output_index":0,"item":added}),
            json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg_1","delta":"hello"}),
        ];
        let (publisher, observed) = ResponsePreview::new();
        let mut decoder = StreamDecoder::with_preview(TOKEN, "model-a", Some(publisher));
        for event in &events {
            decoder
                .feed(format!("data: {event}\n\n").as_bytes())
                .unwrap();
        }
        assert_eq!(observed.borrow().answer, "hello");
        assert_eq!(observed.borrow().tentative, tentative, "{phase:?}");
    }
}

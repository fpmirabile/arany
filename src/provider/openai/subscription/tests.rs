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
        admission: crate::provider::ChatGptAdmission::Conformance,
    };
    for admission in [
        crate::provider::ChatGptAdmission::Conformance,
        crate::provider::ChatGptAdmission::AccountConsent,
    ] {
        let provenance = ChatGptProvenance {
            admission,
            ..provenance.clone()
        };
        let provider = ChatGptProvider::from_checked_account(
            "model-a".into(),
            Effort::High,
            TOKEN.into(),
            provenance.clone(),
        )
        .expect("admitted synthetic account");
        assert_eq!(provider.profile_name(), "chatgpt");
        assert_eq!(provider.model_name(), "model-a");
        assert_eq!(provider.reasoning_effort(), Some(Effort::High));
        assert_eq!(provider.max_concurrent_calls(), 1);
        assert_eq!(provider.chatgpt_provenance(), Some(provenance));
        assert_eq!(
            provider.output_token_bound(),
            OutputTokenBound::LocalAcceptanceOnly
        );
    }
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
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        phase: AgentPhase::RootPlan,
        collaboration: crate::session::CollaborationPolicy::Single,
        model: "model-a".into(),
        instructions: None,
        objective: "Synthetic objective".into(),
        images: Vec::new(),
        includes: Vec::new(),
        history: Vec::new(),
        context_summary: None,
        child_results: Vec::new(),
        max_output_tokens: 64,
        tools: None,
    }
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
    completed_with_text(
        output_tokens,
        r#"{"outcome":{"type":"finish","summary":"safe","result":"done"}}"#,
    )
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
    format!(
        "event: response.created\r\ndata: {{\"type\":\"response.created\"}}\r\n\r\nevent: response.completed\r\ndata: {}\r\n\r\n",
        json!({"type": "response.completed", "response": response})
    ).into_bytes()
}

#[test]
fn subscription_body_is_streaming_and_omits_unsupported_remote_cap() {
    let body = subscription_body(&request(), Effort::Medium).expect("bounded body");
    let value: Value = serde_json::from_slice(&body).expect("JSON");
    assert_eq!(value["model"], "model-a");
    assert_eq!(value["store"], false);
    assert_eq!(value["stream"], true);
    assert_eq!(value["reasoning"]["effort"], "medium");
    assert_eq!(value["input"][0]["role"], "user");
    assert!(
        value["input"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Synthetic objective")
    );
    assert_eq!(value["text"]["format"]["type"], "json_schema");
    for forbidden in ["max_output_tokens", "truncation", "previous_response_id"] {
        assert!(value.get(forbidden).is_none(), "{forbidden}");
    }
    {
        use crate::provider::{ImageAttachment, ImageOrigin, ProviderImage};
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
        let image = ImageAttachment::from_png(&STANDARD.decode(encoded).unwrap()).unwrap();
        let mut selected = request();
        selected.images = vec![ProviderImage {
            origin: ImageOrigin::Objective,
            image,
        }];
        let body = subscription_body(&selected, Effort::Low).expect("streaming image input");
        let multimodal: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            multimodal["input"],
            json!([{"role": "user", "content": [
                {"type": "input_text", "text": "Image 1 (objective): PNG 1x1"},
                {"type": "input_image", "image_url": format!("data:image/png;base64,{encoded}"), "detail": "auto"},
                {"type": "input_text", "text": value["input"][0]["content"]},
            ]}])
        );
        assert_eq!(multimodal["store"], false);
        assert_eq!(multimodal["stream"], true);
        assert_eq!(multimodal["reasoning"]["effort"], "low");
        assert_eq!(multimodal["text"], value["text"]);
        for forbidden in ["max_output_tokens", "truncation", "previous_response_id"] {
            assert!(multimodal.get(forbidden).is_none(), "{forbidden}");
        }
        selected.images[0].origin = ImageOrigin::History { turn_index: 0 };
        selected.history.push(crate::provider::HistoryTurn {
            user: "prior objective".into(),
            assistant: "prior result".into(),
        });
        assert!(
            subscription_body(&selected, Effort::Low).is_ok(),
            "declared history origin"
        );
        selected.phase = AgentPhase::ChildWork;
        assert!(
            matches!(
                subscription_body(&selected, Effort::Low),
                Err(ProviderError::Rejected)
            ),
            "children cannot receive historical pixels"
        );
        selected.phase = AgentPhase::RootPlan;
        selected.history.clear();
        assert!(
            matches!(
                subscription_body(&selected, Effort::Low),
                Err(ProviderError::Rejected)
            ),
            "missing historical origin rejected before transport"
        );
        selected.images[0].origin = ImageOrigin::Objective;
        selected.images = vec![selected.images[0].clone(); 5];
        assert!(
            matches!(
                subscription_body(&selected, Effort::Low),
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
            (
                "Run",
                r#"{"outcome":{"type":"finish","summary":"safe","result":"done"}}"#,
            ),
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
                    wire::decode_streamed_run(&body, "model-a").expect("complete streamed Run");
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
            (
                "malformed outcome",
                "/item/content/0/text",
                json!("plain text"),
            ),
        ] {
            let mut changed = done.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            let stream = format!("data: {created}\n\ndata: {changed}\n\ndata: {terminal}\n\n");
            let body = StreamDecoder::new(TOKEN)
                .feed(stream.as_bytes())
                .unwrap()
                .unwrap();
            assert!(
                wire::decode_streamed_run(&body, "model-a").is_err(),
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
                wire::decode_streamed_run(&body, "model-a").is_err(),
                "{label}"
            );
            assert!(
                wire::decode_streamed_compaction(&body, "model-a").is_err(),
                "{label}"
            );
        }
        let reflected = prefix.replace("safe", TOKEN);
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
    let decoded = wire::decode_streamed_run(&body, "model-a").expect("strict outcome");
    assert!(matches!(decoded.outcome, ProviderOutcome::Finish(_)));
    assert_eq!(decoded.output_tokens, Some(12));
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
    assert!(wire::decode_streamed_run(duplicate_field, "model-a").is_err());

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
        assert_eq!(response.output_tokens, Some(12), "{label}");
        assert!(
            matches!(response.outcome, ProviderOutcome::Finish(ref value) if value.result == "done")
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
            "wrong outcome",
            completed_with_text(
                Some(12),
                r#"{"outcome":{"type":"finish","summary":"safe"}}"#,
            ),
            crate::ProviderFailureReason::OutcomeContract,
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
            assert!(!error.to_string().contains(TOKEN));
            server.await.expect("server");
        }
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
    assert_eq!(response.output_tokens, Some(12));
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
            "content": [{"type": "output_text", "text": r#"{"outcome":{"type":"finish","summary":"safe","result":"done"}}"#}]
        }],
        "usage": {"input_tokens": 8, "output_tokens": 12}
    }).to_string().into_bytes();
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
        assert!(
            matches!(&actual, ProviderError::RemoteHttp(actual_status) if *actual_status == status)
        );
        assert_eq!(actual.failure_class(), expected, "HTTP {status}");
        assert_eq!(
            actual.to_string(),
            format!("Provider returned HTTP {status}")
        );
        server.await.expect("server");
    }
}

#[tokio::test]
async fn model_probe_requires_distinct_direct_delegate_and_compaction_outcomes() {
    let direct = event_reply(completed_with_id(
        "resp_direct",
        Some(12),
        r#"{"outcome":{"type":"finish","summary":"safe","result":"done"}}"#,
    ));
    let delegate = event_reply(completed_with_id(
        "resp_delegate",
        Some(12),
        r#"{"outcome":{"type":"delegate","children":["Explain two"]}}"#,
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
        assert_eq!(
            body["text"]["format"]["name"],
            if index == 2 {
                "arany_compaction"
            } else {
                "arany_outcome"
            }
        );
    }

    let wrong_direct = event_reply(completed_with_id(
        "resp_direct",
        Some(12),
        r#"{"outcome":{"type":"delegate","children":["Wrong phase"]}}"#,
    ));
    let repeated_delegate = event_reply(completed_with_id(
        "resp_direct",
        Some(12),
        r#"{"outcome":{"type":"delegate","children":["Explain two"]}}"#,
    ));
    let wrong_delegate = event_reply(completed_with_id(
        "resp_delegate",
        Some(12),
        r#"{"outcome":{"type":"finish","summary":"safe","result":"done"}}"#,
    ));
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
        r#"{"outcome":{"type":"finish","summary":"safe","result":"done"}}"#,
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
                matches!(error, ProviderError::InvalidOutcome)
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

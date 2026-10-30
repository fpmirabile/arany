use super::transport::{CustomTransport, TransportError};
use crate::provider::{
    AgentPhase, CompactionItem, CompactionRequest, HistoryTurn, PromptRole, ProviderRequest,
    RunInput,
};
use crate::session::{AgentRunId, RunId, SessionId};
use serde_json::json;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const CONTRACT_RESPONSE_BYTES: usize = crate::provider::MAX_RESPONSE_BYTES;

#[derive(Clone, Copy, Debug)]
enum Reply {
    AggregateJsonNodes,
    WrongModel,
    ZeroUsage,
    MissingUsage,
    Redirect,
    Unauthorized,
    RateLimited,
    QuotaExceeded,
    Oversize,
    ChunkedAtLimit,
    ChunkedOversize,
    EncodedBody,
    EncodedBodyAfterIdentity,
    #[cfg(debug_assertions)]
    Disconnect,
    #[cfg(debug_assertions)]
    MalformedJson,
    InputAtLimit,
    InputAboveLimit,
}

fn response_byte_limit(request: &serde_json::Value) -> usize {
    if request["stream"] != true {
        return CONTRACT_RESPONSE_BYTES;
    }
    let cap = request
        .get("max_tokens")
        .or_else(|| request.get("max_output_tokens"))
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
        .expect("stream request output cap");
    crate::provider::max_stream_bytes(cap)
}

fn padded_event_stream(events: &str, target: usize) -> String {
    let padding = target
        .checked_sub(events.len() + 1)
        .expect("stream padding");
    let comment = format!(":{}\n", "x".repeat(CONTRACT_RESPONSE_BYTES - 2));
    let mut body = comment.repeat(padding / comment.len());
    body.push_str(&comment[..padding % comment.len()]);
    body.push('\n');
    body.push_str(events);
    assert_eq!(body.len(), target);
    body
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> (String, serde_json::Value) {
    let request = crate::provider::test_support::read_loopback_request(stream).await;
    let header_end = request.find("\r\n\r\n").expect("HTTP header") + 4;
    let body = serde_json::from_str(&request[header_end..]).expect("JSON request");
    (request[..header_end].to_ascii_lowercase(), body)
}

// Serves one reply to one request and returns the Messages aggregate wire and reconstructed sizes.
async fn serve(listener: TcpListener, reply: Reply, messages: bool) -> Option<(usize, usize)> {
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("connection deadline")
        .expect("connection");
    let (header, body) = read_request(&mut stream).await;
    let path = if messages {
        "/anthropic/v1/messages"
    } else {
        "/v1/responses"
    };
    assert!(header.starts_with(&format!("post {path} http/1.1\r\n")));
    assert!(header.contains(concat!("user-agent: arany/", env!("CARGO_PKG_VERSION"))));
    assert!(!header.contains("anthropic-workspace-id"));
    if messages {
        assert!(header.contains("x-api-key: test-key\r\n"));
        assert!(!header.contains("authorization:"));
    } else {
        assert!(header.contains("authorization: bearer test-key\r\n"));
        assert!(!header.contains("x-api-key:"));
    }
    assert_eq!(
        header.contains("anthropic-version: 2023-06-01\r\n"),
        messages
    );
    let streaming = body["stream"] == true;
    let content_type = if streaming {
        "text/event-stream"
    } else {
        "application/json"
    };
    let instructions = crate::provider::EffectivePrompts::default().compaction_instructions();
    let compaction = if messages {
        body["system"][0]["text"] == instructions
    } else {
        body["instructions"] == instructions
    };
    if compaction {
        for field in ["text", "tools", "tool_choice", "parallel_tool_calls"] {
            assert!(
                body.get(field).is_none(),
                "unexpected compaction field {field}"
            );
        }
        assert!(body["output_config"].get("format").is_none());
    }
    assert_eq!(body["model"], "model-1");
    if messages {
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
    } else {
        assert_eq!(body["store"], false);
        assert_eq!(body["truncation"], "disabled");
        assert!(body.get("text").is_none());
        assert!(body.get("reasoning").is_none());
    }
    assert_eq!(
        body[if messages {
            "max_tokens"
        } else {
            "max_output_tokens"
        }],
        if compaction { 128 } else { 4096 }
    );

    if matches!(reply, Reply::EncodedBody | Reply::EncodedBodyAfterIdentity) {
        let encoding = if matches!(reply, Reply::EncodedBodyAfterIdentity) {
            "identity\r\nContent-Encoding: gzip"
        } else {
            "gzip"
        };
        stream
            .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Encoding: {encoding}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").as_bytes())
            .await
            .expect("encoded response headers");
        let mut next = [0];
        let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut next))
            .await
            .expect("encoded response was rejected without reading its body")
            .expect("encoded response peer state");
        assert_eq!(read, 0);
        return None;
    }

    let (status, response_body) = match reply {
        Reply::Redirect => ("302 Found", String::new()),
        Reply::Unauthorized => ("401 Unauthorized", String::new()),
        Reply::RateLimited => ("429 Too Many Requests", String::new()),
        Reply::QuotaExceeded => (
            "429 Too Many Requests",
            json!({"error":{"type":"insufficient_quota"}}).to_string(),
        ),
        Reply::Oversize => ("200 OK", String::new()),
        #[cfg(debug_assertions)]
        Reply::MalformedJson => ("200 OK", "{".to_owned()),
        #[cfg(debug_assertions)]
        Reply::Disconnect => return None,
        Reply::EncodedBody | Reply::EncodedBodyAfterIdentity => unreachable!(),
        Reply::AggregateJsonNodes
        | Reply::WrongModel
        | Reply::ZeroUsage
        | Reply::MissingUsage
        | Reply::ChunkedAtLimit
        | Reply::ChunkedOversize
        | Reply::InputAtLimit
        | Reply::InputAboveLimit => {
            let text = if compaction {
                json!("plausible summary")
            } else {
                json!("ok")
            };
            let model = if matches!(reply, Reply::WrongModel) {
                "drifted-model"
            } else {
                "model-1"
            };
            let mut response = json!({
                "id": "resp_1",
                "status": "completed",
                "model": model,
                "output": crate::provider::render::semantic::responses_output(&text),
                "usage": {"input_tokens":20,"output_tokens":10}
            });
            if messages {
                response = json!({
                    "id": "msg_1", "type":"message", "role":"assistant",
                    "model":model, "stop_reason":"end_turn",
                    "content":crate::provider::render::semantic::messages_content(&text),
                    "usage":{"input_tokens":20,"output_tokens":10}
                });
            }
            match reply {
                Reply::ZeroUsage => {
                    response["usage"] = json!({"input_tokens":0,"output_tokens":0});
                }
                Reply::MissingUsage => {
                    response.as_object_mut().unwrap().remove("usage");
                }
                Reply::InputAtLimit | Reply::InputAboveLimit => {
                    response["usage"]["input_tokens"] = json!(
                        crate::provider::MAX_REPORTED_INPUT_TOKENS
                            + u32::from(matches!(reply, Reply::InputAboveLimit))
                    );
                }
                Reply::ChunkedAtLimit | Reply::ChunkedOversize if !streaming => {
                    response["padding"] = json!("");
                    let target = response_byte_limit(&body)
                        + usize::from(matches!(reply, Reply::ChunkedOversize));
                    let padding = target - response.to_string().len();
                    response["padding"] = json!("x".repeat(padding));
                    assert_eq!(response.to_string().len(), target);
                }
                _ => {}
            }
            ("200 OK", response.to_string())
        }
    };
    if matches!(reply, Reply::ChunkedAtLimit | Reply::ChunkedOversize) {
        let response_body = if streaming {
            assert!(messages);
            let response: serde_json::Value = serde_json::from_str(&response_body).unwrap();
            let events = crate::provider::render::semantic::messages_stream(&response);
            let target =
                response_byte_limit(&body) + usize::from(matches!(reply, Reply::ChunkedOversize));
            padded_event_stream(&events, target)
        } else {
            response_body
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n{response_body}\r\n0\r\n\r\n",
            response_body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("chunked response");
        return None;
    }
    if matches!(reply, Reply::AggregateJsonNodes) {
        assert!(streaming && messages);
        let mut start: serde_json::Value = serde_json::from_str(&response_body).unwrap();
        start["content"] = json!([]);
        start["stop_reason"] = serde_json::Value::Null;
        start["usage"]["output_tokens"] = json!(0);
        start["usage"]["first_extra"] = json!(vec![0; 132_000]);
        let events = [
            json!({"type":"message_start","message":start.clone()}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":10,"second_extra":vec![0;132_000]}}),
            json!({"type":"message_stop"}),
        ];
        let body: String = events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect();
        assert!(body.len() < CONTRACT_RESPONSE_BYTES);
        let mut assembled = start;
        assembled["content"] = json!([{"type":"text","text":"ok"}]);
        assembled["stop_reason"] = json!("end_turn");
        assembled["usage"]["output_tokens"] = json!(10);
        assembled["usage"]["second_extra"] = json!(vec![0; 132_000]);
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        return Some((body.len(), serde_json::to_vec(&assembled).unwrap().len()));
    }
    if status == "200 OK" && streaming && messages && !response_body.is_empty() {
        let response: serde_json::Value = serde_json::from_str(&response_body).unwrap();
        let body = crate::provider::render::semantic::messages_stream(&response);
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        return None;
    }
    let declared_len = if matches!(reply, Reply::Oversize) {
        response_byte_limit(&body) + 1
    } else {
        response_body.len()
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {declared_len}\r\nConnection: close\r\n\r\n{response_body}"
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("response");
    None
}

fn request() -> ProviderRequest {
    ProviderRequest {
        session_id: SessionId::new(),
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        phase: AgentPhase::RootPlan,
        collaboration: crate::session::CollaborationPolicy::Single,
        model: "model-1".into(),
        max_output_tokens: 4096,
        input: RunInput::new(PromptRole::ReadOnly, "Synthetic request".into()),
    }
}

async fn run_case(
    reply: Reply,
    compaction: bool,
    messages: bool,
    streaming: bool,
) -> Result<(), TransportError> {
    run_case_observed(reply, compaction, messages, streaming)
        .await
        .0
}

async fn run_case_observed(
    reply: Reply,
    compaction: bool,
    messages: bool,
    streaming: bool,
) -> (Result<(), TransportError>, Option<(usize, usize)>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener");
    let port = listener.local_addr().expect("listener address").port();
    let profile = super::local_profile(port, messages);
    let destination = profile
        .resolve_destination()
        .await
        .expect("pinned loopback");
    let server = tokio::spawn(serve(listener, reply, messages));
    let transport =
        CustomTransport::new(&profile, &destination, "test-key".into()).expect("transport");
    let request = request();
    let deadline = Duration::from_secs(10);
    let result = if compaction {
        let request = CompactionRequest {
            session_id: SessionId::new(),
            covered_run_id: RunId::new(),
            model: request.model,
            previous_summary: None,
            items: vec![CompactionItem::Completed(HistoryTurn {
                user: "Synthetic question".into(),
                assistant: "Synthetic answer".into(),
            })],
            max_output_tokens: 128,
            prompts: Default::default(),
        };
        tokio::time::timeout(deadline, transport.compact(&request, None))
            .await
            .expect("compaction transport deadline")
            .map(|_| ())
    } else if streaming {
        tokio::time::timeout(deadline, transport.run_streamed(&request, None, None))
            .await
            .expect("stream deadline")
            .map(|_| ())
    } else {
        tokio::time::timeout(deadline, transport.run(&request, None))
            .await
            .expect("transport deadline")
            .map(|_| ())
    };
    (result, server.await.expect("server completed"))
}

#[tokio::test]
#[cfg(debug_assertions)]
#[cfg_attr(arany_guard, ignore = "requires local test IPC")]
async fn custom_messages_transport_rejections_are_logged_with_observed_shape() {
    if crate::test_process::isolate_test() {
        return;
    }
    let temp = tempfile::tempdir().expect("diagnostic state");
    let root =
        crate::store::StateRoot::admit(&temp.path().join("state")).expect("private state root");
    crate::diagnostics::enable_development_diagnostics(&root);
    let path = root.path().join("development.log");
    let logged = |line: &str| std::fs::read_to_string(&path).unwrap().contains(line);

    assert!(matches!(
        run_case(Reply::Disconnect, false, true, false).await,
        Err(TransportError::Transport)
    ));
    assert!(logged(
        "messages stage=RequestTransport bytes=0 events=0 http_status=None"
    ));

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("route listener");
    let port = listener.local_addr().expect("route address").port();
    let profile = super::local_profile(port, true);
    let pinned = profile.resolve_destination().await.expect("pinned route");
    let transport = CustomTransport::new(&profile, &pinned, "test-key".into()).expect("transport");
    assert!(matches!(
        transport.admit_stream_destination(
            &format!("http://127.0.0.1:{port}/anthropic/redirected"),
            reqwest::StatusCode::OK
        ),
        Err(TransportError::Rejected)
    ));
    assert!(logged(
        "messages stage=Destination bytes=0 events=0 http_status=Some(200)"
    ));

    assert!(matches!(
        run_case(Reply::Unauthorized, false, true, false).await,
        Err(TransportError::Rejected)
    ));
    assert!(logged(
        "messages stage=HttpStatus bytes=0 events=0 http_status=Some(401)"
    ));

    assert!(matches!(
        run_case(Reply::MalformedJson, false, true, false).await,
        Err(TransportError::InvalidOutcome)
    ));
    assert!(logged(
        "messages stage=ResponseEnvelope bytes=1 events=0 http_status=Some(200)"
    ));

    let (aggregate, shape) = run_case_observed(Reply::AggregateJsonNodes, false, true, true).await;
    assert!(matches!(aggregate, Err(TransportError::InvalidOutcome)));
    let (wire_bytes, reconstructed_bytes) = shape.expect("aggregate stream bytes");
    assert_ne!(wire_bytes, reconstructed_bytes);
    assert!(logged(&format!(
        "messages stage=ResponseEnvelope bytes={wire_bytes} events=6 http_status=Some(200)"
    )));
}

#[tokio::test]
// Skipped in the Guard: local test IPC capability is missing.
#[cfg_attr(arany_guard, ignore = "requires local test IPC")]
async fn transport_accepts_exact_cap_and_rejects_excess_or_encoding() {
    assert!(
        matches!(
            run_case(Reply::AggregateJsonNodes, false, true, true).await,
            Err(TransportError::InvalidOutcome)
        ),
        "assembled Messages JSON retains the aggregate node bound"
    );
    for (messages, streaming) in [(false, false), (true, false), (true, true)] {
        assert!(
            run_case(Reply::ChunkedAtLimit, false, messages, streaming)
                .await
                .is_ok()
        );
    }
    for streaming in [false, true] {
        for reply in [
            Reply::Oversize,
            Reply::ChunkedOversize,
            Reply::EncodedBody,
            Reply::EncodedBodyAfterIdentity,
            Reply::InputAboveLimit,
        ] {
            assert!(
                matches!(
                    run_case(reply, false, true, streaming).await,
                    Err(TransportError::InvalidOutcome)
                ),
                "Messages reply={reply:?}, streaming={streaming}"
            );
        }
    }
    for compaction in [false, true] {
        assert!(
            run_case(Reply::InputAtLimit, compaction, false, false)
                .await
                .is_ok(),
            "maximum reported input remains valid; compaction={compaction}"
        );
        assert!(
            matches!(
                run_case(Reply::InputAboveLimit, compaction, false, false).await,
                Err(TransportError::InvalidOutcome)
            ),
            "reported input above shared ceiling must reject; compaction={compaction}"
        );
    }
    for reply in [
        Reply::Oversize,
        Reply::ChunkedOversize,
        Reply::EncodedBody,
        Reply::EncodedBodyAfterIdentity,
    ] {
        assert!(matches!(
            run_case(reply, false, false, false).await,
            Err(TransportError::InvalidOutcome)
        ));
    }
}

#[tokio::test]
#[cfg_attr(arany_guard, ignore = "requires local test IPC")]
async fn every_real_response_is_validated() {
    type Expect = fn(&Result<(), TransportError>) -> bool;
    let invalid: Expect = |result| matches!(result, Err(TransportError::InvalidOutcome));
    let rejected: Expect = |result| matches!(result, Err(TransportError::Rejected));
    for messages in [false, true] {
        for (reply, expected) in [
            (Reply::WrongModel, invalid),
            (Reply::ZeroUsage, invalid),
            (Reply::MissingUsage, invalid),
            (Reply::Redirect, rejected),
            (Reply::Unauthorized, rejected),
            (Reply::RateLimited, |result: &Result<(), TransportError>| {
                matches!(result, Err(TransportError::RateLimited(_)))
            }),
            (
                Reply::QuotaExceeded,
                |result: &Result<(), TransportError>| {
                    matches!(result, Err(TransportError::QuotaExceeded))
                },
            ),
        ] {
            let result = run_case(reply, false, messages, false).await;
            assert!(expected(&result), "reply={reply:?}, messages={messages}");
        }
    }
}

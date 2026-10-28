use super::*;
use crate::provider::LimitUnavailable;
use crate::provider::limits::headers::{MAX_LIMIT_HEADER_VALUE_BYTES, MAX_LIMIT_HEADER_VALUES};
use reqwest::header::HeaderValue;
use std::cell::Cell;

#[test]
fn messages_headers_keep_distinct_partial_counters_and_one_receipt() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "anthropic-workspace-id",
        HeaderValue::from_static("remote-workspace-canary"),
    );
    headers.insert("x-ratelimit-limit-requests", HeaderValue::from_static("9"));
    headers.insert(
        "x-codex-primary-used-percent",
        HeaderValue::from_static("25"),
    );
    let slot = RateLimitSlot::default();
    HEADERS.collect(&headers, &slot, || panic!("foreign headers read no clock"));
    assert!(slot.take().is_none());
    for names in HEADERS.names.as_chunks::<3>().0 {
        headers.insert(names[1], HeaderValue::from_static("0"));
        headers.insert(
            names[2],
            HeaderValue::from_static("1970-01-01T01:30:00.001+01:30"),
        );
    }
    let clock = Cell::new(0);
    HEADERS.collect(&headers, &slot, || {
        clock.set(clock.get() + 1);
        1000
    });
    assert_eq!(clock.get(), 1);
    let sample = slot.take().unwrap();
    assert!(sample.primary.is_none() && sample.secondary.is_none());
    let bytes = serde_json::to_vec(&sample).unwrap();
    assert!(!bytes.windows(b"canary".len()).any(|part| part == b"canary"));
    let api = sample.api.unwrap();
    assert_eq!(api.dialect, ApiLimitDialect::Anthropic);
    assert_eq!(api.rows.len(), HEADERS.dimensions.len());
    for (row, dimension) in api.rows.iter().zip(HEADERS.dimensions) {
        assert_eq!(row.dimension, *dimension);
        assert_eq!(row.limit, None);
        assert_eq!(row.remaining, Some(0));
        assert_eq!(row.reset_at, Some(1));
        assert_eq!(row.observed_at, 1000);
    }
    HEADERS.collect(&headers, &slot, || -1);
    assert!(slot.take().is_none());
}

#[test]
fn messages_optional_header_errors_are_closed_and_bounded() {
    let oversized = "1".repeat(MAX_LIMIT_HEADER_VALUE_BYTES + 1);
    for (values, expected) in [
        (vec!["0"], None),
        (vec!["18446744073709551615"], None),
        (
            vec!["18446744073709551616"],
            Some(LimitUnavailable::Invalid),
        ),
        (vec!["-1"], Some(LimitUnavailable::Invalid)),
        (vec!["1e3"], Some(LimitUnavailable::Invalid)),
        (vec![" 1"], Some(LimitUnavailable::Invalid)),
        (vec!["1", "2"], Some(LimitUnavailable::Duplicate)),
        (vec![oversized.as_str()], Some(LimitUnavailable::Oversized)),
        (
            vec!["1"; MAX_LIMIT_HEADER_VALUES + 1],
            Some(LimitUnavailable::Oversized),
        ),
    ] {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(HEADERS.names[0], value.parse().unwrap());
        }
        headers.insert(HEADERS.names[4], HeaderValue::from_static("1000"));
        let slot = RateLimitSlot::default();
        HEADERS.collect(&headers, &slot, || 1000);
        let api = slot.take().unwrap().api.unwrap();
        assert_eq!(
            api.unavailable
                .map(|failure| failure.reason)
                .or_else(|| api.rows[0].unavailable),
            expected
        );
        if api.unavailable.is_none() {
            assert_eq!(api.rows[1].remaining, Some(1000));
        }
    }
    for reset in [
        "9999-12-31T23:59:59.1Z",
        "1969-12-31T23:59:59Z",
        "9999-12-31T23:59:59-00:01",
        "2023-02-29T00:00:00Z",
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(HEADERS.names[2], reset.parse().unwrap());
        let slot = RateLimitSlot::default();
        HEADERS.collect(&headers, &slot, || 1000);
        assert_eq!(
            slot.take().unwrap().api.unwrap().rows[0].unavailable,
            Some(LimitUnavailable::Invalid)
        );
    }
}

#[tokio::test]
#[cfg_attr(arany_guard, ignore = "requires local test IPC")]
async fn messages_transport_retains_only_admitted_headers_through_failure_and_drop() {
    for streaming in [false, true] {
        for case in [
            "success",
            "429",
            "503",
            "cut",
            "cancelled",
            "reflection",
            "reflection 429",
            "reflection 503",
        ] {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                transport_attempt(streaming, case),
            )
            .await
            .expect("bounded Messages transport fixture");
        }
    }
}

async fn transport_peer(
    listener: tokio::net::TcpListener,
    streaming: bool,
    case: &'static str,
    released: tokio::sync::oneshot::Receiver<()>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    const REQUEST_BOUND: usize = 8192;
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut request = Vec::new();
    while !request.windows(4).any(|part| part == b"\r\n\r\n") {
        let mut chunk = [0; 1024];
        let count = socket.read(&mut chunk).await.unwrap();
        assert!(count > 0 && request.len() + count <= REQUEST_BOUND);
        request.extend_from_slice(&chunk[..count]);
    }
    let status = match case {
        "429" | "reflection 429" => 429,
        "503" | "reflection 503" => 503,
        _ => 200,
    };
    let response = serde_json::json!({
        "id": "msg_limits", "type": "message", "role": "assistant",
        "model": "claude-sonnet-5", "stop_reason": "end_turn",
        "stop_sequence": null, "content": [{"type": "text", "text": "safe answer"}],
        "usage": {"input_tokens": 12, "output_tokens": 8}
    });
    let body = if case == "cut" {
        "broken".into()
    } else if streaming {
        crate::provider::render::semantic::messages_stream(&response)
    } else {
        response.to_string()
    };
    let media = if streaming {
        "text/event-stream"
    } else {
        "application/json"
    };
    let reflection = if case.starts_with("reflection") {
        "x-reflected: selected-secret\r\n"
    } else {
        ""
    };
    socket.write_all(format!(
        "HTTP/1.1 {status} Test\r\nContent-Type: {media}\r\nContent-Length: {}\r\nanthropic-ratelimit-input-tokens-remaining: 0\r\n{reflection}Connection: close\r\n\r\n",
        body.len() + usize::from(case == "cut")
    ).as_bytes()).await.unwrap();
    if case == "cancelled" {
        let _ = released.await;
    } else if !case.starts_with("reflection") {
        socket.write_all(body.as_bytes()).await.unwrap();
    }
}

async fn transport_attempt(streaming: bool, case: &'static str) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let mut peer = tokio::task::JoinSet::new();
    peer.spawn(transport_peer(listener, streaming, case, released));
    let slot = RateLimitSlot::default();
    let http = crate::provider::http_client(std::time::Duration::from_secs(3))
        .build()
        .unwrap()
        .post(format!("http://{address}/messages"));
    let admitted = super::super::send_response(http, "selected-secret", &slot).await;
    match case {
        case if case.starts_with("reflection") => {
            assert!(matches!(admitted, Err(ProviderError::InvalidOutcome)));
        }
        "429" => assert!(matches!(admitted, Err(ProviderError::RateLimited(_)))),
        "503" => assert!(matches!(
            admitted,
            Err(ProviderError::HttpServerUnavailable(_))
        )),
        _ => {
            let mut response = admitted.unwrap();
            let future = async {
                if streaming {
                    let reply = super::super::read_streamed_reply(
                        &mut response,
                        "selected-secret",
                        "claude-sonnet-5",
                        None,
                        1024,
                    )
                    .await?;
                    super::super::wire::decode_compaction_with_counts(
                        &reply.bytes,
                        "claude-sonnet-5",
                        1024,
                        reply.counts,
                    )?;
                } else {
                    let bytes = crate::provider::read_identity_body(
                        &mut response,
                        crate::provider::MAX_RESPONSE_BYTES,
                    )
                    .await?;
                    super::super::wire::decode_compaction(&bytes, "claude-sonnet-5", 1024)?;
                }
                Ok::<_, ProviderError>(())
            };
            if case == "cancelled" {
                assert!(
                    tokio::time::timeout(std::time::Duration::ZERO, future)
                        .await
                        .is_err()
                );
            } else {
                assert_eq!(future.await.is_ok(), case == "success");
            }
        }
    }
    drop(release);
    peer.join_next().await.unwrap().unwrap();
    let sample = slot.take();
    assert_eq!(sample.is_some(), !case.starts_with("reflection"));
    if let Some(sample) = sample {
        let api = sample.api.unwrap();
        assert_eq!(api.dialect, ApiLimitDialect::Anthropic);
        assert_eq!(api.rows[0].dimension, ApiLimitDimension::InputTokens);
        assert_eq!(api.rows[0].remaining, Some(0));
    }
}

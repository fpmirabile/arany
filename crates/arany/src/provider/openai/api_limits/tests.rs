use super::*;
use crate::provider::limits::headers::{MAX_LIMIT_HEADER_VALUE_BYTES, MAX_LIMIT_HEADER_VALUES};
use reqwest::header::HeaderValue;
use std::cell::Cell;

#[test]
fn native_header_samples_are_bounded_independent_and_never_plan_windows() {
    for (values, expected) in [
        (vec!["0"], None),
        (vec!["18446744073709551615"], None),
        (vec!["+1"], Some(LimitUnavailable::Invalid)),
        (vec!["1e2"], Some(LimitUnavailable::Invalid)),
        (
            vec!["18446744073709551616"],
            Some(LimitUnavailable::Invalid),
        ),
        (vec!["1", "2"], Some(LimitUnavailable::Duplicate)),
        (vec!["bad", "2"], Some(LimitUnavailable::Duplicate)),
    ] {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(HEADER_NAMES[0], HeaderValue::from_str(value).unwrap());
        }
        headers.insert(HEADER_NAMES[4], HeaderValue::from_static("0"));
        headers.insert(HEADER_NAMES[8], HeaderValue::from_static("1ms"));
        headers.insert(
            "x-codex-primary-used-percent",
            HeaderValue::from_static("25"),
        );
        headers.insert(
            "anthropic-ratelimit-requests-limit",
            HeaderValue::from_static("99"),
        );
        let clock = Cell::new(0);
        let slot = RateLimitSlot::default();
        headers_at(&headers, &slot, || {
            clock.set(clock.get() + 1);
            1000
        });
        assert_eq!(clock.get(), 1);
        let sample = slot.take().unwrap();
        assert!(sample.primary.is_none() && sample.secondary.is_none());
        let api = sample.api.unwrap();
        assert_eq!(api.rows[0].unavailable, expected);
        assert_eq!(api.rows[1].remaining, Some(0));
        assert_eq!(api.rows[1].limit, None);
        assert_eq!(api.rows[1].reset_at, None);
        assert_eq!(api.rows[2].dimension, ApiLimitDimension::ProjectTokens);
        assert_eq!(api.rows[2].reset_at, Some(1001));
        assert!(api.rows.iter().all(|row| row.observed_at == 1000));
    }
    let oversized = "1".repeat(MAX_LIMIT_HEADER_VALUE_BYTES + 1);
    for (values, reason) in [
        (vec![oversized.as_str(), "1"], LimitUnavailable::Oversized),
        (vec!["1", oversized.as_str()], LimitUnavailable::Duplicate),
        (
            vec!["1"; MAX_LIMIT_HEADER_VALUES + 1],
            LimitUnavailable::Oversized,
        ),
        (
            vec![oversized.as_str(); MAX_LIMIT_HEADER_VALUES],
            LimitUnavailable::Oversized,
        ),
    ] {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(HEADER_NAMES[0], value.parse().unwrap());
        }
        let slot = RateLimitSlot::default();
        headers_at(&headers, &slot, || 1000);
        let api = slot.take().unwrap().api.unwrap();
        assert_eq!(
            api.unavailable
                .map(|f| f.reason)
                .or_else(|| api.rows[0].unavailable),
            Some(reason)
        );
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-codex-primary-used-percent",
        HeaderValue::from_static("25"),
    );
    let slot = RateLimitSlot::default();
    headers_at(&headers, &slot, || {
        panic!("unrecognized metadata reads no clock")
    });
    assert!(slot.take().is_none());
    headers.insert(HEADER_NAMES[0], HeaderValue::from_bytes(b"\xff").unwrap());
    headers_at(&headers, &slot, || 1000);
    assert_eq!(
        slot.take().unwrap().api.unwrap().rows[0].unavailable,
        Some(LimitUnavailable::Invalid)
    );
    headers.insert("x-secret", HeaderValue::from_static("selected-secret"));
    assert!(matches!(
        super::headers(&headers, "selected-secret", &slot),
        Err(ProviderError::InvalidOutcome)
    ));
    assert!(slot.take().is_none());
}

#[test]
fn relative_resets_use_checked_ordered_millisecond_arithmetic() {
    for (value, seconds) in [
        ("0s", 0),
        ("1s", 1),
        ("6m0s", 360),
        ("1d2h3m4s5ms", 93_785),
        ("1.001s", 2),
        ("1.01s", 2),
        ("1.1s", 2),
        ("1ms", 1),
        ("1000ms", 1),
    ] {
        assert_eq!(reset_at(value, 1000), Some(1000 + seconds), "{value}");
    }
    for value in [
        "",
        "1",
        " 1s",
        "1s ",
        "+1s",
        "-1s",
        "1e2s",
        "1s1m",
        "1s1s",
        "1.0001s",
        "1.5m",
        "1.s",
        "1w",
        "18446744073709551615d",
    ] {
        assert_eq!(reset_at(value, 1000), None, "{value}");
    }
    assert_eq!(reset_at("1ms", MAX_LIMIT_TIMESTAMP), None);
    let slot = RateLimitSlot::default();
    let mut headers = HeaderMap::new();
    headers.insert(HEADER_NAMES[2], HeaderValue::from_static("1.001s"));
    headers_at(&headers, &slot, || 1000);
    assert_eq!(
        slot.take().unwrap().api.unwrap().rows[0].reset_at,
        Some(1002)
    );
    headers_at(&headers, &slot, || -1);
    assert!(slot.take().is_none());
}

#[tokio::test]
#[cfg_attr(arany_guard, ignore = "requires local test IPC")]
async fn json_transport_retains_headers_on_status_body_loss_and_dropped_future() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::time::{Duration, timeout};
    const REQUEST_BOUND: usize = 8192;
    const REQUEST_BODY: &str = "{}";
    for (status, cancel, truncated) in [
        (200, false, false),
        (429, false, false),
        (503, false, false),
        (200, false, true),
        (200, true, false),
    ] {
        timeout(Duration::from_secs(5), async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let (release, released) = oneshot::channel::<()>();
            let mut peer = tokio::task::JoinSet::new();
            peer.spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0; 1024];
                    let read = socket.read(&mut chunk).await.unwrap();
                    assert!(read > 0 && request.len() + read <= REQUEST_BOUND);
                    request.extend_from_slice(&chunk[..read]);
                    if request.windows(4).position(|part| part == b"\r\n\r\n")
                        .is_some_and(|end| request.len() >= end + 4 + REQUEST_BODY.len())
                    {
                        break;
                    }
                }
                socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: 2\r\nx-ratelimit-remaining-requests: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                if cancel {
                    let _ = released.await;
                }
                if !truncated && !cancel {
                    socket.write_all(b"{}").await.unwrap();
                }
            });
            let slot = RateLimitSlot::default();
            let (observed, observation) = oneshot::channel();
            {
                let request = crate::provider::http_client(Duration::from_secs(3))
                    .build().unwrap().post(format!("http://{address}/")).body(REQUEST_BODY);
                let future = crate::provider::native::post_json_observed(
                    request, "selected-secret", |headers| {
                        super::headers(headers, "selected-secret", &slot)?;
                        let _ = observed.send(());
                        Ok(())
                    },
                );
                tokio::pin!(future);
                if cancel {
                    tokio::select! {
                        _ = observation => {},
                        _ = &mut future => panic!("body must remain pending"),
                    }
                } else {
                    assert_eq!(future.await.is_ok(), status == 200 && !truncated);
                }
            }
            drop(release);
            peer.join_next().await.unwrap().unwrap();
            assert_eq!(slot.take().unwrap().api.unwrap().rows[0].remaining, Some(0));
        })
        .await
        .expect("bounded JSON transport fixture");
    }
}

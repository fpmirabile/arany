use super::*;
use crate::tools::network::NETWORK_REFUSAL_RESERVE_BYTES;
use std::os::unix::fs::PermissionsExt;
use tokio::net::TcpListener;

fn grant(allow: &[&str]) -> Network {
    Network {
        allow: allow.iter().map(|item| (*item).to_owned()).collect(),
        deny: Vec::new(),
    }
}

fn network(allow: &[&str], deny: &[&str]) -> Network {
    Network {
        allow: allow.iter().map(|item| (*item).to_owned()).collect(),
        deny: deny.iter().map(|item| (*item).to_owned()).collect(),
    }
}

async fn echo_upstream() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut read, mut write) = stream.split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    port
}

async fn head(stream: &mut UnixStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).await.unwrap();
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes).unwrap()
}

async fn connect(proxy: &Proxy, request: &str) -> (UnixStream, String) {
    let mut stream = UnixStream::connect(proxy.socket()).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let response = head(&mut stream).await;
    (stream, response)
}

fn start(allow: &[&str]) -> (tempfile::TempDir, Proxy) {
    let directory = tempfile::tempdir().unwrap();
    let proxy = Proxy::start(grant(allow), directory.path().join("egress.sock")).unwrap();
    (directory, proxy)
}

#[tokio::test]
async fn listed_loopback_tunnels_bytes_sent_with_the_request() {
    let port = echo_upstream().await;
    let (_directory, proxy) = start(&["127.0.0.1"]);
    assert_eq!(
        std::fs::metadata(proxy.socket())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let request = format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: x\r\n\r\nearly");
    let (mut stream, response) = connect(&proxy, &request).await;
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    let mut echoed = [0; 5];
    stream.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"early");
    stream.write_all(b"later").await.unwrap();
    stream.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"later");
    drop(stream);
    assert!(proxy.finish().await.is_empty());
}

#[tokio::test]
async fn unlisted_unsupported_and_unreachable_requests_are_refused_and_recorded() {
    let (_directory, proxy) = start(&["127.0.0.1", "*.pypi.org"]);
    for (request, status, reason) in [
        (
            "CONNECT evil.example:443 HTTP/1.1\r\n\r\n",
            "403",
            "unlisted",
        ),
        (
            "CONNECT evil.example:443 HTTP/1.1\r\n\r\n",
            "403",
            "unlisted",
        ),
        (
            "CONNECT files.pypi.org:80 HTTP/1.1\r\n\r\n",
            "403",
            "unsupported",
        ),
        (
            "PUT http://127.0.0.1/ HTTP/1.1\r\n\r\n",
            "405",
            "unsupported",
        ),
        (
            "GET http://files.pypi.org/ HTTP/1.1\r\n\r\n",
            "403",
            "unsupported",
        ),
        ("BREW / HTTP/1.1\r\n\r\n", "405", "unsupported"),
        ("CONNECT 127.0.0.1:1 HTTP/1.1\r\n\r\n", "502", "destination"),
    ] {
        let (_, response) = connect(&proxy, request).await;
        assert!(
            response.starts_with(&format!("HTTP/1.1 {status} ")),
            "{response}"
        );
        assert!(
            response.contains(&format!("X-Arany-Refusal: {reason}")),
            "{response}"
        );
    }
    let refusals = proxy.finish().await;
    let rows: Vec<_> = refusals
        .iter()
        .map(|entry| (entry.host.as_str(), entry.reason, entry.count))
        .collect();
    assert_eq!(
        rows,
        [
            ("evil.example", NetworkRefusalReason::Unlisted, 2),
            ("files.pypi.org", NetworkRefusalReason::Unsupported, 2),
            ("", NetworkRefusalReason::Unsupported, 2),
            ("127.0.0.1", NetworkRefusalReason::Destination, 1),
        ]
    );
}

#[tokio::test]
async fn terminated_oversized_head_is_refused() {
    let (_directory, proxy) = start(&["127.0.0.1"]);
    let padding = "a".repeat(HEAD_BYTES);
    let request = format!("CONNECT 127.0.0.1:9 HTTP/1.1\r\nX: {padding}\r\n\r\n");
    let (_, response) = connect(&proxy, &request).await;
    assert!(response.starts_with("HTTP/1.1 405 "), "{response}");
    assert_eq!(proxy.finish().await.len(), 1);
}

#[tokio::test]
async fn oversized_head_is_refused() {
    let (_directory, proxy) = start(&["127.0.0.1"]);
    let mut stream = UnixStream::connect(proxy.socket()).await.unwrap();
    let junk = vec![b'a'; HEAD_BYTES + 2048];
    let _ = stream.write_all(&junk).await;
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response).await;
    assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 405 "));
    assert_eq!(proxy.finish().await.len(), 1);
}

#[tokio::test]
async fn transfer_past_the_byte_bound_is_cut_and_counted() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let chunk = vec![0u8; CHUNK_BYTES];
        for _ in 0..=(MAX_NETWORK_BYTES as usize / CHUNK_BYTES) + 2 {
            if stream.write_all(&chunk).await.is_err() {
                return;
            }
        }
    });
    let (_directory, proxy) = start(&["127.0.0.1"]);
    let (mut stream, response) = connect(
        &proxy,
        &format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n"),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200 "));
    let mut received = Vec::new();
    stream.read_to_end(&mut received).await.unwrap();
    assert!(received.len() as u64 <= MAX_NETWORK_BYTES);
    assert!(received.len() as u64 > MAX_NETWORK_BYTES - CHUNK_BYTES as u64);
    let refusals = proxy.finish().await;
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0].reason, NetworkRefusalReason::Limit);
}

#[tokio::test]
async fn connections_past_the_count_bound_are_refused() {
    let port = echo_upstream().await;
    let (_directory, proxy) = start(&["127.0.0.1"]);
    let request = format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n");
    let mut open = Vec::new();
    for _ in 0..MAX_NETWORK_CONNECTIONS {
        let (stream, response) = connect(&proxy, &request).await;
        assert!(response.starts_with("HTTP/1.1 200 "));
        open.push(stream);
    }
    let (_, response) = connect(&proxy, &request).await;
    assert!(response.starts_with("HTTP/1.1 429 "), "{response}");
    let refusals = proxy.finish().await;
    assert_eq!(refusals[0].reason, NetworkRefusalReason::Limit);
}

#[tokio::test]
async fn a_stopped_proxy_reports_unavailable() {
    let (_directory, proxy) = start(&["127.0.0.1"]);
    proxy.task.abort();
    while !proxy.task.is_finished() {
        tokio::task::yield_now().await;
    }
    let refusals = proxy.finish().await;
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0].reason, NetworkRefusalReason::ProxyUnavailable);
}

#[tokio::test]
async fn dropping_the_proxy_removes_its_socket() {
    let (_directory, proxy) = start(&["127.0.0.1"]);
    let socket = proxy.socket().to_owned();
    drop(proxy);
    assert!(!socket.exists());
}

#[test]
fn admission_matches_exact_hosts_and_subdomains_only() {
    let hosts = network(
        &["example.com", "*.pypi.org", "127.0.0.1"],
        &["bad.pypi.org"],
    );
    assert!(hosts.admits("example.com", 443));
    assert!(hosts.admits("EXAMPLE.com", 443));
    assert!(hosts.admits("files.pypi.org", 443));
    assert!(hosts.admits("a.b.pypi.org", 443));
    assert!(!hosts.admits("pypi.org", 443));
    assert!(!hosts.admits("evilpypi.org", 443));
    assert!(!hosts.admits("example.com.evil.test", 443));
    assert!(!hosts.admits("bad.pypi.org", 443));
    assert!(!hosts.admits("example.com", 80));
    assert!(!hosts.admits("example.com", 0));
    assert!(hosts.admits("127.0.0.1", 8080));
    assert!(!hosts.admits("127.0.0.1", 0));
    assert!(!hosts.admits("127.0.0.2", 8080));
    assert!(!hosts.admits("169.254.169.254", 443));
    assert!(!network(&[], &[]).admits("example.com", 443));
    assert!(hosts.lists("example.com") && !hosts.admits("example.com", 80));
}

#[test]
fn refusals_are_deduplicated_sanitized_and_capped() {
    let refusals = Refusals::default();
    refusals.record("Evil.Example", NetworkRefusalReason::Unlisted);
    refusals.record("evil.example", NetworkRefusalReason::Unlisted);
    refusals.record("evil.example", NetworkRefusalReason::Limit);
    refusals.record("a b\u{1b}[31m.test", NetworkRefusalReason::Unlisted);
    for index in 0..2 * NETWORK_REFUSAL_ENTRIES {
        refusals.record(&format!("h{index}.test"), NetworkRefusalReason::Unlisted);
    }
    let entries = refusals.take();
    assert_eq!(entries.len(), NETWORK_REFUSAL_ENTRIES);
    assert_eq!(entries[0].host, "evil.example");
    assert_eq!(entries[0].count, 2);
    assert_eq!(entries[2].host, "a?b?[31m.test");
    assert!(refusals.take().is_empty());
}

#[test]
fn refusal_reserve_fits_the_largest_refusal_list() {
    let entries: Vec<_> = (0..NETWORK_REFUSAL_ENTRIES)
        .map(|_| NetworkRefusal {
            host: "x".repeat(MAX_HOST_BYTES),
            reason: NetworkRefusalReason::ProxyUnavailable,
            count: u32::MAX,
        })
        .collect();
    assert!(serde_json::to_vec(&entries).unwrap().len() < NETWORK_REFUSAL_RESERVE_BYTES);
}

#[tokio::test]
async fn plain_get_and_post_reach_loopback_in_origin_form() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = tokio::spawn(async move {
        let mut heads = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 1024];
            let mut length = 0;
            loop {
                let read = stream.read(&mut bytes[length..]).await.unwrap();
                length += read;
                let text = &bytes[..length];
                let complete = text.windows(4).any(|window| window == b"\r\n\r\n")
                    && (text.starts_with(b"GET") || text.ends_with(b"body"));
                if read == 0 || complete {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.0 200 OK\r\n\r\nok")
                .await
                .unwrap();
            heads.push(String::from_utf8(bytes[..length].to_vec()).unwrap());
        }
        heads
    });
    let (_directory, proxy) = start(&["127.0.0.1"]);
    for request in [
        format!(
            "GET http://127.0.0.1:{port}/a?b=1 HTTP/1.1\r\nHost: x\r\nProxy-Connection: keep-alive\r\nConnection: keep-alive\r\n\r\n"
        ),
        format!(
            "POST http://127.0.0.1:{port} HTTP/1.1\r\nHost: x\r\nContent-Length: 4\r\n\r\nbody"
        ),
    ] {
        let mut stream = UnixStream::connect(proxy.socket()).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        assert!(response.ends_with(b"\r\n\r\nok"), "{response:?}");
    }
    let heads = seen.await.unwrap();
    assert!(
        heads[0].starts_with("GET /a?b=1 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
        "{heads:?}"
    );
    assert!(!heads[0].to_ascii_lowercase().contains("proxy-connection"));
    assert!(
        heads[1].starts_with(
            "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbody"
        ),
        "{heads:?}"
    );
    assert!(proxy.finish().await.is_empty());
}

#[tokio::test]
async fn finish_closes_open_tunnels_before_returning() {
    let port = echo_upstream().await;
    let (_directory, proxy) = start(&["127.0.0.1"]);
    let (mut stream, _) = connect(
        &proxy,
        &format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n"),
    )
    .await;
    assert!(proxy.finish().await.is_empty());
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
}

#[test]
fn refusal_rows_fit_a_result_filled_to_its_command_budget() {
    let limits = crate::tools::types::ToolLimits {
        network: crate::tools::types::NetworkGrant::Proxy,
        ..Default::default()
    };
    let budget = limits.command_result_budget();
    assert!(budget < limits.result_bytes as usize);
    let mut stdout = crate::tools::output::Window::new(limits.result_bytes as usize);
    stdout
        .push(
            &vec![b'x'; 4 * limits.result_bytes as usize],
            crate::tools::output::DRAIN_BYTES,
        )
        .unwrap();
    let stderr = crate::tools::output::Window::new(limits.result_bytes as usize);
    let output = crate::tools::process::fit_selected_command(
        Some(0),
        &stdout,
        &stderr,
        Default::default(),
        budget,
        crate::tools::process::Truncation::Allow,
        Default::default(),
    )
    .unwrap();
    let refusals: Vec<_> = (0..NETWORK_REFUSAL_ENTRIES)
        .map(|index| NetworkRefusal {
            host: format!("{index}{}", "x".repeat(MAX_HOST_BYTES - 1)),
            reason: NetworkRefusalReason::Unlisted,
            count: u32::MAX,
        })
        .collect();
    let merged = annotated(&output, refusals.clone()).unwrap();
    assert!(merged.len() <= limits.result_bytes as usize);
    let result: CommandResult = serde_json::from_str(&merged).unwrap();
    assert_eq!(result.network_refusals, refusals);
}

#[test]
fn capture_refusal_text_keeps_the_network_rows() {
    let rows = vec![NetworkRefusal {
        host: "evil.example".into(),
        reason: NetworkRefusalReason::Unlisted,
        count: 1,
    }];
    let text = annotated(
        "Command overlay capture refused; nothing was published",
        rows,
    )
    .unwrap();
    assert!(text.starts_with("Command overlay capture refused; nothing was published\n"));
    assert!(text.contains(r#""host":"evil.example""#) && text.contains("unlisted"));
}

fn activity(idle: Duration) -> Activity {
    Activity {
        started: Instant::now(),
        last_ms: AtomicU64::new(0),
        idle,
    }
}

#[tokio::test(start_paused = true)]
async fn a_write_blocked_past_the_idle_bound_is_cut() {
    let idle = Duration::from_secs(5);
    let counter = AtomicU64::new(0);
    let activity = activity(idle);
    let (sink, _unread) = tokio::io::duplex(8);
    let source: &[u8] = &[7; 4096];
    let started = Instant::now();
    let result = pump(source, sink, &[], (&counter, &activity)).await;
    assert_eq!(result, Err(NetworkRefusalReason::Limit));
    assert!(started.elapsed() >= idle);
}

#[tokio::test(start_paused = true)]
async fn progress_in_one_direction_keeps_a_silent_direction_open() {
    let idle = Duration::from_secs(5);
    let (counter_up, counter_down) = (AtomicU64::new(0), AtomicU64::new(0));
    let activity = activity(idle);
    let (mut upstream, download_source) = tokio::io::duplex(64);
    let (upload_writer, upload_source) = tokio::io::duplex(64);
    let (download_sink, mut drained) = tokio::io::duplex(1024);
    let (upload_sink, _upload_seen) = tokio::io::duplex(64);
    // Eight ticks span four idle periods; a silent upload judged alone would be cut after the first.
    let driver = tokio::spawn(async move {
        for _ in 0..8 {
            tokio::time::sleep(idle / 2).await;
            upstream.write_all(b"tick").await.unwrap();
        }
        drop(upload_writer);
    });
    let reader = tokio::spawn(async move {
        let mut received = Vec::new();
        drained.read_to_end(&mut received).await.unwrap();
        received
    });
    tokio::try_join!(
        pump(upload_source, upload_sink, &[], (&counter_up, &activity)),
        pump(
            download_source,
            download_sink,
            &[],
            (&counter_down, &activity)
        ),
    )
    .unwrap();
    driver.await.unwrap();
    assert_eq!(reader.await.unwrap(), b"tick".repeat(8));
}

#[tokio::test]
async fn saturated_open_connections_are_refused_and_recorded_before_any_header() {
    let (_directory, proxy) = start(&["127.0.0.1"]);
    let mut silent = Vec::new();
    for _ in 0..OPEN_CONNECTIONS {
        silent.push(UnixStream::connect(proxy.socket()).await.unwrap());
    }
    let mut overflow = UnixStream::connect(proxy.socket()).await.unwrap();
    let response = head(&mut overflow).await;
    assert!(response.starts_with("HTTP/1.1 429 "), "{response}");
    let refusals = proxy.finish().await;
    assert_eq!(refusals.len(), 1);
    assert_eq!(
        (
            refusals[0].host.as_str(),
            refusals[0].reason,
            refusals[0].count
        ),
        ("", NetworkRefusalReason::Limit, 1)
    );
}

#[tokio::test]
async fn completed_connections_stay_bounded_until_reaped_under_sustained_refusals() {
    let context = Arc::new(Context {
        grant: grant(&["127.0.0.1"]),
        idle: Duration::from_millis(NETWORK_IDLE_MS),
        refusals: Arc::new(Refusals::default()),
        connections: AtomicU32::new(0),
        uploaded: AtomicU64::new(0),
        downloaded: AtomicU64::new(0),
    });
    let mut connections = JoinSet::new();
    let mut refusing = JoinSet::new();
    let reply_slots = MAX_NETWORK_CONNECTIONS as usize;
    let attempts = 4 * (OPEN_CONNECTIONS + reply_slots);
    for index in 0..attempts {
        let (mut client, server) = UnixStream::pair().unwrap();
        if index < OPEN_CONNECTIONS {
            client
                .write_all(b"CONNECT evil.example:443 HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
        }
        admit(server, &context, &mut connections, &mut refusing);
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        if index < OPEN_CONNECTIONS {
            assert!(response.starts_with("HTTP/1.1 403 "), "{index}: {response}");
        } else if index < OPEN_CONNECTIONS + reply_slots {
            assert!(response.starts_with("HTTP/1.1 429 "), "{index}: {response}");
        } else {
            assert!(response.is_empty(), "{index}: {response}");
        }
        assert!(connections.len() <= OPEN_CONNECTIONS);
        assert!(refusing.len() <= reply_slots);
    }
    assert_eq!(connections.len(), OPEN_CONNECTIONS);
    assert_eq!(refusing.len(), reply_slots);
    let rows = context.refusals.take();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].reason, NetworkRefusalReason::Unlisted);
    assert_eq!(rows[0].count as usize, OPEN_CONNECTIONS);
    assert_eq!(rows[1].reason, NetworkRefusalReason::Limit);
    assert_eq!(rows[1].count as usize, attempts - OPEN_CONNECTIONS);
    while connections.join_next().await.is_some() {}
    while refusing.join_next().await.is_some() {}
    let (mut client, server) = UnixStream::pair().unwrap();
    client
        .write_all(b"CONNECT evil.example:443 HTTP/1.1\r\n\r\n")
        .await
        .unwrap();
    admit(server, &context, &mut connections, &mut refusing);
    assert!(head(&mut client).await.starts_with("HTTP/1.1 403 "));
    assert!(connections.join_next().await.unwrap().is_ok());
}

#[tokio::test]
async fn a_bridge_overflow_report_is_recorded_as_a_limit_row() {
    let (_directory, proxy) = start(&["127.0.0.1"]);
    let mut stream = UnixStream::connect(proxy.socket()).await.unwrap();
    stream
        .write_all(format!("{BRIDGE_OVERFLOW}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let _ = head(&mut stream).await;
    let refusals = proxy.finish().await;
    assert_eq!(
        (refusals[0].host.as_str(), refusals[0].reason),
        ("", NetworkRefusalReason::Limit)
    );
}

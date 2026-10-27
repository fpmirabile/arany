use super::*;
use tokio::net::TcpListener;

fn grant(allow: &[&str]) -> Network {
    Network {
        allow: allow.iter().map(|item| (*item).to_owned()).collect(),
        deny: Vec::new(),
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

async fn head(stream: &mut Client) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).await.unwrap();
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes).unwrap()
}

async fn connect(proxy: &Proxy, request: &str) -> (Client, String) {
    let mut stream = Client::connect((std::net::Ipv4Addr::LOCALHOST, proxy.port()))
        .await
        .unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let response = head(&mut stream).await;
    (stream, response)
}

#[tokio::test]
async fn listed_loopback_tunnels_through_the_ephemeral_loopback_port() {
    let port = echo_upstream().await;
    let proxy = Proxy::start(grant(&["127.0.0.1"])).unwrap();
    assert_ne!(proxy.port(), 0);
    let request = format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: x\r\n\r\nearly");
    let (mut stream, response) = connect(&proxy, &request).await;
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    let mut echoed = [0; 5];
    stream.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"early");
    drop(stream);
    assert!(proxy.finish().await.is_empty());
}

#[tokio::test]
async fn unlisted_hosts_are_refused_and_recorded() {
    let proxy = Proxy::start(grant(&["127.0.0.1"])).unwrap();
    let (_stream, response) = connect(&proxy, "CONNECT evil.example:443 HTTP/1.1\r\n\r\n").await;
    assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
    assert!(response.contains("X-Arany-Refusal: unlisted"), "{response}");
    let refusals = proxy.finish().await;
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0].host, "evil.example");
    assert_eq!(refusals[0].reason, NetworkRefusalReason::Unlisted);
}

#[tokio::test]
async fn the_bridge_overflow_line_is_an_ordinary_unsupported_request() {
    let proxy = Proxy::start(grant(&["127.0.0.1"])).unwrap();
    let (_stream, response) = connect(&proxy, "ARANY-BRIDGE-OVERFLOW\r\n\r\n").await;
    assert!(response.starts_with("HTTP/1.1 405 "), "{response}");
    let refusals = proxy.finish().await;
    assert_eq!(refusals[0].reason, NetworkRefusalReason::Unsupported);
}

#[tokio::test]
async fn finishing_the_proxy_closes_its_listener() {
    let proxy = Proxy::start(grant(&["127.0.0.1"])).unwrap();
    let port = proxy.port();
    proxy.finish().await;
    assert!(
        Client::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn finishing_the_proxy_closes_an_open_tunnel() {
    let port = echo_upstream().await;
    let proxy = Proxy::start(grant(&["127.0.0.1"])).unwrap();
    let request = format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n");
    let (mut stream, response) = connect(&proxy, &request).await;
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    proxy.finish().await;
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).await.unwrap_or_default();
    assert!(rest.is_empty());
}

#[tokio::test]
async fn a_cleanup_failure_surfaces_only_after_the_tunnel_and_listener_close() {
    let port = echo_upstream().await;
    let proxy = Proxy::start(grant(&["127.0.0.1"])).unwrap();
    let proxy_port = proxy.port();
    let server = proxy.task.abort_handle();
    let request = format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n");
    let (mut stream, response) = connect(&proxy, &request).await;
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    let cleanup = Err(ToolError::GuardRejected(
        crate::tools::guard::GuardStage::ProcessCleanup,
    ));
    let settled = timeout(Duration::from_secs(10), Proxy::settle(Some(proxy), cleanup))
        .await
        .expect("bounded settlement");
    assert!(matches!(
        settled,
        Err(ToolError::GuardRejected(
            crate::tools::guard::GuardStage::ProcessCleanup
        ))
    ));
    // No await since settlement returned: a merely requested abort cannot have completed on this runtime.
    assert!(server.is_finished(), "server and tunnels joined by settlement");
    let mut rest = Vec::new();
    timeout(Duration::from_secs(5), stream.read_to_end(&mut rest))
        .await
        .expect("tunnel closed before settlement returned")
        .unwrap_or_default();
    assert!(rest.is_empty());
    assert!(
        Client::connect((std::net::Ipv4Addr::LOCALHOST, proxy_port))
            .await
            .is_err()
    );
}

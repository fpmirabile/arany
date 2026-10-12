use super::{
    AuthorizationAttempt, AuthorizationError, CodeExchange, MAX_CALLBACK_BYTES, RiskAcknowledgment,
};
use std::{net::Ipv4Addr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

const MAX_REQUEST_BYTES: usize = 8192;
const MAX_HEADERS: usize = 32;
const MAX_CALLBACK_CONNECTIONS: usize = 8;
const CALLBACK_DEADLINE: Duration = Duration::from_secs(120);
const CONNECTION_DEADLINE: Duration = Duration::from_secs(5);
const SUCCESS_BODY: &[u8] = b"Authorization received. Return to Arany.\n";
const FAILURE_BODY: &[u8] = b"Authorization was not accepted. Return to Arany.\n";

pub(crate) struct CallbackListener {
    listener: TcpListener,
    port: u16,
    acknowledgment: RiskAcknowledgment,
}

impl CallbackListener {
    pub(crate) async fn bind(
        acknowledgment: RiskAcknowledgment,
    ) -> Result<Self, AuthorizationError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| AuthorizationError::Unavailable)?;
        let port = listener
            .local_addr()
            .map_err(|_| AuthorizationError::Unavailable)?
            .port();
        Ok(Self {
            listener,
            port,
            acknowledgment,
        })
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    pub(crate) async fn receive(
        self,
        attempt: AuthorizationAttempt,
    ) -> Result<(CodeExchange, RiskAcknowledgment), AuthorizationError> {
        if attempt.redirect_uri != format!("http://127.0.0.1:{}/auth/callback", self.port) {
            return Err(AuthorizationError::Unavailable);
        }
        timeout(CALLBACK_DEADLINE, self.receive_valid(attempt))
            .await
            .map_err(|_| AuthorizationError::Unavailable)?
    }

    async fn receive_valid(
        self,
        attempt: AuthorizationAttempt,
    ) -> Result<(CodeExchange, RiskAcknowledgment), AuthorizationError> {
        for _ in 0..MAX_CALLBACK_CONNECTIONS {
            let (mut stream, _) = self
                .listener
                .accept()
                .await
                .map_err(|_| AuthorizationError::Unavailable)?;
            let result = timeout(CONNECTION_DEADLINE, read_callback(&mut stream, self.port))
                .await
                .unwrap_or(Err(AuthorizationError::InvalidCallback))
                .and_then(|url| attempt.inspect_callback(&url));
            let _ = timeout(
                CONNECTION_DEADLINE,
                write_response(&mut stream, result.is_ok()),
            )
            .await;
            match result {
                Ok(accepted) => {
                    return Ok((attempt.into_code_exchange(accepted), self.acknowledgment));
                }
                Err(AuthorizationError::Denied) => return Err(AuthorizationError::Denied),
                Err(_) => {}
            }
        }
        Err(AuthorizationError::InvalidCallback)
    }
}

async fn read_callback(stream: &mut TcpStream, port: u16) -> Result<String, AuthorizationError> {
    let mut request = Vec::with_capacity(1024);
    let header_end = loop {
        if request.len() == MAX_REQUEST_BYTES {
            return Err(AuthorizationError::InvalidCallback);
        }
        let mut chunk = [0u8; 1024];
        let remaining = MAX_REQUEST_BYTES - request.len();
        let read_limit = remaining.min(chunk.len());
        let read = stream
            .read(&mut chunk[..read_limit])
            .await
            .map_err(|_| AuthorizationError::InvalidCallback)?;
        if read == 0 {
            return Err(AuthorizationError::InvalidCallback);
        }
        request.extend_from_slice(&chunk[..read]);
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    if request.len() != header_end || !request[..header_end].is_ascii() {
        return Err(AuthorizationError::InvalidCallback);
    }
    let text = std::str::from_utf8(&request).map_err(|_| AuthorizationError::InvalidCallback)?;
    let mut lines = text[..header_end - 2].split("\r\n");
    let line = lines.next().ok_or(AuthorizationError::InvalidCallback)?;
    let target = line
        .strip_prefix("GET ")
        .and_then(|line| line.strip_suffix(" HTTP/1.1"))
        .ok_or(AuthorizationError::InvalidCallback)?;
    if target.len() > MAX_CALLBACK_BYTES
        || !target.starts_with("/auth/callback?")
        || target.bytes().any(|byte| !byte.is_ascii_graphic())
    {
        return Err(AuthorizationError::InvalidCallback);
    }
    let expected_host = format!("127.0.0.1:{port}");
    let mut host_seen = false;
    let mut count = 0;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        count += 1;
        if count > MAX_HEADERS
            || line
                .bytes()
                .any(|byte| !byte.is_ascii_graphic() && byte != b' ')
        {
            return Err(AuthorizationError::InvalidCallback);
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(AuthorizationError::InvalidCallback)?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(AuthorizationError::InvalidCallback);
        }
        let value = value.trim();
        if name.eq_ignore_ascii_case("host") {
            if host_seen || value != expected_host {
                return Err(AuthorizationError::InvalidCallback);
            }
            host_seen = true;
        } else if name.eq_ignore_ascii_case("origin")
            || name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("content-type")
        {
            return Err(AuthorizationError::InvalidCallback);
        }
    }
    if !host_seen {
        return Err(AuthorizationError::InvalidCallback);
    }
    Ok(format!("http://{expected_host}{target}"))
}

async fn write_response(stream: &mut TcpStream, accepted: bool) -> Result<(), AuthorizationError> {
    let (status, body) = if accepted {
        ("200 OK", SUCCESS_BODY)
    } else {
        ("400 Bad Request", FAILURE_BODY)
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(header.as_bytes())
        .await
        .map_err(|_| AuthorizationError::Unavailable)?;
    stream
        .write_all(body)
        .await
        .map_err(|_| AuthorizationError::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::credentials::AccountStorage;
    use std::net::SocketAddr;

    async fn send_request(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port)))
            .await
            .expect("connect");
        stream.write_all(request.as_bytes()).await.expect("request");
        let mut response = String::new();
        if let Err(error) = stream.read_to_string(&mut response).await {
            assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        }
        response
    }

    async fn round_trip(
        request: impl FnOnce(u16, &AuthorizationAttempt) -> String,
    ) -> (String, bool) {
        let listener = CallbackListener::bind(
            super::super::consent::RiskPrompt::new(AccountStorage::Keyring)
                .accept("Accept")
                .unwrap(),
        )
        .await
        .expect("listener");
        let port = listener.port();
        let attempt =
            AuthorizationAttempt::from_random(super::super::fixture_host_id(), port, [7; 96])
                .expect("attempt");
        let request = request(port, &attempt);
        let valid = valid_request(port, &attempt);
        let receiver = tokio::spawn(listener.receive(attempt));
        let response = send_request(port, &request).await;
        let accepted = response.starts_with("HTTP/1.1 200 OK\r\n");
        if !accepted {
            assert!(
                send_request(port, &valid)
                    .await
                    .starts_with("HTTP/1.1 200 OK\r\n")
            );
        }
        assert!(receiver.await.expect("receiver").is_ok());
        (response, accepted)
    }

    fn valid_request(port: u16, attempt: &AuthorizationAttempt) -> String {
        format!(
            "GET /auth/callback?state={}&code=synthetic&client_id=oaiapp_test HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/html\r\n\r\n",
            attempt.state
        )
    }

    #[tokio::test]
    async fn callback_accepts_only_its_bound_request() {
        let (response, accepted) = round_trip(valid_request).await;
        assert!(accepted);
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Security-Policy: default-src 'none'"));
        assert!(!response.contains("synthetic"));
    }

    #[tokio::test]
    async fn callback_http_envelope_rejects_cross_origin_and_body_shapes() {
        for mutate in [
            "host",
            "origin",
            "method",
            "body",
            "transfer",
            "duplicate_host",
            "absolute_target",
            "wrong_state",
            "oversize",
        ] {
            let (response, accepted) = round_trip(|port, attempt| {
                let request = valid_request(port, attempt);
                match mutate {
                    "host" => request
                        .replace(&format!("Host: 127.0.0.1:{port}"), "Host: attacker.example"),
                    "origin" => {
                        request.replace("Accept: text/html", "Origin: https://attacker.example")
                    }
                    "method" => request.replacen("GET ", "POST ", 1),
                    "body" => request.replace("Accept: text/html", "Content-Length: 1"),
                    "transfer" => {
                        request.replace("Accept: text/html", "Transfer-Encoding: chunked")
                    }
                    "duplicate_host" => {
                        request.replace("Accept: text/html", &format!("Host: 127.0.0.1:{port}"))
                    }
                    "absolute_target" => {
                        request.replacen("GET /auth", "GET http://127.0.0.1/auth", 1)
                    }
                    "wrong_state" => request.replace(&attempt.state, "wrong"),
                    "oversize" => request.replace(
                        "Accept: text/html",
                        &format!("Accept: {}", "x".repeat(MAX_REQUEST_BYTES)),
                    ),
                    _ => unreachable!(),
                }
            })
            .await;
            assert!(!accepted, "{mutate}");
            if mutate != "oversize" {
                assert!(
                    response.starts_with("HTTP/1.1 400 Bad Request\r\n"),
                    "{mutate}"
                );
            }
            assert!(!response.contains("synthetic"), "{mutate}");
        }
    }

    #[tokio::test]
    async fn matching_denial_stops_waiting_and_invalid_attempts_are_bounded() {
        let acknowledgment = super::super::consent::RiskPrompt::new(AccountStorage::Keyring)
            .accept("Accept")
            .unwrap();
        let listener = CallbackListener::bind(acknowledgment)
            .await
            .expect("listener");
        let port = listener.port();
        let attempt =
            AuthorizationAttempt::from_random(super::super::fixture_host_id(), port, [7; 96])
                .expect("attempt");
        let denial = format!(
            "GET /auth/callback?state={}&error=access_denied HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n",
            attempt.state
        );
        let receiver = tokio::spawn(listener.receive(attempt));
        assert!(
            send_request(port, &denial)
                .await
                .starts_with("HTTP/1.1 400 Bad Request\r\n")
        );
        assert!(matches!(
            receiver.await.expect("denial result"),
            Err(AuthorizationError::Denied)
        ));

        let acknowledgment = super::super::consent::RiskPrompt::new(AccountStorage::Keyring)
            .accept("Accept")
            .unwrap();
        let listener = CallbackListener::bind(acknowledgment)
            .await
            .expect("listener");
        let port = listener.port();
        let attempt =
            AuthorizationAttempt::from_random(super::super::fixture_host_id(), port, [7; 96])
                .expect("attempt");
        let receiver = tokio::spawn(listener.receive(attempt));
        let wrong_state = format!(
            "GET /auth/callback?state=wrong&code=synthetic&client_id=oaiapp_test HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
        );
        for _ in 0..MAX_CALLBACK_CONNECTIONS {
            assert!(
                send_request(port, &wrong_state)
                    .await
                    .starts_with("HTTP/1.1 400 Bad Request\r\n")
            );
        }
        assert!(matches!(
            receiver.await.expect("bounded invalid result"),
            Err(AuthorizationError::InvalidCallback)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn listener_deadline_drops_unused_socket() {
        let listener = CallbackListener::bind(
            super::super::consent::RiskPrompt::new(AccountStorage::Keyring)
                .accept("Accept")
                .unwrap(),
        )
        .await
        .expect("listener");
        let port = listener.port();
        let attempt =
            AuthorizationAttempt::from_random(super::super::fixture_host_id(), port, [7; 96])
                .expect("attempt");
        let receiver = tokio::spawn(listener.receive(attempt));
        tokio::time::advance(CALLBACK_DEADLINE).await;
        assert!(matches!(
            receiver.await.expect("receiver"),
            Err(AuthorizationError::Unavailable)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn accepted_connection_cannot_hold_the_listener_open() {
        let listener = CallbackListener::bind(
            super::super::consent::RiskPrompt::new(AccountStorage::Keyring)
                .accept("Accept")
                .unwrap(),
        )
        .await
        .expect("listener");
        let port = listener.port();
        let attempt =
            AuthorizationAttempt::from_random(super::super::fixture_host_id(), port, [7; 96])
                .expect("attempt");
        let _stream = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port)))
            .await
            .expect("connect");
        let receiver = tokio::spawn(listener.receive(attempt));
        tokio::task::yield_now().await;
        tokio::time::advance(CALLBACK_DEADLINE).await;
        assert!(matches!(
            receiver.await.expect("receiver"),
            Err(AuthorizationError::Unavailable)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "native Unix callback connection-deadline release gate"]
    async fn stalled_connection_expires_then_valid_callback_succeeds() {
        let listener = CallbackListener::bind(
            super::super::consent::RiskPrompt::new(AccountStorage::Keyring)
                .accept("Accept")
                .unwrap(),
        )
        .await
        .expect("listener");
        let port = listener.port();
        let attempt =
            AuthorizationAttempt::from_random(super::super::fixture_host_id(), port, [7; 96])
                .expect("attempt");
        let valid = valid_request(port, &attempt);
        let receiver = tokio::spawn(listener.receive(attempt));
        let mut stalled = TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port)))
            .await
            .expect("stalled connection");
        let mut rejection = String::new();
        tokio::time::timeout(
            CONNECTION_DEADLINE + Duration::from_secs(5),
            stalled.read_to_string(&mut rejection),
        )
        .await
        .expect("stalled connection was bounded")
        .expect("stalled response");
        assert!(rejection.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        let response = send_request(port, &valid).await;
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(receiver.await.expect("receiver").is_ok());
    }
}

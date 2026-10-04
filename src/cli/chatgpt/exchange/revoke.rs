use super::{AuthorizationError, VerifiedCredentials, client, read_json};
use reqwest::{StatusCode, Url, header};
use serde::Deserialize;
use std::time::Duration;

const DISCOVERY_ENDPOINT: &str = "https://auth.openai.com/.well-known/openid-configuration";
const ISSUER: &str = "https://auth.openai.com";
const MAX_DISCOVERY_BYTES: usize = 16 * 1024;
const MAX_FORM_BYTES: usize = 25 * 1024;
const DEADLINE: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct Endpoints {
    discovery: Url,
    https_only: bool,
}

impl Endpoints {
    fn production() -> Self {
        Self {
            discovery: Url::parse(DISCOVERY_ENDPOINT).expect("compiled discovery endpoint"),
            https_only: true,
        }
    }
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    revocation_endpoint: String,
}

pub(super) async fn revoke_remote(
    credentials: &VerifiedCredentials,
) -> Result<(), AuthorizationError> {
    tokio::time::timeout(
        DEADLINE,
        revoke_with_retry(credentials, Endpoints::production()),
    )
    .await
    .map_err(|_| AuthorizationError::Unavailable)?
}

async fn revoke_with_retry(
    credentials: &VerifiedCredentials,
    endpoints: Endpoints,
) -> Result<(), AuthorizationError> {
    for attempt in 0..3u64 {
        match revoke_at(credentials, endpoints.clone()).await {
            Err(AuthorizationError::Unavailable) if attempt < 2 => {
                let mut jitter = [0u8; 1];
                getrandom::fill(&mut jitter).map_err(|_| AuthorizationError::Unavailable)?;
                let backoff_ms = 200 * (1 << attempt) + u64::from(jitter[0] % 100);
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            }
            result => return result,
        }
    }
    Err(AuthorizationError::Unavailable)
}

async fn revoke_at(
    credentials: &VerifiedCredentials,
    endpoints: Endpoints,
) -> Result<(), AuthorizationError> {
    if !crate::cli::chatgpt::valid_client_id(&credentials.client_id)
        || credentials.host_id.is_nil()
        || !crate::cli::chatgpt::bounded_graphic(&credentials.subject, 512)
        || !super::bounded_token(&credentials.refresh_token, 8 * 1024)
    {
        return Err(AuthorizationError::InvalidIdentity);
    }
    let client = client(endpoints.https_only)?;
    let response = client
        .get(endpoints.discovery.clone())
        .header(header::ACCEPT, "application/json")
        .header(header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|_| AuthorizationError::Unavailable)?;
    if response.url() != &endpoints.discovery {
        return Err(AuthorizationError::ExchangeFailed);
    }
    if response.status().is_server_error() {
        return Err(AuthorizationError::Unavailable);
    }
    let bytes = read_json(response, StatusCode::OK, MAX_DISCOVERY_BYTES, false, || {
        AuthorizationError::ExchangeFailed
    })
    .await?;
    let discovery: Discovery =
        serde_json::from_slice(&bytes).map_err(|_| AuthorizationError::ExchangeFailed)?;
    let endpoint = Url::parse(&discovery.revocation_endpoint)
        .map_err(|_| AuthorizationError::ExchangeFailed)?;
    if discovery.issuer != ISSUER
        || endpoint.origin() != endpoints.discovery.origin()
        || endpoint.scheme() != endpoints.discovery.scheme()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || endpoint.path().is_empty()
        || endpoint.path().len() > 256
    {
        return Err(AuthorizationError::ExchangeFailed);
    }
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in [
        ("token", credentials.refresh_token.as_str()),
        ("token_type_hint", "refresh_token"),
        ("client_id", credentials.client_id.as_str()),
    ] {
        serializer.append_pair(name, value);
    }
    let body = serializer.finish();
    if body.len() > MAX_FORM_BYTES {
        return Err(AuthorizationError::ExchangeFailed);
    }
    let mut response = client
        .post(endpoint.clone())
        .header(header::ACCEPT_ENCODING, "identity")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|_| AuthorizationError::Unavailable)?;
    if response.status().is_server_error() {
        return Err(AuthorizationError::Unavailable);
    }
    if response.url() != &endpoint
        || response.status() != StatusCode::OK
        || response
            .headers()
            .get_all(header::CONTENT_ENCODING)
            .iter()
            .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
        || response.content_length().is_some_and(|length| length != 0)
    {
        return Err(AuthorizationError::ExchangeFailed);
    }
    if response
        .chunk()
        .await
        .map_err(|_| AuthorizationError::Unavailable)?
        .is_some()
    {
        return Err(AuthorizationError::ExchangeFailed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    use uuid::Uuid;

    #[derive(Clone, Copy, Debug)]
    enum Case {
        Success,
        WrongIssuer,
        OffOrigin,
        EndpointQuery,
        Malformed,
        Redirect,
        Encoded,
        Oversized,
        NonemptySuccess,
        ChunkedNonempty,
        Unauthorized,
        ServerFailure,
    }

    impl Case {
        fn reaches_revoke(self) -> bool {
            matches!(
                self,
                Self::Success
                    | Self::NonemptySuccess
                    | Self::ChunkedNonempty
                    | Self::Unauthorized
                    | Self::ServerFailure
            )
        }
    }

    fn credentials() -> VerifiedCredentials {
        VerifiedCredentials {
            client_id: "oaiapp_synthetic".into(),
            host_id: Uuid::now_v7(),
            subject: "synthetic-subject".into(),
            id_token: "synthetic-id".into(),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at_unix: 1_800_000_000,
        }
    }

    async fn request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut buffer = [0u8; 1024];
            let count = stream.read(&mut buffer).await.expect("read request");
            assert!(count > 0 && bytes.len() + count <= 32 * 1024);
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8(bytes[..header_end].to_vec()).expect("ASCII request");
        let length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        assert!(length <= MAX_FORM_BYTES);
        while bytes.len() < header_end + length {
            let mut buffer = [0u8; 1024];
            let count = stream.read(&mut buffer).await.expect("read form");
            assert!(count > 0 && bytes.len() + count <= 32 * 1024);
            bytes.extend_from_slice(&buffer[..count]);
        }
        bytes
    }

    async fn reply(
        stream: &mut tokio::net::TcpStream,
        status: &str,
        content_type: Option<&str>,
        encoding: Option<&str>,
        body: &[u8],
        chunked: bool,
    ) {
        let content_type = content_type
            .map(|value| format!("Content-Type: {value}\r\n"))
            .unwrap_or_default();
        let encoding = encoding
            .map(|value| format!("Content-Encoding: {value}\r\n"))
            .unwrap_or_default();
        let location = if status.starts_with("302") {
            "Location: /redirected\r\n"
        } else {
            ""
        };
        let framing = if chunked {
            "Transfer-Encoding: chunked\r\n".to_owned()
        } else {
            format!("Content-Length: {}\r\n", body.len())
        };
        let headers = format!(
            "HTTP/1.1 {status}\r\n{content_type}{encoding}{location}{framing}Connection: close\r\n\r\n"
        );
        stream.write_all(headers.as_bytes()).await.expect("headers");
        if chunked {
            stream
                .write_all(format!("{:X}\r\n", body.len()).as_bytes())
                .await
                .expect("chunk header");
        }
        let _ = stream.write_all(body).await;
        if chunked {
            let _ = stream.write_all(b"\r\n0\r\n\r\n").await;
        }
    }

    async fn issuer(case: Case) -> (Endpoints, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test issuer");
        let origin = format!("http://{}", listener.local_addr().expect("issuer address"));
        let endpoints = Endpoints {
            discovery: Url::parse(&format!("{origin}/.well-known/openid-configuration"))
                .expect("discovery URL"),
            https_only: false,
        };
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            let (mut stream, _) = listener.accept().await.expect("discovery request");
            requests.push(request(&mut stream).await);
            let issuer = if matches!(case, Case::WrongIssuer) {
                "https://other.example"
            } else {
                ISSUER
            };
            let revoke = if matches!(case, Case::OffOrigin) {
                "https://other.example/revoke".to_owned()
            } else if matches!(case, Case::EndpointQuery) {
                format!("{origin}/revoke?capture=token")
            } else {
                format!("{origin}/revoke")
            };
            let metadata = serde_json::to_vec(&serde_json::json!({
                "issuer": issuer,
                "revocation_endpoint": revoke
            }))
            .expect("discovery JSON");
            let oversized = vec![b'x'; MAX_DISCOVERY_BYTES + 1];
            reply(
                &mut stream,
                if matches!(case, Case::Redirect) {
                    "302 Found"
                } else {
                    "200 OK"
                },
                Some("application/json"),
                matches!(case, Case::Encoded).then_some("gzip"),
                if matches!(case, Case::Oversized) {
                    &oversized
                } else if matches!(case, Case::Malformed) {
                    b"{"
                } else {
                    &metadata
                },
                false,
            )
            .await;
            if case.reaches_revoke() {
                let (mut stream, _) = listener.accept().await.expect("revocation request");
                requests.push(request(&mut stream).await);
                let body = if matches!(case, Case::NonemptySuccess | Case::ChunkedNonempty) {
                    b"unexpected".as_slice()
                } else {
                    b"".as_slice()
                };
                reply(
                    &mut stream,
                    if matches!(case, Case::ServerFailure) {
                        "503 Service Unavailable"
                    } else if matches!(case, Case::Unauthorized) {
                        "401 Unauthorized"
                    } else {
                        "200 OK"
                    },
                    None,
                    None,
                    body,
                    matches!(case, Case::ChunkedNonempty),
                )
                .await;
            }
            requests
        });
        (endpoints, task)
    }

    async fn retry_issuer(
        statuses: &'static [&'static str],
    ) -> (Endpoints, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test issuer");
        let origin = format!("http://{}", listener.local_addr().expect("issuer address"));
        let endpoints = Endpoints {
            discovery: Url::parse(&format!("{origin}/.well-known/openid-configuration"))
                .expect("discovery URL"),
            https_only: false,
        };
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for status in statuses {
                let (mut discovery, _) = listener.accept().await.expect("discovery request");
                requests.push(request(&mut discovery).await);
                let metadata = serde_json::to_vec(&serde_json::json!({
                    "issuer": ISSUER,
                    "revocation_endpoint": format!("{origin}/revoke")
                }))
                .expect("discovery JSON");
                reply(
                    &mut discovery,
                    "200 OK",
                    Some("application/json"),
                    None,
                    &metadata,
                    false,
                )
                .await;
                let (mut revoke, _) = listener.accept().await.expect("revocation request");
                requests.push(request(&mut revoke).await);
                if *status != "disconnect" {
                    reply(&mut revoke, status, None, None, b"", false).await;
                }
            }
            requests
        });
        (endpoints, task)
    }

    #[tokio::test]
    async fn retries_only_unavailable_revocation_outcomes() {
        for (statuses, expected) in [
            (&["503 Service Unavailable", "200 OK"][..], Ok(())),
            (&["disconnect", "200 OK"][..], Ok(())),
            (
                &[
                    "503 Service Unavailable",
                    "503 Service Unavailable",
                    "503 Service Unavailable",
                ][..],
                Err(AuthorizationError::Unavailable),
            ),
            (
                &["429 Too Many Requests"][..],
                Err(AuthorizationError::ExchangeFailed),
            ),
            (
                &["401 Unauthorized"][..],
                Err(AuthorizationError::ExchangeFailed),
            ),
        ] {
            let saved = credentials();
            let (endpoints, task) = retry_issuer(statuses).await;
            assert_eq!(revoke_with_retry(&saved, endpoints).await, expected);
            assert_eq!(saved.refresh_token, "synthetic-refresh");
            let requests = tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("issuer deadline")
                .expect("issuer task");
            assert_eq!(requests.len(), statuses.len() * 2);
        }
    }

    #[tokio::test]
    async fn revocation_requires_pinned_discovery_and_empty_confirmation() {
        for (case, expected) in [
            (Case::Success, Ok(())),
            (Case::WrongIssuer, Err(AuthorizationError::ExchangeFailed)),
            (Case::OffOrigin, Err(AuthorizationError::ExchangeFailed)),
            (Case::EndpointQuery, Err(AuthorizationError::ExchangeFailed)),
            (Case::Malformed, Err(AuthorizationError::ExchangeFailed)),
            (Case::Redirect, Err(AuthorizationError::ExchangeFailed)),
            (Case::Encoded, Err(AuthorizationError::ExchangeFailed)),
            (Case::Oversized, Err(AuthorizationError::ExchangeFailed)),
            (
                Case::NonemptySuccess,
                Err(AuthorizationError::ExchangeFailed),
            ),
            (
                Case::ChunkedNonempty,
                Err(AuthorizationError::ExchangeFailed),
            ),
            (Case::Unauthorized, Err(AuthorizationError::ExchangeFailed)),
            (Case::ServerFailure, Err(AuthorizationError::Unavailable)),
        ] {
            let saved = credentials();
            let (endpoints, task) = issuer(case).await;
            let result = revoke_at(&saved, endpoints).await;
            assert_eq!(result, expected, "{case:?}");
            assert_eq!(saved.refresh_token, "synthetic-refresh", "{case:?}");
            let requests = tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("issuer deadline")
                .expect("issuer task");
            assert_eq!(requests.len(), 1 + usize::from(case.reaches_revoke()));
            let discovery = std::str::from_utf8(&requests[0]).expect("discovery request ASCII");
            assert!(discovery.starts_with("GET /.well-known/openid-configuration HTTP/1.1\r\n"));
            assert!(!discovery.contains("synthetic-refresh"));
            if let Some(revoke) = requests.get(1) {
                let revoke = std::str::from_utf8(revoke).expect("revoke request ASCII");
                assert!(revoke.starts_with("POST /revoke HTTP/1.1\r\n"));
                let form = revoke.split("\r\n\r\n").nth(1).expect("revoke form");
                let fields = url::form_urlencoded::parse(form.as_bytes())
                    .into_owned()
                    .collect::<Vec<_>>();
                assert_eq!(
                    fields,
                    [
                        ("token".into(), "synthetic-refresh".into()),
                        ("token_type_hint".into(), "refresh_token".into()),
                        ("client_id".into(), "oaiapp_synthetic".into()),
                    ]
                );
                assert!(!revoke.contains("synthetic-access"));
                assert!(!revoke.contains("synthetic-id"));
            }
        }
    }
}

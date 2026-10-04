use super::{AuthorizationError, VerifiedCredentials, consent::ConsentReceipt};
use crate::cli::credentials::AccountStorage;
use reqwest::{Client, Url, header, redirect::Policy};
use serde::{Deserialize, Deserializer, de};
use std::{collections::HashSet, fmt, time::Duration};

const MODELS_ENDPOINT: &str = "https://api.openai.com/v1/models";
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_MODELS: usize = 4096;
const DEADLINE: Duration = Duration::from_secs(20);

pub(crate) struct ChatGptModel {
    pub(crate) slug: String,
    pub(crate) display_name: String,
}

#[derive(Deserialize)]
struct CatalogReply {
    #[serde(deserialize_with = "bounded_models")]
    models: Vec<CatalogRow>,
}

#[derive(Deserialize)]
struct CatalogRow {
    slug: String,
    display_name: Option<String>,
    visibility: String,
}

pub(crate) async fn list_models(
    credentials: &VerifiedCredentials,
    consent: &ConsentReceipt,
    storage: AccountStorage,
) -> Result<Vec<ChatGptModel>, AuthorizationError> {
    if !consent.matches(credentials, storage)
        || credentials.access_token.is_empty()
        || credentials.access_token.len() > 16 * 1024
        || !credentials
            .access_token
            .bytes()
            .all(|byte| byte.is_ascii_graphic())
    {
        return Err(AuthorizationError::ConsentRequired);
    }
    let client = Client::builder()
        .https_only(true)
        .no_proxy()
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
        .redirect(Policy::none())
        .referer(false)
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(10))
        .timeout(DEADLINE)
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|_| AuthorizationError::Unavailable)?;
    let endpoint = Url::parse(MODELS_ENDPOINT).expect("compiled ChatGPT models endpoint");
    tokio::time::timeout(DEADLINE, list_at(&client, endpoint, credentials))
        .await
        .map_err(|_| AuthorizationError::Unavailable)?
}

async fn list_at(
    client: &Client,
    endpoint: Url,
    credentials: &VerifiedCredentials,
) -> Result<Vec<ChatGptModel>, AuthorizationError> {
    let mut response = client
        .get(endpoint.clone())
        .header(header::ACCEPT, "application/json")
        .header(header::ACCEPT_ENCODING, "identity")
        .bearer_auth(&credentials.access_token)
        .send()
        .await
        .map_err(|_| AuthorizationError::Unavailable)?;
    if response.url() != &endpoint || response.status().is_redirection() {
        return Err(AuthorizationError::InvalidCatalog);
    }
    if response.status() != reqwest::StatusCode::OK {
        return Err(catalog_http_error(response.status()));
    }
    if response
        .headers()
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
        || response
            .content_length()
            .is_some_and(|length| length > MAX_BODY_BYTES as u64)
    {
        return Err(AuthorizationError::InvalidCatalog);
    }
    let mut types = response.headers().get_all(header::CONTENT_TYPE).iter();
    let kind = types
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if types.next().is_some()
        || !kind.is_some_and(|kind| kind.eq_ignore_ascii_case("application/json"))
    {
        return Err(AuthorizationError::InvalidCatalog);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| AuthorizationError::Unavailable)?
    {
        if chunk.len() > MAX_BODY_BYTES - body.len() {
            return Err(AuthorizationError::InvalidCatalog);
        }
        body.extend_from_slice(&chunk);
    }
    if !credentials.access_token.is_empty()
        && body
            .windows(credentials.access_token.len())
            .any(|window| window == credentials.access_token.as_bytes())
    {
        return Err(AuthorizationError::InvalidCatalog);
    }
    parse_catalog(&body, &credentials.access_token)
}

fn catalog_http_error(status: reqwest::StatusCode) -> AuthorizationError {
    let advice = match status.as_u16() {
        401 => "reconnect with /setup",
        403 => "check this account's access or policy",
        429 | 500..=599 => "retry later",
        _ => "check the ChatGPT connection",
    };
    AuthorizationError::CatalogHttp {
        status: status.as_u16(),
        advice,
    }
}

fn parse_catalog(body: &[u8], access_token: &str) -> Result<Vec<ChatGptModel>, AuthorizationError> {
    let reply: CatalogReply =
        serde_json::from_slice(body).map_err(|_| AuthorizationError::InvalidCatalog)?;
    let mut seen = HashSet::new();
    let mut visible = Vec::new();
    for row in reply.models {
        if !valid_slug(&row.slug)
            || row.slug.contains(access_token)
            || row
                .display_name
                .as_deref()
                .is_some_and(|name| name.contains(access_token))
            || !seen.insert(row.slug.clone())
        {
            return Err(AuthorizationError::InvalidCatalog);
        }
        if row.visibility == "list" {
            let name = row.display_name.ok_or(AuthorizationError::InvalidCatalog)?;
            if name.is_empty() || name.len() > 256 || name.chars().any(unsafe_display_character) {
                return Err(AuthorizationError::InvalidCatalog);
            }
            visible.push(ChatGptModel {
                slug: row.slug,
                display_name: name,
            });
        }
    }
    Ok(visible)
}

fn valid_slug(slug: &str) -> bool {
    !slug.is_empty() && slug.len() <= 128 && slug.bytes().all(|byte| byte.is_ascii_graphic())
}

fn unsafe_display_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
}

fn bounded_models<'de, D>(deserializer: D) -> Result<Vec<CatalogRow>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ModelsVisitor;

    impl<'de> de::Visitor<'de> for ModelsVisitor {
        type Value = Vec<CatalogRow>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a bounded ChatGPT model array")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let mut rows = Vec::new();
            while let Some(row) = sequence.next_element::<CatalogRow>()? {
                if rows.len() == MAX_MODELS {
                    return Err(de::Error::custom("too many models"));
                }
                rows.push(row);
            }
            Ok(rows)
        }
    }

    deserializer.deserialize_seq(ModelsVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    use uuid::Uuid;

    fn account() -> (VerifiedCredentials, ConsentReceipt) {
        let credentials = VerifiedCredentials {
            client_id: "oaiapp_synthetic".into(),
            host_id: Uuid::now_v7(),
            subject: "synthetic-subject".into(),
            id_token: "synthetic-id".into(),
            access_token: "synthetic-catalog-token".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at_unix: 1_800_000_000,
        };
        let receipt = super::super::consent::RiskPrompt::new(AccountStorage::Keyring)
            .accept("Accept")
            .unwrap()
            .bind(&credentials)
            .unwrap();
        (credentials, receipt)
    }

    #[tokio::test]
    async fn account_catalog_preserves_all_visible_rows_without_admitting_runs() {
        let (account, receipt) = account();
        assert!(receipt.matches(&account, AccountStorage::Keyring));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let endpoint = Url::parse(&format!(
            "http://{}/v1/models",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let mut bytes = [0u8; 4096];
            let count = socket.read(&mut bytes).await.expect("read");
            let request = String::from_utf8(bytes[..count].to_vec()).expect("request text");
            let body = r#"{"models":[{"slug":"model-z","display_name":"Model Z","visibility":"list"},{"slug":"hidden","visibility":"hide"},{"slug":"model-a","display_name":"Model A","visibility":"list"}]}"#;
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).await.expect("reply");
            request
        });
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let models = list_at(&client, endpoint, &account).await.expect("catalog");
        assert_eq!(
            models
                .iter()
                .map(|model| model.slug.as_str())
                .collect::<Vec<_>>(),
            ["model-z", "model-a"]
        );
        assert_eq!(
            models
                .iter()
                .map(|model| model.display_name.as_str())
                .collect::<Vec<_>>(),
            ["Model Z", "Model A"]
        );
        let request = server.await.expect("server");
        assert!(request.starts_with("GET /v1/models HTTP/1.1\r\n"));
        assert!(request.contains("authorization: Bearer synthetic-catalog-token\r\n"));
        assert!(!request.contains("Workspace"));
    }

    async fn fixture_reply(reply: String) -> Result<Vec<ChatGptModel>, AuthorizationError> {
        let (account, _) = account();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let endpoint = Url::parse(&format!(
            "http://{}/v1/models",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request");
            let mut request = [0u8; 4096];
            let count = socket.read(&mut request).await.expect("read");
            assert!(count > 0);
            let _ = socket.write_all(reply.as_bytes()).await;
        });
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let result = list_at(&client, endpoint, &account).await;
        server.await.expect("server");
        result
    }

    #[tokio::test]
    async fn catalog_transport_rejects_redirect_encoding_type_and_both_size_paths() {
        let body = r#"{"models":[]}"#;
        for reply in [
            "HTTP/1.1 302 Found\r\nLocation: https://attacker.example/models\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n".to_string(),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                MAX_BODY_BYTES + 1
            ),
        ] {
            assert!(matches!(
                fixture_reply(reply).await,
                Err(AuthorizationError::InvalidCatalog)
            ));
        }
        let overhead = r#"{"models":[],"pad":""}"#.len();
        let exact = format!(
            r#"{{"models":[],"pad":"{}"}}"#,
            "x".repeat(MAX_BODY_BYTES - overhead)
        );
        assert_eq!(exact.len(), MAX_BODY_BYTES);
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{exact}",
            exact.len()
        );
        assert!(fixture_reply(reply).await.is_ok());
        let excess = format!(
            r#"{{"models":[],"pad":"{}"}}"#,
            "x".repeat(MAX_BODY_BYTES + 1 - overhead)
        );
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{excess}"
        );
        assert!(matches!(
            fixture_reply(reply).await,
            Err(AuthorizationError::InvalidCatalog)
        ));
    }

    #[tokio::test]
    async fn catalog_http_failures_report_only_status_and_safe_recovery() {
        for (status, expected) in [
            (
                "400 Bad Request",
                "ChatGPT model catalog returned HTTP 400; check the ChatGPT connection",
            ),
            (
                "401 Unauthorized",
                "ChatGPT model catalog returned HTTP 401; reconnect with /setup",
            ),
            (
                "403 Forbidden",
                "ChatGPT model catalog returned HTTP 403; check this account's access or policy",
            ),
            (
                "429 Too Many Requests",
                "ChatGPT model catalog returned HTTP 429; retry later",
            ),
            (
                "503 Service Unavailable",
                "ChatGPT model catalog returned HTTP 503; retry later",
            ),
        ] {
            let body = r#"{"detail":"synthetic-secret-must-not-appear"}"#;
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let error = match fixture_reply(reply).await {
                Err(error) => error,
                Ok(_) => panic!("catalog unexpectedly accepted {status}"),
            };
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn catalog_parser_rejects_duplicate_oversize_and_reflected_secret() {
        let token = "synthetic-catalog-token";
        for body in [
            r#"{"models":[{"slug":"same","display_name":"A","visibility":"list"},{"slug":"same","display_name":"B","visibility":"list"}]}"#,
            r#"{"models":[{"slug":"bad id","display_name":"A","visibility":"list"}]}"#,
            r#"{"models":[{"slug":"model","display_name":"synthetic-catalog-token","visibility":"list"}]}"#,
            r#"{"models":[{"slug":"model","display_name":"bad\nname","visibility":"list"}]}"#,
            r#"{"models":[{"slug":"model","display_name":"bad\u202ename","visibility":"list"}]}"#,
            r#"{"models":[{"slug":"model","display_name":"bad\u2028name","visibility":"list"}]}"#,
            r#"{"models":[{"slug":"model","visibility":"list"}]}"#,
        ] {
            assert!(matches!(
                parse_catalog(body.as_bytes(), token),
                Err(AuthorizationError::InvalidCatalog)
            ));
        }
        let escaped_token = token.replace('-', "\\u002d");
        let reflected = format!(
            r#"{{"models":[{{"slug":"model","display_name":"{escaped_token}","visibility":"list"}}]}}"#
        );
        assert!(matches!(
            parse_catalog(reflected.as_bytes(), token),
            Err(AuthorizationError::InvalidCatalog)
        ));
        let mut rows = String::from("{\"models\":[");
        for index in 0..=MAX_MODELS {
            if index != 0 {
                rows.push(',');
            }
            rows.push_str(&format!(
                "{{\"slug\":\"m{index}\",\"visibility\":\"hide\"}}"
            ));
        }
        rows.push_str("]}");
        assert!(matches!(
            parse_catalog(rows.as_bytes(), token),
            Err(AuthorizationError::InvalidCatalog)
        ));
    }

    #[tokio::test]
    async fn mismatched_consent_rejects_before_catalog_network() {
        let (mut account, consent) = account();
        account.subject = "different-account".into();
        assert!(matches!(
            list_models(&account, &consent, AccountStorage::Keyring).await,
            Err(AuthorizationError::ConsentRequired)
        ));
    }
}

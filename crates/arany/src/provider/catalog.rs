use super::{
    Effort, MAX_RESPONSE_BYTES, NativeApiCredentials, ProviderError, resolve_native_effort,
};
#[cfg(test)]
use reqwest::redirect::Policy;
use reqwest::{Client, Url, header};
use serde::{Deserialize, Deserializer, de};
use std::{collections::HashSet, fmt, time::Duration};

const OPENAI_MODELS: &str = "https://api.openai.com/v1/models";
const ANTHROPIC_MODELS: &str = "https://api.anthropic.com/v1/models";
const MAX_MODELS: usize = 4096;
const MAX_PAGES: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelEntry {
    pub id: String,
    /// Reviewed native metadata, not Run authorization or a live-compatibility guarantee.
    pub runnable: bool,
    pub efforts: Vec<Effort>,
}

impl ModelEntry {
    fn native(profile: &str, id: String) -> Self {
        let runnable = resolve_native_effort(profile, &id, None).is_ok();
        let efforts = Effort::ALL
            .into_iter()
            .filter(|effort| resolve_native_effort(profile, &id, Some(*effort)).is_ok())
            .collect();
        Self {
            id,
            runnable,
            efforts,
        }
    }

    pub fn exact_custom(id: String) -> Self {
        Self {
            id,
            runnable: false,
            efforts: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
struct CatalogRow {
    id: String,
}

#[derive(Deserialize)]
struct OpenAiPage {
    #[serde(deserialize_with = "bounded_rows")]
    data: Vec<CatalogRow>,
}

#[derive(Deserialize)]
struct AnthropicPage {
    #[serde(deserialize_with = "bounded_rows")]
    data: Vec<CatalogRow>,
    has_more: bool,
    last_id: Option<String>,
}

fn bounded_rows<'de, D>(deserializer: D) -> Result<Vec<CatalogRow>, D::Error>
where
    D: Deserializer<'de>,
{
    struct RowsVisitor;

    impl<'de> de::Visitor<'de> for RowsVisitor {
        type Value = Vec<CatalogRow>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("at most 4096 model rows")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let mut rows = Vec::new();
            while let Some(row) = sequence.next_element::<CatalogRow>()? {
                if rows.len() == MAX_MODELS {
                    return Err(de::Error::custom("too many model rows"));
                }
                rows.push(row);
            }
            Ok(rows)
        }
    }

    deserializer.deserialize_seq(RowsVisitor)
}

pub async fn list_native_models(profile: &str) -> Result<Vec<ModelEntry>, ProviderError> {
    let credentials = NativeApiCredentials::from_env(profile)?;
    list_native_models_with_credentials(profile, &credentials).await
}

/// Lists the selected native account's models without reading process credential variables.
pub async fn list_native_models_with_api_key(
    profile: &str,
    key: &str,
) -> Result<Vec<ModelEntry>, ProviderError> {
    let credentials = NativeApiCredentials::new(profile, key.to_owned(), None)?;
    list_native_models_with_credentials(profile, &credentials).await
}

/// Uses the selected native credential's API workspace on every page; never reads the environment.
pub async fn list_native_models_with_credentials(
    profile: &str,
    credentials: &NativeApiCredentials,
) -> Result<Vec<ModelEntry>, ProviderError> {
    let endpoint = match profile {
        "openai" => OPENAI_MODELS,
        "anthropic" => ANTHROPIC_MODELS,
        _ => return Err(ProviderError::Rejected),
    };
    credentials.require_profile(profile)?;
    let client = super::http_client(Duration::from_secs(20))
        .https_only(true)
        .build()
        .map_err(|_| ProviderError::Unavailable)?;
    let endpoint = Url::parse(endpoint).map_err(|_| ProviderError::Unavailable)?;
    list_at(&client, profile, endpoint, credentials).await
}

async fn list_at(
    client: &Client,
    profile: &str,
    endpoint: Url,
    credentials: &NativeApiCredentials,
) -> Result<Vec<ModelEntry>, ProviderError> {
    credentials.require_profile(profile)?;
    let key = credentials.api_key();
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut url = endpoint.clone();
        if profile == "anthropic" {
            url.query_pairs_mut().append_pair("limit", "1000");
            if let Some(after) = cursor.as_deref() {
                url.query_pairs_mut().append_pair("after_id", after);
            }
        }
        let request = client
            .get(url.clone())
            .header(header::ACCEPT, "application/json")
            .header(header::ACCEPT_ENCODING, "identity");
        let request = match profile {
            "openai" => request.bearer_auth(key),
            "anthropic" => request
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01"),
            _ => return Err(ProviderError::Rejected),
        };
        let request = match credentials.anthropic_workspace_id() {
            Some(id) => request.header("anthropic-workspace-id", id),
            None => request,
        };
        let mut response = request
            .send()
            .await
            .map_err(|_| ProviderError::Unavailable)?;
        if response.url() != &url {
            return Err(ProviderError::InvalidOutcome);
        }
        if !response.status().is_success() {
            return Err(
                if response.status().is_server_error()
                    || response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                {
                    ProviderError::Unavailable
                } else {
                    ProviderError::Rejected
                },
            );
        }
        let mut content_types = response.headers().get_all(header::CONTENT_TYPE).iter();
        let kind = content_types
            .next()
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim);
        if content_types.next().is_some()
            || !kind.is_some_and(|kind| kind.eq_ignore_ascii_case("application/json"))
        {
            return Err(ProviderError::InvalidOutcome);
        }
        let bytes = super::read_identity_body(&mut response, MAX_RESPONSE_BYTES).await?;
        if bytes
            .windows(key.len())
            .any(|window| window == key.as_bytes())
        {
            return Err(ProviderError::InvalidOutcome);
        }
        let (rows, next) = if profile == "openai" {
            let page: OpenAiPage =
                serde_json::from_slice(&bytes).map_err(|_| ProviderError::InvalidOutcome)?;
            (page.data, None)
        } else {
            let page: AnthropicPage =
                serde_json::from_slice(&bytes).map_err(|_| ProviderError::InvalidOutcome)?;
            let next = if page.has_more {
                let last = page.last_id.ok_or(ProviderError::InvalidOutcome)?;
                if !valid_model_id(&last)
                    || cursor.as_deref() == Some(&last)
                    || page.data.last().is_none_or(|row| row.id != last)
                {
                    return Err(ProviderError::InvalidOutcome);
                }
                Some(last)
            } else {
                None
            };
            (page.data, next)
        };
        if rows.len() > MAX_MODELS - entries.len() {
            return Err(ProviderError::InvalidOutcome);
        }
        for row in rows {
            if !valid_model_id(&row.id) || row.id.contains(key) || !seen.insert(row.id.clone()) {
                return Err(ProviderError::InvalidOutcome);
            }
            entries.push(ModelEntry::native(profile, row.id));
        }
        match next {
            None => {
                entries.sort_by(|left, right| left.id.cmp(&right.id));
                return Ok(entries);
            }
            Some(next) => cursor = Some(next),
        }
    }
    Err(ProviderError::InvalidOutcome)
}

pub(super) fn valid_model_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|byte| byte.is_ascii_graphic())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[tokio::test]
    async fn explicit_catalog_key_and_profile_are_validated_before_egress() {
        for workspace in [
            "",
            "default",
            "wrkspc_",
            "wrkspc_a b",
            "wrkspc_a\r\nx-header:value",
            "wrkspc_é",
        ] {
            assert!(matches!(
                NativeApiCredentials::new(
                    "anthropic",
                    "synthetic-key".into(),
                    Some(workspace.into())
                ),
                Err(ProviderError::Rejected)
            ));
        }
        assert!(matches!(
            NativeApiCredentials::new(
                "anthropic",
                "synthetic-key".into(),
                Some(format!("wrkspc_{}", "x".repeat(122)))
            ),
            Err(ProviderError::Rejected)
        ));
        assert!(matches!(
            NativeApiCredentials::new("openai", "synthetic-key".into(), Some("wrkspc_One".into())),
            Err(ProviderError::Rejected)
        ));
        let credentials = NativeApiCredentials::new(
            "anthropic",
            "synthetic-key".into(),
            Some("wrkspc_One".into()),
        )
        .unwrap();
        assert!(matches!(
            list_native_models_with_credentials("openai", &credentials).await,
            Err(ProviderError::Rejected)
        ));
        assert!(matches!(
            list_native_models_with_api_key("other", "synthetic-key").await,
            Err(ProviderError::Rejected)
        ));
        for key in ["", "bad key", "bad\nkey"] {
            assert!(matches!(
                list_native_models_with_api_key("openai", key).await,
                Err(ProviderError::Unavailable)
            ));
        }
        assert!(matches!(
            list_native_models_with_api_key("anthropic", &"x".repeat(513)).await,
            Err(ProviderError::Unavailable)
        ));
    }

    async fn fixture(
        profile: &str,
        replies: &[(&str, Option<&str>)],
    ) -> (Result<Vec<ModelEntry>, ProviderError>, Vec<String>) {
        fixture_with_header(profile, replies, "Content-Type: application/json\r\n", None).await
    }

    async fn fixture_with_header(
        profile: &str,
        replies: &[(&str, Option<&str>)],
        content_type_header: &str,
        workspace: Option<&str>,
    ) -> (Result<Vec<ModelEntry>, ProviderError>, Vec<String>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback catalog");
        let endpoint = Url::parse(&format!(
            "http://{}/v1/models",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let replies = replies
            .iter()
            .map(|(body, encoding)| (body.to_string(), encoding.map(str::to_owned)))
            .collect::<Vec<_>>();
        let content_type_header = content_type_header.to_owned();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (body, encoding) in replies {
                let (mut stream, _) = listener.accept().await.expect("catalog request");
                let mut bytes = vec![0; 8192];
                let count = stream.read(&mut bytes).await.expect("read request");
                requests.push(String::from_utf8(bytes[..count].to_vec()).expect("ascii request"));
                let encoding = encoding.map_or(String::new(), |value| {
                    format!("Content-Encoding: {value}\r\n")
                });
                let response = format!(
                    "HTTP/1.1 200 OK\r\n{content_type_header}{encoding}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("catalog reply");
            }
            requests
        });
        let client = Client::builder()
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let credentials =
            NativeApiCredentials::new(profile, "fixture-key".into(), workspace.map(str::to_owned))
                .unwrap();
        let result = list_at(&client, profile, endpoint, &credentials).await;
        (result, server.await.expect("fixture server"))
    }

    #[test]
    fn catalog_classification_is_separate_from_discovery() {
        assert_eq!(
            ModelEntry::native("openai", "gpt-5.4".into()).efforts,
            vec![
                Effort::None,
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::Xhigh,
            ]
        );
        let current_openai = ModelEntry::native("openai", "gpt-6.1-sol".into());
        assert!(current_openai.runnable);
        assert_eq!(current_openai.efforts.first(), Some(&Effort::Low));
        assert_eq!(current_openai.efforts.last(), Some(&Effort::Max));
        let current_anthropic = ModelEntry::native("anthropic", "claude-opus-5-5".into());
        assert!(current_anthropic.runnable);
        assert_eq!(current_anthropic.efforts.first(), Some(&Effort::Low));
        assert_eq!(current_anthropic.efforts.last(), Some(&Effort::Max));
        let unreviewed = ModelEntry::native("openai", "text-embedding-3-small".into());
        assert!(!unreviewed.runnable);
        assert!(unreviewed.efforts.is_empty());
        assert!(!ModelEntry::exact_custom("local-model".into()).runnable);
    }

    #[tokio::test]
    async fn native_catalog_lists_account_visible_models_without_confusing_support() {
        let (openai, requests) = fixture(
            "openai",
            &[(
                r#"{"data":[{"id":"embedding-only"},{"id":"gpt-5.4"},{"id":"gpt-6-luna"}]}"#,
                None,
            )],
        )
        .await;
        let models = openai.expect("OpenAI models");
        assert_eq!(
            models
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            vec!["embedding-only", "gpt-5.4", "gpt-6-luna"]
        );
        assert!(!models[0].runnable);
        assert!(models[1].runnable);
        assert!(models[2].runnable);
        assert_eq!(models[2].efforts.first(), Some(&Effort::None));
        assert_eq!(models[2].efforts.last(), Some(&Effort::Max));
        assert!(requests[0].starts_with("GET /v1/models HTTP/1.1\r\n"));
        assert!(
            !requests[0]
                .to_ascii_lowercase()
                .contains("anthropic-workspace-id:")
        );
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-key\r\n")
        );

        let (anthropic, requests) = fixture_with_header(
            "anthropic",
            &[
                (r#"{"data":[{"id":"claude-sonnet-5"}],"has_more":true,"last_id":"claude-sonnet-5"}"#, None),
                (r#"{"data":[{"id":"claude-opus-5-5"},{"id":"other-model"}],"has_more":false,"last_id":"other-model"}"#, None),
            ],
            "Content-Type: application/json\r\n",
            Some("wrkspc_Fixture"),
        )
        .await;
        let models = anthropic.unwrap();
        assert_eq!(models.len(), 3);
        assert!(models[0].runnable);
        assert!(models[1].runnable);
        assert!(!models[2].runnable);
        assert!(
            requests.iter().all(|request| request
                .to_ascii_lowercase()
                .contains("anthropic-workspace-id: wrkspc_fixture\r\n")),
            "each page must use the selected workspace"
        );
        assert!(requests[0].starts_with("GET /v1/models?limit=1000 HTTP/1.1\r\n"));
        assert!(
            requests[1]
                .starts_with("GET /v1/models?limit=1000&after_id=claude-sonnet-5 HTTP/1.1\r\n")
        );
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("x-api-key: fixture-key\r\n")
        );
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("anthropic-version: 2023-06-01\r\n")
        );
    }

    #[tokio::test]
    async fn malformed_oversized_and_hostile_catalogs_fail_closed() {
        for (profile, body) in [
            ("openai", r#"{"data":[{"id":"gpt-5.4"}]}"#),
            (
                "anthropic",
                r#"{"data":[{"id":"claude-sonnet-5"}],"has_more":false}"#,
            ),
        ] {
            for content_type in [
                "",
                "Content-Type: text/html\r\n",
                "Content-Type: application/json\r\nContent-Type: application/json\r\n",
            ] {
                assert!(
                    matches!(
                        fixture_with_header(profile, &[(body, None)], content_type, None)
                            .await
                            .0,
                        Err(ProviderError::InvalidOutcome)
                    ),
                    "{profile} accepted {content_type:?}"
                );
            }
            assert!(
                fixture_with_header(
                    profile,
                    &[(body, None)],
                    "Content-Type: application/json; charset=utf-8\r\n",
                    None,
                )
                .await
                .0
                .is_ok(),
                "{profile} rejected parameterized JSON"
            );
        }
        for body in [
            "{}",
            r#"{"data":[{"id":"bad\u001bmodel"}]}"#,
            r#"{"data":[{"id":"same"},{"id":"same"}]}"#,
            r#"{"data":[{"id":"fixture-key"}]}"#,
        ] {
            assert!(matches!(
                fixture("openai", &[(body, None)]).await.0,
                Err(ProviderError::InvalidOutcome)
            ));
        }
        assert!(matches!(
            fixture("openai", &[(r#"{"data":[]}"#, Some("gzip"))])
                .await
                .0,
            Err(ProviderError::InvalidOutcome)
        ));
        assert!(matches!(
            fixture(
                "anthropic",
                &[(r#"{"data":[],"has_more":true,"last_id":"x"}"#, None)]
            )
            .await
            .0,
            Err(ProviderError::InvalidOutcome)
        ));
        assert!(matches!(
            fixture(
                "anthropic",
                &[(
                    r#"{"data":[{"id":"x"}],"has_more":true,"last_id":"other"}"#,
                    None
                )]
            )
            .await
            .0,
            Err(ProviderError::InvalidOutcome)
        ));
        assert!(matches!(
            fixture(
                "anthropic",
                &[
                    (
                        r#"{"data":[{"id":"x"}],"has_more":true,"last_id":"x"}"#,
                        None
                    ),
                    (
                        r#"{"data":[{"id":"x"}],"has_more":false,"last_id":"x"}"#,
                        None
                    ),
                ]
            )
            .await
            .0,
            Err(ProviderError::InvalidOutcome)
        ));

        let rows = (0..MAX_MODELS)
            .map(|index| format!("{{\"id\":\"model-{index}\"}}"))
            .collect::<Vec<_>>()
            .join(",");
        let within_limit = format!("{{\"data\":[{rows}]}}");
        assert_eq!(
            serde_json::from_str::<OpenAiPage>(&within_limit)
                .expect("largest bounded page")
                .data
                .len(),
            MAX_MODELS
        );
        let rows = format!("{rows},{{\"id\":\"model-{MAX_MODELS}\"}}");
        let many = format!("{{\"data\":[{rows}]}}");
        assert!(
            serde_json::from_str::<OpenAiPage>(&many)
                .err()
                .expect("bounded OpenAI rows")
                .to_string()
                .contains("too many model rows")
        );
        let anthropic_many = format!("{{\"data\":[{rows}],\"has_more\":false}}");
        assert!(
            serde_json::from_str::<AnthropicPage>(&anthropic_many)
                .err()
                .expect("bounded Anthropic rows")
                .to_string()
                .contains("too many model rows")
        );
        assert!(matches!(
            fixture("openai", &[(many.as_str(), None)]).await.0,
            Err(ProviderError::InvalidOutcome)
        ));
    }
}

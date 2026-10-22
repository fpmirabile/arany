use crate::provider::{Effort, valid_profile_name as valid_name};
use crate::store::{RecordFormat, RecordKind, RecordStep, StateRoot, StoreError};
use reqwest::Url;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

mod adapter;
mod check;
mod destination;
mod transport;
pub use adapter::CustomProvider;
pub use check::{CustomProfileCheck, check_custom_profile, check_custom_profile_with_cancellation};
use destination::PinnedDestination;

const MAX_PROFILES: usize = 32;
const OUTPUT_CAP: u32 = crate::provider::DEFAULT_OUTPUT_TOKEN_CAP;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustomProfile {
    name: String,
    endpoint: String,
    protocol: Protocol,
    model: String,
    credential_env: String,
    capability_evidence_version: u32,
    efforts: Vec<Effort>,
}

#[derive(Debug, thiserror::Error)]
pub enum CustomProfileError {
    #[error("Provider check could not start: {0}")]
    Journal(Box<crate::CheckOpenError>),
    #[error("{source}\n{receipt}")]
    Check {
        source: Box<CustomProfileError>,
        receipt: crate::CheckReceipt,
    },
    #[error("custom Provider profile file is unavailable")]
    Unavailable,
    #[error("custom Provider profile file is unsafe")]
    UnsafeFile,
    #[error("invalid custom Provider profile configuration")]
    InvalidConfiguration,
    #[error("custom Provider profile not found")]
    NotFound,
    #[error("custom Provider credential unavailable")]
    CredentialUnavailable,
    #[error("custom Provider conformance failed")]
    ConformanceFailed,
    #[error("custom Provider evidence unavailable")]
    EvidenceUnavailable,
    #[error("selected custom Provider model does not match its profile")]
    ModelMismatch,
    #[error("selected custom Provider effort is not conformed for its profile")]
    EffortUnavailable,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileDocument {
    version: u8,
    profiles: Vec<ProfileEntry>,
}

#[derive(Deserialize)]
#[serde(tag = "protocol", deny_unknown_fields)]
enum ProfileEntry {
    #[serde(rename = "openai-responses")]
    OpenaiResponses {
        endpoint: String,
        name: String,
        model: String,
        credential_env: String,
        outcome_encoding: OutcomeEncoding,
        privacy: Privacy,
        max_output_tokens: u32,
        capability_evidence_version: u32,
        efforts: Vec<Effort>,
    },
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages {
        base_url: String,
        auth: MessagesAuth,
        name: String,
        model: String,
        credential_env: String,
        outcome_encoding: OutcomeEncoding,
        privacy: Privacy,
        max_output_tokens: u32,
        capability_evidence_version: u32,
        efforts: Vec<Effort>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Protocol {
    OpenaiResponses,
    AnthropicMessages {
        base_url: String,
        auth: MessagesAuth,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
enum MessagesAuth {
    #[serde(rename = "bearer")]
    Bearer,
    #[serde(rename = "x-api-key")]
    ApiKey,
}

#[derive(Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum OutcomeEncoding {
    JsonSchema,
    NativeTools,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Privacy {
    UserAuthorized,
}

pub(crate) const PROVIDER_PROFILES: RecordFormat = RecordFormat {
    kind: RecordKind::ProviderProfiles,
    field: "version",
    floor: 1,
    current: 2,
    steps: &[RecordStep {
        from: 1,
        transform: profiles_v1_to_v2,
    }],
    accepts: |bytes| CustomProfile::parse_document(bytes).is_ok(),
};

fn profiles_v1_to_v2(mut document: serde_json::Value) -> Option<serde_json::Value> {
    if document.get("version")?.as_u64()? != 1
        || document.get("profiles")?.as_array()?.iter().any(|entry| {
            entry.get("protocol").and_then(serde_json::Value::as_str) != Some("openai-responses")
        })
    {
        return None;
    }
    document["version"] = 2.into();
    Some(document)
}

impl CustomProfile {
    async fn resolve_destination(&self) -> Result<PinnedDestination, CustomProfileError> {
        PinnedDestination::resolve(self).await
    }

    pub fn load_named(state: &StateRoot, name: &str) -> Result<Self, CustomProfileError> {
        if !valid_name(name) {
            return Err(CustomProfileError::InvalidConfiguration);
        }
        let bytes = state
            .read_current(&PROVIDER_PROFILES)
            .map_err(|error| match error {
                StoreError::StateNotPrivate => CustomProfileError::UnsafeFile,
                _ => CustomProfileError::Unavailable,
            })?
            .ok_or(CustomProfileError::Unavailable)?;
        Self::parse_document(&bytes)?
            .into_iter()
            .find(|profile| profile.name == name)
            .ok_or(CustomProfileError::NotFound)
    }

    fn parse_document(bytes: &[u8]) -> Result<Vec<Self>, CustomProfileError> {
        let document: ProfileDocument = serde_json::from_value(
            crate::tools::parse_json_bounded(bytes, 64 * 1024, 65_536, 32)
                .map_err(|_| CustomProfileError::InvalidConfiguration)?,
        )
        .map_err(|_| CustomProfileError::InvalidConfiguration)?;
        if document.version != 2 || document.profiles.len() > MAX_PROFILES {
            return Err(CustomProfileError::InvalidConfiguration);
        }
        let mut names = HashSet::new();
        let mut credentials = HashMap::new();
        let mut profiles = Vec::with_capacity(document.profiles.len());
        for entry in document.profiles {
            let profile = Self::validate(entry)?;
            if !names.insert(profile.name.clone()) {
                return Err(CustomProfileError::InvalidConfiguration);
            }
            let origin = Url::parse(&profile.endpoint)
                .map_err(|_| CustomProfileError::InvalidConfiguration)?
                .origin()
                .ascii_serialization();
            if credentials
                .insert(profile.credential_env.clone(), origin.clone())
                .is_some_and(|previous| previous != origin)
            {
                return Err(CustomProfileError::InvalidConfiguration);
            }
            profiles.push(profile);
        }
        Ok(profiles)
    }

    fn validate(entry: ProfileEntry) -> Result<Self, CustomProfileError> {
        let (profile, cap, encoding, expected, _privacy) = match entry {
            ProfileEntry::OpenaiResponses {
                name,
                endpoint,
                model,
                credential_env,
                outcome_encoding,
                privacy,
                max_output_tokens,
                capability_evidence_version,
                efforts,
            } => {
                if !valid_endpoint(&endpoint) {
                    return Err(CustomProfileError::InvalidConfiguration);
                }
                (
                    Self {
                        name,
                        endpoint,
                        protocol: Protocol::OpenaiResponses,
                        model,
                        credential_env,
                        capability_evidence_version,
                        efforts,
                    },
                    max_output_tokens,
                    outcome_encoding,
                    OutcomeEncoding::JsonSchema,
                    privacy,
                )
            }
            ProfileEntry::AnthropicMessages {
                name,
                base_url,
                auth,
                model,
                credential_env,
                outcome_encoding,
                privacy,
                max_output_tokens,
                capability_evidence_version,
                efforts,
            } => {
                let (base_url, endpoint) =
                    messages_endpoint(&base_url).ok_or(CustomProfileError::InvalidConfiguration)?;
                (
                    Self {
                        name,
                        endpoint,
                        protocol: Protocol::AnthropicMessages { base_url, auth },
                        model,
                        credential_env,
                        capability_evidence_version,
                        efforts,
                    },
                    max_output_tokens,
                    outcome_encoding,
                    OutcomeEncoding::NativeTools,
                    privacy,
                )
            }
        };
        if !valid_name(&profile.name)
            || matches!(profile.name.as_str(), "openai" | "anthropic")
            || profile.model.is_empty()
            || profile.model.len() > 128
            || !profile.model.bytes().all(|byte| byte.is_ascii_graphic())
            || cap != OUTPUT_CAP
            || encoding != expected
            || profile.capability_evidence_version != 2
            || profile.efforts.len() > Effort::ALL.len()
            || profile
                .efforts
                .windows(2)
                .any(|pair| effort_index(pair[0]) >= effort_index(pair[1]))
            || !valid_credential_env(&profile.credential_env)
        {
            return Err(CustomProfileError::InvalidConfiguration);
        }
        Ok(profile)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn credential_env(&self) -> &str {
        &self.credential_env
    }

    pub fn max_output_tokens(&self) -> u32 {
        OUTPUT_CAP
    }

    pub fn efforts(&self) -> &[Effort] {
        &self.efforts
    }

    pub fn capability_evidence_version(&self) -> u32 {
        self.capability_evidence_version
    }

    pub fn admits_effort(&self, effort: Option<Effort>) -> bool {
        effort.is_none_or(|value| self.efforts.contains(&value))
    }
}

fn effort_index(effort: Effort) -> usize {
    Effort::ALL
        .iter()
        .position(|value| *value == effort)
        .expect("Effort::ALL contains every Effort variant")
}

fn valid_credential_env(name: &str) -> bool {
    name.len() <= 64
        && name.strip_prefix("ARANY_PROVIDER_").is_some_and(|suffix| {
            !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        })
}

fn valid_endpoint(endpoint: &str) -> bool {
    valid_url(endpoint, false)
        && Url::parse(endpoint).is_ok_and(|url| url.path().ends_with("/responses"))
}

fn messages_endpoint(base: &str) -> Option<(String, String)> {
    if !valid_url(base, true) {
        return None;
    }
    let normalized = base.strip_suffix('/').unwrap_or(base);
    if normalized.ends_with("/v1/messages") {
        return None;
    }
    let endpoint = format!("{normalized}/v1/messages");
    valid_url(&endpoint, false).then(|| (normalized.to_owned(), endpoint))
}

fn valid_url(endpoint: &str, host_only: bool) -> bool {
    if endpoint.is_empty() || endpoint.len() > 512 {
        return false;
    }
    let Ok(url) = Url::parse(endpoint) else {
        return false;
    };
    let canonical = url.as_str() == endpoint
        || (host_only && url.path() == "/" && url.as_str().strip_suffix('/') == Some(endpoint));
    if !canonical
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path().len() > 256
        || url.path().contains('%')
        || url.path().contains("//")
        || url
            .path()
            .split('/')
            .any(|segment| matches!(segment, "." | ".."))
        || !url
            .path()
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
    {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    match host.trim_matches(['[', ']']).parse::<IpAddr>() {
        Ok(address) => address.is_loopback() && matches!(url.scheme(), "http" | "https"),
        Err(_) => url.scheme() == "https" && !host.ends_with('.'),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const VALID: &str = r#"{"version":2,"profiles":[{"name":"local","protocol":"openai-responses","endpoint":"http://127.0.0.1:9321/v1/responses","model":"model-1","credential_env":"ARANY_PROVIDER_LOCAL_KEY","outcome_encoding":"json_schema","privacy":"user_authorized","max_output_tokens":4096,"capability_evidence_version":2,"efforts":[]}]}"#;

    fn state_with_file(temp: &Path, body: &[u8]) -> StateRoot {
        let state = StateRoot::admit(&temp.join("state")).expect("private state root");
        let file = state.path().join("provider-profiles.json");
        std::fs::write(&file, body).expect("profile file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
                .expect("private profile file");
        }
        state
    }

    #[test]
    fn named_profiles_require_private_files_and_exact_closed_configuration() {
        let temp = tempfile::tempdir().expect("private test root");
        let state = state_with_file(temp.path(), VALID.as_bytes());
        let profile = CustomProfile::load_named(&state, "local").expect("valid profile");
        assert_eq!(profile.name(), "local");
        assert_eq!(profile.endpoint(), "http://127.0.0.1:9321/v1/responses");
        assert_eq!(profile.model(), "model-1");
        assert_eq!(profile.credential_env(), "ARANY_PROVIDER_LOCAL_KEY");
        assert_eq!(profile.max_output_tokens(), 4096);
        assert!(matches!(
            CustomProfile::load_named(&state, "absent"),
            Err(CustomProfileError::NotFound)
        ));

        let messages = VALID
            .replace("openai-responses", "anthropic-messages")
            .replace(
                "\"endpoint\":\"http://127.0.0.1:9321/v1/responses\"",
                "\"base_url\":\"https://example.com/anthropic/\",\"auth\":\"x-api-key\"",
            )
            .replace("json_schema", "native_tools");
        let parsed = CustomProfile::parse_document(messages.as_bytes()).unwrap();
        assert_eq!(
            parsed[0].endpoint(),
            "https://example.com/anthropic/v1/messages"
        );
        let mut routes = serde_json::json!({"version":2,"profiles":[]});
        for (name, base, endpoint, model, auth, credential) in [
            (
                "kimi-api",
                "https://api.moonshot.ai/anthropic",
                "https://api.moonshot.ai/anthropic/v1/messages",
                "kimi-k3",
                "bearer",
                "ARANY_PROVIDER_KIMI_API_KEY",
            ),
            (
                "kimi-code",
                "https://api.kimi.com/coding/",
                "https://api.kimi.com/coding/v1/messages",
                "k3-256k",
                "x-api-key",
                "ARANY_PROVIDER_KIMI_CODE_KEY",
            ),
            (
                "glm-api",
                "https://api.z.ai/api/anthropic",
                "https://api.z.ai/api/anthropic/v1/messages",
                "glm-5.3",
                "bearer",
                "ARANY_PROVIDER_GLM_STANDARD_KEY",
            ),
            (
                "glm-flash",
                "https://api.z.ai/api/anthropic",
                "https://api.z.ai/api/anthropic/v1/messages",
                "glm-5.3-flash",
                "bearer",
                "ARANY_PROVIDER_GLM_STANDARD_KEY",
            ),
        ] {
            let mut candidate: serde_json::Value = serde_json::from_str(&messages).unwrap();
            let row = &mut candidate["profiles"][0];
            row["name"] = name.into();
            row["base_url"] = base.into();
            row["model"] = model.into();
            row["auth"] = auth.into();
            row["credential_env"] = credential.into();
            std::fs::write(
                state.path().join("provider-profiles.json"),
                serde_json::to_vec(&candidate).unwrap(),
            )
            .unwrap();
            let parsed = CustomProfile::load_named(&state, name).unwrap();
            assert_eq!(parsed.endpoint(), endpoint);
            assert_eq!(parsed.model(), model);
            assert_eq!(parsed.credential_env(), credential);
            assert!(parsed.efforts().is_empty());
            assert!(
                matches!(&parsed.protocol, Protocol::AnthropicMessages { base_url, auth: selected }
                if base_url == base.trim_end_matches('/') && *selected == if auth == "bearer" { MessagesAuth::Bearer } else { MessagesAuth::ApiKey })
            );
            routes["profiles"]
                .as_array_mut()
                .unwrap()
                .push(candidate["profiles"][0].clone());
        }
        assert!(CustomProfile::parse_document(&serde_json::to_vec(&routes).unwrap()).is_ok());
        for index in 1..4 {
            let mut conflicting = routes.clone();
            conflicting["profiles"][index]["credential_env"] = "ARANY_PROVIDER_KIMI_API_KEY".into();
            assert!(
                CustomProfile::parse_document(&serde_json::to_vec(&conflicting).unwrap()).is_err()
            );
        }
        std::fs::write(state.path().join("provider-profiles.json"), VALID).unwrap();
        for base in [
            "https://example.com",
            "https://example.com/",
            "http://127.0.0.1:9321",
            "https://[::1]/anthropic",
        ] {
            let candidate = messages.replace("https://example.com/anthropic/", base);
            let profile = CustomProfile::parse_document(candidate.as_bytes())
                .unwrap()
                .remove(0);
            assert_eq!(
                profile.endpoint(),
                format!("{}/v1/messages", base.trim_end_matches('/'))
            );
        }
        for base in [
            "https://EXAMPLE.com",
            "https://example.com:443",
            "https://example.com//",
            "https://example.com/a/../b",
            "https://example.com/%61",
            "https://example.com/?a=1",
            "https://key@example.com",
            "https://example.com/#x",
            "https://example.com/v1/messages/",
            "http://example.com",
            "http://169.254.169.254",
            "https://example.com./",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain([
            format!("https://example.com/{}", "a".repeat(245)),
            format!("https://{}/{}", "a".repeat(260), "b".repeat(245)),
        ]) {
            assert!(
                CustomProfile::parse_document(
                    messages
                        .replace("https://example.com/anthropic/", &base)
                        .as_bytes()
                )
                .is_err(),
                "invalid base {base}"
            );
        }
        let boundary = format!("https://example.com/{}", "a".repeat(243));
        assert!(
            CustomProfile::parse_document(
                messages
                    .replace("https://example.com/anthropic/", &boundary)
                    .as_bytes()
            )
            .is_ok()
        );
        let host = format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(56)
        );
        let exact = format!("https://{host}/{}", "a".repeat(243));
        let exact_profile = CustomProfile::parse_document(
            messages
                .replace("https://example.com/anthropic/", &exact)
                .as_bytes(),
        )
        .unwrap()
        .remove(0);
        assert_eq!(exact_profile.endpoint().len(), 512);
        assert!(
            CustomProfile::parse_document(
                messages
                    .replace(
                        "https://example.com/anthropic/",
                        &exact.replace(&host, &format!("{host}a"))
                    )
                    .as_bytes()
            )
            .is_err()
        );
        for invalid in [
            messages.replace("native_tools", "json_schema"),
            messages.replace("x-api-key", "ambient"),
            messages.replace("\"auth\":", "\"headers\":{},\"auth\":"),
            messages.replace(
                "\"auth\":",
                "\"endpoint\":\"https://example.com/responses\",\"auth\":",
            ),
            messages.replace(
                "\"auth\":\"x-api-key\"",
                "\"auth\":\"x-api-key\",\"auth\":\"bearer\"",
            ),
            VALID.replace(
                "\"model\":",
                "\"base_url\":\"https://example.com\",\"model\":",
            ),
            VALID.replace("\"model\":", "\"auth\":\"bearer\",\"model\":"),
        ] {
            assert!(CustomProfile::parse_document(invalid.as_bytes()).is_err());
        }
        let mut mixed: serde_json::Value = serde_json::from_str(VALID).unwrap();
        let mut other: serde_json::Value = serde_json::from_str(&messages).unwrap();
        other["profiles"][0]["name"] = "messages".into();
        mixed["profiles"]
            .as_array_mut()
            .unwrap()
            .push(other["profiles"][0].clone());
        assert!(CustomProfile::parse_document(&serde_json::to_vec(&mixed).unwrap()).is_err());
        mixed["profiles"][1]["credential_env"] = "ARANY_PROVIDER_OTHER_KEY".into();
        assert!(CustomProfile::parse_document(&serde_json::to_vec(&mixed).unwrap()).is_ok());

        let replacements = [
            ("version", "\"version\":3"),
            ("protocol", "\"protocol\":\"openai-chat-completions\""),
            ("encoding", "\"outcome_encoding\":\"json_mode\""),
            ("privacy", "\"privacy\":\"provider_zdr\""),
            ("cap", "\"max_output_tokens\":8192"),
            ("evidence", "\"capability_evidence_version\":1"),
            ("credential", "\"credential_env\":\"OPENAI_API_KEY\""),
            (
                "metadata address",
                "\"endpoint\":\"https://169.254.169.254/v1/responses\"",
            ),
            (
                "plaintext domain",
                "\"endpoint\":\"http://example.com/v1/responses\"",
            ),
            (
                "query",
                "\"endpoint\":\"https://example.com/v1/responses?x=1\"",
            ),
            (
                "noncanonical host",
                "\"endpoint\":\"https://EXAMPLE.com/v1/responses\"",
            ),
            ("unknown field", "\"version\":2,\"unexpected\":true"),
            ("invalid name", "\"name\":\"OpenAI\""),
        ];
        for (case, replacement) in replacements {
            let original = match case {
                "version" | "unknown field" => "\"version\":2",
                "protocol" => "\"protocol\":\"openai-responses\"",
                "encoding" => "\"outcome_encoding\":\"json_schema\"",
                "privacy" => "\"privacy\":\"user_authorized\"",
                "cap" => "\"max_output_tokens\":4096",
                "evidence" => "\"capability_evidence_version\":2",
                "credential" => "\"credential_env\":\"ARANY_PROVIDER_LOCAL_KEY\"",
                "metadata address" | "plaintext domain" | "query" | "noncanonical host" => {
                    "\"endpoint\":\"http://127.0.0.1:9321/v1/responses\""
                }
                "invalid name" => "\"name\":\"local\"",
                _ => unreachable!(),
            };
            let invalid = VALID.replace(original, replacement);
            assert_ne!(invalid, VALID, "{case} fixture must change");
            std::fs::write(state.path().join("provider-profiles.json"), invalid)
                .expect("replace profile file");
            assert!(
                matches!(
                    CustomProfile::load_named(&state, "local"),
                    Err(CustomProfileError::InvalidConfiguration)
                ),
                "{case}"
            );
        }

        let mut versioned: serde_json::Value = serde_json::from_str(VALID).unwrap();
        for (efforts, valid) in [
            (serde_json::json!(["low", "high"]), true),
            (serde_json::json!([]), true),
            (serde_json::json!(["high", "low"]), false),
            (serde_json::json!(["low", "low"]), false),
            (serde_json::json!(["minimal"]), false),
        ] {
            versioned["profiles"][0]["efforts"] = efforts;
            std::fs::write(
                state.path().join("provider-profiles.json"),
                serde_json::to_vec(&versioned).unwrap(),
            )
            .unwrap();
            let loaded = CustomProfile::load_named(&state, "local");
            assert_eq!(loaded.is_ok(), valid);
            if let Ok(profile) = loaded {
                assert_eq!(
                    profile.admits_effort(Some(Effort::Low)),
                    !profile.efforts().is_empty()
                );
                assert!(!profile.admits_effort(Some(Effort::Max)));
                assert_eq!(profile.capability_evidence_version(), 2);
            }
        }
        versioned["profiles"][0]["capability_evidence_version"] = serde_json::json!(1);
        versioned["profiles"][0]["efforts"] = serde_json::json!([]);
        std::fs::write(
            state.path().join("provider-profiles.json"),
            serde_json::to_vec(&versioned).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            CustomProfile::load_named(&state, "local"),
            Err(CustomProfileError::InvalidConfiguration)
        ));

        let mut document: serde_json::Value = serde_json::from_str(VALID).expect("valid JSON");
        let first = document["profiles"][0].clone();
        document["profiles"]
            .as_array_mut()
            .unwrap()
            .push(first.clone());
        std::fs::write(
            state.path().join("provider-profiles.json"),
            serde_json::to_vec(&document).unwrap(),
        )
        .expect("duplicate-name profile file");
        assert!(matches!(
            CustomProfile::load_named(&state, "local"),
            Err(CustomProfileError::InvalidConfiguration)
        ));

        document["profiles"][1]["name"] = "other".into();
        document["profiles"][1]["endpoint"] = "https://example.com/v1/responses".into();
        std::fs::write(
            state.path().join("provider-profiles.json"),
            serde_json::to_vec(&document).unwrap(),
        )
        .expect("cross-origin credential file");
        assert!(matches!(
            CustomProfile::load_named(&state, "local"),
            Err(CustomProfileError::InvalidConfiguration)
        ));

        document["profiles"][1]["credential_env"] = "ARANY_PROVIDER_OTHER_KEY".into();
        std::fs::write(
            state.path().join("provider-profiles.json"),
            serde_json::to_vec(&document).unwrap(),
        )
        .expect("second valid profile");
        assert_eq!(
            CustomProfile::load_named(&state, "other")
                .unwrap()
                .endpoint(),
            "https://example.com/v1/responses"
        );

        let template = document["profiles"][1].clone();
        for index in 2..=MAX_PROFILES {
            let mut profile = template.clone();
            profile["name"] = format!("profile-{index}").into();
            document["profiles"].as_array_mut().unwrap().push(profile);
        }
        std::fs::write(
            state.path().join("provider-profiles.json"),
            serde_json::to_vec(&document).unwrap(),
        )
        .expect("too many profiles");
        assert!(matches!(
            CustomProfile::load_named(&state, "local"),
            Err(CustomProfileError::InvalidConfiguration)
        ));
    }

    #[test]
    fn provider_profiles_v1_migrates_without_authorizing_new_protocols() {
        let temp = tempfile::tempdir().unwrap();
        let v1 = VALID.replace("\"version\":2", "\"version\":1");
        let state = state_with_file(temp.path(), v1.as_bytes());
        CustomProfile::load_named(&state, "local").unwrap();
        let path = state.path().to_path_buf();
        drop(state);
        let state = StateRoot::open_existing(&path).unwrap();
        CustomProfile::load_named(&state, "local").unwrap();
        let current = std::fs::read(path.join("provider-profiles.json")).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&current).unwrap()["version"],
            2
        );
        assert!(CustomProfile::parse_document(v1.as_bytes()).is_err());
        let messages = VALID
            .replace("openai-responses", "anthropic-messages")
            .replace(
                "\"endpoint\":\"http://127.0.0.1:9321/v1/responses\"",
                "\"base_url\":\"https://example.com/anthropic\",\"auth\":\"x-api-key\"",
            )
            .replace("json_schema", "native_tools");
        assert!(CustomProfile::parse_document(messages.as_bytes()).is_ok());
        for invalid in [
            v1.replace("json_schema", "native_tools"),
            v1.replace("\"version\":1", "\"version\":1,\"version\":1"),
            messages.replace("\"version\":2", "\"version\":1"),
            VALID.replace("\"version\":2", "\"version\":3"),
            VALID.replace("json_schema", "native_tools"),
        ] {
            std::fs::write(path.join("provider-profiles.json"), &invalid).unwrap();
            assert!(CustomProfile::load_named(&state, "local").is_err());
            assert_eq!(
                std::fs::read(path.join("provider-profiles.json")).unwrap(),
                invalid.as_bytes()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn profile_file_rejects_links_public_modes_and_oversize() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir().expect("private test root");
        let state = state_with_file(temp.path(), VALID.as_bytes());
        let profile_path = state.path().join("provider-profiles.json");

        std::fs::set_permissions(&profile_path, std::fs::Permissions::from_mode(0o644))
            .expect("public mode");
        assert!(matches!(
            CustomProfile::load_named(&state, "local"),
            Err(CustomProfileError::UnsafeFile)
        ));

        std::fs::set_permissions(&profile_path, std::fs::Permissions::from_mode(0o600))
            .expect("private mode");
        let hardlink = temp.path().join("profile-hardlink");
        std::fs::hard_link(&profile_path, &hardlink).expect("hardlink");
        assert!(matches!(
            CustomProfile::load_named(&state, "local"),
            Err(CustomProfileError::UnsafeFile)
        ));
        std::fs::remove_file(&hardlink).expect("remove temporary hardlink");

        std::fs::write(&profile_path, vec![b' '; 64 * 1024 + 1]).expect("overlong file");
        assert!(matches!(
            CustomProfile::load_named(&state, "local"),
            Err(CustomProfileError::UnsafeFile)
        ));

        std::fs::remove_file(&profile_path).expect("replace temporary profile");
        symlink(temp.path().join("missing"), &profile_path).expect("profile symlink");
        assert!(CustomProfile::load_named(&state, "local").is_err());
    }
}

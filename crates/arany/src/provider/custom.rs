use crate::provider::{Effort, valid_profile_name as valid_name};
use crate::store::{RecordFormat, RecordKind, StateRoot, StoreError};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

mod adapter;
mod destination;
mod transport;
#[cfg(test)]
mod transport_tests;
pub use adapter::CustomProvider;
use destination::PinnedDestination;
pub(crate) use destination::resolve_admitted;

const MAX_PROFILES: usize = 32;
pub const MAX_CUSTOM_PROFILE_NAME_BYTES: usize = super::roster::MAX_PROFILE_NAME_BYTES;
pub const MAX_CUSTOM_PROFILE_ENDPOINT_BYTES: usize = 512;
pub const MAX_CUSTOM_PROFILE_MODEL_BYTES: usize = super::MAX_MODEL_ID_BYTES;
pub const MAX_CUSTOM_PROFILE_CREDENTIAL_ENV_BYTES: usize = 64;
pub const MAX_CUSTOM_PROFILE_EFFORTS_BYTES: usize = 64;
const CUSTOM_OUTPUT_TOKEN_CAP: u32 = crate::provider::DEFAULT_OUTPUT_TOKEN_CAP;

// A validated declaration and the request endpoint derived from it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustomProfile {
    declaration: CustomProfileDraft,
    endpoint: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CustomProtocol {
    OpenaiResponses,
    AnthropicMessages,
    OpenaiChatCompletions,
}

// One stored entry: `url` is the Responses endpoint or the Messages/Chat Completions base URL.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CustomProfileDraft {
    pub name: String,
    pub protocol: CustomProtocol,
    pub url: String,
    // Messages only; the other protocols always send a bearer token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<MessagesAuth>,
    pub model: String,
    pub credential_env: String,
    pub efforts: Vec<Effort>,
}

#[derive(Debug, thiserror::Error)]
pub enum CustomProfileError {
    #[error("custom Provider profile file is unavailable")]
    Unavailable,
    #[error("custom Provider profile file is unsafe")]
    UnsafeFile,
    #[error("custom Provider profile changed since it was inspected")]
    Changed,
    #[error("custom Provider profile publication may have completed")]
    PublicationUncertain,
    #[error("invalid custom Provider profile configuration")]
    InvalidConfiguration,
    #[error("custom Provider profile not found")]
    NotFound,
    #[error("custom Provider credential unavailable")]
    CredentialUnavailable,
    #[error("selected custom Provider model does not match its profile")]
    ModelMismatch,
    #[error("selected custom Provider effort is not declared by its profile")]
    EffortUnavailable,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProfileDocument {
    profiles: Vec<CustomProfileDraft>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum MessagesAuth {
    #[serde(rename = "bearer")]
    Bearer,
    #[serde(rename = "x-api-key")]
    ApiKey,
}

pub(crate) const PROVIDER_PROFILES: RecordFormat = RecordFormat {
    kind: RecordKind::ProviderProfiles,
    accepts: |bytes| CustomProfile::parse_document(bytes).is_ok(),
};

impl CustomProfile {
    pub fn validate_drafts(profiles: &[CustomProfileDraft]) -> Result<(), CustomProfileError> {
        encode_profiles(profiles).map(|_| ())
    }

    pub fn snapshot(state: &StateRoot) -> Result<Option<Vec<u8>>, CustomProfileError> {
        state
            .record(PROVIDER_PROFILES.kind)
            .read()
            .map_err(|error| match error {
                StoreError::StateNotPrivate => CustomProfileError::UnsafeFile,
                _ => CustomProfileError::Unavailable,
            })
    }

    pub fn load_all(state: &StateRoot) -> Result<Vec<Self>, CustomProfileError> {
        Self::load_all_with_snapshot(state).map(|(_, profiles)| profiles)
    }

    pub fn load_all_with_snapshot(
        state: &StateRoot,
    ) -> Result<(Option<Vec<u8>>, Vec<Self>), CustomProfileError> {
        let bytes = Self::snapshot(state)?;
        let profiles = bytes
            .as_deref()
            .map(Self::parse_document)
            .transpose()?
            .unwrap_or_default();
        Ok((bytes, profiles))
    }

    pub fn write_profiles(
        state: &StateRoot,
        expected: Option<&[u8]>,
        profiles: &[CustomProfileDraft],
    ) -> Result<Vec<u8>, CustomProfileError> {
        let bytes = encode_profiles(profiles)?;
        if Self::snapshot(state)?.as_deref() != expected {
            return Err(CustomProfileError::Changed);
        }
        state
            .edit_provider_record(
                crate::store::ProviderRecordTarget::Profiles,
                expected,
                if bytes.is_empty() {
                    crate::store::ProviderRecordEdit::Remove
                } else {
                    crate::store::ProviderRecordEdit::Replace(&bytes)
                },
                &PROVIDER_PROFILES,
            )
            .map_err(|error| match error {
                StoreError::RecordChanged => CustomProfileError::Changed,
                StoreError::ReplaceUnconfirmed => CustomProfileError::PublicationUncertain,
                _ => CustomProfileError::Unavailable,
            })?;
        Ok(bytes)
    }

    async fn resolve_destination(&self) -> Result<PinnedDestination, CustomProfileError> {
        PinnedDestination::resolve(self).await
    }

    pub fn load_named(state: &StateRoot, name: &str) -> Result<Self, CustomProfileError> {
        if !valid_name(name) {
            return Err(CustomProfileError::InvalidConfiguration);
        }
        let bytes = state
            .record(PROVIDER_PROFILES.kind)
            .read()
            .map_err(|error| match error {
                StoreError::StateNotPrivate => CustomProfileError::UnsafeFile,
                _ => CustomProfileError::Unavailable,
            })?
            .ok_or(CustomProfileError::Unavailable)?;
        Self::parse_document(&bytes)?
            .into_iter()
            .find(|profile| profile.name() == name)
            .ok_or(CustomProfileError::NotFound)
    }

    fn parse_document(bytes: &[u8]) -> Result<Vec<Self>, CustomProfileError> {
        let document: ProfileDocument = serde_json::from_value(
            crate::tools::parse_json_bounded(bytes, 64 * 1024, 65_536, 32)
                .map_err(|_| CustomProfileError::InvalidConfiguration)?,
        )
        .map_err(|_| CustomProfileError::InvalidConfiguration)?;
        if document.profiles.len() > MAX_PROFILES {
            return Err(CustomProfileError::InvalidConfiguration);
        }
        let mut names = HashSet::new();
        let mut credentials = HashMap::new();
        let mut profiles = Vec::with_capacity(document.profiles.len());
        for draft in document.profiles {
            let profile = Self::validate(draft)?;
            if !names.insert(profile.name().to_owned()) {
                return Err(CustomProfileError::InvalidConfiguration);
            }
            let origin = Url::parse(&profile.endpoint)
                .map_err(|_| CustomProfileError::InvalidConfiguration)?
                .origin()
                .ascii_serialization();
            if credentials
                .insert(profile.credential_env().to_owned(), origin.clone())
                .is_some_and(|previous| previous != origin)
            {
                return Err(CustomProfileError::InvalidConfiguration);
            }
            profiles.push(profile);
        }
        Ok(profiles)
    }

    fn validate(mut declaration: CustomProfileDraft) -> Result<Self, CustomProfileError> {
        let derived = match (declaration.protocol, declaration.auth) {
            (CustomProtocol::OpenaiResponses, None) => valid_endpoint(&declaration.url)
                .then(|| (declaration.url.clone(), declaration.url.clone())),
            (CustomProtocol::AnthropicMessages, Some(_)) => messages_endpoint(&declaration.url),
            (CustomProtocol::OpenaiChatCompletions, None) => {
                chat_completions_endpoint(&declaration.url)
            }
            _ => None,
        };
        let (url, endpoint) = derived.ok_or(CustomProfileError::InvalidConfiguration)?;
        declaration.url = url;
        let profile = Self {
            declaration,
            endpoint,
        };
        let declared = &profile.declaration;
        if !valid_name(&declared.name)
            || matches!(declared.name.as_str(), "openai" | "anthropic")
            || declared.model.is_empty()
            || declared.model.len() > MAX_CUSTOM_PROFILE_MODEL_BYTES
            || !declared.model.bytes().all(|byte| byte.is_ascii_graphic())
            || declared.efforts.len() > Effort::ALL.len()
            || declared
                .efforts
                .windows(2)
                .any(|pair| effort_index(pair[0]) >= effort_index(pair[1]))
            || !valid_credential_env(&declared.credential_env)
        {
            return Err(CustomProfileError::InvalidConfiguration);
        }
        Ok(profile)
    }

    pub fn name(&self) -> &str {
        &self.declaration.name
    }

    pub fn draft(&self) -> CustomProfileDraft {
        self.declaration.clone()
    }

    fn protocol(&self) -> CustomProtocol {
        self.declaration.protocol
    }

    fn auth(&self) -> MessagesAuth {
        self.declaration.auth.unwrap_or(MessagesAuth::Bearer)
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn model(&self) -> &str {
        &self.declaration.model
    }

    pub fn credential_env(&self) -> &str {
        &self.declaration.credential_env
    }

    pub fn max_output_tokens(&self) -> u32 {
        CUSTOM_OUTPUT_TOKEN_CAP
    }

    pub fn efforts(&self) -> &[Effort] {
        &self.declaration.efforts
    }

    pub fn admits_effort(&self, effort: Option<Effort>) -> bool {
        effort.is_none_or(|value| self.declaration.efforts.contains(&value))
    }

    // Binds every declared field, so Run provenance names the exact admitted declaration.
    fn digest(&self) -> [u8; 32] {
        crate::validate::digest(
            &serde_json::to_vec(&self.declaration).expect("typed declaration is serializable"),
        )
    }
}

fn encode_profiles(profiles: &[CustomProfileDraft]) -> Result<Vec<u8>, CustomProfileError> {
    if profiles.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = serde_json::to_vec(&ProfileDocument {
        profiles: profiles.to_vec(),
    })
    .map_err(|_| CustomProfileError::InvalidConfiguration)?;
    CustomProfile::parse_document(&bytes)?;
    Ok(bytes)
}

// The loopback Responses route, or a Messages route under `/anthropic` with an `x-api-key` header.
#[cfg(test)]
fn local_profile(port: u16, messages: bool) -> CustomProfile {
    let mut draft = crate::test_support::local_custom_profile(port);
    if messages {
        draft.protocol = CustomProtocol::AnthropicMessages;
        draft.url = format!("http://127.0.0.1:{port}/anthropic");
        draft.auth = Some(MessagesAuth::ApiKey);
    }
    CustomProfile::validate(draft).expect("valid loopback profile")
}

fn effort_index(effort: Effort) -> usize {
    Effort::ALL
        .iter()
        .position(|value| *value == effort)
        .expect("Effort::ALL contains every Effort variant")
}

fn valid_credential_env(name: &str) -> bool {
    name.len() <= MAX_CUSTOM_PROFILE_CREDENTIAL_ENV_BYTES
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
    derived_endpoint(base, "/v1/messages")
}

fn chat_completions_endpoint(base: &str) -> Option<(String, String)> {
    derived_endpoint(base, "/chat/completions")
}

fn derived_endpoint(base: &str, suffix: &str) -> Option<(String, String)> {
    if !valid_url(base, true) {
        return None;
    }
    let normalized = base.strip_suffix('/').unwrap_or(base);
    if normalized.ends_with(suffix) {
        return None;
    }
    let endpoint = format!("{normalized}{suffix}");
    valid_url(&endpoint, false).then(|| (normalized.to_owned(), endpoint))
}

fn valid_url(endpoint: &str, host_only: bool) -> bool {
    if endpoint.is_empty() || endpoint.len() > MAX_CUSTOM_PROFILE_ENDPOINT_BYTES {
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
    use serde_json::{Value, json};
    use std::path::Path;

    fn local_draft(model: &str) -> CustomProfileDraft {
        local_profile(9321, false).draft().with_model(model)
    }

    impl CustomProfileDraft {
        fn with_model(mut self, model: &str) -> Self {
            self.model = model.into();
            self
        }
    }

    fn messages_draft(url: &str) -> CustomProfileDraft {
        CustomProfileDraft {
            protocol: CustomProtocol::AnthropicMessages,
            url: url.into(),
            auth: Some(MessagesAuth::ApiKey),
            ..local_draft("model-1")
        }
    }

    fn document(profiles: &[CustomProfileDraft]) -> Value {
        serde_json::to_value(ProfileDocument {
            profiles: profiles.to_vec(),
        })
        .unwrap()
    }

    fn parse(document: &Value) -> Result<Vec<CustomProfile>, CustomProfileError> {
        CustomProfile::parse_document(&serde_json::to_vec(document).unwrap())
    }

    fn state_with_file(temp: &Path, body: &[u8]) -> StateRoot {
        let state = StateRoot::admit(&temp.join("state")).expect("private state root");
        let file = state.path().join("provider-profiles.json");
        std::fs::write(&file, body).expect("profile file");
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
        let valid = document(&[local_draft("model-1")]);
        let state = state_with_file(temp.path(), valid.to_string().as_bytes());
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

        let parsed = parse(&document(&[messages_draft(
            "https://example.com/anthropic/",
        )]))
        .unwrap()
        .remove(0);
        assert_eq!(
            parsed.endpoint(),
            "https://example.com/anthropic/v1/messages"
        );
        assert_eq!(parsed.draft().url, "https://example.com/anthropic");
        let mut routes = Vec::new();
        for (name, base, endpoint, model, auth, credential) in [
            (
                "kimi-api",
                "https://api.moonshot.ai/anthropic",
                "https://api.moonshot.ai/anthropic/v1/messages",
                "kimi-k3",
                MessagesAuth::Bearer,
                "ARANY_PROVIDER_KIMI_API_KEY",
            ),
            (
                "kimi-code",
                "https://api.kimi.com/coding/",
                "https://api.kimi.com/coding/v1/messages",
                "k3-256k",
                MessagesAuth::ApiKey,
                "ARANY_PROVIDER_KIMI_CODE_KEY",
            ),
            (
                "glm-api",
                "https://api.z.ai/api/anthropic",
                "https://api.z.ai/api/anthropic/v1/messages",
                "glm-5.3",
                MessagesAuth::Bearer,
                "ARANY_PROVIDER_GLM_STANDARD_KEY",
            ),
            (
                "glm-flash",
                "https://api.z.ai/api/anthropic",
                "https://api.z.ai/api/anthropic/v1/messages",
                "glm-5.3-flash",
                MessagesAuth::Bearer,
                "ARANY_PROVIDER_GLM_STANDARD_KEY",
            ),
        ] {
            let draft = CustomProfileDraft {
                name: name.into(),
                auth: Some(auth),
                credential_env: credential.into(),
                ..messages_draft(base).with_model(model)
            };
            std::fs::write(
                state.path().join("provider-profiles.json"),
                document(std::slice::from_ref(&draft)).to_string(),
            )
            .unwrap();
            let parsed = CustomProfile::load_named(&state, name).unwrap();
            assert_eq!(parsed.endpoint(), endpoint);
            assert_eq!(parsed.model(), model);
            assert_eq!(parsed.credential_env(), credential);
            assert!(parsed.efforts().is_empty());
            assert_eq!(parsed.auth(), auth);
            assert_eq!(parsed.draft().url, base.trim_end_matches('/'));
            routes.push(draft);
        }
        assert!(parse(&document(&routes)).is_ok());
        for index in 1..4 {
            let mut conflicting = routes.clone();
            conflicting[index].credential_env = "ARANY_PROVIDER_KIMI_API_KEY".into();
            assert!(parse(&document(&conflicting)).is_err());
        }
        std::fs::write(
            state.path().join("provider-profiles.json"),
            valid.to_string(),
        )
        .unwrap();
        for base in [
            "https://example.com",
            "https://example.com/",
            "http://127.0.0.1:9321",
            "https://[::1]/anthropic",
        ] {
            let profile = parse(&document(&[messages_draft(base)])).unwrap().remove(0);
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
                parse(&document(&[messages_draft(&base)])).is_err(),
                "invalid base {base}"
            );
        }
        let boundary = format!("https://example.com/{}", "a".repeat(243));
        assert!(parse(&document(&[messages_draft(&boundary)])).is_ok());
        let host = format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(56)
        );
        let exact = format!("https://{host}/{}", "a".repeat(243));
        let exact_profile = parse(&document(&[messages_draft(&exact)]))
            .unwrap()
            .remove(0);
        assert_eq!(exact_profile.endpoint().len(), 512);
        assert!(
            parse(&document(&[messages_draft(
                &exact.replace(&host, &format!("{host}a"))
            )]))
            .is_err()
        );

        let messages = document(&[messages_draft("https://example.com/anthropic")]);
        let entry_cases: [(&str, Value); 8] = [
            ("messages without auth", json!({"auth": null})),
            ("ambient auth", json!({"auth": "ambient"})),
            ("extra header field", json!({"headers": {}})),
            (
                "removed outcome encoding",
                json!({"outcome_encoding": "native_tools"}),
            ),
            ("removed privacy", json!({"privacy": "user_authorized"})),
            ("removed output cap", json!({"max_output_tokens": 4096})),
            (
                "responses with auth",
                json!({"protocol": "openai-responses", "url": "https://example.com/v1/responses", "auth": "bearer"}),
            ),
            (
                "chat with auth",
                json!({"protocol": "openai-chat-completions", "auth": "bearer"}),
            ),
        ];
        for (case, change) in entry_cases {
            let mut invalid = messages.clone();
            for (field, value) in change.as_object().unwrap() {
                if value.is_null() {
                    invalid["profiles"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove(field);
                } else {
                    invalid["profiles"][0][field] = value.clone();
                }
            }
            assert!(parse(&invalid).is_err(), "{case}");
        }
        let duplicate_key = messages.to_string().replace(
            "\"auth\":\"x-api-key\"",
            "\"auth\":\"x-api-key\",\"auth\":\"bearer\"",
        );
        assert!(CustomProfile::parse_document(duplicate_key.as_bytes()).is_err());
        let mut mixed = document(&[
            local_draft("model-1"),
            CustomProfileDraft {
                name: "messages".into(),
                ..messages_draft("https://example.com/anthropic")
            },
        ]);
        assert!(parse(&mixed).is_err());
        mixed["profiles"][1]["credential_env"] = "ARANY_PROVIDER_OTHER_KEY".into();
        assert!(parse(&mixed).is_ok());

        let invalid_cases: [(&str, &str, Value); 8] = [
            ("legacy version", "", json!({"version": 3})),
            ("unknown field", "", json!({"unexpected": true})),
            ("credential", "credential_env", json!("OPENAI_API_KEY")),
            (
                "metadata address",
                "url",
                json!("https://169.254.169.254/v1/responses"),
            ),
            (
                "plaintext domain",
                "url",
                json!("http://example.com/v1/responses"),
            ),
            (
                "query",
                "url",
                json!("https://example.com/v1/responses?x=1"),
            ),
            (
                "noncanonical host",
                "url",
                json!("https://EXAMPLE.com/v1/responses"),
            ),
            ("invalid name", "name", json!("OpenAI")),
        ];
        for (case, field, value) in invalid_cases {
            let mut invalid = valid.clone();
            if field.is_empty() {
                let (key, value) = value.as_object().unwrap().iter().next().unwrap();
                invalid[key] = value.clone();
            } else {
                invalid["profiles"][0][field] = value;
            }
            std::fs::write(
                state.path().join("provider-profiles.json"),
                invalid.to_string(),
            )
            .expect("replace profile file");
            assert!(
                matches!(
                    CustomProfile::load_named(&state, "local"),
                    Err(CustomProfileError::InvalidConfiguration)
                ),
                "{case}"
            );
            assert!(
                !(PROVIDER_PROFILES.accepts)(invalid.to_string().as_bytes()),
                "{case}"
            );
        }

        let mut efforts_document = valid.clone();
        for (efforts, valid) in [
            (json!(["low", "high"]), true),
            (json!([]), true),
            (json!(["high", "low"]), false),
            (json!(["low", "low"]), false),
            (json!(["minimal"]), false),
        ] {
            efforts_document["profiles"][0]["efforts"] = efforts;
            std::fs::write(
                state.path().join("provider-profiles.json"),
                efforts_document.to_string(),
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
            }
        }

        let mut drafts = vec![local_draft("model-1"), local_draft("model-1")];
        let write = |drafts: &[CustomProfileDraft]| {
            std::fs::write(
                state.path().join("provider-profiles.json"),
                document(drafts).to_string(),
            )
            .unwrap();
        };
        write(&drafts);
        assert!(matches!(
            CustomProfile::load_named(&state, "local"),
            Err(CustomProfileError::InvalidConfiguration)
        ));

        drafts[1].name = "other".into();
        drafts[1].url = "https://example.com/v1/responses".into();
        write(&drafts);
        assert!(matches!(
            CustomProfile::load_named(&state, "local"),
            Err(CustomProfileError::InvalidConfiguration)
        ));

        drafts[1].credential_env = "ARANY_PROVIDER_OTHER_KEY".into();
        write(&drafts);
        assert_eq!(
            CustomProfile::load_named(&state, "other")
                .unwrap()
                .endpoint(),
            "https://example.com/v1/responses"
        );

        let template = drafts[1].clone();
        for index in 2..=MAX_PROFILES {
            drafts.push(CustomProfileDraft {
                name: format!("profile-{index}"),
                ..template.clone()
            });
        }
        write(&drafts);
        assert!(matches!(
            CustomProfile::load_named(&state, "local"),
            Err(CustomProfileError::InvalidConfiguration)
        ));
    }

    #[tokio::test]
    async fn custom_profile_writer_accepts_a_bounded_typed_draft() {
        let temp = tempfile::tempdir().expect("private test root");
        let state = StateRoot::admit(&temp.path().join("state")).expect("private state root");
        let written = CustomProfile::write_profiles(&state, None, &[local_draft("model-1")])
            .expect("write validated custom profile");
        assert_eq!(
            CustomProfile::parse_document(&written).expect("written profiles parse")[0].name(),
            "local"
        );
    }

    #[test]
    fn custom_draft_validation_rejects_non_ascii_models_before_publication() {
        assert!(CustomProfile::validate_drafts(&[local_draft("model-é")]).is_err());
    }

    #[tokio::test]
    async fn custom_profile_stale_editor_cannot_overwrite_newer_snapshot() {
        let temp = tempfile::tempdir().expect("private test root");
        let state = StateRoot::admit(&temp.path().join("state")).expect("private state root");
        let original = CustomProfile::write_profiles(&state, None, &[local_draft("model-1")])
            .expect("write initial profile");
        let newer =
            CustomProfile::write_profiles(&state, Some(&original), &[local_draft("model-2")])
                .expect("write concurrent editor");
        assert!(matches!(
            CustomProfile::write_profiles(&state, Some(&original), &[local_draft("model-3")]),
            Err(CustomProfileError::Changed)
        ));
        assert_eq!(CustomProfile::snapshot(&state).unwrap(), Some(newer));
    }

    #[tokio::test]
    async fn custom_profile_directory_sync_failure_is_reported_uncertain() {
        let temp = tempfile::tempdir().expect("private test root");
        let state = StateRoot::admit(&temp.path().join("state")).expect("private state root");
        let original = CustomProfile::write_profiles(&state, None, &[local_draft("model-1")])
            .expect("write original profile");
        crate::fault::FaultPlan::fail_record_sync(
            state.path().to_path_buf(),
            "provider-profiles.json",
        );
        assert!(matches!(
            CustomProfile::write_profiles(&state, Some(&original), &[local_draft("model-2")]),
            Err(CustomProfileError::PublicationUncertain)
        ));
        let current = state
            .record(RecordKind::ProviderProfiles)
            .read()
            .expect("read profile")
            .expect("published profile remains inspectable");
        assert!(String::from_utf8_lossy(&current).contains("model-2"));
    }

    #[test]
    fn profile_file_rejects_links_public_modes_and_oversize() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let temp = tempfile::tempdir().expect("private test root");
        let valid = document(&[local_draft("model-1")]).to_string();
        let state = state_with_file(temp.path(), valid.as_bytes());
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

    #[test]
    fn chat_completions_profile_derives_endpoint_and_rejects_noncanonical_bases() {
        let chat = |url: &str, auth| CustomProfileDraft {
            protocol: CustomProtocol::OpenaiChatCompletions,
            url: url.into(),
            auth,
            ..local_draft("model-1")
        };
        let parsed = parse(&document(&[chat("http://127.0.0.1:9321/v1", None)]))
            .unwrap()
            .remove(0);
        assert_eq!(
            parsed.endpoint(),
            "http://127.0.0.1:9321/v1/chat/completions"
        );
        assert_eq!(parsed.draft().url, "http://127.0.0.1:9321/v1");
        let serialized = encode_profiles(&[parsed.draft()]).unwrap();
        assert_eq!(
            CustomProfile::parse_document(&serialized).unwrap(),
            [parsed]
        );
        for base in [
            "http://127.0.0.1:9321/v1/chat/completions",
            "http://127.0.0.1:9321/v1?key=value",
        ] {
            assert!(parse(&document(&[chat(base, None)])).is_err(), "{base}");
        }
        for auth in [MessagesAuth::Bearer, MessagesAuth::ApiKey] {
            assert!(parse(&document(&[chat("http://127.0.0.1:9321/v1", Some(auth))])).is_err());
        }
    }
}

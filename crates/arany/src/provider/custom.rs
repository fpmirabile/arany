use crate::provider::{Effort, valid_profile_name as valid_name};
use crate::store::{RecordFormat, RecordKind, StateRoot, StoreError};
use reqwest::Url;
use serde::Deserialize;
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
pub const MAX_CUSTOM_PROFILE_MODEL_BYTES: usize = 128;
pub const MAX_CUSTOM_PROFILE_CREDENTIAL_ENV_BYTES: usize = 64;
pub const MAX_CUSTOM_PROFILE_EFFORTS_BYTES: usize = 64;
const CUSTOM_OUTPUT_TOKEN_CAP: u32 = crate::provider::DEFAULT_OUTPUT_TOKEN_CAP;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustomProfile {
    name: String,
    endpoint: String,
    protocol: Protocol,
    model: String,
    credential_env: String,
    efforts: Vec<Effort>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CustomProtocol {
    OpenaiResponses,
    AnthropicMessages,
    OpenaiChatCompletions,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustomProfileDraft {
    pub name: String,
    pub protocol: CustomProtocol,
    pub endpoint: String,
    pub model: String,
    pub credential_env: String,
    pub messages_auth: crate::provider::custom::MessagesAuth,
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
        efforts: Vec<Effort>,
    },
    #[serde(rename = "openai-chat-completions")]
    OpenaiChatCompletions {
        base_url: String,
        auth: MessagesAuth,
        name: String,
        model: String,
        credential_env: String,
        outcome_encoding: OutcomeEncoding,
        privacy: Privacy,
        max_output_tokens: u32,
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
    OpenaiChatCompletions {
        base_url: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub enum MessagesAuth {
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
    current: 3,
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
            .find(|profile| profile.name == name)
            .ok_or(CustomProfileError::NotFound)
    }

    fn parse_document(bytes: &[u8]) -> Result<Vec<Self>, CustomProfileError> {
        let document: ProfileDocument = serde_json::from_value(
            crate::tools::parse_json_bounded(bytes, 64 * 1024, 65_536, 32)
                .map_err(|_| CustomProfileError::InvalidConfiguration)?,
        )
        .map_err(|_| CustomProfileError::InvalidConfiguration)?;
        if u64::from(document.version) != PROVIDER_PROFILES.current
            || document.profiles.len() > MAX_PROFILES
        {
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
                        efforts,
                    },
                    max_output_tokens,
                    outcome_encoding,
                    OutcomeEncoding::NativeTools,
                    privacy,
                )
            }
            ProfileEntry::OpenaiChatCompletions {
                name,
                base_url,
                auth,
                model,
                credential_env,
                outcome_encoding,
                privacy,
                max_output_tokens,
                efforts,
            } => {
                if auth != MessagesAuth::Bearer {
                    return Err(CustomProfileError::InvalidConfiguration);
                }
                let (base_url, endpoint) = chat_completions_endpoint(&base_url)
                    .ok_or(CustomProfileError::InvalidConfiguration)?;
                (
                    Self {
                        name,
                        endpoint,
                        protocol: Protocol::OpenaiChatCompletions { base_url },
                        model,
                        credential_env,
                        efforts,
                    },
                    max_output_tokens,
                    outcome_encoding,
                    OutcomeEncoding::JsonSchema,
                    privacy,
                )
            }
        };
        if !valid_name(&profile.name)
            || matches!(profile.name.as_str(), "openai" | "anthropic")
            || profile.model.is_empty()
            || profile.model.len() > MAX_CUSTOM_PROFILE_MODEL_BYTES
            || !profile.model.bytes().all(|byte| byte.is_ascii_graphic())
            || cap != CUSTOM_OUTPUT_TOKEN_CAP
            || encoding != expected
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

    pub fn draft(&self) -> CustomProfileDraft {
        let (protocol, endpoint, messages_auth) = match &self.protocol {
            Protocol::OpenaiResponses => (
                CustomProtocol::OpenaiResponses,
                self.endpoint.clone(),
                MessagesAuth::ApiKey,
            ),
            Protocol::AnthropicMessages { base_url, auth } => {
                (CustomProtocol::AnthropicMessages, base_url.clone(), *auth)
            }
            Protocol::OpenaiChatCompletions { base_url } => (
                CustomProtocol::OpenaiChatCompletions,
                base_url.clone(),
                MessagesAuth::Bearer,
            ),
        };
        CustomProfileDraft {
            name: self.name.clone(),
            protocol,
            endpoint,
            model: self.model.clone(),
            credential_env: self.credential_env.clone(),
            messages_auth,
            efforts: self.efforts.clone(),
        }
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
        CUSTOM_OUTPUT_TOKEN_CAP
    }

    pub fn efforts(&self) -> &[Effort] {
        &self.efforts
    }

    pub fn admits_effort(&self, effort: Option<Effort>) -> bool {
        effort.is_none_or(|value| self.efforts.contains(&value))
    }

    // Binds every declared field, so Run provenance names the exact admitted declaration.
    fn digest(&self) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let (protocol, base, auth) = match &self.protocol {
            Protocol::OpenaiResponses => ("openai-responses", "", "bearer"),
            Protocol::AnthropicMessages { base_url, auth } => (
                "anthropic-messages",
                base_url.as_str(),
                match auth {
                    MessagesAuth::Bearer => "bearer",
                    MessagesAuth::ApiKey => "x-api-key",
                },
            ),
            Protocol::OpenaiChatCompletions { base_url } => {
                ("openai-chat-completions", base_url.as_str(), "bearer")
            }
        };
        let mut digest = Sha256::new();
        for field in [
            self.name.as_str(),
            &self.endpoint,
            &self.model,
            &self.credential_env,
            protocol,
            base,
            auth,
        ]
        .into_iter()
        .chain(self.efforts.iter().map(|effort| effort.as_str()))
        {
            digest.update((field.len() as u64).to_be_bytes());
            digest.update(field.as_bytes());
        }
        digest.finalize().into()
    }
}

fn encode_profiles(profiles: &[CustomProfileDraft]) -> Result<Vec<u8>, CustomProfileError> {
    if profiles.len() > MAX_PROFILES
        || profiles.iter().any(|profile| {
            !valid_name(&profile.name)
                || profile.endpoint.len() > MAX_CUSTOM_PROFILE_ENDPOINT_BYTES
                || profile.model.len() > MAX_CUSTOM_PROFILE_MODEL_BYTES
                || profile.credential_env.len() > MAX_CUSTOM_PROFILE_CREDENTIAL_ENV_BYTES
                || profile.efforts.len() > Effort::ALL.len()
        })
    {
        return Err(CustomProfileError::InvalidConfiguration);
    }
    if profiles.is_empty() {
        return Ok(Vec::new());
    }
    let entries = profiles
        .iter()
        .map(CustomProfileDraft::to_value)
        .collect::<Vec<_>>();
    let bytes = serde_json::to_vec(
        &serde_json::json!({"version": PROVIDER_PROFILES.current, "profiles": entries}),
    )
    .map_err(|_| CustomProfileError::InvalidConfiguration)?;
    CustomProfile::parse_document(&bytes)?;
    Ok(bytes)
}

impl CustomProfileDraft {
    fn to_value(&self) -> serde_json::Value {
        let common = serde_json::json!({
            "name": self.name,
            "model": self.model,
            "credential_env": self.credential_env,
            "privacy": "user_authorized",
            "max_output_tokens": CUSTOM_OUTPUT_TOKEN_CAP,
            "efforts": self.efforts,
        });
        match self.protocol {
            CustomProtocol::OpenaiResponses => {
                let mut entry = common.as_object().cloned().unwrap_or_default();
                entry.insert("protocol".into(), "openai-responses".into());
                entry.insert("endpoint".into(), self.endpoint.clone().into());
                entry.insert("outcome_encoding".into(), "json_schema".into());
                serde_json::Value::Object(entry)
            }
            CustomProtocol::AnthropicMessages => {
                let mut entry = common.as_object().cloned().unwrap_or_default();
                entry.insert("protocol".into(), "anthropic-messages".into());
                entry.insert("base_url".into(), self.endpoint.clone().into());
                entry.insert(
                    "auth".into(),
                    match self.messages_auth {
                        MessagesAuth::Bearer => "bearer",
                        MessagesAuth::ApiKey => "x-api-key",
                    }
                    .into(),
                );
                entry.insert("outcome_encoding".into(), "native_tools".into());
                serde_json::Value::Object(entry)
            }
            CustomProtocol::OpenaiChatCompletions => {
                let mut entry = common.as_object().cloned().unwrap_or_default();
                entry.insert("protocol".into(), "openai-chat-completions".into());
                entry.insert("base_url".into(), self.endpoint.clone().into());
                entry.insert("auth".into(), "bearer".into());
                entry.insert("outcome_encoding".into(), "json_schema".into());
                serde_json::Value::Object(entry)
            }
        }
    }
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
    use std::path::Path;

    const VALID_PROFILES: &str = r#"[{"name":"local","protocol":"openai-responses","endpoint":"http://127.0.0.1:9321/v1/responses","model":"model-1","credential_env":"ARANY_PROVIDER_LOCAL_KEY","outcome_encoding":"json_schema","privacy":"user_authorized","max_output_tokens":4096,"efforts":[]}]"#;

    fn valid() -> String {
        format!(
            r#"{{"version":{},"profiles":{VALID_PROFILES}}}"#,
            PROVIDER_PROFILES.current
        )
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
        let state = state_with_file(temp.path(), valid().as_bytes());
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

        let messages = valid()
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
        let mut routes = serde_json::json!({"version":PROVIDER_PROFILES.current,"profiles":[]});
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
        std::fs::write(state.path().join("provider-profiles.json"), valid()).unwrap();
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
            valid().replace(
                "\"model\":",
                "\"base_url\":\"https://example.com\",\"model\":",
            ),
            valid().replace("\"model\":", "\"auth\":\"bearer\",\"model\":"),
        ] {
            assert!(CustomProfile::parse_document(invalid.as_bytes()).is_err());
        }
        let mut mixed: serde_json::Value = serde_json::from_str(&valid()).unwrap();
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
            ("version", "\"version\":0"),
            ("protocol", "\"protocol\":\"openai-chat-completions\""),
            ("encoding", "\"outcome_encoding\":\"json_mode\""),
            ("privacy", "\"privacy\":\"provider_zdr\""),
            ("cap", "\"max_output_tokens\":8192"),
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
            ("unknown field", "\"unexpected\":true,\"profiles\":"),
            ("invalid name", "\"name\":\"OpenAI\""),
        ];
        let current = format!("\"version\":{}", PROVIDER_PROFILES.current);
        for (case, replacement) in replacements {
            let original = match case {
                "version" => current.as_str(),
                "unknown field" => "\"profiles\":",
                "protocol" => "\"protocol\":\"openai-responses\"",
                "encoding" => "\"outcome_encoding\":\"json_schema\"",
                "privacy" => "\"privacy\":\"user_authorized\"",
                "cap" => "\"max_output_tokens\":4096",
                "credential" => "\"credential_env\":\"ARANY_PROVIDER_LOCAL_KEY\"",
                "metadata address" | "plaintext domain" | "query" | "noncanonical host" => {
                    "\"endpoint\":\"http://127.0.0.1:9321/v1/responses\""
                }
                "invalid name" => "\"name\":\"local\"",
                _ => unreachable!(),
            };
            let invalid = valid().replace(original, replacement);
            assert_ne!(invalid, valid(), "{case} fixture must change");
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

        let mut versioned: serde_json::Value = serde_json::from_str(&valid()).unwrap();
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
            }
        }

        let mut document: serde_json::Value = serde_json::from_str(&valid()).expect("valid JSON");
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

    #[tokio::test]
    async fn custom_profile_writer_accepts_a_bounded_typed_draft() {
        let temp = tempfile::tempdir().expect("private test root");
        let state = StateRoot::admit(&temp.path().join("state")).expect("private state root");
        let draft = CustomProfileDraft {
            name: "local".into(),
            protocol: CustomProtocol::OpenaiResponses,
            endpoint: "http://127.0.0.1:9321/v1/responses".into(),
            model: "model-1".into(),
            credential_env: "ARANY_PROVIDER_LOCAL_KEY".into(),
            messages_auth: MessagesAuth::ApiKey,
            efforts: Vec::new(),
        };
        let written = CustomProfile::write_profiles(&state, None, &[draft])
            .expect("write validated custom profile");
        assert_eq!(
            CustomProfile::parse_document(&written).expect("written profiles parse")[0].name(),
            "local"
        );
    }

    fn local_draft(model: &str) -> CustomProfileDraft {
        CustomProfileDraft {
            name: "local".into(),
            protocol: CustomProtocol::OpenaiResponses,
            endpoint: "http://127.0.0.1:9321/v1/responses".into(),
            model: model.into(),
            credential_env: "ARANY_PROVIDER_LOCAL_KEY".into(),
            messages_auth: MessagesAuth::ApiKey,
            efforts: Vec::new(),
        }
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
        let state = state_with_file(temp.path(), valid().as_bytes());
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
        let profile = valid()
            .replace("openai-responses", "openai-chat-completions")
            .replace(
                "\"endpoint\":\"http://127.0.0.1:9321/v1/responses\"",
                "\"base_url\":\"http://127.0.0.1:9321/v1\",\"auth\":\"bearer\"",
            );
        let parsed = CustomProfile::parse_document(profile.as_bytes())
            .unwrap()
            .remove(0);
        assert_eq!(
            parsed.endpoint(),
            "http://127.0.0.1:9321/v1/chat/completions"
        );
        assert!(matches!(
            &parsed.protocol,
            Protocol::OpenaiChatCompletions { base_url }
                if base_url == "http://127.0.0.1:9321/v1"
        ));
        let draft = parsed.draft();
        let serialized = encode_profiles(std::slice::from_ref(&draft)).unwrap();
        assert_eq!(
            CustomProfile::parse_document(&serialized).unwrap(),
            [parsed]
        );
        for base in [
            "http://127.0.0.1:9321/v1/chat/completions",
            "http://127.0.0.1:9321/v1?key=value",
        ] {
            let invalid = profile.replace("http://127.0.0.1:9321/v1\"", &format!("{base}\""));
            assert!(
                CustomProfile::parse_document(invalid.as_bytes()).is_err(),
                "{base}"
            );
        }
        assert!(
            CustomProfile::parse_document(profile.replace(",\"auth\":\"bearer\"", "").as_bytes())
                .is_err()
        );
        assert!(
            CustomProfile::parse_document(
                profile
                    .replace("\"auth\":\"bearer\"", "\"auth\":\"x-api-key\"")
                    .as_bytes()
            )
            .is_err()
        );
    }
}

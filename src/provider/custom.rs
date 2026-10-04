use crate::provider::Effort;
use crate::store::{StateRoot, StoreError};
use reqwest::Url;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

mod adapter;
mod check;
mod destination;
mod transport;
pub use adapter::CustomProvider;
pub use check::{CustomProfileCheck, check_custom_profile};
use destination::PinnedDestination;

const MAX_PROFILES: usize = 32;
const OUTPUT_CAP: u32 = 4096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CustomProfile {
    name: String,
    endpoint: String,
    model: String,
    credential_env: String,
    capability_evidence_version: u32,
    efforts: Vec<Effort>,
}

#[derive(Debug, thiserror::Error)]
pub enum CustomProfileError {
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
#[serde(deny_unknown_fields)]
struct ProfileEntry {
    name: String,
    protocol: Protocol,
    endpoint: String,
    model: String,
    credential_env: String,
    outcome_encoding: OutcomeEncoding,
    privacy: Privacy,
    max_output_tokens: u32,
    capability_evidence_version: u32,
    #[serde(default)]
    efforts: Option<Vec<Effort>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Protocol {
    OpenaiResponses,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum OutcomeEncoding {
    JsonSchema,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Privacy {
    UserAuthorized,
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
            .read_provider_profiles()
            .map_err(|error| match error {
                StoreError::StateNotPrivate => CustomProfileError::UnsafeFile,
                _ => CustomProfileError::Unavailable,
            })?;
        let document: ProfileDocument =
            serde_json::from_slice(&bytes).map_err(|_| CustomProfileError::InvalidConfiguration)?;
        if document.version != 1 || document.profiles.len() > MAX_PROFILES {
            return Err(CustomProfileError::InvalidConfiguration);
        }
        let mut names = HashSet::new();
        let mut credentials = HashMap::new();
        let mut selected = None;
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
            if profile.name == name {
                selected = Some(profile);
            }
        }
        selected.ok_or(CustomProfileError::NotFound)
    }

    fn validate(entry: ProfileEntry) -> Result<Self, CustomProfileError> {
        let _ = (entry.protocol, entry.outcome_encoding, entry.privacy);
        if !valid_name(&entry.name)
            || matches!(entry.name.as_str(), "openai" | "anthropic")
            || entry.model.is_empty()
            || entry.model.len() > 128
            || !entry.model.bytes().all(|byte| byte.is_ascii_graphic())
            || entry.max_output_tokens != OUTPUT_CAP
            || !matches!(entry.capability_evidence_version, 1 | 2)
            || entry.capability_evidence_version == 1 && entry.efforts.is_some()
            || entry.capability_evidence_version == 2
                && entry.efforts.as_ref().is_none_or(Vec::is_empty)
            || entry
                .efforts
                .as_ref()
                .is_some_and(|values| values.len() > Effort::ALL.len())
            || entry.efforts.as_ref().is_some_and(|values| {
                values
                    .windows(2)
                    .any(|pair| effort_index(pair[0]) >= effort_index(pair[1]))
            })
            || !valid_credential_env(&entry.credential_env)
            || !valid_endpoint(&entry.endpoint)
        {
            return Err(CustomProfileError::InvalidConfiguration);
        }
        Ok(Self {
            name: entry.name,
            endpoint: entry.endpoint,
            model: entry.model,
            credential_env: entry.credential_env,
            capability_evidence_version: entry.capability_evidence_version,
            efforts: entry.efforts.unwrap_or_default(),
        })
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
        .unwrap()
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
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
    if endpoint.is_empty() || endpoint.len() > 512 {
        return false;
    }
    let Ok(url) = Url::parse(endpoint) else {
        return false;
    };
    if url.as_str() != endpoint
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().ends_with("/responses")
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

    const VALID: &str = r#"{"version":1,"profiles":[{"name":"local","protocol":"openai-responses","endpoint":"http://127.0.0.1:9321/v1/responses","model":"model-1","credential_env":"ARANY_PROVIDER_LOCAL_KEY","outcome_encoding":"json_schema","privacy":"user_authorized","max_output_tokens":4096,"capability_evidence_version":1}]}"#;

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

        let replacements = [
            ("version", "\"version\":2"),
            ("protocol", "\"protocol\":\"openai-chat-completions\""),
            ("encoding", "\"outcome_encoding\":\"json_mode\""),
            ("privacy", "\"privacy\":\"provider_zdr\""),
            ("cap", "\"max_output_tokens\":8192"),
            ("evidence", "\"capability_evidence_version\":2"),
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
            ("unknown field", "\"version\":1,\"unexpected\":true"),
            ("invalid name", "\"name\":\"OpenAI\""),
        ];
        for (case, replacement) in replacements {
            let original = match case {
                "version" | "unknown field" => "\"version\":1",
                "protocol" => "\"protocol\":\"openai-responses\"",
                "encoding" => "\"outcome_encoding\":\"json_schema\"",
                "privacy" => "\"privacy\":\"user_authorized\"",
                "cap" => "\"max_output_tokens\":4096",
                "evidence" => "\"capability_evidence_version\":1",
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
        versioned["profiles"][0]["capability_evidence_version"] = serde_json::json!(2);
        for (efforts, valid) in [
            (serde_json::json!(["low", "high"]), true),
            (serde_json::json!([]), false),
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
                assert!(profile.admits_effort(Some(Effort::Low)));
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

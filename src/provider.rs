use crate::session::{AgentRunId, CollaborationPolicy, RunId, SessionId};
use serde::{Deserialize, Serialize};
use std::future::Future;
use uuid::{Uuid, Version};

mod anthropic;
mod catalog;
mod custom;
mod effort;
mod image;
#[cfg(test)]
pub(crate) use image::test_image;
pub(crate) use image::{MAX_IMAGE_METADATA_BYTES, deserialize_images, valid_image_collection};
mod native_check;
mod openai;
pub use anthropic::AnthropicProvider;
pub use catalog::{
    ModelEntry, list_native_models, list_native_models_with_api_key,
    list_native_models_with_credentials,
};
pub use custom::{
    CustomProfile, CustomProfileCheck, CustomProfileError, CustomProvider, check_custom_profile,
};
pub use effort::{Effort, resolve_native_effort, resolve_native_effort_for_run};
pub use image::{ImageAttachment, ImageOrigin, MAX_IMAGE_BYTES, MAX_MESSAGE_IMAGES, ProviderImage};
pub use native_check::{
    NativeCheckError, check_native_model_from_env, check_native_model_with_api_key,
    check_native_model_with_credentials,
};
pub use openai::{ChatGptProvider, OpenAiProvider, probe_chatgpt_model};

pub(crate) const MAX_REPORTED_INPUT_TOKENS: u32 = 1_000_000;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_NATIVE_API_KEY_BYTES: usize = 512;
pub const MAX_ANTHROPIC_WORKSPACE_ID_BYTES: usize = 128;

/// A selected native key and its explicit API workspace, never ambient account state.
pub struct NativeApiCredentials {
    profile: &'static str,
    key: String,
    anthropic_workspace_id: Option<String>,
}

impl NativeApiCredentials {
    pub fn new(
        profile: &str,
        key: String,
        anthropic_workspace_id: Option<String>,
    ) -> Result<Self, ProviderError> {
        let profile = match profile {
            "openai" if anthropic_workspace_id.is_none() => "openai",
            "anthropic" => "anthropic",
            _ => return Err(ProviderError::Rejected),
        };
        if anthropic_workspace_id.as_deref().is_some_and(|id| {
            id.len() > MAX_ANTHROPIC_WORKSPACE_ID_BYTES
                || !id.strip_prefix("wrkspc_").is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
                })
        }) {
            return Err(ProviderError::Rejected);
        }
        validate_native_api_key(&key)?;
        Ok(Self {
            profile,
            key,
            anthropic_workspace_id,
        })
    }

    pub fn from_env(profile: &str) -> Result<Self, ProviderError> {
        let variable = match profile {
            "openai" => "OPENAI_API_KEY",
            "anthropic" => "ANTHROPIC_API_KEY",
            _ => return Err(ProviderError::Rejected),
        };
        let workspace = if profile == "anthropic" {
            match std::env::var("ANTHROPIC_WORKSPACE_ID") {
                Ok(value) => Some(value),
                Err(std::env::VarError::NotPresent) => None,
                Err(_) => return Err(ProviderError::Rejected),
            }
        } else {
            None
        };
        let key = std::env::var(variable).map_err(|_| ProviderError::Unavailable)?;
        Self::new(profile, key, workspace)
    }

    pub fn api_key(&self) -> &str {
        &self.key
    }

    pub fn anthropic_workspace_id(&self) -> Option<&str> {
        self.anthropic_workspace_id.as_deref()
    }

    pub fn into_api_key(self) -> String {
        self.key
    }

    pub(super) fn require_profile(&self, profile: &str) -> Result<(), ProviderError> {
        if self.profile == profile {
            Ok(())
        } else {
            Err(ProviderError::Rejected)
        }
    }
}

pub fn validate_native_api_key(key: &str) -> Result<(), ProviderError> {
    if key.is_empty()
        || key.len() > MAX_NATIVE_API_KEY_BYTES
        || !key.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(ProviderError::Unavailable);
    }
    Ok(())
}

pub fn validate_native_model_id(model: &str) -> Result<(), ProviderError> {
    if catalog::valid_model_id(model) {
        Ok(())
    } else {
        Err(ProviderError::Rejected)
    }
}

pub(crate) fn valid_saved_api_account_id(profile: &str, id: Option<Uuid>) -> bool {
    id.is_none_or(|id| {
        matches!(profile, "openai" | "anthropic") && id.get_version() == Some(Version::SortRand)
    })
}

pub(crate) fn raw_reflects_secret(bytes: &[u8], secret: &str) -> bool {
    secret.is_empty()
        || bytes
            .windows(secret.len())
            .any(|window| window == secret.as_bytes())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPhase {
    RootPlan,
    ChildWork,
    RootSynthesis,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryTurn {
    pub user: String,
    pub assistant: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildResult {
    pub objective: String,
    pub summary: String,
    pub result: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderRequest {
    pub run_id: RunId,
    pub agent_run_id: AgentRunId,
    pub phase: AgentPhase,
    pub collaboration: CollaborationPolicy,
    pub model: String,
    pub instructions: Option<String>,
    pub objective: String,
    pub images: Vec<ProviderImage>,
    pub includes: Vec<String>,
    pub history: Vec<HistoryTurn>,
    /// Model-authored derived data; adapters must render it as data, never instructions.
    pub context_summary: Option<String>,
    pub child_results: Vec<ChildResult>,
    pub tools: Option<crate::tools::ToolContext>,
    pub max_output_tokens: u32,
}

impl ProviderRequest {
    pub(crate) fn validate_scope(&self) -> Result<(), ProviderError> {
        self.collaboration
            .validate()
            .map_err(|_| ProviderError::Rejected)?;
        if self.phase == AgentPhase::ChildWork
            && (self.collaboration != CollaborationPolicy::Single || self.tools.is_some())
        {
            return Err(ProviderError::Rejected);
        }
        Ok(())
    }
}

pub(crate) fn restrict_outcome_schema(
    mut schema: serde_json::Value,
    request: &ProviderRequest,
) -> serde_json::Value {
    let branches = schema["properties"]["outcome"]["anyOf"]
        .as_array_mut()
        .expect("compiled outcome branches");
    branches.retain(
        |branch| match branch["properties"]["type"]["enum"][0].as_str() {
            Some("finish") => {
                request.phase != AgentPhase::RootPlan
                    || !matches!(request.collaboration, CollaborationPolicy::Team { .. })
            }
            Some("delegate") => {
                request.phase == AgentPhase::RootPlan && request.collaboration.max_children() > 0
            }
            _ => false,
        },
    );
    if request.tools.is_some() {
        branches.push(crate::tools::types::outcome_branch());
    }
    schema
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionRequest {
    pub session_id: SessionId,
    pub covered_run_id: RunId,
    pub model: String,
    pub previous_summary: Option<String>,
    pub items: Vec<CompactionItem>,
    pub max_output_tokens: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnansweredStatus {
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompactionItem {
    Completed(HistoryTurn),
    Unanswered {
        user: String,
        status: UnansweredStatus,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionResponse {
    pub summary: String,
    pub response_id: Option<String>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub wire_provenance: Option<ProviderWireProvenance>,
}

impl CompactionResponse {
    pub(crate) fn reflects_secret(&self, secret: &str) -> bool {
        secret.is_empty()
            || self.summary.contains(secret)
            || self
                .response_id
                .as_ref()
                .is_some_and(|id| id.contains(secret))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Finish {
    pub summary: String,
    pub result: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Delegate {
    pub children: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderOutcome {
    Finish(Finish),
    Delegate(Delegate),
    Tool(crate::tools::ToolCall),
}

/// Accepted wire condition and local request control, not a remote retention guarantee.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderWireProvenance {
    ResponsesCompletedStoreFalseRequested,
    MessagesEndTurnStorageUnspecified,
}

impl ProviderWireProvenance {
    pub(crate) fn valid_for(
        self,
        response_id: Option<&str>,
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
    ) -> bool {
        response_id.is_some()
            && (self != Self::MessagesEndTurnStorageUnspecified
                || input_tokens.is_some() && output_tokens.is_some())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderResponse {
    pub outcome: ProviderOutcome,
    pub response_id: Option<String>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub wire_provenance: Option<ProviderWireProvenance>,
}

impl ProviderResponse {
    pub(crate) fn reflects_secret(&self, secret: &str) -> bool {
        secret.is_empty()
            || self
                .response_id
                .as_ref()
                .is_some_and(|id| id.contains(secret))
            || match &self.outcome {
                ProviderOutcome::Finish(value) => {
                    value.summary.contains(secret) || value.result.contains(secret)
                }
                ProviderOutcome::Delegate(value) => {
                    value.children.iter().any(|child| child.contains(secret))
                }
                ProviderOutcome::Tool(call) => {
                    serde_json::to_value(call)
                        .is_ok_and(|value| tool_reflects_secret(&value, secret))
                        || if let crate::tools::ToolCall::McpCall { arguments, .. } = call {
                            serde_json::from_str(arguments)
                                .is_ok_and(|value| tool_reflects_secret(&value, secret))
                        } else {
                            false
                        }
                }
            }
    }
}

fn tool_reflects_secret(value: &serde_json::Value, secret: &str) -> bool {
    match value {
        serde_json::Value::String(value) => value.contains(secret),
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| tool_reflects_secret(value, secret)),
        serde_json::Value::Object(values) => values
            .iter()
            .any(|(key, value)| key.contains(secret) || tool_reflects_secret(value, secret)),
        _ => false,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("Provider unavailable")]
    Unavailable,
    #[error("Provider rejected the request")]
    Rejected,
    #[error("Provider returned an invalid outcome")]
    InvalidOutcome,
    #[error("Provider stream was incomplete or malformed")]
    InvalidStream,
    #[error("Provider response did not match the selected response contract")]
    InvalidResponseContract,
    #[error("Provider output did not match Arany's structured outcome contract")]
    InvalidOutcomeContract,
    #[error("Provider reported output above Arany's local token limit")]
    LocalOutputLimit,
    #[error("Provider returned HTTP {0}")]
    RemoteHttp(u16),
    #[error("Provider stream failed: {0}")]
    RemoteStreamCode(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderFailureClass {
    Unavailable,
    Rejected,
    InvalidOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFailureReason {
    AccountAccess,
    UsageLimit,
    UsageTemporarilyUnavailable,
    ServiceUnavailable,
    StreamProtocol,
    ResponseContract,
    OutcomeContract,
    LocalOutputLimit,
}

impl ProviderError {
    pub(crate) fn failure_reason(&self) -> Option<ProviderFailureReason> {
        match self {
            Self::InvalidStream => Some(ProviderFailureReason::StreamProtocol),
            Self::InvalidResponseContract => Some(ProviderFailureReason::ResponseContract),
            Self::InvalidOutcomeContract => Some(ProviderFailureReason::OutcomeContract),
            Self::LocalOutputLimit => Some(ProviderFailureReason::LocalOutputLimit),
            Self::RemoteHttp(401 | 403) => Some(ProviderFailureReason::AccountAccess),
            Self::RemoteHttp(429) => Some(ProviderFailureReason::UsageLimit),
            Self::RemoteHttp(500..=599) => Some(ProviderFailureReason::ServiceUnavailable),
            Self::RemoteStreamCode(code)
                if matches!(
                    code.as_str(),
                    "subscription_sharing_usage_unavailable"
                        | "subscription_sharing_user_unavailable"
                ) =>
            {
                Some(ProviderFailureReason::UsageTemporarilyUnavailable)
            }
            _ => None,
        }
    }

    pub(crate) fn failure_class(&self) -> ProviderFailureClass {
        match self {
            Self::Unavailable | Self::RemoteHttp(500..=599) => ProviderFailureClass::Unavailable,
            Self::RemoteStreamCode(_)
                if self.failure_reason()
                    == Some(ProviderFailureReason::UsageTemporarilyUnavailable) =>
            {
                ProviderFailureClass::Unavailable
            }
            Self::Rejected | Self::RemoteHttp(400..=499) | Self::RemoteStreamCode(_) => {
                ProviderFailureClass::Rejected
            }
            Self::InvalidOutcome
            | Self::InvalidStream
            | Self::InvalidResponseContract
            | Self::InvalidOutcomeContract
            | Self::LocalOutputLimit
            | Self::RemoteHttp(_) => ProviderFailureClass::InvalidOutcome,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomProfileProvenance {
    pub endpoint: String,
    pub evidence_digest: [u8; 32],
    pub capability_evidence_version: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputTokenBound {
    #[default]
    ProviderEnforced,
    LocalAcceptanceOnly,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatGptProvenance {
    pub account_id: Uuid,
    pub evidence_fingerprint: [u8; 32],
    #[serde(default, skip_serializing_if = "ChatGptAdmission::is_conformance")]
    pub admission: ChatGptAdmission,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatGptAdmission {
    #[default]
    Conformance,
    AccountConsent,
}

impl ChatGptAdmission {
    fn is_conformance(&self) -> bool {
        *self == Self::Conformance
    }
}

impl ChatGptProvenance {
    pub(crate) fn valid(&self) -> bool {
        self.account_id.get_version() == Some(Version::SortRand)
            && self.evidence_fingerprint != [0; 32]
    }
}

impl CustomProfileProvenance {
    pub(crate) fn valid(&self) -> bool {
        !self.endpoint.is_empty()
            && self.endpoint.len() <= 512
            && !self.endpoint.chars().any(char::is_control)
            && matches!(self.capability_evidence_version, 1 | 2)
    }
}

/// One resolved inference adapter. Profile, model, effort, concurrency and provenance
/// declarations remain immutable while the Engine uses this value.
///
/// Requests and results carry Arany semantics, not wire or persistence objects.
/// The Engine owns admission, aggregate budgets, journal ordering and Tool authority;
/// adapters validate supported request scope before egress and bound transport/results.
/// Calls never retry automatically or switch destinations, models, encodings or billing.
/// Reported input must fit Arany's reporting ceiling; observed output fits the request cap.
/// Missing usage means unknown usage, not zero; an adapter may require reported usage.
///
/// Dropping a call future stops local waiting but cannot undo disclosure, generation
/// or remote usage. Implementations must not detach unowned effectful work.
pub trait Provider: Send + Sync {
    fn profile_name(&self) -> &str;

    fn model_name(&self) -> &str;

    fn reasoning_effort(&self) -> Option<Effort> {
        None
    }

    /// A declared ceiling enforced by the Engine, not internal call serialization.
    fn max_concurrent_calls(&self) -> u8;

    fn custom_profile_provenance(&self) -> Option<CustomProfileProvenance> {
        None
    }

    fn saved_api_account_id(&self) -> Option<Uuid> {
        None
    }

    fn chatgpt_provenance(&self) -> Option<ChatGptProvenance> {
        None
    }

    fn output_token_bound(&self) -> OutputTokenBound {
        OutputTokenBound::ProviderEnforced
    }

    /// Returns one complete semantic outcome or an error, never an accepted partial result.
    /// The Engine independently validates its phase, bounds and durable transition.
    fn invoke(
        &self,
        request: ProviderRequest,
    ) -> impl Future<Output = Result<ProviderResponse, ProviderError>> + Send;

    /// Summarizes accepted conversation data without Workspace guidance or effects.
    /// Unsupported compaction returns `Unavailable`. Adapters enforce the declared
    /// output bound; a local-only bound requires explicit account consent and cannot
    /// guarantee a remote generation cap. Errors never publish a partial summary.
    fn compact(
        &self,
        _request: CompactionRequest,
    ) -> impl Future<Output = Result<CompactionResponse, ProviderError>> + Send {
        async { Err(ProviderError::Unavailable) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_http_status_keeps_engine_failure_class() {
        for (status, expected, reason) in [
            (302, ProviderFailureClass::InvalidOutcome, None),
            (400, ProviderFailureClass::Rejected, None),
            (
                401,
                ProviderFailureClass::Rejected,
                Some(ProviderFailureReason::AccountAccess),
            ),
            (
                403,
                ProviderFailureClass::Rejected,
                Some(ProviderFailureReason::AccountAccess),
            ),
            (404, ProviderFailureClass::Rejected, None),
            (
                429,
                ProviderFailureClass::Rejected,
                Some(ProviderFailureReason::UsageLimit),
            ),
            (
                500,
                ProviderFailureClass::Unavailable,
                Some(ProviderFailureReason::ServiceUnavailable),
            ),
            (
                503,
                ProviderFailureClass::Unavailable,
                Some(ProviderFailureReason::ServiceUnavailable),
            ),
            (
                599,
                ProviderFailureClass::Unavailable,
                Some(ProviderFailureReason::ServiceUnavailable),
            ),
            (600, ProviderFailureClass::InvalidOutcome, None),
        ] {
            assert_eq!(ProviderError::RemoteHttp(status).failure_class(), expected);
            assert_eq!(ProviderError::RemoteHttp(status).failure_reason(), reason);
        }
        assert_eq!(
            ProviderError::RemoteStreamCode("subscription_sharing_usage_unavailable".into())
                .failure_class(),
            ProviderFailureClass::Unavailable
        );
        assert_eq!(
            ProviderError::RemoteStreamCode("subscription_sharing_usage_limit_exceeded".into())
                .failure_class(),
            ProviderFailureClass::Rejected
        );
        for code in [
            "subscription_sharing_usage_unavailable",
            "subscription_sharing_user_unavailable",
        ] {
            assert_eq!(
                ProviderError::RemoteStreamCode(code.into()).failure_reason(),
                Some(ProviderFailureReason::UsageTemporarilyUnavailable)
            );
        }
        for error in [
            ProviderError::Rejected,
            ProviderError::Unavailable,
            ProviderError::InvalidOutcome,
            ProviderError::RemoteStreamCode("unknown_code".into()),
            ProviderError::RemoteStreamCode("subscription_sharing_usage_unavailable\n".into()),
            ProviderError::RemoteStreamCode("subscription_sharing_usage_limit_exceeded".into()),
        ] {
            assert_eq!(error.failure_reason(), None);
        }
        for (error, reason) in [
            (
                ProviderError::InvalidStream,
                ProviderFailureReason::StreamProtocol,
            ),
            (
                ProviderError::InvalidResponseContract,
                ProviderFailureReason::ResponseContract,
            ),
            (
                ProviderError::InvalidOutcomeContract,
                ProviderFailureReason::OutcomeContract,
            ),
            (
                ProviderError::LocalOutputLimit,
                ProviderFailureReason::LocalOutputLimit,
            ),
        ] {
            assert_eq!(error.failure_class(), ProviderFailureClass::InvalidOutcome);
            assert_eq!(error.failure_reason(), Some(reason));
        }
    }

    #[test]
    fn credential_reflection_covers_raw_and_committable_fields() {
        let secret = "synthetic-secret";
        assert!(raw_reflects_secret(
            b"prefix synthetic-secret suffix",
            secret
        ));
        assert!(!raw_reflects_secret(
            b"prefix synthetic-\\u0073ecret suffix",
            secret
        ));
        assert!(raw_reflects_secret(b"anything", ""));

        let mut response = ProviderResponse {
            outcome: ProviderOutcome::Finish(Finish {
                summary: "safe".into(),
                result: "safe".into(),
            }),
            response_id: Some("safe-id".into()),
            input_tokens: Some(1),
            output_tokens: Some(1),
            wire_provenance: None,
        };
        assert!(!response.reflects_secret(secret));
        response.response_id = Some(format!("id-{secret}"));
        assert!(response.reflects_secret(secret));
        response.response_id = None;
        response.outcome = ProviderOutcome::Finish(Finish {
            summary: secret.into(),
            result: "safe".into(),
        });
        assert!(response.reflects_secret(secret));
        response.outcome = ProviderOutcome::Finish(Finish {
            summary: "safe".into(),
            result: secret.into(),
        });
        assert!(response.reflects_secret(secret));
        response.outcome = ProviderOutcome::Delegate(Delegate {
            children: vec![format!("child {secret}")],
        });
        assert!(response.reflects_secret(secret));

        let mut compact = CompactionResponse {
            summary: "safe".into(),
            response_id: None,
            input_tokens: Some(1),
            output_tokens: Some(1),
            wire_provenance: None,
        };
        assert!(!compact.reflects_secret(secret));
        compact.summary = secret.into();
        assert!(compact.reflects_secret(secret));
        compact.summary = "safe".into();
        compact.response_id = Some(secret.into());
        assert!(compact.reflects_secret(secret));
    }
}

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use uuid::{Uuid, Version};

fn parse_v7(value: &str) -> Result<Uuid, &'static str> {
    let id = Uuid::parse_str(value).map_err(|_| "invalid ID")?;
    if id.get_version() != Some(Version::SortRand) {
        return Err("ID must be UUIDv7");
    }
    Ok(id)
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(Uuid);

impl SessionId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}
impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl FromStr for SessionId {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_v7(value).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(Uuid);

impl RunId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}
impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl FromStr for RunId {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_v7(value).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentRunId(Uuid);

impl AgentRunId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}
impl Default for AgentRunId {
    fn default() -> Self {
        Self::new()
    }
}
impl fmt::Display for AgentRunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl FromStr for AgentRunId {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_v7(value).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum CollaborationPolicy {
    Single,
    Auto { max_active_children: u8 },
    Team { max_active_children: u8 },
}

impl Default for CollaborationPolicy {
    fn default() -> Self {
        Self::Auto {
            max_active_children: 3,
        }
    }
}
impl CollaborationPolicy {
    pub fn max_children(self) -> u8 {
        match self {
            Self::Single => 0,
            Self::Auto {
                max_active_children,
            }
            | Self::Team {
                max_active_children,
            } => max_active_children,
        }
    }
    pub(crate) fn validate(self) -> Result<(), ReplayError> {
        match self {
            Self::Single => Ok(()),
            Self::Auto {
                max_active_children,
            } if max_active_children <= 8 => Ok(()),
            Self::Team {
                max_active_children,
            } if (1..=8).contains(&max_active_children) => Ok(()),
            _ => Err(ReplayError::InvalidPayload),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionDefaults {
    pub provider: Option<String>,
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<crate::provider::Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<Uuid>,
    pub policy: CollaborationPolicy,
}

impl SessionDefaults {
    pub(crate) fn validate(&self) -> Result<(), ReplayError> {
        if self.model.is_some() && self.provider.is_none()
            || self.effort.is_some() && self.model.is_none()
            || self.account_id.is_some()
                && (!matches!(
                    self.provider.as_deref(),
                    Some("openai" | "anthropic" | "chatgpt")
                ) || self.provider.as_deref() != Some("chatgpt") && self.model.is_none())
            || self.provider.as_deref() == Some("chatgpt") && self.account_id.is_none()
            || self
                .account_id
                .is_some_and(|id| id.get_version() != Some(Version::SortRand))
        {
            return Err(ReplayError::InvalidPayload);
        }
        if let Some(provider) = &self.provider {
            bounded(provider, 128)?;
        }
        if let Some(model) = &self.model {
            bounded(model, 128)?;
        }
        self.policy.validate()
    }
}

#[cfg(test)]
mod account_default_tests {
    use super::*;

    #[test]
    fn saved_account_binding_requires_a_native_selected_provider() {
        let id = Uuid::now_v7();
        let mut defaults = SessionDefaults {
            provider: Some("openai".into()),
            model: Some("gpt-5.4".into()),
            effort: None,
            account_id: Some(id),
            policy: CollaborationPolicy::Single,
        };
        assert!(defaults.validate().is_ok());
        defaults.provider = Some("custom:local".into());
        assert!(defaults.validate().is_err());
        defaults.provider = Some("openai".into());
        defaults.model = None;
        assert!(defaults.validate().is_err());
        defaults.model = Some("gpt-5.4".into());
        defaults.account_id = Some(Uuid::nil());
        assert!(defaults.validate().is_err());
        defaults.account_id = Some(id);
        defaults.provider = Some("chatgpt".into());
        defaults.model = None;
        assert!(defaults.validate().is_ok());
        defaults.account_id = None;
        assert!(defaults.validate().is_err());
    }

    #[test]
    fn legacy_defaults_without_account_binding_remain_readable() {
        let defaults: SessionDefaults = serde_json::from_str(
            r#"{"provider":"openai","model":"gpt-5.4","policy":{"mode":"single"}}"#,
        )
        .expect("legacy defaults");
        assert_eq!(defaults.account_id, None);
        assert!(defaults.validate().is_ok());
    }

    #[test]
    fn run_account_provenance_separates_api_keys_and_subscription_bounds() {
        let mut config = RunConfig {
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            effort: None,
            custom_profile_provenance: None,
            saved_api_account_id: Some(Uuid::now_v7()),
            chatgpt_provenance: None,
            output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
            policy: CollaborationPolicy::Single,
            output_token_cap: 4096,
            provider_concurrency: 1,
            workspace_device: 1,
            workspace_inode: 1,
            instruction_digest: None,
            include_digests: Vec::new(),
            history_run_ids: Vec::new(),
            excluded_history_runs: 0,
            context_usage: None,
            compaction_event_sequence: None,
            compaction_content_digest: None,
            tool_policy: None,
        };
        assert!(config.validate().is_ok());
        config.saved_api_account_id = Some(Uuid::nil());
        assert!(matches!(
            config.validate(),
            Err(ReplayError::InvalidPayload)
        ));
        config.saved_api_account_id = Some(Uuid::now_v7());
        config.provider = "scripted".into();
        assert!(matches!(
            config.validate(),
            Err(ReplayError::InvalidPayload)
        ));
        config.saved_api_account_id = None;
        assert!(config.validate().is_ok());
        let legacy: RunConfig =
            serde_json::from_value(serde_json::to_value(&config).expect("legacy JSON"))
                .expect("legacy Run config");
        assert_eq!(legacy.saved_api_account_id, None);
        assert_eq!(legacy.chatgpt_provenance, None);
        assert_eq!(
            legacy.output_token_bound,
            crate::provider::OutputTokenBound::ProviderEnforced
        );

        config.provider = "chatgpt".into();
        config.model = "visible-model".into();
        config.effort = Some(crate::provider::Effort::High);
        config.chatgpt_provenance = Some(crate::provider::ChatGptProvenance {
            account_id: Uuid::now_v7(),
            evidence_fingerprint: [7; 32],
            admission: crate::provider::ChatGptAdmission::Conformance,
        });
        assert!(config.validate().is_err());
        config.output_token_bound = crate::provider::OutputTokenBound::LocalAcceptanceOnly;
        assert!(config.validate().is_ok());
        config.saved_api_account_id = Some(Uuid::now_v7());
        assert!(config.validate().is_err());
        config.saved_api_account_id = None;
        config.chatgpt_provenance.as_mut().unwrap().account_id = Uuid::nil();
        assert!(config.validate().is_err());
        config.chatgpt_provenance.as_mut().unwrap().account_id = Uuid::now_v7();
        config
            .chatgpt_provenance
            .as_mut()
            .unwrap()
            .evidence_fingerprint = [0; 32];
        assert!(config.validate().is_err());
        config
            .chatgpt_provenance
            .as_mut()
            .unwrap()
            .evidence_fingerprint = [7; 32];
        config.provider_concurrency = 2;
        assert!(config.validate().is_err());
        config.provider_concurrency = 1;
        assert!(config.validate().is_ok());
        let encoded = serde_json::to_value(&config).unwrap();
        assert_eq!(encoded["output_token_bound"], "local_acceptance_only");
        assert_eq!(encoded["chatgpt_provenance"]["evidence_fingerprint"][0], 7);
        assert!(encoded["chatgpt_provenance"].get("admission").is_none());
        let decoded: RunConfig = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(decoded, config);
        config.chatgpt_provenance.as_mut().unwrap().admission =
            crate::provider::ChatGptAdmission::AccountConsent;
        assert!(config.validate().is_ok());
        let current = serde_json::to_value(&config).unwrap();
        assert_eq!(
            current["chatgpt_provenance"]["admission"],
            "account_consent"
        );
        assert_eq!(
            serde_json::from_value::<RunConfig>(current).unwrap(),
            config
        );
        for (field, value) in [
            ("admission", serde_json::json!("unchecked")),
            ("admission", serde_json::Value::Null),
            ("unknown", serde_json::json!(true)),
        ] {
            let mut invalid = encoded.clone();
            invalid["chatgpt_provenance"][field] = value;
            assert!(serde_json::from_value::<RunConfig>(invalid).is_err());
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextUsage {
    pub used_bytes: u32,
    pub budget_bytes: u32,
    pub compactable_bytes: u32,
    #[serde(default, skip_serializing_if = "zero")]
    pub tool_history_bytes: u32,
}

fn zero(value: &u32) -> bool {
    *value == 0
}

impl ContextUsage {
    pub fn utilization_percent(&self) -> u32 {
        let budget = u64::from(self.budget_bytes).max(1);
        ((u64::from(self.used_bytes) * 100 + budget / 2) / budget).min(100) as u32
    }

    pub fn near_limit(&self) -> bool {
        u64::from(self.used_bytes) * 5 >= u64::from(self.budget_bytes) * 4
            && u64::from(self.compactable_bytes) * 10 >= u64::from(self.budget_bytes)
    }

    fn valid(&self) -> bool {
        self.budget_bytes > 0
            && self.budget_bytes <= super::MAX_CONTEXT_CONTENT_BYTES as u32
            && self.used_bytes <= self.budget_bytes
            && self.compactable_bytes <= self.used_bytes
            && self.tool_history_bytes <= self.compactable_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunConfig {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<crate::provider::Effort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_profile_provenance: Option<crate::provider::CustomProfileProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_api_account_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chatgpt_provenance: Option<crate::provider::ChatGptProvenance>,
    #[serde(default, skip_serializing_if = "provider_enforced_bound")]
    pub output_token_bound: crate::provider::OutputTokenBound,
    pub policy: CollaborationPolicy,
    pub output_token_cap: u32,
    #[serde(default = "default_provider_concurrency")]
    pub provider_concurrency: u8,
    pub workspace_device: u64,
    pub workspace_inode: u64,
    pub instruction_digest: Option<[u8; 32]>,
    pub include_digests: Vec<[u8; 32]>,
    pub history_run_ids: Vec<RunId>,
    pub excluded_history_runs: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<ContextUsage>,
    #[serde(default)]
    pub compaction_event_sequence: Option<u64>,
    #[serde(default)]
    pub compaction_content_digest: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_policy: Option<crate::tools::ToolPolicyReceipt>,
}

impl RunConfig {
    pub fn auto_compaction_due(&self) -> bool {
        self.context_usage.as_ref().is_some_and(|usage| {
            usage.near_limit()
                || usage.compactable_bytes as usize
                    >= if self.tool_policy.is_some() || usage.tool_history_bytes > 0 {
                        super::compaction::AUTO_TOOL_COMPACTION_HISTORY_BYTES
                    } else {
                        super::compaction::AUTO_COMPACTION_HISTORY_BYTES
                    }
        }) || self.history_run_ids.len() + 1 >= super::MAX_HISTORY_RUNS * 3 / 4
    }

    pub(crate) fn validate(&self) -> Result<(), ReplayError> {
        if !(crate::provider::ProviderIdentity {
            profile: &self.provider,
            model: &self.model,
            effort: self.effort,
            concurrency: self.provider_concurrency,
            custom_profile_provenance: self.custom_profile_provenance.as_ref(),
            saved_api_account_id: self.saved_api_account_id,
            chatgpt_provenance: self.chatgpt_provenance.as_ref(),
            output_token_bound: self.output_token_bound,
        })
        .valid()
            || self.output_token_cap != 4096
            || self
                .tool_policy
                .as_ref()
                .is_some_and(|policy| !policy.valid())
            || self.provider.starts_with("custom:") && self.tool_policy.is_some()
            || self.include_digests.len() > 16
            || self.history_run_ids.len() > super::MAX_HISTORY_RUNS
            || self.compaction_event_sequence.is_some() != self.compaction_content_digest.is_some()
            || self
                .context_usage
                .as_ref()
                .is_some_and(|usage| !usage.valid())
        {
            return Err(ReplayError::InvalidPayload);
        }
        self.policy.validate()
    }
}

fn default_provider_concurrency() -> u8 {
    1
}

fn provider_enforced_bound(bound: &crate::provider::OutputTokenBound) -> bool {
    *bound == crate::provider::OutputTokenBound::ProviderEnforced
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Primary,
    Child,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDisposition {
    Finished,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunDisposition {
    Finished,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionFailure {
    ProviderUnavailable,
    ProviderRejected,
    InvalidOutcome,
    TimedOut,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompactionStatus {
    Succeeded {
        summary: String,
        content_digest: [u8; 32],
        summary_bytes: u32,
        summary_tokens_estimate: u32,
    },
    Failed {
        reason: CompactionFailure,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionRecord {
    pub covered_run_id: RunId,
    pub covered_sequence: u64,
    pub source_digest: [u8; 32],
    pub provider: String,
    pub model: String,
    pub prompt_version: u16,
    pub schema_version: u16,
    pub compiler_version: u16,
    pub source_bytes: u32,
    pub source_tokens_estimate: u32,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_provenance: Option<crate::provider::ProviderWireProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chatgpt_provenance: Option<crate::provider::ChatGptProvenance>,
    #[serde(default, skip_serializing_if = "provider_enforced_bound")]
    pub output_token_bound: crate::provider::OutputTokenBound,
    pub status: CompactionStatus,
}

impl CompactionRecord {
    pub(crate) fn validate(&self) -> Result<(), ReplayError> {
        if self.covered_sequence == 0
            || self.prompt_version != 1
            || self.schema_version != 1
            || !matches!(self.compiler_version, 1..=3)
            || self.source_bytes == 0
            || self.source_bytes > 256 * 1024
            || self.source_tokens_estimate != self.source_bytes.div_ceil(4)
            || self.input_tokens.is_some_and(|value| value > 1_000_000)
            || self.output_tokens.is_some_and(|value| value > 1_000_000)
            || self.response_id.as_ref().is_some_and(|value| {
                value.is_empty()
                    || value.len() > 128
                    || !value.bytes().all(|byte| byte.is_ascii_graphic())
            })
            || self.wire_provenance.is_some_and(|provenance| {
                !provenance.valid_for(
                    self.response_id.as_deref(),
                    self.input_tokens,
                    self.output_tokens,
                )
            })
            || (self.provider == "chatgpt") != self.chatgpt_provenance.is_some()
            || (self.provider == "chatgpt")
                != (self.output_token_bound
                    == crate::provider::OutputTokenBound::LocalAcceptanceOnly)
            || self
                .chatgpt_provenance
                .as_ref()
                .is_some_and(|provenance| !provenance.valid())
        {
            return Err(ReplayError::InvalidSnapshot);
        }
        bounded(&self.provider, 128).map_err(|_| ReplayError::InvalidSnapshot)?;
        bounded(&self.model, 128).map_err(|_| ReplayError::InvalidSnapshot)?;
        if let CompactionStatus::Succeeded {
            summary,
            summary_bytes,
            summary_tokens_estimate,
            ..
        } = &self.status
        {
            bounded(summary, 8 * 1024).map_err(|_| ReplayError::InvalidSnapshot)?;
            if *summary_bytes != summary.len() as u32
                || *summary_tokens_estimate != summary_bytes.div_ceil(4)
                || self.output_tokens.is_some_and(|count| count > 1024)
            {
                return Err(ReplayError::InvalidSnapshot);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod subscription_compaction_tests {
    use super::*;

    #[test]
    fn subscription_compaction_pins_account_and_weaker_bound() {
        let mut record = CompactionRecord {
            covered_run_id: RunId::new(),
            covered_sequence: 1,
            source_digest: [1; 32],
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            prompt_version: 1,
            schema_version: 1,
            compiler_version: 1,
            source_bytes: 4,
            source_tokens_estimate: 1,
            input_tokens: None,
            output_tokens: None,
            response_id: None,
            wire_provenance: None,
            chatgpt_provenance: None,
            output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
            status: CompactionStatus::Failed {
                reason: CompactionFailure::ProviderUnavailable,
            },
        };
        assert!(record.validate().is_ok());
        let legacy = serde_json::to_value(&record).unwrap();
        assert!(legacy.get("chatgpt_provenance").is_none());
        assert!(legacy.get("output_token_bound").is_none());
        record.provider = "chatgpt".into();
        record.chatgpt_provenance = Some(crate::provider::ChatGptProvenance {
            account_id: Uuid::now_v7(),
            evidence_fingerprint: [7; 32],
            admission: crate::provider::ChatGptAdmission::Conformance,
        });
        assert!(record.validate().is_err());
        record.output_token_bound = crate::provider::OutputTokenBound::LocalAcceptanceOnly;
        assert!(record.validate().is_ok());
        let legacy = serde_json::to_value(&record).unwrap();
        assert!(legacy["chatgpt_provenance"].get("admission").is_none());
        assert_eq!(
            serde_json::from_value::<CompactionRecord>(legacy.clone()).unwrap(),
            record
        );
        record.chatgpt_provenance.as_mut().unwrap().admission =
            crate::provider::ChatGptAdmission::AccountConsent;
        assert!(record.validate().is_ok());
        let current = serde_json::to_value(&record).unwrap();
        assert_eq!(
            current["chatgpt_provenance"]["admission"],
            "account_consent"
        );
        assert_eq!(
            serde_json::from_value::<CompactionRecord>(current).unwrap(),
            record
        );
        for (field, value) in [
            ("admission", serde_json::json!("unchecked")),
            ("admission", serde_json::Value::Null),
            ("unknown", serde_json::json!(true)),
        ] {
            let mut invalid = legacy.clone();
            invalid["chatgpt_provenance"][field] = value;
            assert!(serde_json::from_value::<CompactionRecord>(invalid).is_err());
        }
        record
            .chatgpt_provenance
            .as_mut()
            .unwrap()
            .evidence_fingerprint = [0; 32];
        assert!(record.validate().is_err());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCallDisposition {
    Finished,
    Delegated,
    ToolRequested,
    Unavailable,
    Rejected,
    InvalidResponse,
    OutputLimit,
    TimedOut,
    Cancelled,
    TaskPanic,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCallRecord {
    pub phase: crate::provider::AgentPhase,
    pub disposition: ProviderCallDisposition,
    pub response_id: Option<String>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_provenance: Option<crate::provider::ProviderWireProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<crate::provider::ProviderFailureReason>,
}

impl ProviderCallRecord {
    fn validate(&self) -> Result<(), ReplayError> {
        if self.response_id.as_ref().is_some_and(|id| {
            id.is_empty() || id.len() > 128 || !id.bytes().all(|byte| byte.is_ascii_graphic())
        }) || self
            .input_tokens
            .is_some_and(|count| count > crate::provider::MAX_REPORTED_INPUT_TOKENS)
            || self.output_tokens.is_some_and(|count| count > 4096)
            || self.wire_provenance.is_some_and(|provenance| {
                !provenance.valid_for(
                    self.response_id.as_deref(),
                    self.input_tokens,
                    self.output_tokens,
                )
            })
            || self.failure_reason.is_some_and(|reason| {
                use crate::provider::ProviderFailureReason;
                !matches!(
                    (self.disposition, reason),
                    (
                        ProviderCallDisposition::Rejected,
                        ProviderFailureReason::AccountAccess | ProviderFailureReason::UsageLimit
                    ) | (
                        ProviderCallDisposition::Unavailable,
                        ProviderFailureReason::UsageTemporarilyUnavailable
                            | ProviderFailureReason::ServiceUnavailable
                    ) | (
                        ProviderCallDisposition::InvalidResponse,
                        ProviderFailureReason::StreamProtocol
                            | ProviderFailureReason::ResponseContract
                            | ProviderFailureReason::OutcomeContract
                            | ProviderFailureReason::LocalOutputLimit
                    )
                )
            })
            || (!matches!(
                self.disposition,
                ProviderCallDisposition::Finished
                    | ProviderCallDisposition::Delegated
                    | ProviderCallDisposition::ToolRequested
                    | ProviderCallDisposition::InvalidResponse
            ) && (self.response_id.is_some()
                || self.input_tokens.is_some()
                || self.output_tokens.is_some()
                || self.wire_provenance.is_some()))
        {
            return Err(ReplayError::InvalidPayload);
        }
        Ok(())
    }
}

#[cfg(test)]
mod provider_call_tests {
    use super::*;

    #[test]
    fn rejects_unbounded_or_unearned_provider_metadata() {
        let mut record = ProviderCallRecord {
            phase: crate::provider::AgentPhase::RootPlan,
            disposition: ProviderCallDisposition::Finished,
            response_id: Some("response-1".into()),
            input_tokens: Some(100),
            output_tokens: Some(10),
            wire_provenance: None,
            failure_reason: None,
        };
        assert!(record.validate().is_ok());
        record.response_id = Some("bad\nidentifier".into());
        assert!(record.validate().is_err());
        record.response_id = Some("r".repeat(129));
        assert!(record.validate().is_err());
        record.response_id = None;
        record.input_tokens = Some(crate::provider::MAX_REPORTED_INPUT_TOKENS + 1);
        assert!(record.validate().is_err());
        record.input_tokens = Some(100);
        record.output_tokens = Some(4097);
        assert!(record.validate().is_err());
        record.output_tokens = None;
        record.disposition = ProviderCallDisposition::Unavailable;
        record.input_tokens = Some(100);
        assert!(record.validate().is_err());
        record.input_tokens = None;
        assert!(record.validate().is_ok());
        record.disposition = ProviderCallDisposition::Finished;
        record.response_id = Some("response-1".into());
        record.wire_provenance =
            Some(crate::provider::ProviderWireProvenance::MessagesEndTurnStorageUnspecified);
        assert!(record.validate().is_err());
        record.input_tokens = Some(100);
        record.output_tokens = Some(10);
        assert!(record.validate().is_ok());
        record.disposition = ProviderCallDisposition::TimedOut;
        assert!(record.validate().is_err());
        record.disposition = ProviderCallDisposition::Finished;
        record.wire_provenance = None;
        let legacy = serde_json::to_value(&record).expect("legacy payload");
        assert!(legacy.get("wire_provenance").is_none());
        let legacy: ProviderCallRecord = serde_json::from_value(legacy).expect("legacy replay");
        assert_eq!(legacy.wire_provenance, None);
        assert_eq!(legacy.failure_reason, None);

        use crate::provider::ProviderFailureReason;
        let run = RunId::new();
        let agent = AgentRunId::new();
        let run_id = Some(run);
        let agent_run_id = Some(agent);
        let legacy_event = Event::ProviderCallRecorded {
            run_id: run,
            agent_run_id: agent,
            record: legacy,
        };
        let legacy_payload = legacy_event.payload().expect("v1 payload");
        assert_eq!(legacy_event.version(), 1);
        assert_eq!(
            Event::decode(
                "ProviderCallRecorded",
                1,
                &legacy_payload,
                run_id,
                agent_run_id
            )
            .expect("v1 replay"),
            legacy_event
        );
        assert!(
            Event::decode(
                "ProviderCallRecorded",
                2,
                &legacy_payload,
                run_id,
                agent_run_id
            )
            .is_err()
        );
        for (reason, expected) in [
            (
                ProviderFailureReason::AccountAccess,
                ProviderCallDisposition::Rejected,
            ),
            (
                ProviderFailureReason::UsageLimit,
                ProviderCallDisposition::Rejected,
            ),
            (
                ProviderFailureReason::UsageTemporarilyUnavailable,
                ProviderCallDisposition::Unavailable,
            ),
            (
                ProviderFailureReason::ServiceUnavailable,
                ProviderCallDisposition::Unavailable,
            ),
            (
                ProviderFailureReason::StreamProtocol,
                ProviderCallDisposition::InvalidResponse,
            ),
            (
                ProviderFailureReason::ResponseContract,
                ProviderCallDisposition::InvalidResponse,
            ),
            (
                ProviderFailureReason::OutcomeContract,
                ProviderCallDisposition::InvalidResponse,
            ),
            (
                ProviderFailureReason::LocalOutputLimit,
                ProviderCallDisposition::InvalidResponse,
            ),
        ] {
            for disposition in [
                ProviderCallDisposition::Finished,
                ProviderCallDisposition::Delegated,
                ProviderCallDisposition::Unavailable,
                ProviderCallDisposition::Rejected,
                ProviderCallDisposition::InvalidResponse,
                ProviderCallDisposition::OutputLimit,
                ProviderCallDisposition::TimedOut,
                ProviderCallDisposition::Cancelled,
                ProviderCallDisposition::TaskPanic,
            ] {
                let record = ProviderCallRecord {
                    phase: crate::provider::AgentPhase::RootPlan,
                    disposition,
                    response_id: None,
                    input_tokens: None,
                    output_tokens: None,
                    wire_provenance: None,
                    failure_reason: Some(reason),
                };
                assert_eq!(
                    record.validate().is_ok(),
                    disposition == expected,
                    "{disposition:?}/{reason:?}"
                );
                if disposition != expected {
                    continue;
                }
                let event = Event::ProviderCallRecorded {
                    run_id: run,
                    agent_run_id: agent,
                    record,
                };
                let payload = event.payload().expect("v2 payload");
                assert_eq!(event.version(), 2);
                assert_eq!(
                    Event::decode("ProviderCallRecorded", 2, &payload, run_id, agent_run_id)
                        .expect("v2 replay"),
                    event
                );
                for version in [0, 1, 3] {
                    assert!(
                        Event::decode(
                            "ProviderCallRecorded",
                            version,
                            &payload,
                            run_id,
                            agent_run_id
                        )
                        .is_err(),
                        "version {version}"
                    );
                }
                for scope in [(None, agent_run_id), (run_id, None)] {
                    assert!(
                        Event::decode("ProviderCallRecorded", 2, &payload, scope.0, scope.1)
                            .is_err()
                    );
                }
                let mut invalid: serde_json::Value =
                    serde_json::from_str(&payload).expect("v2 JSON");
                invalid["failure_reason"] = "unknown_reason".into();
                assert!(
                    Event::decode(
                        "ProviderCallRecorded",
                        2,
                        &invalid.to_string(),
                        run_id,
                        agent_run_id
                    )
                    .is_err()
                );
                invalid["failure_reason"] = serde_json::Value::Null;
                assert!(
                    Event::decode(
                        "ProviderCallRecorded",
                        2,
                        &invalid.to_string(),
                        run_id,
                        agent_run_id
                    )
                    .is_err()
                );
                invalid.as_object_mut().unwrap().remove("failure_reason");
                assert!(
                    Event::decode(
                        "ProviderCallRecorded",
                        2,
                        &invalid.to_string(),
                        run_id,
                        agent_run_id
                    )
                    .is_err()
                );
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    SessionStarted {
        title: String,
        workspace_identity: Option<(u64, u64)>,
    },
    SessionDefaultChanged {
        defaults: SessionDefaults,
    },
    SessionForked {
        title: String,
        source_session_id: SessionId,
        source_run_id: RunId,
        source_sequence: u64,
        prefix_digest: [u8; 32],
    },
    ContextCompacted {
        record: CompactionRecord,
    },
    SessionRenamed {
        title: String,
    },
    MessageAccepted {
        run_id: RunId,
        text: String,
        images: Vec<crate::provider::ImageAttachment>,
    },
    RunStarted {
        run_id: RunId,
        config: RunConfig,
    },
    AgentSpawned {
        run_id: RunId,
        agent_run_id: AgentRunId,
        role: AgentRole,
        ordinal: u8,
        objective: Option<String>,
    },
    ProviderCallRecorded {
        run_id: RunId,
        agent_run_id: AgentRunId,
        record: ProviderCallRecord,
    },
    ToolStarted {
        run_id: RunId,
        agent_run_id: AgentRunId,
        intent: crate::tools::EffectIntent,
    },
    ToolFinished {
        run_id: RunId,
        agent_run_id: AgentRunId,
        observation: crate::tools::ToolObservation,
    },
    AgentFinished {
        run_id: RunId,
        agent_run_id: AgentRunId,
        disposition: AgentDisposition,
        summary: Option<String>,
        result: Option<String>,
    },
    MessageCommitted {
        run_id: RunId,
        text: String,
    },
    RunFinished {
        run_id: RunId,
        disposition: RunDisposition,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventEnvelope {
    pub sequence: u64,
    pub session_id: SessionId,
    pub run_id: Option<RunId>,
    pub agent_run_id: Option<AgentRunId>,
    pub event: Event,
    pub created_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForkLineage {
    pub source_session_id: SessionId,
    pub source_run_id: RunId,
    pub source_sequence: u64,
    pub prefix_digest: [u8; 32],
}

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("invalid Event payload")]
    InvalidPayload,
    #[error("unknown Event kind or version")]
    UnknownEvent,
    #[error("invalid Session transition")]
    InvalidTransition,
    #[error("invalid compaction snapshot")]
    InvalidSnapshot,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TitlePayload {
    title: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartedPayloadV2 {
    title: String,
    workspace_device: u64,
    workspace_inode: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForkPayload {
    title: String,
    source_session_id: SessionId,
    source_run_id: RunId,
    source_sequence: u64,
    prefix_digest: [u8; 32],
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextPayload {
    text: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MessagePayloadV2 {
    text: String,
    #[serde(deserialize_with = "crate::provider::deserialize_images")]
    images: Vec<crate::provider::ImageAttachment>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnPayload {
    role: AgentRole,
    ordinal: u8,
    objective: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentFinishedPayload {
    disposition: AgentDisposition,
    summary: Option<String>,
    result: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunFinishedPayload {
    disposition: RunDisposition,
}

impl Event {
    pub(crate) fn finished_payload_size(summary: &str, result: &str) -> Result<usize, ReplayError> {
        serialize(&AgentFinishedPayload {
            disposition: AgentDisposition::Finished,
            summary: Some(summary.into()),
            result: Some(result.into()),
        })
        .map(|payload| payload.len())
    }

    pub(crate) fn version(&self) -> i64 {
        match self {
            Self::SessionStarted {
                workspace_identity: Some(_),
                ..
            } => 2,
            Self::ProviderCallRecorded { record, .. } if record.failure_reason.is_some() => 2,
            Self::MessageAccepted { images, .. } if !images.is_empty() => 2,
            _ => 1,
        }
    }

    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::SessionStarted { .. } => "SessionStarted",
            Self::SessionDefaultChanged { .. } => "SessionDefaultChanged",
            Self::SessionForked { .. } => "SessionForked",
            Self::ContextCompacted { .. } => "ContextCompacted",
            Self::SessionRenamed { .. } => "SessionRenamed",
            Self::MessageAccepted { .. } => "MessageAccepted",
            Self::RunStarted { .. } => "RunStarted",
            Self::AgentSpawned { .. } => "AgentSpawned",
            Self::ProviderCallRecorded { .. } => "ProviderCallRecorded",
            Self::ToolStarted { .. } => "ToolStarted",
            Self::ToolFinished { .. } => "ToolFinished",
            Self::AgentFinished { .. } => "AgentFinished",
            Self::MessageCommitted { .. } => "MessageCommitted",
            Self::RunFinished { .. } => "RunFinished",
        }
    }

    pub(crate) fn scope(&self) -> (Option<RunId>, Option<AgentRunId>) {
        match self {
            Self::SessionStarted { .. }
            | Self::SessionDefaultChanged { .. }
            | Self::SessionForked { .. }
            | Self::ContextCompacted { .. }
            | Self::SessionRenamed { .. } => (None, None),
            Self::MessageAccepted { run_id, .. }
            | Self::RunStarted { run_id, .. }
            | Self::MessageCommitted { run_id, .. }
            | Self::RunFinished { run_id, .. } => (Some(*run_id), None),
            Self::AgentSpawned {
                run_id,
                agent_run_id,
                ..
            }
            | Self::ProviderCallRecorded {
                run_id,
                agent_run_id,
                ..
            }
            | Self::ToolStarted {
                run_id,
                agent_run_id,
                ..
            }
            | Self::ToolFinished {
                run_id,
                agent_run_id,
                ..
            }
            | Self::AgentFinished {
                run_id,
                agent_run_id,
                ..
            } => (Some(*run_id), Some(*agent_run_id)),
        }
    }

    pub(crate) fn payload(&self) -> Result<String, ReplayError> {
        self.validate()?;
        match self {
            Self::SessionStarted {
                title,
                workspace_identity: Some((workspace_device, workspace_inode)),
            } => serialize(&StartedPayloadV2 {
                title: title.clone(),
                workspace_device: *workspace_device,
                workspace_inode: *workspace_inode,
            }),
            Self::SessionStarted {
                title,
                workspace_identity: None,
            }
            | Self::SessionRenamed { title } => serialize(&TitlePayload {
                title: title.clone(),
            }),
            Self::SessionDefaultChanged { defaults } => serialize(defaults),
            Self::SessionForked {
                title,
                source_session_id,
                source_run_id,
                source_sequence,
                prefix_digest,
            } => serialize(&ForkPayload {
                title: title.clone(),
                source_session_id: *source_session_id,
                source_run_id: *source_run_id,
                source_sequence: *source_sequence,
                prefix_digest: *prefix_digest,
            }),
            Self::ContextCompacted { record } => serialize(record),
            Self::MessageAccepted { text, images, .. } if !images.is_empty() => {
                serialize(&MessagePayloadV2 {
                    text: text.clone(),
                    images: images.clone(),
                })
            }
            Self::MessageAccepted { text, .. } | Self::MessageCommitted { text, .. } => {
                serialize(&TextPayload { text: text.clone() })
            }
            Self::RunStarted { config, .. } => serialize(config),
            Self::AgentSpawned {
                role,
                ordinal,
                objective,
                ..
            } => serialize(&SpawnPayload {
                role: *role,
                ordinal: *ordinal,
                objective: objective.clone(),
            }),
            Self::ProviderCallRecorded { record, .. } => serialize(record),
            Self::ToolStarted { intent, .. } => serialize(intent),
            Self::ToolFinished { observation, .. } => serialize(observation),
            Self::AgentFinished {
                disposition,
                summary,
                result,
                ..
            } => serialize(&AgentFinishedPayload {
                disposition: *disposition,
                summary: summary.clone(),
                result: result.clone(),
            }),
            Self::RunFinished { disposition, .. } => serialize(&RunFinishedPayload {
                disposition: *disposition,
            }),
        }
    }

    pub(crate) fn decode(
        kind: &str,
        version: i64,
        payload: &str,
        run_id: Option<RunId>,
        agent_run_id: Option<AgentRunId>,
    ) -> Result<Self, ReplayError> {
        if kind == "MessageAccepted" && version == 2 {
            if agent_run_id.is_some() {
                return Err(ReplayError::InvalidPayload);
            }
            let data: MessagePayloadV2 = parse(payload)?;
            if data.images.is_empty() {
                return Err(ReplayError::InvalidPayload);
            }
            let event = Self::MessageAccepted {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                text: data.text,
                images: data.images,
            };
            event.validate()?;
            return Ok(event);
        }
        if kind == "ProviderCallRecorded" && matches!(version, 1 | 2) {
            let record: ProviderCallRecord = parse(payload)?;
            if (version == 2) != record.failure_reason.is_some() {
                return Err(ReplayError::InvalidPayload);
            }
            let event = Self::ProviderCallRecorded {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                record,
            };
            event.validate()?;
            return Ok(event);
        }
        if kind == "SessionStarted" && version == 2 {
            if run_id.is_some() || agent_run_id.is_some() {
                return Err(ReplayError::InvalidPayload);
            }
            let data: StartedPayloadV2 = parse(payload)?;
            let event = Self::SessionStarted {
                title: data.title,
                workspace_identity: Some((data.workspace_device, data.workspace_inode)),
            };
            event.validate()?;
            return Ok(event);
        }
        if version != 1 {
            return Err(ReplayError::UnknownEvent);
        }
        let event = match kind {
            "SessionStarted" if run_id.is_none() && agent_run_id.is_none() => {
                Self::SessionStarted {
                    title: parse::<TitlePayload>(payload)?.title,
                    workspace_identity: None,
                }
            }
            "SessionDefaultChanged" if run_id.is_none() && agent_run_id.is_none() => {
                Self::SessionDefaultChanged {
                    defaults: parse(payload)?,
                }
            }
            "SessionForked" if run_id.is_none() && agent_run_id.is_none() => {
                let data: ForkPayload = parse(payload)?;
                Self::SessionForked {
                    title: data.title,
                    source_session_id: data.source_session_id,
                    source_run_id: data.source_run_id,
                    source_sequence: data.source_sequence,
                    prefix_digest: data.prefix_digest,
                }
            }
            "ContextCompacted" if run_id.is_none() && agent_run_id.is_none() => {
                Self::ContextCompacted {
                    record: parse(payload)?,
                }
            }
            "SessionRenamed" if run_id.is_none() && agent_run_id.is_none() => {
                Self::SessionRenamed {
                    title: parse::<TitlePayload>(payload)?.title,
                }
            }
            "MessageAccepted" if run_id.is_some() && agent_run_id.is_none() => {
                Self::MessageAccepted {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    text: parse::<TextPayload>(payload)?.text,
                    images: Vec::new(),
                }
            }
            "RunStarted" if run_id.is_some() && agent_run_id.is_none() => Self::RunStarted {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                config: parse(payload)?,
            },
            "AgentSpawned" if run_id.is_some() && agent_run_id.is_some() => {
                let data: SpawnPayload = parse(payload)?;
                Self::AgentSpawned {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    role: data.role,
                    ordinal: data.ordinal,
                    objective: data.objective,
                }
            }
            "AgentFinished" if run_id.is_some() && agent_run_id.is_some() => {
                let data: AgentFinishedPayload = parse(payload)?;
                Self::AgentFinished {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                    disposition: data.disposition,
                    summary: data.summary,
                    result: data.result,
                }
            }
            "ToolStarted" if run_id.is_some() && agent_run_id.is_some() => Self::ToolStarted {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                intent: parse(payload)?,
            },
            "ToolFinished" if run_id.is_some() && agent_run_id.is_some() => Self::ToolFinished {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                agent_run_id: agent_run_id.ok_or(ReplayError::InvalidPayload)?,
                observation: parse(payload)?,
            },
            "MessageCommitted" if run_id.is_some() && agent_run_id.is_none() => {
                Self::MessageCommitted {
                    run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                    text: parse::<TextPayload>(payload)?.text,
                }
            }
            "RunFinished" if run_id.is_some() && agent_run_id.is_none() => Self::RunFinished {
                run_id: run_id.ok_or(ReplayError::InvalidPayload)?,
                disposition: parse::<RunFinishedPayload>(payload)?.disposition,
            },
            "SessionStarted"
            | "SessionDefaultChanged"
            | "SessionForked"
            | "ContextCompacted"
            | "SessionRenamed"
            | "MessageAccepted"
            | "RunStarted"
            | "AgentSpawned"
            | "ProviderCallRecorded"
            | "ToolStarted"
            | "ToolFinished"
            | "AgentFinished"
            | "MessageCommitted"
            | "RunFinished" => {
                return Err(ReplayError::InvalidPayload);
            }
            _ => return Err(ReplayError::UnknownEvent),
        };
        event.validate()?;
        Ok(event)
    }

    pub(super) fn validate(&self) -> Result<(), ReplayError> {
        match self {
            Self::SessionStarted { title, .. } | Self::SessionRenamed { title } => {
                bounded(title, 128)
            }
            Self::SessionDefaultChanged { defaults } => defaults.validate(),
            Self::SessionForked {
                title,
                source_sequence,
                ..
            } if *source_sequence > 0 => bounded(title, 128),
            Self::SessionForked { .. } => Err(ReplayError::InvalidPayload),
            Self::ContextCompacted { record } => record.validate(),
            Self::MessageAccepted { text, images, .. } => {
                if text.len() > super::MAX_USER_MESSAGE_BYTES
                    || text.is_empty() && images.is_empty()
                    || !crate::provider::valid_image_collection(images)
                {
                    return Err(ReplayError::InvalidPayload);
                }
                Ok(())
            }
            Self::MessageCommitted { text, .. } => {
                bounded(text, super::MAX_ASSISTANT_MESSAGE_BYTES)
            }
            Self::RunStarted { config, .. } => config.validate(),
            Self::AgentSpawned {
                role,
                ordinal,
                objective,
                ..
            } => match role {
                AgentRole::Primary if *ordinal == 0 && objective.is_none() => Ok(()),
                AgentRole::Child if (1..=8).contains(ordinal) => bounded(
                    objective.as_deref().ok_or(ReplayError::InvalidPayload)?,
                    2 * 1024,
                ),
                _ => Err(ReplayError::InvalidPayload),
            },
            Self::ProviderCallRecorded { record, .. } => record.validate(),
            Self::ToolStarted {
                run_id,
                agent_run_id,
                intent,
            } => {
                if intent.valid()
                    && intent.run_id == *run_id
                    && intent.agent_run_id == *agent_run_id
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::ToolFinished {
                run_id,
                agent_run_id,
                observation,
            } => {
                if observation.valid()
                    && observation.intent.run_id == *run_id
                    && observation.intent.agent_run_id == *agent_run_id
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::AgentFinished {
                disposition,
                summary,
                result,
                ..
            } => match disposition {
                AgentDisposition::Finished => {
                    bounded(
                        summary.as_deref().ok_or(ReplayError::InvalidPayload)?,
                        2 * 1024,
                    )?;
                    bounded(
                        result.as_deref().ok_or(ReplayError::InvalidPayload)?,
                        super::MAX_ASSISTANT_MESSAGE_BYTES,
                    )
                }
                AgentDisposition::Failed | AgentDisposition::Cancelled
                    if summary.is_none() && result.is_none() =>
                {
                    Ok(())
                }
                _ => Err(ReplayError::InvalidPayload),
            },
            Self::RunFinished { .. } => Ok(()),
        }
    }
}

fn bounded(value: &str, max: usize) -> Result<(), ReplayError> {
    if value.is_empty() || value.len() > max {
        return Err(ReplayError::InvalidPayload);
    }
    Ok(())
}

fn serialize<T: Serialize>(value: &T) -> Result<String, ReplayError> {
    serde_json::to_string(value).map_err(|_| ReplayError::InvalidPayload)
}

fn parse<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T, ReplayError> {
    serde_json::from_str(value).map_err(|_| ReplayError::InvalidPayload)
}

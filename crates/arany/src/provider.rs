use crate::session::{CollaborationPolicy, RunId, SessionId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use uuid::{Uuid, Version};

mod anthropic;
mod catalog;
mod conversation;
mod custom;
mod decode;
mod effort;
pub(crate) mod evidence;
mod image;
#[cfg(any(test, feature = "test-support"))]
pub use image::test_image;
pub(crate) use image::{MAX_IMAGE_METADATA_BYTES, deserialize_images, valid_image_collection};
mod limits;
mod native;
mod native_check;
pub use limits::{
    LimitFailure, LimitSource, LimitUnavailable, MAX_LIMIT_TIMESTAMP, MAX_RATE_LIMIT_BYTES,
    PlanWindow, RateLimitSlot, RateLimits, UsedPercent,
};
mod openai;
pub(crate) mod prompts;
mod render;
mod roster;
pub use roster::{AdmittedRoster, NamedProviderRoutes, RosterSnapshot, valid_profile_name};
mod routing;
pub use routing::{
    AdmittedProviders, MAX_PROVIDER_FALLBACKS, MAX_PROVIDER_RETRIES, MAX_PROVIDER_RETRY_DELAY_MS,
    MAX_PROVIDER_SWITCHES, ProviderFallback, ProviderRetry, ProviderRole, ProviderRoutes,
    Providers, RetryCause,
};
mod streaming;
#[cfg(test)]
pub(crate) mod test_support;
mod tool_specs;
pub use anthropic::AnthropicProvider;
pub use catalog::{
    MAX_CATALOG_MODELS, ModelEntry, list_native_models, list_native_models_with_api_key,
    list_native_models_with_credentials,
};
pub(crate) use conversation::{MAX_HARNESS_FEEDBACK_BYTES, max_turn_bytes};
pub use conversation::{
    PromptRole, ProviderRequest, ProviderTurn, RunInput, StopHookFeedback, ToolUseId,
    WorkspaceGuidance,
};
pub use custom::{
    CustomProfile, CustomProfileCheck, CustomProfileError, CustomProvider, check_custom_profile,
    check_custom_profile_with_cancellation,
};
pub(crate) use custom::{MAX_ADDRESSES, PROVIDER_PROFILES};
pub use effort::{Effort, resolve_native_effort, resolve_native_effort_for_run};
pub use image::{
    ImageAttachment, ImageOrigin, MAX_ENCODED_IMAGE_BYTES, MAX_IMAGE_BYTES, MAX_MESSAGE_IMAGES,
    ProviderImage,
};
pub use native_check::{
    NativeCheckError, NativeCheckRequest, check_native_model, check_native_model_from_env,
};
pub use openai::{ChatGptProvider, OpenAiProvider, probe_chatgpt_model};
pub(crate) use render::instructions_text;
#[cfg(feature = "test-support")]
pub use render::semantic;
pub(crate) use render::{history_refusal_block, history_tool_block, history_unanswered_block};
pub use streaming::{PreviewText, ResponsePreview};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
    ChatGpt,
    Custom,
}

impl ProviderKind {
    pub const NAMES: [&str; 4] = ["openai", "anthropic", "chatgpt", "custom:"];

    pub fn native_name(self) -> Option<&'static str> {
        match self {
            Self::OpenAi => Some("openai"),
            Self::Anthropic => Some("anthropic"),
            Self::ChatGpt => Some("chatgpt"),
            Self::Custom => None,
        }
    }

    pub fn is_native_api(self) -> bool {
        match self {
            Self::OpenAi | Self::Anthropic => true,
            Self::ChatGpt | Self::Custom => false,
        }
    }

    pub fn is_native(self) -> bool {
        self.native_name().is_some()
    }

    pub fn parse(profile: &str) -> Option<Self> {
        match profile {
            "openai" => Some(Self::OpenAi),
            "anthropic" => Some(Self::Anthropic),
            "chatgpt" => Some(Self::ChatGpt),
            value if Self::custom_name(value).is_some() => Some(Self::Custom),
            _ => None,
        }
    }

    pub fn custom_name(profile: &str) -> Option<&str> {
        profile.strip_prefix("custom:")
    }
}

fn http_client(timeout: std::time::Duration) -> reqwest::ClientBuilder {
    // Decompression stays off even when another dependency enables reqwest's compression features.
    reqwest::Client::builder()
        .no_proxy()
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .retry(reqwest::retry::never())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(timeout)
        .pool_max_idle_per_host(0)
}

async fn read_identity_body(
    response: &mut reqwest::Response,
    max: usize,
) -> Result<Vec<u8>, ProviderError> {
    if response
        .headers()
        .get_all(reqwest::header::CONTENT_ENCODING)
        .iter()
        .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
        || response
            .content_length()
            .is_some_and(|length| length > max as u64)
    {
        return Err(ProviderError::InvalidOutcome);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ProviderError::PartialTransport)?
    {
        if chunk.len() > max - bytes.len() {
            return Err(ProviderError::InvalidOutcome);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

const MAX_RATE_LIMIT_BODY_BYTES: usize = 16 * 1024;

// A send that times out has spent its deadline; any other send failure happened before a response.
pub(crate) fn send_failure(error: &reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        ProviderError::Unavailable
    } else {
        ProviderError::Transport
    }
}

pub(crate) async fn http_failure(response: &mut reqwest::Response) -> ProviderError {
    let status = response.status();
    if status.is_server_error() {
        ProviderError::HttpServerUnavailable(retry_after(response.headers()))
    } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        rate_limit_failure(response).await
    } else {
        ProviderError::Rejected
    }
}

// Only delta-seconds of a status rejection; the subscription route keeps its numeric status.
pub(crate) fn server_retry_after(response: &reqwest::Response) -> Option<Duration> {
    retry_after(response.headers())
}

// The bounded 429 body only tells a quota from a rate limit; it is never kept or shown.
pub(crate) async fn rate_limit_failure(response: &mut reqwest::Response) -> ProviderError {
    let after = retry_after(response.headers());
    let body = read_identity_body(response, MAX_RATE_LIMIT_BODY_BYTES)
        .await
        .unwrap_or_default();
    if quota_exhausted(&body) {
        ProviderError::QuotaExceeded
    } else {
        ProviderError::RateLimited(after)
    }
}

// Error codes a wait cannot clear: exhausted credit, quota, plan or spending and usage limits,
// as Codex classifies them. Anthropic 429s are rate limits only.
const QUOTA_CODES: [&str; 7] = [
    "insufficient_quota",
    "credit_balance_exhausted",
    "organization_spend_limit_exceeded",
    "project_spend_limit_exceeded",
    "organization_usage_limit_exceeded",
    "usage_limit_reached",
    "usage_not_included",
];

fn quota_exhausted(body: &[u8]) -> bool {
    let Ok(document) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    let error = &document["error"];
    [&error["code"], &error["type"]].into_iter().any(|field| {
        field
            .as_str()
            .is_some_and(|code| QUOTA_CODES.contains(&code))
    })
}

// Only delta-seconds; an HTTP date or malformed value leaves the Engine's own backoff in place.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.as_bytes();
    if value.is_empty() || value.len() > 10 || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(value)
        .ok()?
        .parse()
        .ok()
        .map(Duration::from_secs)
}

pub(crate) const MAX_REPORTED_INPUT_TOKENS: u32 = 1_000_000;
pub(crate) const MAX_REPORTED_OUTPUT_TOKENS: u32 = 1_000_000;
pub(crate) const MAX_OUTPUT_TOKEN_CAP: u32 = 32_768;
const MAX_STREAM_EVENTS_PER_OUTPUT_TOKEN: usize = 2;
const MAX_STREAM_FIXED_EVENTS: usize = 2048;
const MIN_STREAM_EVENTS: usize = 8192;
const MAX_STREAM_ENVELOPE_BYTES_PER_EVENT: usize = 512;

fn max_stream_events(max_output_tokens: u32) -> usize {
    (MAX_STREAM_EVENTS_PER_OUTPUT_TOKEN * max_output_tokens.min(MAX_OUTPUT_TOKEN_CAP) as usize
        + MAX_STREAM_FIXED_EVENTS)
        .max(MIN_STREAM_EVENTS)
}

fn max_stream_bytes(max_output_tokens: u32) -> usize {
    MAX_STREAM_ENVELOPE_BYTES_PER_EVENT * max_stream_events(max_output_tokens)
}
pub(crate) const DEFAULT_OUTPUT_TOKEN_CAP: u32 = 4096;
pub(crate) const PROBE_TOTAL_DEADLINE: Duration = Duration::from_secs(180);
pub(crate) const PROBE_OUTPUT_CAP: u32 = 1024;
pub(crate) const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_RESPONSE_JSON_NODES: usize = MAX_RESPONSE_BYTES / 4;
const MAX_RESPONSE_JSON_DEPTH: usize = 64;

fn parse_response_json(bytes: &[u8]) -> Result<Value, crate::tools::ToolError> {
    crate::tools::parse_json_bounded(
        bytes,
        MAX_RESPONSE_BYTES,
        MAX_RESPONSE_JSON_NODES,
        MAX_RESPONSE_JSON_DEPTH,
    )
}

fn outcome_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"outcome": {"anyOf": [
            {"type": "object", "properties": {
                "type": {"type": "string", "enum": ["delegate"]},
                "children": {"type": "array", "items": {
                    "type": "object", "properties": {
                        "objective": {"type": "string"},
                        "profile": {"type": ["string", "null"], "enum": [null], "description": "Eligible admitted profile, or null for the pinned child default."}
                    }, "required": ["objective", "profile"], "additionalProperties": false
                }},
                "agent": {"type": ["string", "null"], "description": "Admitted agent definition name, or null. An editor assignment selects its own exact admitted definition."}
            }, "required": ["type", "children", "agent"], "additionalProperties": false}
        ]}},
        "required": ["outcome"],
        "additionalProperties": false
    })
}

pub(crate) fn compaction_input(request: &CompactionRequest) -> Value {
    let items = request
        .items
        .iter()
        .map(|item| match item {
            CompactionItem::Completed(turn) => {
                json!({"status": "completed", "user": turn.user, "assistant": turn.assistant})
            }
            CompactionItem::Unanswered { user, status } => {
                let status = match status {
                    UnansweredStatus::Failed => "failed",
                    UnansweredStatus::Cancelled => "cancelled",
                    UnansweredStatus::Interrupted => "interrupted",
                    UnansweredStatus::StoppedByHook => "stopped_by_hook",
                };
                json!({"status": status, "user": user})
            }
        })
        .collect::<Vec<_>>();
    let input = json!({"previous_derived_summary": request.previous_summary, "items": items});
    input
}

pub(crate) const MAX_PROVIDER_REQUEST_BYTES: usize = 512 * 1024;
pub(crate) const MAX_WINDOW_REQUEST_BYTES: usize = 1280 * 1024;
const WINDOW_REQUEST_HEADROOM_BYTES: usize = 256 * 1024;
pub const MIN_CONTEXT_WINDOW_TOKENS: u32 = 32_768;
pub const MAX_CONTEXT_WINDOW_TOKENS: u32 = 2_000_000;

/// Admits a context window reported by an authenticated catalog; out-of-range values are absent.
#[must_use]
pub fn admitted_context_window(tokens: u64) -> Option<u32> {
    u32::try_from(tokens)
        .ok()
        .filter(|tokens| (MIN_CONTEXT_WINDOW_TOKENS..=MAX_CONTEXT_WINDOW_TOKENS).contains(tokens))
}

pub(crate) fn request_ceiling(context_window_tokens: Option<u32>) -> usize {
    match context_window_tokens {
        None => MAX_PROVIDER_REQUEST_BYTES,
        Some(_) => (crate::session::content_ceiling(context_window_tokens)
            + WINDOW_REQUEST_HEADROOM_BYTES)
            .min(MAX_WINDOW_REQUEST_BYTES),
    }
}
// JSON-string embedding escapes quotes and backslashes again, doubling their request bytes.
pub(crate) const COMPACTION_INPUT_EXPANSION: usize = 2;
pub(crate) const COMPACTION_REQUEST_ENVELOPE_BYTES: usize = 8 * 1024;

pub(crate) fn compaction_input_bytes(
    request: &CompactionRequest,
) -> Result<usize, serde_json::Error> {
    serde_json::to_vec(&compaction_input(request)).map(|input| input.len())
}

pub const MAX_NATIVE_API_KEY_BYTES: usize = 512;
pub const MAX_ANTHROPIC_WORKSPACE_ID_BYTES: usize = 128;

/// A selected native key and its explicit API workspace, never ambient account state.
#[derive(Clone, Eq, PartialEq)]
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
        ProviderKind::parse(profile).is_some_and(ProviderKind::is_native_api)
            && id.get_version() == Some(Version::SortRand)
    })
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Provenance {
    Native {
        #[serde(deserialize_with = "Option::deserialize")]
        saved_api_account_id: Option<Uuid>,
    },
    Custom(CustomProfileProvenance),
    ChatGpt(ChatGptProvenance),
}

/// Declared ceilings the Engine enforces for every call of one adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallLimits {
    pub concurrency: u8,
    /// Output tokens a primary call requests; children and Tool review request smaller caps.
    pub max_output_tokens: u32,
    pub output_token_bound: OutputTokenBound,
    /// The wall-clock bound for one primary, child or Tool-review call.
    pub deadline: std::time::Duration,
}

impl Default for CallLimits {
    fn default() -> Self {
        Self {
            concurrency: 1,
            max_output_tokens: DEFAULT_OUTPUT_TOKEN_CAP,
            output_token_bound: OutputTokenBound::ProviderEnforced,
            deadline: std::time::Duration::from_secs(120),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderIdentity {
    pub profile: String,
    pub model: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub effort: Option<Effort>,
    pub provenance: Provenance,
    pub limits: CallLimits,
}

impl Provenance {
    pub fn native() -> Self {
        Self::Native {
            saved_api_account_id: None,
        }
    }
}

impl ProviderIdentity {
    pub fn new(
        profile: impl Into<String>,
        model: impl Into<String>,
        provenance: Provenance,
    ) -> Self {
        Self {
            profile: profile.into(),
            model: model.into(),
            effort: None,
            provenance,
            limits: CallLimits::default(),
        }
    }

    pub(crate) fn custom_provenance(&self) -> Option<&CustomProfileProvenance> {
        match &self.provenance {
            Provenance::Custom(provenance) => Some(provenance),
            _ => None,
        }
    }

    pub(crate) fn saved_api_account_id(&self) -> Option<Uuid> {
        match self.provenance {
            Provenance::Native {
                saved_api_account_id,
            } => saved_api_account_id,
            _ => None,
        }
    }

    pub(crate) fn chatgpt_provenance(&self) -> Option<&ChatGptProvenance> {
        match &self.provenance {
            Provenance::ChatGpt(provenance) => Some(provenance),
            _ => None,
        }
    }

    pub(crate) fn ceiling_retry_cause(&self) -> Option<RetryCause> {
        self.chatgpt_provenance().map(|_| RetryCause::StreamCut)
    }

    pub(crate) fn valid(&self) -> bool {
        let provider = ProviderKind::parse(&self.profile);
        let chatgpt = provider == Some(ProviderKind::ChatGpt);
        let limits = &self.limits;
        !self.profile.is_empty()
            && self.profile.len() <= 128
            && !self.model.is_empty()
            && self.model.len() <= 128
            && (provider == Some(ProviderKind::Custom)) == self.custom_provenance().is_some()
            && valid_saved_api_account_id(&self.profile, self.saved_api_account_id())
            && chatgpt == self.chatgpt_provenance().is_some()
            && chatgpt == (limits.output_token_bound == OutputTokenBound::LocalAcceptanceOnly)
            && (!chatgpt || self.effort.is_some() && limits.concurrency == 1)
            && self
                .chatgpt_provenance()
                .is_none_or(ChatGptProvenance::valid)
            && self
                .custom_provenance()
                .is_none_or(CustomProfileProvenance::valid)
            && (1..=8).contains(&limits.concurrency)
            && (1..=MAX_OUTPUT_TOKEN_CAP).contains(&limits.max_output_tokens)
    }
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
    ToolReview,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalContextFacts {
    pub provider_calls: u32,
    pub known_tokens: u64,
    pub token_usage_unknown: bool,
    pub active_ms: u64,
    pub reserved_active_ms: u64,
    pub uncertain_active_ms: u64,
    pub success_criterion_satisfied: Option<bool>,
}

impl GoalContextFacts {
    pub(crate) const fn maximum() -> Self {
        Self {
            provider_calls: u32::MAX,
            known_tokens: u64::MAX,
            token_usage_unknown: true,
            active_ms: u64::MAX,
            reserved_active_ms: u64::MAX,
            uncertain_active_ms: u64::MAX,
            success_criterion_satisfied: Some(true),
        }
    }
}

impl ProviderRequest {
    pub(crate) fn validate_scope(&self) -> Result<(), ProviderError> {
        self.collaboration
            .validate()
            .map_err(|_| ProviderError::Rejected)?;
        let input = &self.input;
        let root = matches!(self.phase, AgentPhase::RootPlan | AgentPhase::RootSynthesis);
        let role_fits = match input.role {
            PromptRole::Primary => root && input.tools.is_some(),
            PromptRole::Editor => self.phase == AgentPhase::ChildWork && input.tools.is_some(),
            PromptRole::Researcher => self.phase == AgentPhase::ChildWork && input.tools.is_some(),
            PromptRole::ReadOnly => self.phase != AgentPhase::ToolReview && input.tools.is_none(),
            PromptRole::ToolReview => self.phase == AgentPhase::ToolReview && input.tools.is_none(),
        };
        if !input.stop_hook_feedback.is_empty()
            && (self.phase == AgentPhase::ToolReview
                || input
                    .stop_hook_feedback
                    .iter()
                    .map(|feedback| feedback.text.len())
                    .sum::<usize>()
                    > crate::tools::permissions::MAX_STOP_FEEDBACK_BYTES
                || input.stop_hook_feedback.iter().any(|feedback| {
                    feedback.after_step > input.tools.as_ref().map_or(0, |tools| tools.steps.len())
                }))
        {
            return Err(ProviderError::Rejected);
        }
        if !role_fits
            || !root && (input.outcome_correction || input.previous_run_failure.is_some())
            || self.phase == AgentPhase::ChildWork
                && (self.collaboration != CollaborationPolicy::Single
                    || input.goal_context_facts.is_some())
            || self.phase == AgentPhase::ToolReview
                && (!input.steering_notes.is_empty()
                    || input.tools.as_ref().is_some_and(|tools| {
                        tools
                            .steps
                            .iter()
                            .any(|step| !step.steering_notes.is_empty())
                    })
                    || input
                        .stop_hook_feedback
                        .iter()
                        .any(|feedback| !feedback.steering_notes.is_empty()))
            || !root && !input.assignment_profiles.is_empty()
            || input.assignment_profiles.len() > MAX_ASSIGNMENT_PROFILES
            || input
                .assignment_profiles
                .iter()
                .enumerate()
                .any(|(index, name)| {
                    !valid_profile_name(name) || input.assignment_profiles[..index].contains(name)
                })
        {
            return Err(ProviderError::Rejected);
        }
        Ok(())
    }
}

pub(crate) fn restrict_outcome_schema(
    schema: serde_json::Value,
    request: &ProviderRequest,
) -> serde_json::Value {
    let phases = if request.phase == AgentPhase::RootSynthesis
        && request
            .input
            .tools
            .as_ref()
            .and_then(|t| serde_json::from_str::<Value>(t.catalog()).ok())
            .is_some_and(|c| {
                c["agents"].as_array().is_some_and(|agents| {
                    agents
                        .iter()
                        .any(|a| a["editor"] == true && a["supported"] == true)
                })
            }) {
        &[AgentPhase::RootPlan, AgentPhase::RootSynthesis][..]
    } else {
        std::slice::from_ref(&request.phase)
    };
    outcome_schema_for_phases(
        assignment_schema(schema, request),
        request.collaboration,
        phases,
    )
}

pub(crate) fn run_outcome_schema(request: &ProviderRequest) -> Value {
    let phases = if request.input.role == PromptRole::Primary {
        &[AgentPhase::RootPlan, AgentPhase::RootSynthesis][..]
    } else {
        std::slice::from_ref(&request.phase)
    };
    outcome_schema_for_phases(
        assignment_schema(outcome_schema(), request),
        request.collaboration,
        phases,
    )
}

fn assignment_schema(mut schema: Value, request: &ProviderRequest) -> Value {
    let mut profiles: Vec<Value> = request
        .input
        .assignment_profiles
        .iter()
        .map(|name| json!(name))
        .collect();
    profiles.push(Value::Null);
    schema["properties"]["outcome"]["anyOf"][0]["properties"]["children"]["items"]["properties"]
        ["profile"]["enum"] = json!(profiles);
    if request.input.role == PromptRole::Primary
        && let Some(tools) = &request.input.tools
        && let Ok(catalog) = serde_json::from_str::<Value>(tools.catalog())
        && let Some(agents) = catalog["agents"].as_array()
    {
        let calls = crate::tools::catalog::catalog_call_schemas(&catalog);
        let contract =
            crate::tools::catalog::plan_schema(&calls, &crate::session::GoalBudget::default());
        let criterion = contract["properties"]["success_criterion"].clone();
        let items =
            &mut schema["properties"]["outcome"]["anyOf"][0]["properties"]["children"]["items"];
        let mut branches = vec![items.clone()];
        for agent in agents
            .iter()
            .filter(|agent| agent["editor"] == true && agent["supported"] == true)
        {
            let modes = if agent["background"] == true {
                json!(["background"])
            } else {
                json!(["foreground", "background"])
            };
            branches.push(json!({
                "type":"object", "properties": {
                    "objective":{"type":"string", "description":"Repeat editor.mission exactly."},
                    "profile":{"type":"null"},
                    "editor":{"type":"object", "properties": {
                        "definition":{"type":"string","enum":[agent["name"]]},
                        "definition_digest":{"type":"array","items":{"type":"integer"},"description":format!("Exact admitted digest: {}",agent["digest"])},
                        "mission":{"type":"string"}, "direction":{"type":"string"},
                        "commands":{"type":"array","maxItems":8,"items":{"type":"string","maxLength":64},"description":"Sorted unique admitted offline commands for the private worker view."},
                        "lease":{"type":"array","items":{"type":"string"},"description":"Sorted nonoverlapping ASCII relative files or subtrees within the Goal plan and parent grant."},
                        "criterion":criterion,
                        "mode":{"type":"string","enum":modes},
                        "max_turns":{"type":"integer","description":format!("At most {}",agent["max_turns"])},
                        "wave":{"type":"integer","minimum":1,"maximum":8},
                        "resumes":{"type":["string", "null"], "description":"Exact terminal historical child ID in this Goal for a fresh assignment with bounded source context; never resumes private state or authority."}, "supersedes":{"type":["string", "null"]}
                    }, "required":["definition","definition_digest","mission","direction","lease","commands","criterion","mode","max_turns","wave","resumes","supersedes"],"additionalProperties":false}
                },"required":["objective","profile","editor"],"additionalProperties":false
            }));
        }
        if branches.len() > 1 {
            *items = json!({"anyOf":branches});
        }
    }
    schema
}

fn outcome_schema_for_phases(
    mut schema: Value,
    collaboration: CollaborationPolicy,
    phases: &[AgentPhase],
) -> Value {
    let branches = schema["properties"]["outcome"]["anyOf"]
        .as_array_mut()
        .expect("compiled outcome branches");
    branches.retain(|branch| {
        phases.iter().any(
            |phase| match branch["properties"]["type"]["enum"][0].as_str() {
                Some("delegate") => {
                    *phase == AgentPhase::RootPlan && collaboration.max_children() > 0
                }
                _ => false,
            },
        )
    });
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
    StoppedByHook,
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

/// Provider-reported token usage. Cache fields are subsets of `input_tokens`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub cache_read_input_tokens: Option<u32>,
    pub cache_creation_input_tokens: Option<u32>,
}

impl Usage {
    pub(crate) fn cache_consistent(self) -> bool {
        match self.input_tokens {
            None => {
                self.cache_read_input_tokens.is_none() && self.cache_creation_input_tokens.is_none()
            }
            Some(input) => self
                .cache_read_input_tokens
                .unwrap_or(0)
                .checked_add(self.cache_creation_input_tokens.unwrap_or(0))
                .is_some_and(|cached| cached <= input),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionResponse {
    pub summary: String,
    pub response_id: Option<String>,
    pub usage: Usage,
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
pub struct Answer {
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildAssignment {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor: Option<Box<crate::session::worker::EditorAssignment>>,
    pub objective: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub profile: Option<String>,
}

const MAX_CHILD_OBJECTIVE_BYTES: usize = 2 * 1024;
const MAX_CHILD_ASSIGNMENTS: usize = 8;
const MAX_DELEGATION_BYTES: usize = 24 * 1024;
const MAX_ASSIGNMENT_PROFILES: usize = 8;

impl ChildAssignment {
    pub(crate) fn valid(&self) -> bool {
        !self.objective.trim().is_empty()
            && self.objective.len() <= MAX_CHILD_OBJECTIVE_BYTES
            && self.profile.as_deref().is_none_or(valid_profile_name)
            && self.editor.as_ref().is_none_or(|editor| {
                self.profile.is_none()
                    && self.objective == editor.mission
                    && crate::session::worker::ChildAssignment::Editor {
                        assignment: editor.clone(),
                    }
                    .valid()
            })
    }
}

pub(crate) fn valid_assignments(children: &[ChildAssignment], agent: Option<&str>) -> bool {
    (1..=MAX_CHILD_ASSIGNMENTS).contains(&children.len())
        && children.iter().all(ChildAssignment::valid)
        && serde_json::to_vec(
            &json!({"outcome": {"type": "delegate", "children": children, "agent": agent}}),
        )
        .is_ok_and(|bytes| bytes.len() <= MAX_DELEGATION_BYTES)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Delegate {
    pub agent: Option<String>,
    pub children: Vec<ChildAssignment>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanRequest {
    pub id: ToolUseId,
    pub contract: crate::session::GoalPlanContract,
}

impl PlanRequest {
    pub(crate) fn result(&self) -> Value {
        json!({"status":"plan_recorded", "scope": {
            "paths": self.contract.paths, "commands": self.contract.commands,
            "skills": self.contract.skills, "mcp_tools": self.contract.mcp_tools
        }})
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewVerdict {
    Approve,
    Ask,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderOutcome {
    Plan(Box<PlanRequest>),
    Answer(Answer),
    Review(ReviewVerdict),
    Delegate(Delegate),
    /// Non-empty, in model order; the Engine accepts exactly one per step.
    Tools(Vec<crate::tools::ToolRequest>),
}

/// Accepted wire condition and local request control, not a remote retention guarantee.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderWireProvenance {
    ResponsesCompletedStoreFalseRequested,
    MessagesEndTurnStorageUnspecified,
    MessagesToolUseStorageUnspecified,
}

impl ProviderWireProvenance {
    pub(crate) fn valid_for(
        self,
        response_id: Option<&str>,
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
    ) -> bool {
        response_id.is_some()
            && (self == Self::ResponsesCompletedStoreFalseRequested
                || input_tokens.is_some() && output_tokens.is_some())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderResponse {
    pub outcome: ProviderOutcome,
    /// The exact assistant output, replayed by the next step of this live Run only.
    pub turn: ProviderTurn,
    pub response_id: Option<String>,
    pub usage: Usage,
    pub wire_provenance: Option<ProviderWireProvenance>,
}

impl ProviderResponse {
    pub(crate) fn reflects_secret(&self, secret: &str) -> bool {
        secret.is_empty()
            || self
                .response_id
                .as_ref()
                .is_some_and(|id| id.contains(secret))
            || self.turn.reflects_secret(secret)
            || match &self.outcome {
                ProviderOutcome::Answer(value) => value.text.contains(secret),
                ProviderOutcome::Review(_) => false,
                ProviderOutcome::Delegate(value) => {
                    value
                        .agent
                        .as_ref()
                        .is_some_and(|name| name.contains(secret))
                        || value.children.iter().any(|child| {
                            child.objective.contains(secret)
                                || child.editor.as_ref().is_some_and(|editor| {
                                    serde_json::to_value(editor).is_ok_and(|value| {
                                        streaming::reflects_secret(&value, secret)
                                    })
                                })
                                || child
                                    .profile
                                    .as_ref()
                                    .is_some_and(|name| name.contains(secret))
                        })
                }
                ProviderOutcome::Plan(plan) => {
                    plan.id.as_str().contains(secret)
                        || serde_json::to_value(&plan.contract)
                            .is_ok_and(|value| streaming::reflects_secret(&value, secret))
                }
                ProviderOutcome::Tools(requests) => requests
                    .iter()
                    .any(|request| tool_request_reflects_secret(request, secret)),
            }
    }
}

fn tool_request_reflects_secret(request: &crate::tools::ToolRequest, secret: &str) -> bool {
    use streaming::reflects_secret as value_reflects_secret;
    request.id.as_str().contains(secret)
        || serde_json::to_value(&request.call)
            .is_ok_and(|value| value_reflects_secret(&value, secret))
        || if let crate::tools::ToolCall::McpCall { arguments, .. } = &request.call {
            crate::tools::types::parse_mcp_arguments(arguments)
                .map_or(true, |value| value_reflects_secret(&value, secret))
        } else {
            false
        }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("Provider unavailable")]
    Unavailable,
    // HTTP 5xx at status admission, with the server's bounded delta-seconds Retry-After.
    #[error("Provider server unavailable before response admission")]
    HttpServerUnavailable(Option<Duration>),
    // A subscription route's 5xx keeps its status, its reason and the server's Retry-After.
    #[error("Provider returned HTTP {0}")]
    RemoteServerError(u16, Option<Duration>),
    // The request could not be sent or answered before any response; nothing was decoded.
    #[error("Provider connection failed before a response")]
    Transport,
    #[error("Provider transport failed after response admission")]
    PartialTransport,
    #[error("Provider call timed out")]
    CallTimedOut,
    #[error("Provider rejected the request")]
    Rejected,
    #[error("Provider returned an invalid outcome")]
    InvalidOutcome,
    #[error("Provider stream was incomplete or malformed")]
    InvalidStream,
    // A well-formed stream that ended cleanly before its terminal event.
    #[error("Provider stream ended before a complete outcome")]
    StreamEnded,
    #[error("Provider response did not match the selected response contract")]
    InvalidResponseContract,
    #[error("Provider output did not match Arany's structured outcome contract")]
    InvalidOutcomeContract,
    #[error("Provider reported output above Arany's local token limit")]
    LocalOutputLimit,
    // HTTP 429 at status admission, with the server's bounded delta-seconds Retry-After.
    #[error("Provider returned HTTP 429")]
    RateLimited(Option<Duration>),
    // HTTP 429 whose bounded body names an exhausted quota or plan limit.
    #[error("Provider returned HTTP 429")]
    QuotaExceeded,
    // Only status-check failures, before any response body is admitted.
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
    ProposalRefused,
    AccountAccess,
    UsageLimit,
    RateLimited,
    UsageTemporarilyUnavailable,
    ServiceUnavailable,
    StreamProtocol,
    OutcomeDecode,
    ResponseContract,
    OutcomeContract,
    LocalOutputLimit,
    WorkerAnswerDeferred,
    WorkerDelegateRefused,
    WorkerToolRefused,
    CriterionNotMetInView,
}

impl ProviderError {
    // Only a server failure at status admission may switch to a fallback route.
    #[cfg(test)]
    pub(crate) fn allows_fallback(&self) -> bool {
        self.retry_cause()
            .is_some_and(|(cause, _)| cause == RetryCause::ServerError)
    }

    // Only failures before a decoded outcome qualify: a send failure, a 429 or 5xx at status
    // admission, or a body or stream that ends early. A quota 429 does not recover by waiting.
    pub(crate) fn retry_cause(&self) -> Option<(RetryCause, Option<Duration>)> {
        match self {
            Self::Transport => Some((RetryCause::Transport, None)),
            Self::RateLimited(after) => Some((RetryCause::RateLimited, *after)),
            Self::RemoteHttp(429) => Some((RetryCause::RateLimited, None)),
            Self::HttpServerUnavailable(after) | Self::RemoteServerError(_, after) => {
                Some((RetryCause::ServerError, *after))
            }
            Self::RemoteHttp(500..=599) => Some((RetryCause::ServerError, None)),
            Self::PartialTransport | Self::StreamEnded => Some((RetryCause::StreamCut, None)),
            _ => None,
        }
    }

    pub(crate) fn failure_reason(&self) -> Option<ProviderFailureReason> {
        match self {
            Self::InvalidStream | Self::StreamEnded => Some(ProviderFailureReason::StreamProtocol),
            Self::InvalidOutcome => Some(ProviderFailureReason::OutcomeDecode),
            Self::InvalidResponseContract => Some(ProviderFailureReason::ResponseContract),
            Self::InvalidOutcomeContract => Some(ProviderFailureReason::OutcomeContract),
            Self::LocalOutputLimit => Some(ProviderFailureReason::LocalOutputLimit),
            Self::RemoteHttp(401 | 403) => Some(ProviderFailureReason::AccountAccess),
            Self::QuotaExceeded => Some(ProviderFailureReason::UsageLimit),
            Self::RateLimited(_) | Self::RemoteHttp(429) => {
                Some(ProviderFailureReason::RateLimited)
            }
            Self::RemoteHttp(500..=599) | Self::RemoteServerError(..) => {
                Some(ProviderFailureReason::ServiceUnavailable)
            }
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
            Self::Unavailable
            | Self::HttpServerUnavailable(_)
            | Self::RemoteServerError(..)
            | Self::Transport
            | Self::PartialTransport
            | Self::CallTimedOut
            | Self::RemoteHttp(500..=599) => ProviderFailureClass::Unavailable,
            Self::RemoteStreamCode(_)
                if self.failure_reason()
                    == Some(ProviderFailureReason::UsageTemporarilyUnavailable) =>
            {
                ProviderFailureClass::Unavailable
            }
            Self::Rejected
            | Self::RateLimited(_)
            | Self::QuotaExceeded
            | Self::RemoteHttp(400..=499)
            | Self::RemoteStreamCode(_) => ProviderFailureClass::Rejected,
            Self::InvalidOutcome
            | Self::InvalidStream
            | Self::StreamEnded
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

impl OutputTokenBound {
    // A local-acceptance adapter checks visible output itself; its reported total also counts
    // reasoning, so only the reporting ceiling bounds it here.
    pub(crate) fn admits_reported_output(self, output_tokens: Option<u32>, cap: u32) -> bool {
        output_tokens.is_none_or(|count| match self {
            Self::ProviderEnforced => count <= cap,
            Self::LocalAcceptanceOnly => count <= MAX_REPORTED_OUTPUT_TOKENS,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatGptProvenance {
    pub account_id: Uuid,
    pub evidence_fingerprint: [u8; 32],
    pub admission: ChatGptAdmission,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatGptAdmission {
    AccountConsent,
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
            && self.capability_evidence_version == 2
    }
}

/// One resolved inference adapter. Its identity (profile, model, effort, provenance and
/// call limits) remains immutable while the Engine uses this value.
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
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn identity(&self) -> &ProviderIdentity;

    /// The input window from this route's authenticated catalog evidence; `None` keeps the default bounds.
    fn context_window_tokens(&self) -> Option<u32> {
        None
    }

    /// Refreshes this route's window evidence for a new Run admission.
    async fn context_window_tokens_for_run(&self) -> Option<u32> {
        self.context_window_tokens()
    }

    /// Returns one complete semantic outcome or an error, never an accepted partial result.
    /// The Engine independently validates its phase, bounds and durable transition.
    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError>;

    async fn invoke_with_preview(
        &self,
        request: ProviderRequest,
        _preview: ResponsePreview,
    ) -> Result<ProviderResponse, ProviderError> {
        self.invoke(request).await
    }

    async fn invoke_observed(
        &self,
        request: ProviderRequest,
        preview: Option<ResponsePreview>,
        _limits: RateLimitSlot,
    ) -> Result<ProviderResponse, ProviderError> {
        match preview {
            Some(preview) => self.invoke_with_preview(request, preview).await,
            None => self.invoke(request).await,
        }
    }

    async fn compact_observed(
        &self,
        request: CompactionRequest,
        _limits: RateLimitSlot,
    ) -> Result<CompactionResponse, ProviderError> {
        self.compact(request).await
    }

    /// Summarizes accepted conversation data without Workspace guidance or effects.
    /// Unsupported compaction returns `Unavailable`. Adapters enforce the declared
    /// output bound; a local-only bound requires explicit account consent and cannot
    /// guarantee a remote generation cap. Errors never publish a partial summary.
    async fn compact(
        &self,
        _request: CompactionRequest,
    ) -> Result<CompactionResponse, ProviderError> {
        Err(ProviderError::Unavailable)
    }
}

#[async_trait::async_trait]
impl<T: Provider + ?Sized> Provider for std::sync::Arc<T> {
    fn identity(&self) -> &ProviderIdentity {
        (**self).identity()
    }
    fn context_window_tokens(&self) -> Option<u32> {
        (**self).context_window_tokens()
    }
    async fn context_window_tokens_for_run(&self) -> Option<u32> {
        (**self).context_window_tokens_for_run().await
    }
    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        (**self).invoke(request).await
    }
    async fn invoke_with_preview(
        &self,
        request: ProviderRequest,
        preview: ResponsePreview,
    ) -> Result<ProviderResponse, ProviderError> {
        (**self).invoke_with_preview(request, preview).await
    }
    async fn invoke_observed(
        &self,
        request: ProviderRequest,
        preview: Option<ResponsePreview>,
        limits: RateLimitSlot,
    ) -> Result<ProviderResponse, ProviderError> {
        (**self).invoke_observed(request, preview, limits).await
    }
    async fn compact_observed(
        &self,
        request: CompactionRequest,
        limits: RateLimitSlot,
    ) -> Result<CompactionResponse, ProviderError> {
        (**self).compact_observed(request, limits).await
    }
    async fn compact(
        &self,
        request: CompactionRequest,
    ) -> Result<CompactionResponse, ProviderError> {
        (**self).compact(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::AgentRunId;

    #[test]
    fn provider_kind_exposes_names_and_extracts_custom_profiles() {
        assert_eq!(
            ProviderKind::NAMES,
            ["openai", "anthropic", "chatgpt", "custom:"]
        );
        assert_eq!(ProviderKind::parse("custom:"), Some(ProviderKind::Custom));
        assert_eq!(ProviderKind::custom_name("custom:"), Some(""));
        assert_eq!(ProviderKind::custom_name("custom:local"), Some("local"));
        assert_eq!(ProviderKind::custom_name("local"), None);
    }

    #[test]
    fn only_chatgpt_ceiling_timeouts_are_retryable_stream_cuts() {
        for (profile, provenance, expected) in [
            ("openai", Provenance::native(), None),
            ("anthropic", Provenance::native(), None),
            (
                "custom:local",
                Provenance::Custom(CustomProfileProvenance {
                    endpoint: "https://example.com/v1/responses".into(),
                    evidence_digest: [7; 32],
                    capability_evidence_version: 2,
                }),
                None,
            ),
            (
                "chatgpt",
                Provenance::ChatGpt(ChatGptProvenance {
                    account_id: Uuid::now_v7(),
                    evidence_fingerprint: [7; 32],
                    admission: ChatGptAdmission::AccountConsent,
                }),
                Some(RetryCause::StreamCut),
            ),
        ] {
            let identity = ProviderIdentity::new(profile, "model-a", provenance);
            assert_eq!(identity.ceiling_retry_cause(), expected, "{profile}");
        }
    }

    #[test]
    fn stream_bounds_follow_the_declared_output_cap() {
        for cap in [
            1,
            PROBE_OUTPUT_CAP,
            DEFAULT_OUTPUT_TOKEN_CAP,
            MAX_OUTPUT_TOKEN_CAP,
        ] {
            let events = (MAX_STREAM_EVENTS_PER_OUTPUT_TOKEN * cap as usize
                + MAX_STREAM_FIXED_EVENTS)
                .max(MIN_STREAM_EVENTS);
            assert_eq!(max_stream_events(cap), events);
            assert_eq!(
                max_stream_bytes(cap),
                MAX_STREAM_ENVELOPE_BYTES_PER_EVENT * events
            );
        }
        assert_eq!(max_stream_events(1), MIN_STREAM_EVENTS);
        assert_eq!(max_stream_events(PROBE_OUTPUT_CAP), MIN_STREAM_EVENTS);
        assert!(max_stream_bytes(PROBE_OUTPUT_CAP) < max_stream_bytes(DEFAULT_OUTPUT_TOKEN_CAP));
        assert!(
            max_stream_bytes(DEFAULT_OUTPUT_TOKEN_CAP) < max_stream_bytes(MAX_OUTPUT_TOKEN_CAP)
        );
        assert_eq!(
            max_stream_events(u32::MAX),
            max_stream_events(MAX_OUTPUT_TOKEN_CAP)
        );
        assert_eq!(
            max_stream_bytes(u32::MAX),
            max_stream_bytes(MAX_OUTPUT_TOKEN_CAP)
        );
    }

    #[test]
    fn output_cap_covers_tool_content_and_answer_headroom() {
        assert!(
            crate::tools::types::MAX_EDIT_CONTENT_BYTES.div_ceil(3)
                + DEFAULT_OUTPUT_TOKEN_CAP as usize
                <= MAX_OUTPUT_TOKEN_CAP as usize
        );
    }

    fn request(
        role: PromptRole,
        phase: AgentPhase,
        tools: Option<crate::tools::ToolContext>,
    ) -> ProviderRequest {
        ProviderRequest {
            session_id: crate::session::SessionId::new(),
            run_id: RunId::new(),
            agent_run_id: AgentRunId::new(),
            phase,
            collaboration: CollaborationPolicy::Single,
            model: "scope-model".into(),
            max_output_tokens: 256,
            input: RunInput {
                tools,
                ..RunInput::new(role, "objective".into())
            },
        }
    }

    fn context() -> crate::tools::ToolContext {
        crate::tools::ToolContext::new(
            json!({"write": false, "commands": [], "skills": [], "mcp_servers": [], "tool_calls_remaining": 2}),
        )
    }

    #[test]
    fn editor_delegate_schema_does_not_change_when_the_plan_contract_arrives() {
        let mut before = test_support::primary_request("scope-model");
        before.collaboration = CollaborationPolicy::Auto {
            max_active_children: 3,
        };
        let tools = before.input.tools.as_mut().unwrap();
        let mut catalog: Value = serde_json::from_str(tools.catalog()).unwrap();
        catalog["agents"] = json!([{"name": "editor", "editor": true, "supported": true, "digest": "d", "max_turns": 4}]);
        tools.set_catalog(&catalog);
        let mut after = before.clone();
        after.input.goal_plan_contract = Some(test_support::goal_plan_contract());
        assert_eq!(run_outcome_schema(&before), run_outcome_schema(&after));
    }

    #[test]
    fn fallback_requires_a_server_failure_at_the_http_status_stage() {
        for (limit, reason) in [
            (
                ProviderError::RateLimited(None),
                ProviderFailureReason::RateLimited,
            ),
            (
                ProviderError::QuotaExceeded,
                ProviderFailureReason::UsageLimit,
            ),
        ] {
            assert_eq!(limit.failure_class(), ProviderFailureClass::Rejected);
            assert_eq!(limit.failure_reason(), Some(reason));
            assert!(!limit.allows_fallback());
        }
        assert_eq!(
            ProviderError::HttpServerUnavailable(None).failure_class(),
            ProviderError::Unavailable.failure_class()
        );
        assert_eq!(
            ProviderError::HttpServerUnavailable(None).failure_reason(),
            ProviderError::Unavailable.failure_reason()
        );
        for (error, eligible) in [
            (ProviderError::HttpServerUnavailable(None), true),
            (ProviderError::RemoteHttp(500), true),
            (ProviderError::RemoteHttp(503), true),
            (ProviderError::RemoteHttp(529), true),
            (ProviderError::RemoteHttp(400), false),
            (ProviderError::RemoteHttp(401), false),
            (ProviderError::RemoteHttp(402), false),
            (ProviderError::RemoteHttp(413), false),
            (ProviderError::RemoteHttp(429), false),
            (ProviderError::RateLimited(None), false),
            (ProviderError::Transport, false),
            (ProviderError::StreamEnded, false),
            (ProviderError::Unavailable, false),
            (ProviderError::PartialTransport, false),
            (ProviderError::CallTimedOut, false),
            (ProviderError::Rejected, false),
            (
                ProviderError::RemoteStreamCode("subscription_sharing_usage_unavailable".into()),
                false,
            ),
        ] {
            assert_eq!(error.allows_fallback(), eligible, "{error:?}");
        }
    }

    #[test]
    fn only_failures_before_a_decoded_outcome_retry_and_quota_bodies_never_do() {
        let headers = |value: &str| {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::RETRY_AFTER,
                reqwest::header::HeaderValue::from_str(value).unwrap(),
            );
            headers
        };
        for (value, after) in [
            ("7", Some(Duration::from_secs(7))),
            ("0", Some(Duration::ZERO)),
            ("9999999999", Some(Duration::from_secs(9_999_999_999))),
            ("99999999999", None),
            ("-1", None),
            ("1.5", None),
            ("Wed, 21 Oct 2026 07:28:00 GMT", None),
            ("", None),
        ] {
            assert_eq!(retry_after(&headers(value)), after, "{value}");
        }
        for (body, quota) in [
            (
                br#"{"error":{"code":"insufficient_quota"}}"#.as_slice(),
                true,
            ),
            (br#"{"error":{"type":"usage_limit_reached"}}"#, true),
            (br#"{"error":{"code":"usage_not_included"}}"#, true),
            (br#"{"error":{"code":"credit_balance_exhausted"}}"#, true),
            (
                br#"{"error":{"code":"organization_spend_limit_exceeded"}}"#,
                true,
            ),
            (
                br#"{"error":{"code":"project_spend_limit_exceeded"}}"#,
                true,
            ),
            (
                br#"{"error":{"code":"organization_usage_limit_exceeded"}}"#,
                true,
            ),
            (
                br#"{"error":{"type":"rate_limit_error","code":"slow_down"}}"#,
                false,
            ),
            (br#"{"error":{"code":"rate_limit_exceeded"}}"#, false),
            (
                br#"{"type":"error","error":{"type":"rate_limit_error"}}"#,
                false,
            ),
            (br#"{"detail":"insufficient_quota"}"#, false),
            (b"insufficient_quota", false),
            (b"", false),
        ] {
            assert_eq!(quota_exhausted(body), quota);
        }
        for (error, cause) in [
            (ProviderError::Transport, Some(RetryCause::Transport)),
            (
                ProviderError::RateLimited(Some(Duration::from_secs(3))),
                Some(RetryCause::RateLimited),
            ),
            (ProviderError::QuotaExceeded, None),
            (ProviderError::StreamEnded, Some(RetryCause::StreamCut)),
            (
                ProviderError::RemoteHttp(429),
                Some(RetryCause::RateLimited),
            ),
            (
                ProviderError::RemoteHttp(500),
                Some(RetryCause::ServerError),
            ),
            (
                ProviderError::HttpServerUnavailable(None),
                Some(RetryCause::ServerError),
            ),
            (ProviderError::PartialTransport, Some(RetryCause::StreamCut)),
            (ProviderError::Unavailable, None),
            (ProviderError::CallTimedOut, None),
            (ProviderError::InvalidStream, None),
            (ProviderError::Rejected, None),
            (ProviderError::RemoteHttp(403), None),
            (
                ProviderError::RemoteStreamCode("subscription_sharing_usage_unavailable".into()),
                None,
            ),
        ] {
            assert_eq!(
                error.retry_cause().map(|(cause, _)| cause),
                cause,
                "{error:?}"
            );
        }
    }

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
                Some(ProviderFailureReason::RateLimited),
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
                ProviderError::InvalidOutcome,
                ProviderFailureReason::OutcomeDecode,
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
            outcome: ProviderOutcome::Answer(Answer {
                text: "safe".into(),
            }),
            turn: ProviderTurn::scripted(),
            response_id: Some("safe-id".into()),
            usage: Usage {
                input_tokens: Some(1),
                output_tokens: Some(1),
                ..Usage::default()
            },
            wire_provenance: None,
        };
        assert!(!response.reflects_secret(secret));
        response.response_id = Some(format!("id-{secret}"));
        assert!(response.reflects_secret(secret));
        response.response_id = None;
        response.outcome = ProviderOutcome::Answer(Answer {
            text: "safe".into(),
        });
        assert!(!response.reflects_secret(secret));
        response.outcome = ProviderOutcome::Answer(Answer {
            text: secret.into(),
        });
        assert!(response.reflects_secret(secret));
        response.outcome = ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![crate::ChildAssignment {
                editor: None,
                objective: format!("child {secret}"),
                profile: None,
            }],
        });
        assert!(response.reflects_secret(secret));
        response.outcome = ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![ChildAssignment {
                editor: None,
                objective: "safe".into(),
                profile: Some(secret.into()),
            }],
        });
        assert!(response.reflects_secret(secret));
        let tool = |contract: Option<crate::session::GoalPlanContract>| {
            ProviderOutcome::Plan(Box::new(PlanRequest {
                id: ToolUseId::new("plan_1").unwrap(),
                contract: contract.unwrap(),
            }))
        };
        let contract = |mission: String| crate::session::GoalPlanContract {
            mission,
            paths: vec!["src".into()],
            commands: Vec::new(),
            resource_class: crate::tools::ToolResourceClass::Default,
            command_timeout_ms: None,
            skills: vec![crate::session::GoalSkillBinding {
                name: format!("skill-{secret}"),
                resource: None,
            }],
            mcp_tools: Vec::new(),
            success_criterion: crate::session::GoalSuccessCriterion::WorkspaceFileContent {
                path: "src/main.rs".into(),
                content: "safe".into(),
            },
            budget: crate::session::GoalBudget::default(),
        };
        response.outcome = tool(Some(contract(format!("verified with {secret}"))));
        assert!(response.reflects_secret(secret));
        response.outcome = tool(Some(contract("safe".into())));
        assert!(
            response.reflects_secret(secret),
            "a skill name carries the secret"
        );
        response.outcome = ProviderOutcome::Tools(vec![crate::tools::ToolRequest {
            id: ToolUseId::new("call_1").unwrap(),
            call: crate::tools::ToolCall::Write {
                path: "src/main.rs".into(),
                expected_digest: None,
                content: "safe".into(),
            },
        }]);
        assert!(!response.reflects_secret(secret));
        let ProviderOutcome::Tools(requests) = &mut response.outcome else {
            unreachable!("scripted Tool outcome");
        };
        requests[0].id = ToolUseId::new(format!("id-{secret}")).unwrap();
        assert!(
            response.reflects_secret(secret),
            "a Tool call id carries the secret"
        );
        response.outcome = ProviderOutcome::Answer(Answer {
            text: "safe".into(),
        });
        response.turn = ProviderTurn::new(
            "openai",
            &format!(r#"[{{"type":"reasoning","summary":"{secret}"}}]"#),
            "other",
            4096,
        )
        .unwrap();
        assert!(
            response.reflects_secret(secret),
            "the replayed turn carries the secret"
        );
        response.turn = ProviderTurn::new(
            "openai",
            r#"[{"type":"reasoning","summary":"\u0073ynthetic-secret"}]"#,
            "other",
            4096,
        )
        .unwrap();
        assert!(
            response.reflects_secret(secret),
            "an escaped secret in the decoded turn is caught"
        );

        let mut compact = CompactionResponse {
            summary: "safe".into(),
            response_id: None,
            usage: Usage {
                input_tokens: Some(1),
                output_tokens: Some(1),
                ..Usage::default()
            },
            wire_provenance: None,
        };
        assert!(!compact.reflects_secret(secret));
        compact.summary = secret.into();
        assert!(compact.reflects_secret(secret));
        compact.summary = "safe".into();
        compact.response_id = Some(secret.into());
        assert!(compact.reflects_secret(secret));
    }

    #[test]
    fn role_scope_admits_only_matching_phase_and_tools() {
        let mut researcher = request(
            PromptRole::Researcher,
            AgentPhase::ChildWork,
            Some(context()),
        );
        assert!(researcher.validate_scope().is_ok());
        researcher.input.goal_context_facts = Some(GoalContextFacts::maximum());
        assert!(researcher.validate_scope().is_err());
        researcher.input.goal_context_facts = None;
        researcher.collaboration = CollaborationPolicy::Auto {
            max_active_children: 1,
        };
        assert!(researcher.validate_scope().is_err());
        researcher.collaboration = CollaborationPolicy::Single;
        researcher.input.assignment_profiles = vec!["inspect".into()];
        assert!(researcher.validate_scope().is_err());
        researcher.input.assignment_profiles.clear();
        researcher.input.steering_notes = vec!["note".into()];
        assert!(researcher.validate_scope().is_ok());

        for (role, phase, tools, fits) in [
            (
                PromptRole::Primary,
                AgentPhase::RootPlan,
                Some(context()),
                true,
            ),
            (
                PromptRole::Primary,
                AgentPhase::RootSynthesis,
                Some(context()),
                true,
            ),
            (PromptRole::Primary, AgentPhase::RootPlan, None, false),
            (
                PromptRole::Primary,
                AgentPhase::ChildWork,
                Some(context()),
                false,
            ),
            (
                PromptRole::Researcher,
                AgentPhase::RootPlan,
                Some(context()),
                false,
            ),
            (PromptRole::ReadOnly, AgentPhase::RootPlan, None, true),
            (PromptRole::ReadOnly, AgentPhase::ChildWork, None, true),
            (
                PromptRole::ReadOnly,
                AgentPhase::RootPlan,
                Some(context()),
                false,
            ),
            (PromptRole::ReadOnly, AgentPhase::ToolReview, None, false),
            (PromptRole::ToolReview, AgentPhase::ToolReview, None, true),
            (PromptRole::ToolReview, AgentPhase::RootPlan, None, false),
            (
                PromptRole::ToolReview,
                AgentPhase::ToolReview,
                Some(context()),
                false,
            ),
        ] {
            assert_eq!(
                request(role, phase, tools).validate_scope().is_ok(),
                fits,
                "{role:?} {phase:?}"
            );
        }
    }

    #[test]
    fn steering_notes_are_allowed_in_work_roles_and_excluded_from_review() {
        let mut request = request(PromptRole::ReadOnly, AgentPhase::RootPlan, None);
        request.input.steering_notes = vec!["note".into()];
        assert!(request.validate_scope().is_ok());
        request.phase = AgentPhase::RootSynthesis;
        assert!(request.validate_scope().is_ok());
        request.phase = AgentPhase::ChildWork;
        assert!(request.validate_scope().is_ok());
        request.phase = AgentPhase::ToolReview;
        request.input.role = PromptRole::ToolReview;
        assert!(request.validate_scope().is_err());
        request.input.steering_notes.clear();
        assert!(request.validate_scope().is_ok());
    }

    #[test]
    fn delegation_schema_follows_phase_and_policy() {
        let mut request = request(PromptRole::Primary, AgentPhase::RootPlan, Some(context()));
        for (collaboration, phase, expected) in [
            (CollaborationPolicy::Single, AgentPhase::RootPlan, vec![]),
            (
                CollaborationPolicy::Auto {
                    max_active_children: 0,
                },
                AgentPhase::RootPlan,
                vec![],
            ),
            (
                CollaborationPolicy::Auto {
                    max_active_children: 3,
                },
                AgentPhase::RootPlan,
                vec!["delegate"],
            ),
            (
                CollaborationPolicy::Team {
                    max_active_children: 3,
                },
                AgentPhase::RootPlan,
                vec!["delegate"],
            ),
            (
                CollaborationPolicy::Team {
                    max_active_children: 3,
                },
                AgentPhase::RootSynthesis,
                vec![],
            ),
        ] {
            request.collaboration = collaboration;
            request.phase = phase;
            let schema = restrict_outcome_schema(outcome_schema(), &request);
            let offered: Vec<_> = schema["properties"]["outcome"]["anyOf"]
                .as_array()
                .unwrap()
                .iter()
                .map(|branch| branch["properties"]["type"]["enum"][0].as_str().unwrap())
                .collect();
            assert_eq!(offered, expected, "{collaboration:?} {phase:?}");
        }
    }
}

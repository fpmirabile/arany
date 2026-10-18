use crate::session::{CollaborationPolicy, RunId, SessionId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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
mod native_check;
mod openai;
pub(crate) mod prompts;
mod render;
mod roster;
pub use roster::{AdmittedRoster, NamedProviderRoutes, RosterSnapshot, valid_profile_name};
mod routing;
pub use routing::{
    AdmittedProviders, MAX_PROVIDER_FALLBACKS, MAX_PROVIDER_SWITCHES, ProviderFallback,
    ProviderRole, ProviderRoutes, Providers,
};
mod streaming;
#[cfg(test)]
mod test_support;
mod tool_specs;
pub use anthropic::AnthropicProvider;
pub use catalog::{
    ModelEntry, list_native_models, list_native_models_with_api_key,
    list_native_models_with_credentials,
};
pub use conversation::{PromptRole, ProviderRequest, ProviderTurn, RunInput, ToolUseId};
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
pub(crate) use render::history_tool_block;
#[cfg(feature = "test-support")]
pub use render::semantic;
pub use streaming::{PreviewText, ResponsePreview};

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

pub(crate) const MAX_REPORTED_INPUT_TOKENS: u32 = 1_000_000;
pub(crate) const MAX_OUTPUT_TOKEN_CAP: u32 = 16_384;
pub(crate) const DEFAULT_OUTPUT_TOKEN_CAP: u32 = 4096;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

fn outcome_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"outcome": {"anyOf": [
            {"type": "object", "properties": {
                "type": {"type": "string", "enum": ["finish"]},
                "summary": {"type": "string"},
                "result": {"type": "string"}
            }, "required": ["type", "summary", "result"], "additionalProperties": false},
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

fn summary_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"summary": {"type": "string"}},
        "required": ["summary"],
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
                };
                json!({"status": status, "user": user})
            }
        })
        .collect::<Vec<_>>();
    let input = json!({"previous_derived_summary": request.previous_summary, "items": items});
    input
}

pub(crate) const MAX_PROVIDER_REQUEST_BYTES: usize = 512 * 1024;
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

    pub(crate) fn valid(&self) -> bool {
        let chatgpt = self.profile == "chatgpt";
        let limits = &self.limits;
        !self.profile.is_empty()
            && self.profile.len() <= 128
            && !self.model.is_empty()
            && self.model.len() <= 128
            && self.profile.starts_with("custom:") == self.custom_provenance().is_some()
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
        if !role_fits
            || self.phase == AgentPhase::ChildWork
                && (self.collaboration != CollaborationPolicy::Single
                    || input.goal_context_facts.is_some())
            || !input.steering_notes.is_empty() && !root
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
    outcome_schema_for_phases(
        assignment_schema(schema, request),
        request.collaboration,
        &[request.phase],
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
    schema["properties"]["outcome"]["anyOf"][1]["properties"]["children"]["items"]["properties"]
        ["profile"]["enum"] = json!(profiles);
    if request.input.role == PromptRole::Primary
        && let Some(tools) = &request.input.tools
        && let Ok(catalog) = serde_json::from_str::<Value>(&tools.catalog)
        && let Some(agents) = catalog["agents"].as_array()
    {
        let calls = crate::tools::catalog::catalog_call_schemas(&catalog);
        let contract = crate::tools::catalog::plan_contract_property(&calls);
        let criterion = contract["anyOf"][0]["properties"]["success_criterion"].clone();
        let items =
            &mut schema["properties"]["outcome"]["anyOf"][1]["properties"]["children"]["items"];
        let mut branches = vec![items.clone()];
        for agent in agents
            .iter()
            .filter(|agent| agent["editor"] == true && agent["supported"] == true)
        {
            branches.push(json!({
                "type":"object", "properties": {
                    "objective":{"type":"string", "description":"Repeat editor.mission exactly."},
                    "profile":{"type":"null"},
                    "editor":{"type":"object", "properties": {
                        "definition":{"type":"string","enum":[agent["name"]]},
                        "definition_digest":{"type":"array","items":{"type":"integer"},"description":format!("Exact admitted digest: {}",agent["digest"])},
                        "mission":{"type":"string"}, "direction":{"type":"string"},
                        "lease":{"type":"array","items":{"type":"string"},"description":"Sorted nonoverlapping ASCII relative files or subtrees within the Goal plan and parent grant."},
                        "criterion":criterion,
                        "mode":{"type":"string","enum":["foreground"]},
                        "max_turns":{"type":"integer","description":format!("At most {}",agent["max_turns"])},
                        "wave":{"type":"integer","enum":[1]},
                        "resumes":{"type":"null"}, "supersedes":{"type":"null"}
                    }, "required":["definition","definition_digest","mission","direction","lease","criterion","mode","max_turns","wave","resumes","supersedes"],"additionalProperties":false}
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
                Some("finish") => {
                    *phase != AgentPhase::RootPlan
                        || !matches!(collaboration, CollaborationPolicy::Team { .. })
                }
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
pub struct Finish {
    pub summary: String,
    pub result: String,
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
pub enum ProviderOutcome {
    Finish(Finish),
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
                ProviderOutcome::Finish(value) => {
                    value.summary.contains(secret) || value.result.contains(secret)
                }
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
        || request.plan_contract.as_ref().is_some_and(|contract| {
            serde_json::to_value(contract).is_ok_and(|value| value_reflects_secret(&value, secret))
        })
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("Provider unavailable")]
    Unavailable,
    #[error("Provider server unavailable before response admission")]
    HttpServerUnavailable,
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
    #[error("Provider response did not match the selected response contract")]
    InvalidResponseContract,
    #[error("Provider output did not match Arany's structured outcome contract")]
    InvalidOutcomeContract,
    #[error("Provider reported output above Arany's local token limit")]
    LocalOutputLimit,
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
    pub(crate) fn allows_fallback(&self) -> bool {
        matches!(
            self,
            Self::HttpServerUnavailable | Self::RemoteHttp(500..=599)
        )
    }

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
            Self::Unavailable
            | Self::HttpServerUnavailable
            | Self::PartialTransport
            | Self::CallTimedOut
            | Self::RemoteHttp(500..=599) => ProviderFailureClass::Unavailable,
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
        crate::tools::ToolContext {
            catalog: json!({"write": false, "commands": [], "skills": [], "mcp_servers": [], "tool_calls_remaining": 2}).to_string(),
            history_summary: None,
            steps: Vec::new(),
        }
    }

    #[test]
    fn editor_delegate_schema_does_not_change_when_the_plan_contract_arrives() {
        let mut before = test_support::primary_request("scope-model");
        before.collaboration = CollaborationPolicy::Auto {
            max_active_children: 3,
        };
        let tools = before.input.tools.as_mut().unwrap();
        let mut catalog: Value = serde_json::from_str(&tools.catalog).unwrap();
        catalog["agents"] = json!([{"name": "editor", "editor": true, "supported": true, "digest": "d", "max_turns": 4}]);
        tools.catalog = catalog.to_string();
        let mut after = before.clone();
        after.input.goal_plan_contract = Some(test_support::goal_plan_contract());
        assert_eq!(run_outcome_schema(&before), run_outcome_schema(&after));
    }

    #[test]
    fn fallback_requires_a_server_failure_at_the_http_status_stage() {
        let anthropic_rate_limit = anthropic::http_failure(reqwest::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            anthropic_rate_limit.failure_class(),
            ProviderFailureClass::Unavailable
        );
        assert_eq!(anthropic_rate_limit.failure_reason(), None);
        assert!(!anthropic_rate_limit.allows_fallback());
        assert_eq!(
            ProviderError::HttpServerUnavailable.failure_class(),
            ProviderError::Unavailable.failure_class()
        );
        assert_eq!(
            ProviderError::HttpServerUnavailable.failure_reason(),
            ProviderError::Unavailable.failure_reason()
        );
        for (error, eligible) in [
            (ProviderError::HttpServerUnavailable, true),
            (ProviderError::RemoteHttp(500), true),
            (ProviderError::RemoteHttp(503), true),
            (ProviderError::RemoteHttp(529), true),
            (ProviderError::RemoteHttp(400), false),
            (ProviderError::RemoteHttp(401), false),
            (ProviderError::RemoteHttp(402), false),
            (ProviderError::RemoteHttp(413), false),
            (ProviderError::RemoteHttp(429), false),
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
            ProviderOutcome::Tools(vec![crate::tools::ToolRequest {
                id: ToolUseId::new("call_1").unwrap(),
                call: crate::tools::ToolCall::Write {
                    path: "src/main.rs".into(),
                    expected_digest: None,
                    content: "safe".into(),
                },
                plan_contract: contract.map(Box::new),
            }])
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
        response.outcome = tool(None);
        assert!(!response.reflects_secret(secret));
        let ProviderOutcome::Tools(requests) = &mut response.outcome else {
            unreachable!("scripted Tool outcome");
        };
        requests[0].id = ToolUseId::new(format!("id-{secret}")).unwrap();
        assert!(
            response.reflects_secret(secret),
            "a Tool call id carries the secret"
        );
        response.outcome = ProviderOutcome::Finish(Finish {
            summary: "safe".into(),
            result: "safe".into(),
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
        assert!(researcher.validate_scope().is_err());

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
    fn steering_notes_are_primary_only_and_never_child_or_review_scope() {
        let mut request = request(PromptRole::ReadOnly, AgentPhase::RootPlan, None);
        request.input.steering_notes = vec!["note".into()];
        assert!(request.validate_scope().is_ok());
        request.phase = AgentPhase::RootSynthesis;
        assert!(request.validate_scope().is_ok());
        request.phase = AgentPhase::ChildWork;
        assert!(request.validate_scope().is_err());
        request.phase = AgentPhase::ToolReview;
        request.input.role = PromptRole::ToolReview;
        assert!(request.validate_scope().is_err());
        request.input.steering_notes.clear();
        assert!(request.validate_scope().is_ok());
    }

    #[test]
    fn outcome_schema_offers_only_finish_and_delegate_by_phase_and_policy() {
        let mut request = request(PromptRole::Primary, AgentPhase::RootPlan, Some(context()));
        for (collaboration, phase, expected) in [
            (
                CollaborationPolicy::Single,
                AgentPhase::RootPlan,
                vec!["finish"],
            ),
            (
                CollaborationPolicy::Auto {
                    max_active_children: 0,
                },
                AgentPhase::RootPlan,
                vec!["finish"],
            ),
            (
                CollaborationPolicy::Auto {
                    max_active_children: 3,
                },
                AgentPhase::RootPlan,
                vec!["finish", "delegate"],
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
                vec!["finish"],
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

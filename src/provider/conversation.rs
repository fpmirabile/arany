use super::streaming::reflects_secret;
use super::tool_specs::ToolSpec;
use super::{
    AgentPhase, ChildResult, GoalContextFacts, HistoryTurn, MAX_RESPONSE_BYTES, ProviderError,
    ProviderImage, raw_reflects_secret,
};
use crate::session::{
    AgentRunId, CollaborationPolicy, GoalBudget, GoalPlanContract, RunId, SessionId,
};
use crate::tools::ToolContext;
use serde_json::Value;

/// Selects the frozen system prompt and the Tool set offered for a request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptRole {
    Primary,
    Researcher,
    ReadOnly,
    ToolReview,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderRequest {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub agent_run_id: AgentRunId,
    pub phase: AgentPhase,
    pub collaboration: CollaborationPolicy,
    pub model: String,
    pub max_output_tokens: u32,
    pub input: RunInput,
}

/// Typed, trust-labelled inputs of one call. The renderer alone decides what becomes system text,
/// user input or labelled untrusted data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunInput {
    pub role: PromptRole,
    pub objective: String,
    pub images: Vec<ProviderImage>,
    /// Notes recorded before the first step; later ones live in their `ToolStep`.
    pub steering_notes: Vec<String>,
    pub goal_budget: Option<GoalBudget>,
    pub goal_plan_contract: Option<GoalPlanContract>,
    /// Facts of the first call, or of a call whose steps were all compacted away.
    pub goal_context_facts: Option<GoalContextFacts>,
    pub workspace_guidance: Option<String>,
    pub includes: Vec<String>,
    pub history: Vec<HistoryTurn>,
    /// Model-authored derived data; rendered as data, never instructions.
    pub context_summary: Option<String>,
    pub child_results: Vec<ChildResult>,
    pub tools: Option<ToolContext>,
}

impl RunInput {
    pub fn new(role: PromptRole, objective: String) -> Self {
        Self {
            role,
            objective,
            images: Vec::new(),
            steering_notes: Vec::new(),
            goal_budget: None,
            goal_plan_contract: None,
            goal_context_facts: None,
            workspace_guidance: None,
            includes: Vec::new(),
            history: Vec::new(),
            context_summary: None,
            child_results: Vec::new(),
            tools: None,
        }
    }
}

/// Text that may be `system`: nothing else can ever become system text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SystemBlock {
    Prompt(&'static str),
    Policy(CollaborationPolicy),
}

impl SystemBlock {
    pub(crate) fn text(&self) -> String {
        match self {
            Self::Prompt(text) => (*text).to_owned(),
            Self::Policy(policy) => format!(
                "Pinned collaboration policy: {}.",
                serde_json::to_string(policy).expect("closed policy serializes")
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Block {
    /// User text or a labelled data block.
    Text(String),
    Image {
        label: String,
        image: super::ImageAttachment,
    },
    /// Exact replay of a previous model turn; the only content an assistant message carries.
    AssistantTurn(ProviderTurn),
    ToolResult {
        id: ToolUseId,
        output: Value,
        is_error: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Message {
    pub role: Role,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug)]
pub(crate) struct Conversation {
    pub system: Vec<SystemBlock>,
    /// `Some` when the call offers native Tools; the set never changes within a Run.
    pub tools: Option<Vec<ToolSpec>>,
    pub messages: Vec<Message>,
    /// Finish and Delegate JSON schema restricted by phase and policy.
    pub outcome: Value,
}

/// Provider-assigned Tool call id: 1..=128 ASCII-graphic bytes, unique within a step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolUseId(String);

impl ToolUseId {
    pub fn new(id: impl Into<String>) -> Result<Self, ProviderError> {
        let id = id.into();
        if id.is_empty() || id.len() > 128 || !id.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(ProviderError::InvalidOutcome);
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A turn is bounded by what the model could have generated: 8 bytes per output token plus 64 KiB
/// of item framing and ciphertext overhead, never above the response bound.
pub(crate) fn max_turn_bytes(max_output_tokens: u32) -> usize {
    (8 * max_output_tokens as usize + 64 * 1024).min(MAX_RESPONSE_BYTES)
}

/// The assistant output of one call exactly as its adapter must replay it: an Anthropic `content`
/// array or the Responses `output` items. It lives only in memory inside one live Run and is never
/// journaled; Arany never interprets it.
#[derive(Clone, Eq, PartialEq)]
pub struct ProviderTurn {
    adapter: &'static str,
    json: String,
}

/// Opaque model output, possibly carrying ciphertext or private reasoning; never printed.
impl std::fmt::Debug for ProviderTurn {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderTurn")
            .field("adapter", &self.adapter)
            .field("bytes", &self.json.len())
            .finish_non_exhaustive()
    }
}

impl ProviderTurn {
    /// Admits a turn after bounds and credential reflection over the raw bytes and every decoded string.
    pub(crate) fn new(
        adapter: &'static str,
        json: &str,
        secret: &str,
        max_output_tokens: u32,
    ) -> Result<Self, ProviderError> {
        if json.len() > max_turn_bytes(max_output_tokens)
            || raw_reflects_secret(json.as_bytes(), secret)
        {
            return Err(ProviderError::InvalidOutcome);
        }
        let value = crate::tools::parse_json(json.as_bytes(), json.len())
            .map_err(|_| ProviderError::InvalidOutcome)?;
        if !value
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_object))
            || reflects_secret(&value, secret)
        {
            return Err(ProviderError::InvalidOutcome);
        }
        let json = serde_json::to_string(&value).map_err(|_| ProviderError::InvalidOutcome)?;
        if json.len() > max_turn_bytes(max_output_tokens) {
            return Err(ProviderError::InvalidOutcome);
        }
        Ok(Self { adapter, json })
    }

    /// The empty turn a scripted Provider returns.
    pub fn scripted() -> Self {
        Self {
            adapter: "scripted",
            json: "[]".into(),
        }
    }

    /// Drops Anthropic thinking blocks, whose signatures bind the history before them. Compaction
    /// rewrites that history, so the kept turns must stop carrying them.
    pub(crate) fn without_thinking(&mut self) {
        if self.adapter != "anthropic" {
            return;
        }
        let Some(mut items) = self.items("anthropic") else {
            return;
        };
        items.retain(|item| {
            !matches!(
                item["type"].as_str(),
                Some("thinking" | "redacted_thinking")
            )
        });
        if !items.is_empty()
            && let Ok(json) = serde_json::to_string(&items)
        {
            self.json = json;
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.json.len()
    }

    pub(crate) fn reflects_secret(&self, secret: &str) -> bool {
        secret.is_empty()
            || raw_reflects_secret(self.json.as_bytes(), secret)
            || serde_json::from_str::<Value>(&self.json)
                .map_or(true, |value| reflects_secret(&value, secret))
    }

    /// The items to splice into a request of `adapter`; `None` for a turn of another route.
    pub(crate) fn items(&self, adapter: &'static str) -> Option<Vec<Value>> {
        if self.adapter != adapter {
            return None;
        }
        match serde_json::from_str(&self.json) {
            Ok(Value::Array(items)) => Some(items),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "synthetic-turn-secret";

    #[test]
    fn turn_with_unicode_escaped_secret_is_rejected() {
        assert!(
            ProviderTurn::new(
                "anthropic",
                r#"[{"type":"text","text":"safe"}]"#,
                SECRET,
                4096
            )
            .is_ok()
        );
        let escaped = r#"[{"type":"text","text":"\u0073ynthetic-turn-secret"}]"#;
        assert!(!raw_reflects_secret(escaped.as_bytes(), SECRET));
        assert!(ProviderTurn::new("anthropic", escaped, SECRET, 4096).is_err());
        let escaped_key = r#"[{"\u0073ynthetic-turn-secret":1}]"#;
        assert!(ProviderTurn::new("anthropic", escaped_key, SECRET, 4096).is_err());
        let raw = format!(r#"[{{"type":"text","text":"{SECRET}"}}]"#);
        assert!(ProviderTurn::new("anthropic", &raw, SECRET, 4096).is_err());
        let turn = ProviderTurn::new("openai", r#"[{"type":"reasoning"}]"#, SECRET, 4096).unwrap();
        assert!(!turn.reflects_secret(SECRET));
    }

    #[test]
    fn turn_rejects_size_depth_and_non_array_shapes() {
        for json in [
            "{}",
            "[1]",
            r#"["text"]"#,
            "[[]]",
            "null",
            "[{",
            r#"[{"a":1}] trailing"#,
        ] {
            assert!(
                ProviderTurn::new("anthropic", json, SECRET, 4096).is_err(),
                "{json}"
            );
        }
        let mut nested = String::from("1");
        for _ in 0..40 {
            nested = format!("[{nested}]");
        }
        assert!(
            ProviderTurn::new("anthropic", &format!(r#"[{{"a":{nested}}}]"#), SECRET, 4096)
                .is_err()
        );
        let nodes = format!("[{{\"a\":[{}]}}]", vec!["1"; 5000].join(","));
        assert!(ProviderTurn::new("anthropic", &nodes, SECRET, 4096).is_err());
    }

    #[test]
    fn turn_bound_follows_max_output_tokens() {
        let filler = |bytes: usize| format!(r#"[{{"text":"{}"}}]"#, "x".repeat(bytes));
        assert_eq!(max_turn_bytes(4096), 96 * 1024);
        assert_eq!(
            max_turn_bytes(16_384),
            192 * 1024,
            "the native 16,384-token cap"
        );
        assert_eq!(max_turn_bytes(u32::MAX), MAX_RESPONSE_BYTES);
        assert!(ProviderTurn::new("openai", &filler(90 * 1024), SECRET, 4096).is_ok());
        assert!(ProviderTurn::new("openai", &filler(97 * 1024), SECRET, 4096).is_err());
        assert!(ProviderTurn::new("openai", &filler(70 * 1024), SECRET, 256).is_err());
        assert!(ProviderTurn::new("openai", &filler(60 * 1024), SECRET, 256).is_ok());
    }

    #[test]
    fn turn_replays_only_into_its_own_adapter() {
        let turn = ProviderTurn::new("anthropic", r#"[{"type":"text","text":"a"}]"#, SECRET, 4096)
            .unwrap();
        assert_eq!(turn.items("anthropic").unwrap().len(), 1);
        assert!(turn.items("openai").is_none());
    }

    #[test]
    fn compaction_strips_signed_thinking_from_anthropic_turns_only() {
        let content = r#"[{"type":"thinking","thinking":"t","signature":"s"},{"type":"redacted_thinking","data":"d"},{"type":"tool_use","id":"t1","name":"arany_read","input":{}}]"#;
        let mut turn = ProviderTurn::new("anthropic", content, SECRET, 4096).unwrap();
        turn.without_thinking();
        let kept = turn.items("anthropic").unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0]["type"], "tool_use");
        let mut reasoning = ProviderTurn::new(
            "openai",
            r#"[{"type":"reasoning","encrypted_content":"c"}]"#,
            SECRET,
            4096,
        )
        .unwrap();
        let before = reasoning.clone();
        reasoning.without_thinking();
        assert_eq!(reasoning, before);
    }

    #[test]
    fn tool_use_ids_are_bounded_ascii_graphic() {
        assert!(ToolUseId::new("toolu_01A").is_ok());
        for bad in ["", "has space", "ü", &"x".repeat(129)] {
            assert!(ToolUseId::new(bad).is_err());
        }
    }
}

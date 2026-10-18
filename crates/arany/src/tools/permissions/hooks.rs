use super::{
    ConfigError, Decision, MAX_PATTERN_BYTES, PermissionMode, compile, peer_name, valid_call,
};
use crate::tools::{EffectIntent, ToolCall, ToolObservation};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const MAX_HOOK_GROUPS: usize = 64;
pub const MAX_HOOKS: usize = 128;
pub const MAX_COMMAND_BYTES: usize = 4096;
pub const MAX_TIMEOUT_SECONDS: u64 = 600;
pub const MAX_HOOKS_PER_OCCURRENCE: usize = 8;
// Half of the Guard task ceiling: JSON escaping can double the input inside the task.
pub const MAX_HOOK_INPUT_BYTES: usize = 32 * 1024;
pub const MAX_HOOK_REASON_BYTES: usize = 1024;
pub const HOOK_RESULT_BYTES: u32 = 32 * 1024;
pub const MAX_HOOK_FEEDBACK_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    Stop,
    SubagentStop,
    PreCompact,
}

pub(super) type RawHooks = BTreeMap<HookEvent, Vec<RawGroup>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawGroup {
    #[serde(default, deserialize_with = "present_value")]
    matcher: Option<String>,
    hooks: Vec<CommandHook>,
}

#[derive(Debug)]
pub struct HookConfig {
    groups: BTreeMap<HookEvent, Vec<HookGroup>>,
}

#[derive(Debug)]
pub struct HookGroup {
    matcher: Matcher,
    hooks: Vec<CommandHook>,
}

#[derive(Debug)]
enum Matcher {
    All,
    Names(Vec<String>),
    Pattern(Regex),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandHook {
    #[serde(rename = "type")]
    kind: CommandType,
    command: String,
    #[serde(default, deserialize_with = "present_value")]
    args: Option<Vec<String>>,
    #[serde(default = "default_timeout")]
    timeout: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CommandType {
    Command,
}

fn default_timeout() -> u64 {
    MAX_TIMEOUT_SECONDS
}

fn present_value<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

impl HookConfig {
    pub(super) fn validate(raw: RawHooks) -> Result<Self, ConfigError> {
        if raw.values().map(Vec::len).sum::<usize>() > MAX_HOOK_GROUPS {
            return Err(ConfigError::Limit("64 hook groups in total"));
        }
        if raw
            .values()
            .flatten()
            .map(|group| group.hooks.len())
            .sum::<usize>()
            > MAX_HOOKS
        {
            return Err(ConfigError::Limit("128 command hooks in total"));
        }
        let mut groups = BTreeMap::new();
        for (event, entries) in raw {
            let mut validated = Vec::new();
            for group in entries {
                if group.hooks.is_empty() {
                    return Err(ConfigError::Hook(
                        "each group requires at least one command",
                    ));
                }
                let matcher = Matcher::parse(group.matcher.as_deref().unwrap_or(""))?;
                if event == HookEvent::Stop && !matches!(matcher, Matcher::All) {
                    return Err(ConfigError::Hook("Stop does not support a matcher"));
                }
                for hook in &group.hooks {
                    hook.validate()?;
                }
                validated.push(HookGroup {
                    matcher,
                    hooks: group.hooks,
                });
            }
            groups.insert(event, validated);
        }
        Ok(Self { groups })
    }

    pub fn groups(&self, event: HookEvent) -> &[HookGroup] {
        self.groups.get(&event).map(Vec::as_slice).unwrap_or(&[])
    }
}

impl HookGroup {
    pub fn matches(&self, value: &str) -> Result<bool, ConfigError> {
        if value.len() > MAX_PATTERN_BYTES || value.chars().any(char::is_control) {
            return Err(ConfigError::Hook(
                "invalid match subject; block the pending action",
            ));
        }
        Ok(match &self.matcher {
            Matcher::All => true,
            Matcher::Names(names) => names.iter().any(|name| name == value),
            Matcher::Pattern(pattern) => pattern.is_match(value),
        })
    }

    pub fn matches_tool(&self, call: &ToolCall) -> Result<bool, ConfigError> {
        if !valid_call(call) {
            return Err(ConfigError::Hook(
                "invalid Tool match subject; block the pending action",
            ));
        }
        if matches!(call, ToolCall::McpCall { server, .. } | ToolCall::McpList { server } if server.contains("__"))
        {
            return Err(ConfigError::Hook(
                "ambiguous MCP server delimiter; block the pending action",
            ));
        }
        self.matches(&peer_name(call))
    }

    pub fn commands(&self) -> &[CommandHook] {
        &self.hooks
    }
}

impl Matcher {
    fn parse(value: &str) -> Result<Self, ConfigError> {
        if value.len() > MAX_PATTERN_BYTES || value.chars().any(char::is_control) {
            return Err(ConfigError::Hook(
                "matcher must be control-free and at most 1024 bytes",
            ));
        }
        if value.is_empty() || value == "*" {
            return Ok(Self::All);
        }
        if value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_- ,|".contains(&byte))
        {
            let names: Vec<_> = value
                .split(['|', ','])
                .map(str::trim)
                .map(str::to_owned)
                .collect();
            if names.iter().any(String::is_empty) {
                return Err(ConfigError::Hook("empty matcher alternative"));
            }
            Ok(Self::Names(names))
        } else {
            // Finite automata avoid backtracking on untrusted matcher text.
            Ok(Self::Pattern(compile(value).map_err(ConfigError::Hook)?))
        }
    }
}

impl CommandHook {
    fn validate(&self) -> Result<(), ConfigError> {
        let CommandType::Command = self.kind;
        if self.command.trim().is_empty()
            || self.command.len() > MAX_COMMAND_BYTES
            || self
                .command
                .chars()
                .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
        {
            return Err(ConfigError::Hook(
                "command must be nonempty, at most 4096 bytes and contain no unsafe controls",
            ));
        }
        if self.timeout == 0 || self.timeout > MAX_TIMEOUT_SECONDS {
            return Err(ConfigError::Hook(
                "timeout must be an integer from 1 through 600 seconds",
            ));
        }
        if let Some(args) = &self.args
            && (args.len() > 64
                || args
                    .iter()
                    .any(|arg| arg.len() > MAX_COMMAND_BYTES || arg.contains('\0')))
        {
            return Err(ConfigError::Hook(
                "exec form permits at most 64 arguments of at most 4096 bytes without NUL",
            ));
        }
        Ok(())
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    pub fn args(&self) -> Option<&[String]> {
        self.args.as_deref()
    }

    pub fn timeout_seconds(&self) -> u64 {
        self.timeout
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookInvocation {
    pub event: HookEvent,
    pub group: u8,
    pub handler: u8,
    pub subject_digest: [u8; 32],
    pub intent: EffectIntent,
    pub input_digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookOutcome {
    pub observation: ToolObservation,
    pub result: HookResult,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookResult {
    Passed,
    Restricted {
        decision: Decision,
        #[serde(deserialize_with = "Option::deserialize")]
        reason: Option<String>,
    },
    Feedback {
        lines: Vec<String>,
    },
    Ignored {
        why: HookIgnoreReason,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookIgnoreReason {
    NonZeroExit,
    InvalidJson,
    AllowIsNoOp,
    NotCompleted,
}

impl HookResult {
    pub fn restriction(&self) -> Decision {
        match self {
            Self::Restricted { decision, .. } => *decision,
            Self::Passed | Self::Feedback { .. } | Self::Ignored { .. } => Decision::Allow,
        }
    }

    pub(crate) fn valid_for(&self, observation: &ToolObservation) -> bool {
        // The parse is deterministic and bounded, so a recorded result must equal it.
        [HookEvent::PreToolUse, HookEvent::PostToolUse]
            .into_iter()
            .any(|event| *self == Self::for_event(event, observation))
            && match self {
                Self::Restricted { reason, .. } => reason.as_deref().is_none_or(valid_reason),
                Self::Feedback { lines } => lines.iter().all(|line| valid_reason(line)),
                _ => true,
            }
    }

    pub(crate) fn for_event(event: HookEvent, observation: &ToolObservation) -> Self {
        use crate::tools::ToolDisposition::{Failed, Limit, Succeeded};
        let not_completed = Self::Ignored {
            why: HookIgnoreReason::NotCompleted,
        };
        match observation.disposition {
            Succeeded | Failed if observation.attestation.is_some() => {
                serde_json::from_str::<crate::tools::types::CommandResult>(&observation.output)
                    .map_or(not_completed, |result| {
                        if event == HookEvent::PostToolUse {
                            parse_post_output(result.exit_code, &result.stdout, &result.stderr)
                        } else {
                            parse_output(result.exit_code, &result.stdout, &result.stderr)
                        }
                    })
            }
            // A cut-off hook cannot be told from a hostile one, so it fails closed.
            Limit if event == HookEvent::PostToolUse => Self::Feedback {
                lines: vec!["hook limit reached".into()],
            },
            Limit => Self::Restricted {
                decision: Decision::Deny,
                reason: Some("hook limit reached".into()),
            },
            _ => not_completed,
        }
    }
}

fn valid_reason(reason: &str) -> bool {
    reason.len() <= MAX_HOOK_REASON_BYTES && !reason.chars().any(char::is_control)
}

fn bounded_reason(text: &str) -> Option<String> {
    let mut reason = String::new();
    for ch in text.trim().chars().filter(|ch| !ch.is_control()) {
        if reason.len() + ch.len_utf8() > MAX_HOOK_REASON_BYTES {
            break;
        }
        reason.push(ch);
    }
    (!reason.is_empty()).then_some(reason)
}

pub(crate) fn pre_tool_use_input(
    session_id: impl std::fmt::Display,
    mode: PermissionMode,
    call: &ToolCall,
    tool_use_id: uuid::Uuid,
) -> Result<String, ConfigError> {
    let input = serde_json::json!({
        "session_id": session_id.to_string(),
        "cwd": "/workspace",
        "permission_mode": mode,
        "hook_event_name": "PreToolUse",
        "tool_name": peer_name(call),
        "tool_input": call,
        "tool_use_id": tool_use_id,
    })
    .to_string();
    if input.len() > MAX_HOOK_INPUT_BYTES {
        return Err(ConfigError::Limit("32 KiB hook input"));
    }
    Ok(input)
}

// A `tool_response` over `budget` bytes of input is cut to the room the rest leaves and marked truncated.
pub(crate) fn post_tool_use_input(
    session_id: impl std::fmt::Display,
    mode: PermissionMode,
    call: &ToolCall,
    tool_use_id: uuid::Uuid,
    tool_response: Value,
    budget: usize,
) -> Result<String, ConfigError> {
    let budget = budget.min(MAX_HOOK_INPUT_BYTES);
    let build = |response: Value| {
        serde_json::json!({
            "session_id": session_id.to_string(),
            "cwd": "/workspace",
            "permission_mode": mode,
            "hook_event_name": "PostToolUse",
            "tool_name": peer_name(call),
            "tool_input": call,
            "tool_use_id": tool_use_id,
            "tool_response": response,
        })
        .to_string()
    };
    let input = build(tool_response.clone());
    if input.len() <= budget {
        return Ok(input);
    }
    let marker = |text: &str| serde_json::json!({"truncated": true, "text": text});
    let bare = build(marker("")).len();
    if bare > MAX_HOOK_INPUT_BYTES {
        return Err(ConfigError::Limit("32 KiB hook input"));
    }
    let room = budget.saturating_sub(bare);
    let text = tool_response.to_string();
    let mut used = 0;
    let mut end = 0;
    for ch in text.chars() {
        used +=
            serde_json::to_string(&ch.to_string()).map_or(usize::MAX, |escaped| escaped.len() - 2);
        if used > room {
            break;
        }
        end += ch.len_utf8();
    }
    Ok(build(marker(&text[..end])))
}

pub(crate) fn feedback_lines<'a>(results: impl IntoIterator<Item = &'a HookResult>) -> Vec<String> {
    let mut total = 0;
    let mut lines = Vec::new();
    for line in results.into_iter().flat_map(|result| match result {
        HookResult::Feedback { lines } => lines.as_slice(),
        _ => &[],
    }) {
        let line = format!("hook feedback: {line}");
        total += line.len();
        if total > MAX_HOOK_FEEDBACK_BYTES {
            break;
        }
        lines.push(line);
    }
    lines
}

pub(crate) fn parse_post_output(exit_code: Option<i32>, stdout: &str, stderr: &str) -> HookResult {
    let feedback = |lines: Vec<String>| {
        if lines.is_empty() {
            HookResult::Passed
        } else {
            HookResult::Feedback { lines }
        }
    };
    const BLOCKED: &str = "blocked without a reason";
    match exit_code {
        Some(0) => {}
        Some(2) => {
            return feedback(vec![
                bounded_reason(stderr).unwrap_or_else(|| BLOCKED.into()),
            ]);
        }
        _ => {
            return HookResult::Ignored {
                why: HookIgnoreReason::NonZeroExit,
            };
        }
    }
    let Ok(Value::Object(output)) =
        super::super::parse_json(stdout.trim().as_bytes(), HOOK_RESULT_BYTES as usize)
    else {
        return HookResult::Passed;
    };
    let text = |value: Option<&Value>| value.and_then(Value::as_str).and_then(bounded_reason);
    let mut lines = Vec::new();
    if output.get("decision").and_then(Value::as_str) == Some("block") {
        lines.push(text(output.get("reason")).unwrap_or_else(|| BLOCKED.into()));
    }
    if let Some(specific) = output.get("hookSpecificOutput") {
        let Some(specific) = specific.as_object() else {
            return HookResult::Ignored {
                why: HookIgnoreReason::InvalidJson,
            };
        };
        if specific.get("hookEventName").and_then(Value::as_str) != Some("PostToolUse") {
            return HookResult::Ignored {
                why: HookIgnoreReason::InvalidJson,
            };
        }
        lines.extend(text(specific.get("additionalContext")));
    }
    feedback(lines)
}

pub(crate) fn parse_output(exit_code: Option<i32>, stdout: &str, stderr: &str) -> HookResult {
    let deny = |reason: Option<String>| HookResult::Restricted {
        decision: Decision::Deny,
        reason,
    };
    let invalid = HookResult::Ignored {
        why: HookIgnoreReason::InvalidJson,
    };
    match exit_code {
        Some(0) => {}
        Some(2) => return deny(bounded_reason(stderr)),
        _ => {
            return HookResult::Ignored {
                why: HookIgnoreReason::NonZeroExit,
            };
        }
    }
    let Ok(Value::Object(output)) =
        super::super::parse_json(stdout.trim().as_bytes(), HOOK_RESULT_BYTES as usize)
    else {
        return HookResult::Passed;
    };
    let text = |value: Option<&Value>| value.and_then(Value::as_str).and_then(bounded_reason);
    if output.get("decision").and_then(Value::as_str) == Some("block") {
        return deny(text(output.get("reason")));
    }
    if output.get("continue") == Some(&Value::Bool(false)) {
        return deny(text(output.get("stopReason")));
    }
    let Some(specific) = output.get("hookSpecificOutput") else {
        return HookResult::Passed;
    };
    let Some(specific) = specific.as_object() else {
        return invalid;
    };
    if specific.get("hookEventName").and_then(Value::as_str) != Some("PreToolUse") {
        return invalid;
    }
    let decision = match specific.get("permissionDecision") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => match value.as_str() {
            "deny" => Some(Decision::Deny),
            "ask" => Some(Decision::Ask),
            "allow" => Some(Decision::Allow),
            _ => return invalid,
        },
        Some(_) => return invalid,
    };
    let reason = text(specific.get("permissionDecisionReason"));
    if decision == Some(Decision::Deny) {
        return deny(reason);
    }
    if specific
        .get("updatedInput")
        .is_some_and(|value| !value.is_null())
    {
        return deny(Some("hook input update does not narrow".into()));
    }
    match decision {
        Some(Decision::Ask) => HookResult::Restricted {
            decision: Decision::Ask,
            reason,
        },
        Some(_) => HookResult::Ignored {
            why: HookIgnoreReason::AllowIsNoOp,
        },
        None => HookResult::Passed,
    }
}

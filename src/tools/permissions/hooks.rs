use super::{ConfigError, MAX_PATTERN_BYTES, compile, peer_name};
use crate::tools::ToolCall;
use regex::Regex;
use serde::Deserialize;
use std::collections::BTreeMap;

pub const MAX_HOOK_GROUPS: usize = 64;
pub const MAX_HOOKS: usize = 128;
pub const MAX_COMMAND_BYTES: usize = 4096;
pub const MAX_TIMEOUT_SECONDS: u64 = 600;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Deserialize)]
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
    pub fn matches(&self, value: &str) -> bool {
        if value.len() > MAX_PATTERN_BYTES || value.chars().any(char::is_control) {
            return false;
        }
        match &self.matcher {
            Matcher::All => true,
            Matcher::Names(names) => names.iter().any(|name| name == value),
            Matcher::Pattern(pattern) => pattern.is_match(value),
        }
    }

    pub fn matches_tool(&self, call: &ToolCall) -> bool {
        self.matches(peer_name(call))
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

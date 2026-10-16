mod hooks;

pub use hooks::{CommandHook, HookConfig, HookEvent, HookGroup};

use super::catalog::Family;
use super::{ApprovalMode, ToolCall};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;

pub const MAX_CONFIG_BYTES: usize = 64 * 1024;
pub const MAX_RULES: usize = 128;
pub const MAX_PATTERN_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    #[default]
    Default,
    AcceptEdits,
    Plan,
    Auto,
    DontAsk,
}

impl PermissionMode {
    pub fn approval_mode(self) -> Option<ApprovalMode> {
        match self {
            Self::Default => Some(ApprovalMode::Request),
            Self::AcceptEdits => Some(ApprovalMode::AutoEdits),
            Self::Auto => Some(ApprovalMode::Auto),
            Self::Plan | Self::DontAsk => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
    Allow,
    Deny,
    Ask,
}

impl Decision {
    pub fn narrow(self, restriction: Self) -> Self {
        match (self, restriction) {
            (Self::Deny, _) | (_, Self::Deny) => Self::Deny,
            (Self::Ask, _) | (_, Self::Ask) => Self::Ask,
            _ => Self::Allow,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("permission/hook JSON is invalid, duplicated, too deep or too large (maximum 64 KiB)")]
    Json,
    #[error("permission/hook schema has an unknown field, unsupported type or invalid value")]
    Schema,
    #[error("permission/hook limit exceeded: {0}")]
    Limit(&'static str),
    #[error("invalid {list} rule at index {index}: {reason}")]
    Rule {
        list: &'static str,
        index: usize,
        reason: &'static str,
    },
    #[error("invalid hook configuration: {0}")]
    Hook(&'static str),
}

#[derive(Debug)]
pub struct PermissionConfig {
    mode: PermissionMode,
    rules: Vec<(Decision, Rule)>,
    hooks: HookConfig,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    #[serde(default)]
    permissions: Permissions,
    #[serde(default)]
    hooks: hooks::RawHooks,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Permissions {
    #[serde(default)]
    default_mode: PermissionMode,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
    #[serde(default)]
    ask: Vec<String>,
}

impl PermissionConfig {
    pub fn parse(bytes: &[u8]) -> Result<Self, ConfigError> {
        let value = super::parse_json(bytes, MAX_CONFIG_BYTES).map_err(|_| ConfigError::Json)?;
        let settings: Settings = serde_json::from_value(value).map_err(|_| ConfigError::Schema)?;
        let permissions = settings.permissions;
        if permissions.allow.len() + permissions.deny.len() + permissions.ask.len() > MAX_RULES {
            return Err(ConfigError::Limit("128 permission rules in total"));
        }
        let mut rules = Vec::new();
        for (list, decision, inputs) in [
            ("deny", Decision::Deny, permissions.deny),
            ("ask", Decision::Ask, permissions.ask),
            ("allow", Decision::Allow, permissions.allow),
        ] {
            for (index, input) in inputs.into_iter().enumerate() {
                let rule = Rule::parse(&input, decision).map_err(|reason| ConfigError::Rule {
                    list,
                    index,
                    reason,
                })?;
                rules.push((decision, rule));
            }
        }
        Ok(Self {
            mode: permissions.default_mode,
            rules,
            hooks: HookConfig::validate(settings.hooks)?,
        })
    }

    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    pub fn hooks(&self) -> &HookConfig {
        &self.hooks
    }

    pub fn approval_mode(&self, call: &ToolCall) -> Option<ApprovalMode> {
        if self.decision(call) != Decision::Ask {
            return None;
        }
        let explicit_ask = self
            .rules
            .iter()
            .any(|(decision, rule)| *decision == Decision::Ask && rule.matches(call, *decision));
        Some(if self.mode == PermissionMode::Auto && !explicit_ask {
            ApprovalMode::Auto
        } else {
            ApprovalMode::Request
        })
    }

    pub fn decision(&self, call: &ToolCall) -> Decision {
        if serde_json::to_writer(CallSize(0), call).is_err() || !call.valid() {
            return Decision::Deny;
        }
        let facts = call.facts();
        if self.mode == PermissionMode::Plan
            && (facts.mutates || (facts.spawns_process && facts.family != Family::Git))
        {
            return Decision::Deny;
        }
        let decision = self
            .rules
            .iter()
            .find_map(|(decision, rule)| rule.matches(call, *decision).then_some(*decision))
            .unwrap_or_else(|| {
                if !facts.asks_approval
                    || (self.mode == PermissionMode::AcceptEdits && facts.mutates)
                {
                    Decision::Allow
                } else {
                    Decision::Ask
                }
            });
        if self.mode == PermissionMode::DontAsk && decision == Decision::Ask {
            Decision::Deny
        } else {
            decision
        }
    }
}

struct CallSize(usize);

impl std::io::Write for CallSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > super::types::MAX_TOOL_ARGUMENT_BYTES {
            return Err(std::io::ErrorKind::FileTooLarge.into());
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct Rule {
    tool: Tool,
    pattern: Option<Regex>,
}

#[derive(Debug)]
enum Tool {
    Bash,
    Read,
    Edit,
    Write,
    Glob,
    Grep,
    Skill,
    Native(&'static str),
    NameGlob(Regex),
}

impl Rule {
    fn parse(input: &str, decision: Decision) -> Result<Self, &'static str> {
        if input.is_empty()
            || input.len() > MAX_PATTERN_BYTES
            || input.chars().any(char::is_control)
        {
            return Err("rule must be nonempty, control-free and at most 1024 bytes");
        }
        let (name, specifier) = match input.split_once('(') {
            Some((name, rest)) => (
                name,
                Some(
                    rest.strip_suffix(')')
                        .ok_or("missing closing parenthesis")?,
                ),
            ),
            None => (input, None),
        };
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_*-".contains(&byte))
        {
            return Err(
                "Tool name must contain only ASCII letters, digits, underscore, hyphen or wildcard",
            );
        }
        let tool = match name {
            "Bash" => Tool::Bash,
            "Read" => Tool::Read,
            "Edit" => Tool::Edit,
            "Write" => Tool::Write,
            "Glob" => Tool::Glob,
            "Grep" => Tool::Grep,
            "Skill" => Tool::Skill,
            name if name.contains('*') => {
                if specifier.is_some() || decision == Decision::Allow {
                    return Err("tool-name wildcards support bare deny/ask rules only");
                }
                Tool::NameGlob(compile(&format!("\\A{}\\z", command_glob(name)))?)
            }
            name => Tool::Native(
                super::catalog::by_name(name)
                    .ok_or("unknown Tool name")?
                    .name,
            ),
        };
        let pattern = match specifier {
            None => None,
            Some("") => return Err("empty specifier"),
            Some("*") if matches!(tool, Tool::Bash | Tool::Native("command")) => None,
            Some(pattern) => Some(match tool {
                Tool::Bash | Tool::Native("command") => {
                    let pattern = pattern
                        .strip_suffix(":*")
                        .map(|p| format!("{p} *"))
                        .unwrap_or_else(|| pattern.to_owned());
                    let mut expression = command_glob(&pattern);
                    if pattern.ends_with(" *") && pattern.matches('*').count() == 1 {
                        expression = format!(
                            "(?:{}|{})",
                            regex::escape(&pattern[..pattern.len() - 2]),
                            expression
                        );
                    }
                    compile(&format!("\\A{expression}\\z"))?
                }
                Tool::Read
                | Tool::Edit
                | Tool::Grep
                | Tool::Native(
                    "list" | "read" | "search" | "mkdir" | "write" | "edit" | "git_status"
                    | "git_diff" | "git_log",
                ) => path_glob(pattern, decision)?,
                Tool::Skill | Tool::Native("skill") => {
                    compile(&format!("\\A{}\\z", command_glob(pattern)))?
                }
                _ => {
                    return Err(
                        "this Tool supports bare rules only; use Read/Edit for file patterns",
                    );
                }
            }),
        };
        Ok(Self { tool, pattern })
    }

    fn matches(&self, call: &ToolCall, decision: Decision) -> bool {
        let facts = call.facts();
        let matches_tool = match &self.tool {
            Tool::Bash => facts.family == Family::Process,
            Tool::Read => {
                facts.family == Family::File && (!facts.mutates || decision == Decision::Deny)
            }
            Tool::Edit => facts.family == Family::File && facts.mutates,
            Tool::Write => matches!(call, ToolCall::Write { .. }),
            Tool::Glob => matches!(call, ToolCall::List { .. }),
            Tool::Grep => matches!(call, ToolCall::Search { .. }),
            Tool::Skill => facts.family == Family::Skill,
            Tool::Native(name) => facts.name == *name,
            Tool::NameGlob(pattern) => {
                pattern.is_match(peer_name(call)) || pattern.is_match(facts.name)
            }
        };
        if !matches_tool {
            return false;
        }
        let Some(pattern) = &self.pattern else {
            return true;
        };
        match call {
            ToolCall::Command { program, args, .. } => {
                if !std::iter::once(program).chain(args).all(|word| {
                    !word.is_empty()
                        && word.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() || b"_./:@+=,%~-".contains(&byte)
                        })
                }) {
                    return decision != Decision::Allow;
                }
                let mut text = program.clone();
                for arg in args {
                    text.push(' ');
                    text.push_str(arg);
                }
                pattern.is_match(&text)
            }
            ToolCall::Skill { name, .. } => pattern.is_match(name),
            ToolCall::ApplyChangeset { changeset, .. } => {
                if decision == Decision::Allow {
                    !changeset.files.is_empty()
                        && changeset
                            .files
                            .iter()
                            .all(|file| pattern.is_match(&file.path))
                } else {
                    changeset
                        .files
                        .iter()
                        .any(|file| pattern.is_match(&file.path))
                }
            }
            _ => path_of(call).is_some_and(|path| pattern.is_match(path)),
        }
    }
}

fn path_of(call: &ToolCall) -> Option<&str> {
    match call {
        ToolCall::List { path }
        | ToolCall::Read { path, .. }
        | ToolCall::Search { path, .. }
        | ToolCall::Mkdir { path }
        | ToolCall::Write { path, .. }
        | ToolCall::Edit { path, .. }
        | ToolCall::GitStatus { path }
        | ToolCall::GitDiff { path, .. }
        | ToolCall::GitLog { path, .. } => Some(if path.is_empty() { "." } else { path }),
        _ => None,
    }
}

pub(super) fn peer_name(call: &ToolCall) -> &'static str {
    match call {
        ToolCall::Command { .. } => "Bash",
        ToolCall::Read { .. } => "Read",
        ToolCall::List { .. } => "Glob",
        ToolCall::Search { .. } => "Grep",
        ToolCall::Write { .. } => "Write",
        ToolCall::Mkdir { .. } | ToolCall::Edit { .. } | ToolCall::ApplyChangeset { .. } => "Edit",
        ToolCall::Skill { .. } => "Skill",
        _ => call.facts().name,
    }
}

fn command_glob(pattern: &str) -> String {
    pattern
        .split('*')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(".*")
}

fn path_glob(input: &str, decision: Decision) -> Result<Regex, &'static str> {
    if input.starts_with(['!', '#']) || input.starts_with("//") || input.starts_with('~') {
        return Err(
            "negation, comments, home and absolute anchors are unsupported; use Workspace-relative paths",
        );
    }
    let anchored = input.starts_with('/') || input.starts_with("./");
    let pattern = input
        .strip_prefix("./")
        .or_else(|| input.strip_prefix('/'))
        .unwrap_or(input);
    if pattern.is_empty()
        || pattern
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("empty path component or traversal in pattern");
    }
    let anywhere = !anchored
        && (!pattern.contains('/')
            || (decision != Decision::Allow
                && pattern.ends_with("/**")
                && pattern.matches('/').count() == 1));
    let mut expression = String::from("\\A");
    if anywhere {
        expression.push_str("(?:.*/)?");
    }
    let mut chars = pattern.chars().peekable();
    let mut segment_start = true;
    while let Some(ch) = chars.next() {
        match ch {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                if !segment_start || chars.peek().is_some_and(|ch| *ch != '/') {
                    return Err("** must occupy a whole path segment");
                }
                if chars.peek() == Some(&'/') {
                    chars.next();
                    expression.push_str("(?:[^/]+/)*");
                } else if expression.ends_with('/') {
                    expression.pop();
                    expression.push_str("(?:/.*)?");
                } else {
                    expression.push_str(".*");
                }
                segment_start = true;
                continue;
            }
            '*' => expression.push_str("[^/]*"),
            '?' => expression.push_str("[^/]"),
            '[' => {
                let mut class = String::new();
                if chars.peek() == Some(&'!') {
                    chars.next();
                    class.push('^');
                }
                loop {
                    match chars.next() {
                        Some(']') if !class.is_empty() && class != "^" => break,
                        Some(ch) if ch.is_ascii_alphanumeric() || ch == '-' => class.push(ch),
                        _ => return Err("invalid or unsupported character class"),
                    }
                }
                expression.push_str("[[");
                expression.push_str(&class);
                expression.push_str("]&&[^/]]");
            }
            '\\' => {
                let escaped = chars.next().ok_or("trailing escape")?;
                if escaped == '/' {
                    return Err("escaped path separator");
                }
                expression.push_str(&regex::escape(&escaped.to_string()));
            }
            '/' => expression.push('/'),
            ch => expression.push_str(&regex::escape(&ch.to_string())),
        }
        segment_start = ch == '/';
    }
    expression.push_str("(?:/.*)?\\z");
    compile(&expression)
}

fn compile(pattern: &str) -> Result<Regex, &'static str> {
    RegexBuilder::new(pattern)
        .size_limit(128 * 1024)
        .dfa_size_limit(32 * 1024)
        .nest_limit(32)
        .dot_matches_new_line(true)
        .build()
        .map_err(|_| "invalid or over-limit pattern")
}

#[cfg(test)]
mod tests;

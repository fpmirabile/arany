mod hooks;

pub use hooks::{
    CommandHook, HOOK_RESULT_BYTES, HookConfig, HookEvent, HookGroup, HookIgnoreReason,
    HookInvocation, HookOutcome, HookResult, MAX_HOOK_FEEDBACK_BYTES, MAX_HOOK_INPUT_BYTES,
    MAX_HOOK_REASON_BYTES, MAX_HOOKS_PER_OCCURRENCE,
};
pub(crate) use hooks::{feedback_lines, post_tool_use_input, pre_tool_use_input};

use super::catalog::Family;
use super::{ApprovalMode, ToolCall};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

pub const MAX_CONFIG_BYTES: usize = 64 * 1024;
pub const MAX_RULES: usize = 128;
pub const MAX_PATTERN_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    #[default]
    Default,
    AcceptEdits,
    Plan,
    Auto,
    DontAsk,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
    Ask,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionSource {
    Rule,
    Mode,
    Human,
    AiReview,
    UnavailableApproval,
    PinnedGrant,
    ExactCommandRule,
    Hook,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionVerdict {
    pub intent_digest: [u8; 32],
    pub decision: Decision,
    #[serde(deserialize_with = "Option::deserialize")]
    pub mode: Option<PermissionMode>,
    pub basis: PermissionSource,
    pub source: PermissionSource,
    pub approved: bool,
    #[serde(deserialize_with = "Option::deserialize")]
    pub hook_restriction: Option<Decision>,
    pub hooks: u8,
    pub narrowed: bool,
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

    pub(crate) fn matched_tool_hooks(
        &self,
        event: HookEvent,
        call: &ToolCall,
    ) -> Result<Vec<(u8, u8, &CommandHook)>, ConfigError> {
        let mut matched = Vec::new();
        if call.is_internal() {
            return Ok(matched);
        }
        for (group_index, group) in self.hooks.groups(event).iter().enumerate() {
            if !group.matches_tool(call)? {
                continue;
            }
            for (handler_index, hook) in group.commands().iter().enumerate() {
                if matched.len() == MAX_HOOKS_PER_OCCURRENCE {
                    return Err(ConfigError::Limit("8 matched hooks per occurrence"));
                }
                matched.push((
                    u8::try_from(group_index).map_err(|_| ConfigError::Limit("hook groups"))?,
                    u8::try_from(handler_index).map_err(|_| ConfigError::Limit("hooks"))?,
                    hook,
                ));
            }
        }
        Ok(matched)
    }

    pub fn approval_mode(&self, call: &ToolCall) -> Option<ApprovalMode> {
        self.evaluate(call).2
    }

    pub fn decision(&self, call: &ToolCall) -> Decision {
        self.decision_with_source(call).0
    }

    pub(crate) fn evaluate_mode(
        mode: PermissionMode,
        call: &ToolCall,
    ) -> (Decision, PermissionSource, Option<ApprovalMode>) {
        Self {
            mode,
            rules: Vec::new(),
            hooks: HookConfig::validate(Default::default()).expect("empty hooks"),
        }
        .evaluate(call)
    }

    pub(crate) fn evaluate(
        &self,
        call: &ToolCall,
    ) -> (Decision, PermissionSource, Option<ApprovalMode>) {
        self.evaluate_worker_paths(call, None)
    }

    pub(crate) fn evaluate_worker_paths(
        &self,
        call: &ToolCall,
        paths: Option<&[crate::session::worker::WorkerFileFact]>,
    ) -> (Decision, PermissionSource, Option<ApprovalMode>) {
        let (decision, source) = self.decision_with_worker_paths(call, paths);
        let approval = (decision == Decision::Ask).then_some(
            if self.mode == PermissionMode::Auto
                && source != PermissionSource::Rule
                && !call.is_unguarded()
            {
                ApprovalMode::Auto
            } else {
                ApprovalMode::Request
            },
        );
        (decision, source, approval)
    }

    fn decision_with_source(&self, call: &ToolCall) -> (Decision, PermissionSource) {
        self.decision_with_worker_paths(call, None)
    }

    fn decision_with_worker_paths(
        &self,
        call: &ToolCall,
        paths: Option<&[crate::session::worker::WorkerFileFact]>,
    ) -> (Decision, PermissionSource) {
        if !valid_call(call) {
            return (Decision::Deny, PermissionSource::Mode);
        }
        let facts = call.facts();
        if self.mode == PermissionMode::Plan
            && (facts.mutates || (facts.spawns_process && facts.family != Family::Git))
        {
            return (Decision::Deny, PermissionSource::Mode);
        }
        let auto_review = self.mode == PermissionMode::Auto
            && (facts.asks_approval
                || facts.spawns_process
                || matches!(facts.family, Family::Mcp | Family::Git));
        let allow_skips_approval = !call.is_unguarded()
            && !auto_review
            && !(self.mode == PermissionMode::AcceptEdits && facts.asks_approval && !facts.mutates);
        let matched = self.rules.iter().find_map(|(decision, rule)| {
            if *decision == Decision::Allow && !allow_skips_approval {
                None
            } else {
                rule.matches_worker_paths(call, *decision, paths)
                    .then_some(*decision)
            }
        });
        let decision = matched.unwrap_or_else(|| {
            if auto_review {
                Decision::Ask
            } else if !facts.asks_approval
                || (self.mode == PermissionMode::AcceptEdits && facts.mutates)
            {
                Decision::Allow
            } else {
                Decision::Ask
            }
        });
        if self.mode == PermissionMode::DontAsk && decision == Decision::Ask {
            (Decision::Deny, PermissionSource::Mode)
        } else {
            (
                decision,
                if matched.is_some() {
                    PermissionSource::Rule
                } else {
                    PermissionSource::Mode
                },
            )
        }
    }
}

// Provable narrowing only: a window or subtree strictly inside the original. Argv, arguments and content
// have no order, so every other operation is a new intent the model must propose.
pub(crate) fn narrows(original: &ToolCall, updated: &ToolCall) -> bool {
    use ToolCall::{GitDiff, GitLog, GitStatus, List, Read, Search};
    if !updated.valid() {
        return false;
    }
    let within = |parent: &str, child: &str| {
        matches!(parent, "" | ".")
            || child == parent
            || child
                .strip_prefix(parent)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    match (original, updated) {
        (
            Read {
                path,
                offset,
                limit,
            },
            Read {
                path: new_path,
                offset: new_offset,
                limit: new_limit,
            },
        ) => {
            path == new_path
                && new_offset >= offset
                && u64::from(*new_offset) + u64::from(*new_limit)
                    <= u64::from(*offset) + u64::from(*limit)
        }
        (List { path }, List { path: new_path })
        | (GitStatus { path }, GitStatus { path: new_path }) => within(path, new_path),
        (
            Search { path, query },
            Search {
                path: new_path,
                query: new_query,
            },
        ) => query == new_query && within(path, new_path),
        (
            GitDiff {
                path,
                base,
                head,
                staged,
            },
            GitDiff {
                path: new_path,
                base: new_base,
                head: new_head,
                staged: new_staged,
            },
        ) => (base, head, staged) == (new_base, new_head, new_staged) && within(path, new_path),
        (
            GitLog {
                path,
                revision,
                max_count,
            },
            GitLog {
                path: new_path,
                revision: new_revision,
                max_count: new_max,
            },
        ) => (revision, max_count) == (new_revision, new_max) && within(path, new_path),
        _ => false,
    }
}

fn valid_call(call: &ToolCall) -> bool {
    serde_json::to_writer(CallSize(0), call).is_ok() && call.valid()
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
    Mcp {
        server: Regex,
        tool: Option<Regex>,
        restriction: Option<Regex>,
    },
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
            name if name.starts_with("mcp__") => {
                if specifier.is_some() {
                    return Err("MCP rules do not support specifiers");
                }
                let full_name = name;
                let name = &name[5..];
                let (server, tool) = name
                    .split_once("__")
                    .map_or((name, None), |(server, tool)| (server, Some(tool)));
                if server.is_empty()
                    || server.len() > 64
                    || !server.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-*".contains(&byte)
                    })
                    || tool == Some("")
                    || tool.is_some_and(|tool| tool.len() > 128)
                    || (decision == Decision::Allow && server.contains('*'))
                {
                    return Err(
                        "MCP server/Tool pattern is invalid or over-limit; allow requires a literal server",
                    );
                }
                Tool::Mcp {
                    server: compile(&format!("\\A{}\\z", command_glob(server)))?,
                    tool: tool
                        .map(|tool| compile(&format!("\\A{}\\z", command_glob(tool))))
                        .transpose()?,
                    restriction: (decision != Decision::Allow)
                        .then(|| compile(&format!("\\A{}(?:__.*)?\\z", command_glob(full_name))))
                        .transpose()?,
                }
            }
            name if name.contains('*') => {
                if specifier.is_some() || decision == Decision::Allow {
                    return Err("tool-name wildcards support bare deny/ask rules only");
                }
                let pattern = compile(&format!("\\A{}\\z", command_glob(name)))?;
                if !super::catalog::ALL.iter().any(|facts| {
                    pattern.is_match(facts.name)
                        || pattern.is_match(peer_operation_name(facts.name))
                }) {
                    return Err("Tool-name glob matches no known peer or native operation");
                }
                Tool::NameGlob(pattern)
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

    fn matches_worker_paths(
        &self,
        call: &ToolCall,
        decision: Decision,
        paths: Option<&[crate::session::worker::WorkerFileFact]>,
    ) -> bool {
        let facts = call.facts();
        let matches_tool = match &self.tool {
            Tool::Bash => facts.family == Family::Process,
            Tool::Read => {
                facts.family == Family::File && (!facts.mutates || decision == Decision::Deny)
                    || facts.family == Family::Git && decision != Decision::Allow
            }
            Tool::Edit => facts.family == Family::File && facts.mutates,
            Tool::Write => matches!(call, ToolCall::Write { .. }),
            Tool::Glob => matches!(call, ToolCall::List { .. }),
            Tool::Grep => matches!(call, ToolCall::Search { .. }),
            Tool::Skill => facts.family == Family::Skill,
            Tool::Native(name) => facts.name == *name,
            Tool::NameGlob(pattern) => {
                pattern.is_match(&peer_name(call)) || pattern.is_match(facts.name)
            }
            Tool::Mcp {
                server,
                tool,
                restriction,
            } => {
                facts.family == Family::Mcp
                    && restriction
                        .as_ref()
                        .is_some_and(|pattern| pattern.is_match(&peer_name(call)))
                    || match call {
                        ToolCall::McpCall {
                            server: actual_server,
                            tool: actual_tool,
                            ..
                        } => {
                            server.is_match(actual_server)
                                && tool.as_ref().is_none_or(|tool| tool.is_match(actual_tool))
                        }
                        ToolCall::McpList {
                            server: actual_server,
                        } => server.is_match(actual_server) && tool.is_none(),
                        _ => false,
                    }
            }
        };
        if !matches_tool {
            return false;
        }
        let Some(pattern) = &self.pattern else {
            return true;
        };
        match call {
            ToolCall::Command { program, args, .. }
            | ToolCall::CommandUnguarded { program, args, .. } => {
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
                    || decision != Decision::Allow
                        && text
                            .match_indices(' ')
                            .any(|(index, _)| pattern.is_match(&text[index + 1..]))
            }
            ToolCall::Skill { name, .. } => pattern.is_match(name),
            ToolCall::ApplyChangeset {
                source: crate::session::worker::ChangesetSource::WorkerHandover { .. },
            } => paths.map_or(decision != Decision::Allow, |paths| {
                if decision == Decision::Allow {
                    !paths.is_empty()
                        && paths
                            .iter()
                            .all(|file| path_matches(pattern, &file.path, decision))
                } else {
                    paths
                        .iter()
                        .any(|file| path_matches(pattern, &file.path, decision))
                }
            }),
            ToolCall::ApplyChangeset {
                source: crate::session::worker::ChangesetSource::CommandReceipt { changeset, .. },
            } => {
                if decision == Decision::Allow {
                    !changeset.files.is_empty()
                        && changeset
                            .files
                            .iter()
                            .all(|file| path_matches(pattern, &file.path, decision))
                } else {
                    changeset
                        .files
                        .iter()
                        .any(|file| path_matches(pattern, &file.path, decision))
                }
            }
            _ => path_of(call).is_some_and(|path| path_matches(pattern, path, decision)),
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

pub(crate) fn peer_name(call: &ToolCall) -> Cow<'_, str> {
    match call {
        ToolCall::McpCall { server, tool, .. } => Cow::Owned(format!("mcp__{server}__{tool}")),
        ToolCall::McpList { server } => Cow::Owned(format!("mcp__{server}")),
        _ => Cow::Borrowed(peer_operation_name(call.facts().name)),
    }
}

fn peer_operation_name(name: &str) -> &str {
    match name {
        "command" | "command_unguarded" => "Bash",
        "read" => "Read",
        "list" => "Glob",
        "search" => "Grep",
        "write" => "Write",
        "mkdir" | "edit" | "apply_changeset" => "Edit",
        "skill" => "Skill",
        _ => name,
    }
}

fn path_matches(pattern: &Regex, path: &str, decision: Decision) -> bool {
    decision != Decision::Allow && !path.is_ascii() || pattern.is_match(path)
}

fn command_glob(pattern: &str) -> String {
    pattern
        .split('*')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(".*")
}

fn path_glob(input: &str, decision: Decision) -> Result<Regex, &'static str> {
    if decision != Decision::Allow && !input.is_ascii() {
        return Err(
            "deny/ask path patterns require ASCII literals; non-ASCII paths conservatively match restrictions",
        );
    }
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
    compile(&if decision == Decision::Allow {
        expression
    } else {
        format!("(?i:{expression})")
    })
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

use arany::{Effort, MAX_CATALOG_MODELS, ModelEntry, ProviderKind, resolve_native_effort};
use uuid::Uuid;

const MAX_INPUT_BYTES: usize = 8 * 1024;
const MAX_COMMAND_NAME_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractiveCommand {
    Help,
    Setup,
    Settings,
    Limits,
    Transcript,
    Paste,
    Copy,
    Status,
    Usage,
    New,
    Resume,
    Rewind,
    Goal,
    Fork,
    Rename,
    Compact,
    Agents,
    Provider,
    Model,
    Permissions,
    Cache,
    Storage,
    Exit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandAvailability {
    Available,
    ViewOnly,
    Locked,
}

impl InteractiveCommand {
    pub fn name(self) -> &'static str {
        COMMANDS
            .iter()
            .find(|spec| spec.command == self)
            .expect("compiled command")
            .name
    }

    pub fn availability(self, run_active: bool) -> CommandAvailability {
        if !run_active {
            return CommandAvailability::Available;
        }
        match self {
            Self::Help
            | Self::Usage
            | Self::Status
            | Self::Permissions
            | Self::Transcript
            | Self::Goal => CommandAvailability::Available,
            Self::Agents | Self::Provider | Self::Model | Self::Exit | Self::Paste => {
                CommandAvailability::ViewOnly
            }
            Self::Setup
            | Self::Copy
            | Self::Settings
            | Self::Limits
            | Self::Cache
            | Self::Storage
            | Self::New
            | Self::Resume
            | Self::Rewind
            | Self::Fork
            | Self::Rename
            | Self::Compact => CommandAvailability::Locked,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Submission<'a> {
    Objective(&'a str),
    Skill {
        name: &'a str,
        task: &'a str,
    },
    Command {
        command: InteractiveCommand,
        argument: Option<&'a str>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CommandParseError {
    #[error("Commands and Skills cannot be mixed with text attachments; draft retained")]
    Attachments,
    #[error("empty input")]
    Empty,
    #[error("message is too long; shorten it")]
    TooLong,
    #[error("unknown command")]
    Unknown { suggestion: Option<&'static str> },
    #[error("Usage: /{command}")]
    UnexpectedArgument { command: &'static str },
}

#[derive(Clone, Copy)]
struct CommandSpec {
    name: &'static str,
    command: InteractiveCommand,
    argument: Option<&'static str>,
    values: &'static [&'static str],
}

const fn spec(
    name: &'static str,
    command: InteractiveCommand,
    argument: Option<&'static str>,
    values: &'static [&'static str],
) -> CommandSpec {
    CommandSpec {
        name,
        command,
        argument,
        values,
    }
}

const COMMANDS: [CommandSpec; 24] = [
    spec("help", InteractiveCommand::Help, None, &[]),
    spec("setup", InteractiveCommand::Setup, None, &[]),
    spec("paste", InteractiveCommand::Paste, None, &[]),
    spec("copy", InteractiveCommand::Copy, None, &[]),
    spec("usage", InteractiveCommand::Usage, None, &[]),
    spec("status", InteractiveCommand::Status, None, &[]),
    spec("new", InteractiveCommand::New, None, &[]),
    spec("clear", InteractiveCommand::New, None, &[]),
    spec("rewind", InteractiveCommand::Rewind, None, &[]),
    spec(
        "resume",
        InteractiveCommand::Resume,
        Some("[session-id]"),
        &[],
    ),
    spec(
        "goal",
        InteractiveCommand::Goal,
        Some("[pause|resume|continue|objective]"),
        &["pause", "resume", "continue"],
    ),
    spec("fork", InteractiveCommand::Fork, Some("[session-id]"), &[]),
    spec("rename", InteractiveCommand::Rename, Some("<title>"), &[]),
    spec("compact", InteractiveCommand::Compact, None, &[]),
    spec(
        "agents",
        InteractiveCommand::Agents,
        Some("<single|auto|team> [max-active-children]"),
        &["single", "auto", "team"],
    ),
    spec(
        "provider",
        InteractiveCommand::Provider,
        Some("<provider>"),
        &ProviderKind::NAMES,
    ),
    spec(
        "model",
        InteractiveCommand::Model,
        Some("<model-id> [effort|default]"),
        &[],
    ),
    spec("permissions", InteractiveCommand::Permissions, None, &[]),
    spec(
        "cache",
        InteractiveCommand::Cache,
        Some("clear [all]"),
        &["clear"],
    ),
    spec("storage", InteractiveCommand::Storage, None, &[]),
    spec("settings", InteractiveCommand::Settings, None, &[]),
    spec("limits", InteractiveCommand::Limits, None, &[]),
    spec(
        "transcript",
        InteractiveCommand::Transcript,
        Some("[agents [number]]"),
        &["agents"],
    ),
    spec("exit", InteractiveCommand::Exit, None, &[]),
];

// `/transcript agents [N]` opens agent chat N (1 is the primary); None rejects other arguments.
pub fn transcript_agents(argument: &str) -> Option<Option<usize>> {
    let mut words = argument.split_whitespace();
    if words.next()? != "agents" {
        return None;
    }
    let number = match words.next() {
        None => None,
        Some(word) if word.len() <= 3 && word.bytes().all(|byte| byte.is_ascii_digit()) => {
            Some(word.parse().ok().filter(|number| *number > 0)?)
        }
        Some(_) => return None,
    };
    words.next().is_none().then_some(number)
}

pub(super) struct HelpEntry {
    pub command: InteractiveCommand,
    pub name: &'static str,
    pub argument: Option<&'static str>,
    pub description: &'static str,
}

pub(super) fn help_len() -> usize {
    COMMANDS.len()
}

pub(super) fn help_entry(index: usize) -> Option<HelpEntry> {
    let spec = COMMANDS.get(index)?;
    let description = match spec.command {
        InteractiveCommand::Help => "List commands",
        InteractiveCommand::Setup => "Set up an account",
        InteractiveCommand::Settings => "Slash menu, chat view and newline shortcut",
        InteractiveCommand::Limits => {
            "Task budgets, command timeouts, build resources, context window and editor changes"
        }
        InteractiveCommand::Transcript => {
            "Write or toggle transcript details; agents lists this Run's agent chats"
        }
        InteractiveCommand::Paste => "Paste clipboard into draft",
        InteractiveCommand::Copy => "Copy the latest answer to the clipboard",
        InteractiveCommand::Status => "Session status and usage",
        InteractiveCommand::Usage => "Reported token usage",
        InteractiveCommand::New => "New Session",
        InteractiveCommand::Resume => "Resume Session",
        InteractiveCommand::Rewind => "Restore code, conversation or both; compare changes",
        InteractiveCommand::Goal => {
            "Goal status; pause during a Run; resume, continue or start when idle"
        }
        InteractiveCommand::Fork => "Fork Session",
        InteractiveCommand::Rename => "Rename Session",
        InteractiveCommand::Compact => "Compact context",
        InteractiveCommand::Agents => "Agent details",
        InteractiveCommand::Provider => "Choose Provider",
        InteractiveCommand::Model => "Choose model; refresh re-reads the list",
        InteractiveCommand::Permissions => "Folder trust and approval settings",
        InteractiveCommand::Cache => "Clear private build caches",
        InteractiveCommand::Storage => "Report owned storage metadata",
        InteractiveCommand::Exit => "Exit Session",
    };
    Some(HelpEntry {
        command: spec.command,
        name: spec.name,
        argument: spec.argument,
        description,
    })
}

pub(super) struct CommandPreview {
    pub ghost: String,
    pub ghost_is_placeholder: bool,
    pub status: String,
}

#[derive(Default)]
pub(super) struct CommandChoices {
    profile: Option<String>,
    model: Option<String>,
    account_id: Option<Uuid>,
    catalog: Vec<ModelEntry>,
    catalog_loaded: bool,
    exact_custom: bool,
}

impl CommandChoices {
    pub fn set_selection(
        &mut self,
        profile: Option<&str>,
        model: Option<&str>,
        account_id: Option<Uuid>,
    ) {
        let source_changed = self.profile.as_deref() != profile || self.account_id != account_id;
        if source_changed {
            self.catalog.clear();
            self.catalog_loaded = false;
            self.exact_custom = false;
            self.profile = profile.map(str::to_owned);
            self.account_id = account_id;
        }
        if !source_changed && self.model.as_deref() == model {
            return;
        }
        self.model = model.map(str::to_owned);
    }

    pub fn set_catalog(&mut self, profile: &str, items: &[ModelEntry], exact_custom: bool) {
        if self.profile.as_deref() != Some(profile) {
            return;
        }
        self.catalog = items
            .iter()
            .filter(|item| {
                !item.id.is_empty()
                    && item.id.len() <= 128
                    && item.id.bytes().all(|byte| byte.is_ascii_graphic())
            })
            .take(MAX_CATALOG_MODELS)
            .map(|item| ModelEntry {
                id: item.id.clone(),
                context_window_tokens: item.context_window_tokens,
                runnable: item.runnable,
                efforts: item
                    .efforts
                    .iter()
                    .copied()
                    .take(Effort::ALL.len())
                    .collect(),
            })
            .collect();
        self.catalog_loaded = true;
        self.exact_custom = exact_custom;
    }

    fn effort_values(&self, model: &str) -> Vec<&'static str> {
        let Some(profile) = self.profile.as_deref() else {
            return Vec::new();
        };
        let mut values = if ProviderKind::parse(profile) == Some(ProviderKind::Custom) {
            if self.exact_custom {
                self.catalog
                    .iter()
                    .find(|entry| entry.id == model)
                    .map_or_else(Vec::new, |entry| {
                        entry.efforts.iter().map(|effort| effort.as_str()).collect()
                    })
            } else {
                Vec::new()
            }
        } else if ProviderKind::parse(profile).is_some_and(ProviderKind::is_native) {
            let reviewed = resolve_native_effort(profile, model, None).is_ok();
            Effort::ALL
                .into_iter()
                .filter(|effort| {
                    !reviewed || resolve_native_effort(profile, model, Some(*effort)).is_ok()
                })
                .map(Effort::as_str)
                .collect()
        } else {
            Vec::new()
        };
        if self.default_available(model) {
            values.push("default");
        }
        values
    }

    fn default_available(&self, model: &str) -> bool {
        let Some(profile) = self.profile.as_deref() else {
            return false;
        };
        if ProviderKind::parse(profile) == Some(ProviderKind::Custom) {
            self.exact_custom && self.catalog.iter().any(|entry| entry.id == model)
        } else {
            resolve_native_effort(profile, model, None).is_ok()
        }
    }

    fn unique_value(&self, spec: &CommandSpec, prefix: &str) -> Option<&str> {
        match spec.command {
            InteractiveCommand::Model => {
                unique_value(self.catalog.iter().map(|item| item.id.as_str()), prefix)
            }
            _ => unique_value(spec.values.iter().copied(), prefix),
        }
    }

    fn is_exact_value(&self, spec: &CommandSpec, value: &str) -> bool {
        match spec.command {
            InteractiveCommand::Model => self.catalog.iter().any(|item| item.id == value),
            _ => spec.values.contains(&value),
        }
    }
}

pub fn parse_submission(line: &str) -> Result<Submission<'_>, CommandParseError> {
    if line.len() > MAX_INPUT_BYTES {
        return Err(CommandParseError::TooLong);
    }
    if line.trim().is_empty() {
        return Err(CommandParseError::Empty);
    }
    if line.starts_with("//") {
        return Ok(Submission::Objective(&line[1..]));
    }
    let Some(control) = line.strip_prefix('/') else {
        return Ok(Submission::Objective(line));
    };
    let control = control.trim_end();
    let (name, argument) = control
        .split_once(' ')
        .map_or((control, None), |(name, rest)| (name, Some(rest.trim())));
    let argument = argument.filter(|argument| !argument.is_empty());
    if let Some(spec) = COMMANDS.iter().find(|spec| spec.name == name) {
        if spec.argument.is_none() && argument.is_some() {
            return Err(CommandParseError::UnexpectedArgument { command: spec.name });
        }
        // `/goal Resume`, `/goal CONTINUE` and `/goal PAUSE` are the same explicit words; only they are normalized.
        let argument = match argument {
            Some(word)
                if spec.command == InteractiveCommand::Goal
                    && word.eq_ignore_ascii_case("resume") =>
            {
                Some("resume")
            }
            Some(word)
                if spec.command == InteractiveCommand::Goal
                    && word.eq_ignore_ascii_case("continue") =>
            {
                Some("continue")
            }
            Some(word)
                if spec.command == InteractiveCommand::Goal
                    && word.eq_ignore_ascii_case("pause") =>
            {
                Some("pause")
            }
            other => other,
        };
        return Ok(Submission::Command {
            command: spec.command,
            argument,
        });
    }
    Err(CommandParseError::Unknown {
        suggestion: suggest(name),
    })
}

pub fn skill_invocation<'a>(text: &'a str, skills: &[String]) -> Option<(&'a str, &'a str)> {
    let token = slash_token(text, skills.iter().map(String::as_str))?;
    (token.kind == "skill").then_some((&token.text[1..], text[token.text.len()..].trim()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SlashToken<'a> {
    pub(super) text: &'a str,
    pub(super) kind: &'static str,
}

pub(super) fn slash_token<'a, 's>(
    text: &'a str,
    skills: impl IntoIterator<Item = &'s str>,
) -> Option<SlashToken<'a>> {
    let control = text.strip_prefix('/')?;
    let name = control
        .split_once(char::is_whitespace)
        .map_or(control, |(name, _)| name);
    let command_control = control.trim_end();
    let command_name = command_control
        .split_once(' ')
        .map_or(command_control, |(name, _)| name);
    let (kind, name) = if COMMANDS.iter().any(|command| command.name == command_name) {
        ("command", command_name)
    } else if COMMANDS.iter().any(|command| command.name == name) {
        return None;
    } else if skills.into_iter().any(|skill| skill == name) {
        ("skill", name)
    } else {
        return None;
    };
    Some(SlashToken {
        text: &text[..name.len() + 1],
        kind,
    })
}

pub(super) fn invoked_skill_token<'a>(
    text: &'a str,
    skill: Option<&str>,
) -> Option<SlashToken<'a>> {
    slash_token(text, [skill?]).filter(|token| token.kind == "skill")
}

pub fn command_completions(prefix: &str) -> Vec<&'static str> {
    let Some(name) = prefix.strip_prefix('/') else {
        return Vec::new();
    };
    if !valid_command_prefix(name) {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .filter(|spec| spec.name.starts_with(name))
        .map(|spec| spec.name)
        .collect()
}

pub(super) fn completion_suffix(draft: &str, choices: &CommandChoices) -> Option<String> {
    let (name, argument) = command_parts(draft)?;
    if let Some(argument) = argument {
        let spec = COMMANDS.iter().find(|spec| spec.name == name)?;
        let argument = argument.trim_start_matches(' ');
        if spec.command == InteractiveCommand::Cache {
            return match argument {
                "c" => Some("lear".to_owned()),
                "clear" => Some(" ".to_owned()),
                "clear " => Some("all".to_owned()),
                "clear a" => Some("ll".to_owned()),
                _ => None,
            };
        }
        if spec.command == InteractiveCommand::Model
            && let Some((model, effort)) = argument.split_once(' ')
        {
            let prefix = effort.trim_start_matches(' ');
            if prefix.is_empty() || prefix.contains(' ') {
                return None;
            }
            return unique_value(choices.effort_values(model).into_iter(), prefix)
                .filter(|value| *value != prefix)
                .map(|value| value[prefix.len()..].to_owned());
        }
        if argument.is_empty() || argument.contains(' ') {
            return None;
        }
        if spec.command == InteractiveCommand::Agents && matches!(argument, "auto" | "team") {
            return Some(" ".to_owned());
        }
        return choices
            .unique_value(spec, argument)
            .filter(|value| *value != argument)
            .map(|value| value[argument.len()..].to_owned());
    }
    let matches = COMMANDS
        .iter()
        .filter(|spec| spec.name.starts_with(name))
        .collect::<Vec<_>>();
    if let Some(spec) = matches.iter().find(|spec| spec.name == name) {
        return spec.argument.map(|_| " ".to_owned());
    }
    let [spec] = matches.as_slice() else {
        return None;
    };
    let mut suffix = spec.name[name.len()..].to_owned();
    if spec.argument.is_some() {
        suffix.push(' ');
    }
    Some(suffix)
}

pub(super) fn command_preview(draft: &str, choices: &CommandChoices) -> Option<CommandPreview> {
    let (name, argument) = command_parts(draft)?;
    let matches = COMMANDS
        .iter()
        .filter(|spec| spec.name.starts_with(name))
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return None;
    }
    if let Some(argument) = argument {
        let spec = matches.into_iter().find(|spec| spec.name == name)?;
        let argument = argument.trim_start_matches(' ');
        if spec.command == InteractiveCommand::Model
            && let Some((model, effort)) = argument.split_once(' ')
        {
            return Some(effort_preview(model, effort, choices));
        }
        return Some(argument_preview(spec, argument, choices));
    }
    Some(name_preview(&matches, name, choices))
}

// `/model ID EFFORT` completes the effort among the model's reasoning choices.
fn effort_preview(model: &str, effort: &str, choices: &CommandChoices) -> CommandPreview {
    let prefix = effort.trim_start_matches(' ');
    let values = choices.effort_values(model);
    let placeholder = if choices.default_available(model) {
        "<effort|default>"
    } else {
        "<effort>"
    };
    let matched = unique_value(values.iter().copied(), prefix);
    let ghost = if prefix.is_empty() {
        placeholder.to_owned()
    } else {
        matched
            .filter(|value| *value != prefix)
            .map_or_else(String::new, |value| value[prefix.len()..].to_owned())
    };
    let status = if values.contains(&prefix) {
        "Enter submits this command".to_owned()
    } else if !prefix.is_empty() && !ghost.is_empty() {
        format!("Tab: {}", matched.expect("unique effort"))
    } else {
        format!("Model reasoning · choices: {}", values.join(", "))
    };
    CommandPreview {
        ghost,
        ghost_is_placeholder: prefix.is_empty(),
        status,
    }
}

// A typed argument shows its placeholder or unique completion and whether Enter submits it.
fn argument_preview(
    spec: &CommandSpec,
    argument: &str,
    choices: &CommandChoices,
) -> CommandPreview {
    let (ghost, ghost_is_placeholder) = if argument.is_empty() {
        (argument_hint(spec, choices).unwrap_or("").to_owned(), true)
    } else if spec.command == InteractiveCommand::Provider && argument == "custom:" {
        ("<name>".to_owned(), true)
    } else if spec.command == InteractiveCommand::Agents
        && matches!(argument.trim(), "auto" | "team")
    {
        (
            format!(
                "{}[max-active-children]",
                if argument.ends_with(' ') { "" } else { " " }
            ),
            true,
        )
    } else {
        (
            choices
                .unique_value(spec, argument)
                .filter(|value| *value != argument)
                .map_or_else(String::new, |value| value[argument.len()..].to_owned()),
            false,
        )
    };
    let status = if !ghost_is_placeholder && !ghost.is_empty() {
        choices.unique_value(spec, argument).map_or_else(
            || command_status(spec, choices),
            |value| format!("Tab: {value}"),
        )
    } else if ghost.is_empty() && choices.is_exact_value(spec, argument.trim_end_matches(' ')) {
        "Enter submits this command".to_owned()
    } else {
        command_status(spec, choices)
    };
    CommandPreview {
        ghost,
        ghost_is_placeholder,
        status,
    }
}

// A command name: an exact match shows its argument hint, a unique prefix completes, several list.
fn name_preview(matches: &[&CommandSpec], name: &str, choices: &CommandChoices) -> CommandPreview {
    if let Some(spec) = matches.iter().find(|spec| spec.name == name) {
        return CommandPreview {
            ghost: argument_hint(spec, choices)
                .map_or_else(String::new, |value| format!(" {value}")),
            ghost_is_placeholder: spec.argument.is_some(),
            status: command_status(spec, choices),
        };
    }
    if let [spec] = matches {
        let ghost = format!(
            "{}{}",
            &spec.name[name.len()..],
            argument_hint(spec, choices).map_or_else(String::new, |value| format!(" {value}"))
        );
        return CommandPreview {
            ghost,
            ghost_is_placeholder: false,
            status: format!("Tab: /{}", spec.name),
        };
    }
    let names = matches
        .iter()
        .take(5)
        .map(|spec| format!("/{}", spec.name))
        .collect::<Vec<_>>()
        .join(", ");
    let remainder = matches.len().saturating_sub(5);
    let status = if remainder == 0 {
        format!("Commands: {names}")
    } else {
        format!("Commands: {names}, +{remainder} more")
    };
    CommandPreview {
        ghost: String::new(),
        ghost_is_placeholder: false,
        status,
    }
}

fn command_parts(draft: &str) -> Option<(&str, Option<&str>)> {
    let control = draft.strip_prefix('/')?;
    if control.starts_with('/') {
        return None;
    }
    let (name, argument) = control
        .split_once(' ')
        .map_or((control, None), |(name, argument)| (name, Some(argument)));
    valid_command_prefix(name).then_some((name, argument))
}

fn valid_command_prefix(name: &str) -> bool {
    name.len() <= MAX_COMMAND_NAME_BYTES && name.bytes().all(|byte| byte.is_ascii_lowercase())
}

fn unique_value<'a>(values: impl Iterator<Item = &'a str>, prefix: &str) -> Option<&'a str> {
    let mut matches = values.filter(|value| value.starts_with(prefix));
    let value = matches.next()?;
    matches.next().is_none().then_some(value)
}

fn argument_hint(spec: &CommandSpec, _choices: &CommandChoices) -> Option<&'static str> {
    spec.argument
}

fn command_status(spec: &CommandSpec, choices: &CommandChoices) -> String {
    if spec.command == InteractiveCommand::Model {
        if !choices.catalog_loaded {
            return "Command: /model <model-id> [effort|default]; run /model to load Tab choices"
                .into();
        }
        return format!(
            "Command: /model <model-id> [effort|default]; {} selectable from last /model",
            choices.catalog.len()
        );
    }
    if spec.values.is_empty() {
        format!(
            "Command: /{}{}",
            spec.name,
            spec.argument
                .map_or_else(String::new, |value| format!(" {value}"))
        )
    } else {
        format!(
            "Command: /{} {}; Tab: {}",
            spec.name,
            spec.argument.unwrap_or(""),
            spec.values.join(", ")
        )
    }
}

fn suggest(name: &str) -> Option<&'static str> {
    if name.is_empty() || !valid_command_prefix(name) {
        return None;
    }
    COMMANDS
        .iter()
        .filter_map(|spec| {
            let distance = edit_distance(name, spec.name);
            (distance <= 2).then_some((distance, spec.name))
        })
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, registered)| registered)
}

fn edit_distance(left: &str, right: &str) -> usize {
    let mut prior: Vec<_> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (row, left_byte) in left.bytes().enumerate() {
        current[0] = row + 1;
        for (column, right_byte) in right.bytes().enumerate() {
            current[column + 1] = (prior[column + 1] + 1)
                .min(current[column] + 1)
                .min(prior[column] + usize::from(left_byte != right_byte));
        }
        std::mem::swap(&mut prior, &mut current);
    }
    prior[right.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_agents_accepts_only_a_bounded_positive_agent_number() {
        for (argument, expected) in [
            ("agents", Some(None)),
            ("  agents   3 ", Some(Some(3))),
            ("agents 999", Some(Some(999))),
            ("agents 0", None),
            ("agents 1000", None),
            ("agents +2", None),
            ("agents 2 3", None),
            ("agent 2", None),
            ("details", None),
        ] {
            assert_eq!(transcript_agents(argument), expected, "{argument:?}");
        }
    }

    #[test]
    fn closed_registry_aliases_completion_and_lifecycle_are_exact() {
        assert_eq!(COMMANDS.len(), 24);
        assert_eq!(completion_suffix("/c", &CommandChoices::default()), None);
        assert_eq!(
            completion_suffix("/ca", &CommandChoices::default()),
            Some("che ".into())
        );
        for spec in COMMANDS {
            assert_eq!(
                parse_submission(&format!("/{}", spec.name)),
                Ok(Submission::Command {
                    command: spec.command,
                    argument: None,
                })
            );
            assert!(command_completions(&format!("/{}", spec.name)).contains(&spec.name));
            assert_eq!(
                parse_submission(&format!("/{}   ", spec.name)),
                Ok(Submission::Command {
                    command: spec.command,
                    argument: None,
                })
            );
            if spec.argument.is_none() {
                for suffix in ["\t", "\n", "\r", "\r\n", " \t\n\r"] {
                    assert_eq!(
                        parse_submission(&format!("/{}{suffix}", spec.name)),
                        Ok(Submission::Command {
                            command: spec.command,
                            argument: None,
                        }),
                        "trailing whitespace on /{}: {suffix:?}",
                        spec.name
                    );
                }
                for argument in ["unexpected", "two words", "\u{1b}[31m"] {
                    let message = parse_submission(&format!("/{} {argument}", spec.name))
                        .expect_err("extra text must not activate a no-argument control")
                        .to_string();
                    assert_eq!(message, format!("Usage: /{}", spec.name));
                }
                for separator in ["\t", "\n", "\r"] {
                    assert!(matches!(
                        parse_submission(&format!("/{}{separator}unexpected", spec.name)),
                        Err(CommandParseError::Unknown { .. })
                    ));
                }
            } else {
                assert_eq!(
                    parse_submission(&format!("/{} literal argument", spec.name)),
                    Ok(Submission::Command {
                        command: spec.command,
                        argument: Some("literal argument"),
                    })
                );
            }
        }
        for line in ["//literal\t", "//literal\n", "//literal\r", "literal\t\n\r"] {
            assert_eq!(
                parse_submission(line),
                Ok(Submission::Objective(if line.starts_with("//") {
                    &line[1..]
                } else {
                    line
                }))
            );
        }
        assert_eq!(parse_submission("/clear"), parse_submission("/new"));
        assert_eq!(
            parse_submission("/cache clear"),
            Ok(Submission::Command {
                command: InteractiveCommand::Cache,
                argument: Some("clear"),
            })
        );
        assert_eq!(
            parse_submission("/cache clear all"),
            Ok(Submission::Command {
                command: InteractiveCommand::Cache,
                argument: Some("clear all"),
            })
        );
        assert_eq!(
            completion_suffix("/cache c", &CommandChoices::default()),
            Some("lear".into())
        );
        assert_eq!(
            completion_suffix("/cache clear ", &CommandChoices::default()),
            Some("all".into())
        );
        assert_eq!(
            parse_submission("/settings tabs"),
            Err(CommandParseError::UnexpectedArgument {
                command: "settings"
            })
        );
        for (retired, suggestion) in [("/quit", Some("exit")), ("/sessions", None)] {
            assert_eq!(
                parse_submission(retired),
                Err(CommandParseError::Unknown { suggestion })
            );
        }
        assert_eq!(command_completions("/stat"), vec!["status"]);
        assert_eq!(command_completions("/hel"), vec!["help"]);
        assert_eq!(command_completions("/set"), vec!["setup", "settings"]);
        assert_eq!(command_completions("/pas"), vec!["paste"]);
        assert_eq!(command_completions("/mo"), vec!["model"]);
        assert_eq!(command_completions("/goa"), vec!["goal"]);
        assert_eq!(command_completions("/pau"), Vec::<&str>::new());
        assert_eq!(
            completion_suffix("/goal r", &CommandChoices::default()),
            Some("esume".into())
        );
        assert_eq!(
            completion_suffix("/goal p", &CommandChoices::default()),
            Some("ause".into())
        );
        assert_eq!(
            parse_submission("/goal pause"),
            Ok(Submission::Command {
                command: InteractiveCommand::Goal,
                argument: Some("pause"),
            })
        );
        assert_eq!(
            parse_submission("/goal  PAUSE "),
            Ok(Submission::Command {
                command: InteractiveCommand::Goal,
                argument: Some("pause"),
            })
        );
        assert_eq!(
            parse_submission("/goal"),
            Ok(Submission::Command {
                command: InteractiveCommand::Goal,
                argument: None,
            })
        );
        assert_eq!(
            parse_submission("/goal resume"),
            Ok(Submission::Command {
                command: InteractiveCommand::Goal,
                argument: Some("resume"),
            })
        );
        assert_eq!(
            parse_submission("/goal  Resume "),
            Ok(Submission::Command {
                command: InteractiveCommand::Goal,
                argument: Some("resume"),
            })
        );
        assert_eq!(
            parse_submission("/goal  Continue "),
            Ok(Submission::Command {
                command: InteractiveCommand::Goal,
                argument: Some("continue"),
            })
        );
        assert_eq!(
            completion_suffix("/goal c", &CommandChoices::default()),
            Some("ontinue".into())
        );
        assert_eq!(
            parse_submission("/goal continue now"),
            Ok(Submission::Command {
                command: InteractiveCommand::Goal,
                argument: Some("continue now"),
            })
        );
        assert_eq!(
            parse_submission("/goal resume now"),
            Ok(Submission::Command {
                command: InteractiveCommand::Goal,
                argument: Some("resume now"),
            })
        );
        assert_eq!(
            parse_submission("/goal  finish the migration "),
            Ok(Submission::Command {
                command: InteractiveCommand::Goal,
                argument: Some("finish the migration"),
            })
        );
        assert_eq!(
            parse_submission("/resume-goal"),
            Err(CommandParseError::Unknown { suggestion: None })
        );
        assert_eq!(
            InteractiveCommand::Goal.availability(true),
            CommandAvailability::Available
        );
        let preview = command_preview("/goal", &CommandChoices::default()).unwrap();
        assert_eq!(
            (
                preview.ghost.as_str(),
                preview.ghost_is_placeholder,
                preview.status.as_str()
            ),
            (
                " [pause|resume|continue|objective]",
                true,
                "Command: /goal [pause|resume|continue|objective]; Tab: pause, resume, continue",
            )
        );
        assert_eq!(
            parse_submission("/models"),
            Err(CommandParseError::Unknown {
                suggestion: Some("model"),
            })
        );
        assert_eq!(
            parse_submission("/resume-goa"),
            Err(CommandParseError::Unknown { suggestion: None })
        );
        assert!(command_completions("/sessions").is_empty());
        assert_eq!(
            InteractiveCommand::Resume.availability(true),
            CommandAvailability::Locked
        );
        assert!(command_completions("/Help").is_empty());
        assert_eq!(
            InteractiveCommand::Provider.availability(true),
            CommandAvailability::ViewOnly
        );
        assert_eq!(
            InteractiveCommand::New.availability(true),
            CommandAvailability::Locked
        );
        assert_eq!(
            InteractiveCommand::Setup.availability(true),
            CommandAvailability::Locked
        );
        assert_eq!(
            InteractiveCommand::Model.availability(true),
            CommandAvailability::ViewOnly
        );
        assert_eq!(
            InteractiveCommand::Help.availability(true),
            CommandAvailability::Available
        );
        assert_eq!(
            InteractiveCommand::Paste.availability(true),
            CommandAvailability::ViewOnly
        );
        assert_eq!(
            InteractiveCommand::Copy.availability(true),
            CommandAvailability::Locked
        );
    }

    #[test]
    fn leading_slash_corpus_never_turns_unknown_controls_into_objectives() {
        assert_eq!(
            parse_submission("//literal"),
            Ok(Submission::Objective("/literal"))
        );
        assert_eq!(
            parse_submission(" /help"),
            Ok(Submission::Objective(" /help"))
        );
        assert_eq!(
            parse_submission("@literal"),
            Ok(Submission::Objective("@literal"))
        );
        assert_eq!(
            parse_submission("/provider openai"),
            Ok(Submission::Command {
                command: InteractiveCommand::Provider,
                argument: Some("openai"),
            })
        );
        assert_eq!(
            parse_submission("/hepl"),
            Err(CommandParseError::Unknown {
                suggestion: Some("help"),
            })
        );
        for line in ["/Help", "/helpful", "/help\tbad", "/\u{1b}[31m"] {
            assert!(matches!(
                parse_submission(line),
                Err(CommandParseError::Unknown { .. })
            ));
        }
        assert_eq!(parse_submission(" "), Err(CommandParseError::Empty));
        assert_eq!(
            parse_submission(&"a".repeat(MAX_INPUT_BYTES + 1)),
            Err(CommandParseError::TooLong)
        );
    }

    #[test]
    fn previews_and_tab_suffixes_are_trusted_and_nonexecuting() {
        let choices = CommandChoices::default();
        assert_eq!(completion_suffix("/prov", &choices), Some("ider ".into()));
        assert_eq!(completion_suffix("/provider", &choices), Some(" ".into()));
        assert_eq!(
            completion_suffix("/provider ope", &choices),
            Some("nai".into())
        );
        assert_eq!(
            completion_suffix("/provider   ope", &choices),
            Some("nai".into())
        );
        assert_eq!(
            completion_suffix("/provider cus", &choices),
            Some("tom:".into())
        );
        assert_eq!(
            completion_suffix("/agents auto", &choices),
            Some(" ".into())
        );
        assert_eq!(completion_suffix("/mo", &choices), Some("del ".into()));
        assert_eq!(
            command_preview("/hel", &choices).unwrap().status,
            "Tab: /help"
        );
        assert_eq!(completion_suffix("/provider ", &choices), None);
        assert_eq!(completion_suffix("//prov", &choices), None);
        assert_eq!(completion_suffix("/Provider", &choices), None);
        assert_eq!(
            command_preview("/prov", &choices).unwrap().ghost,
            "ider <provider>"
        );
        assert_eq!(
            command_preview("/provider", &choices).unwrap().ghost,
            " <provider>"
        );
        assert_eq!(
            command_preview("/provider ", &choices).unwrap().ghost,
            "<provider>"
        );
        assert_eq!(
            command_preview("/provider ope", &choices).unwrap().ghost,
            "nai"
        );
        assert_eq!(
            command_preview("/provider custom:", &choices)
                .unwrap()
                .ghost,
            "<name>"
        );
        assert!(
            command_preview("/provider openai", &choices)
                .unwrap()
                .ghost
                .is_empty()
        );
        assert_eq!(
            command_preview("/provider ope", &choices).unwrap().status,
            "Tab: openai"
        );
        assert_eq!(
            command_preview("/provider openai", &choices)
                .unwrap()
                .status,
            "Enter submits this command"
        );
        assert_eq!(
            command_preview("/provider openai ", &choices)
                .unwrap()
                .status,
            "Enter submits this command"
        );
        assert_eq!(
            command_preview("/agents auto", &choices).unwrap().ghost,
            " [max-active-children]"
        );
        assert_eq!(
            command_preview("/agents auto ", &choices).unwrap().ghost,
            "[max-active-children]"
        );
        assert!(
            command_preview("/agents auto 2", &choices)
                .unwrap()
                .ghost
                .is_empty()
        );
        assert!(command_preview("//literal", &choices).is_none());
        assert!(command_preview("/unknown", &choices).is_none());
        assert!(
            command_preview("/mo", &choices)
                .unwrap()
                .status
                .contains("/model")
        );
    }

    #[test]
    fn model_and_effort_hints_follow_the_explicit_catalog_and_selection() {
        let mut choices = CommandChoices::default();
        assert!(command_preview("/effort", &choices).is_none());
        assert!(completion_suffix("/effort hig", &choices).is_none());
        assert!(matches!(
            parse_submission("/effort high"),
            Err(CommandParseError::Unknown { .. })
        ));
        assert!(
            command_preview("/model ", &choices)
                .unwrap()
                .status
                .contains("run /model")
        );
        for (profile, model, default, allowed) in [
            ("chatgpt", "visible-chat-model", false, &Effort::ALL[..]),
            ("openai", "gpt-unreviewed", false, &Effort::ALL[..]),
            ("anthropic", "claude-unreviewed", false, &Effort::ALL[..]),
            ("openai", "gpt-5.4", true, &Effort::ALL[..5]),
            ("anthropic", "claude-sonnet-5", true, &Effort::ALL[1..]),
        ] {
            choices.set_selection(Some(profile), Some("another-model"), None);
            for effort in Effort::ALL {
                let value = effort.as_str();
                let prefix = format!("/model {model} {}", &value[..value.len() - 1]);
                assert_eq!(
                    completion_suffix(&prefix, &choices),
                    allowed
                        .contains(&effort)
                        .then(|| value[value.len() - 1..].to_owned()),
                    "{profile}/{model}/{effort}"
                );
            }
            assert_eq!(
                completion_suffix(&format!("/model {model} def"), &choices),
                default.then(|| "ault".to_owned())
            );
            assert_eq!(
                command_preview(&format!("/model {model} "), &choices)
                    .unwrap()
                    .ghost,
                if default {
                    "<effort|default>"
                } else {
                    "<effort>"
                }
            );
            assert_eq!(
                command_preview(&format!("/model {model} high"), &choices)
                    .unwrap()
                    .status,
                "Enter submits this command"
            );
        }
        let entries = [
            ModelEntry {
                id: "gpt-5.4".into(),
                context_window_tokens: None,
                runnable: true,
                efforts: vec![Effort::Low, Effort::Medium],
            },
            ModelEntry {
                id: "gpt-unreviewed".into(),
                context_window_tokens: None,
                runnable: false,
                efforts: vec![],
            },
        ];
        choices.set_selection(Some("openai"), Some("gpt-5.4"), None);
        choices.set_catalog("openai", &entries, false);
        for (prefix, suffix) in [
            ("/model gpt-5", ".4"),
            ("/model gpt-u", "nreviewed"),
            ("/model gpt-5.4 med", "ium"),
        ] {
            assert_eq!(completion_suffix(prefix, &choices), Some(suffix.into()));
        }
        assert_eq!(
            command_preview("/model gpt-5.4 medium", &choices)
                .unwrap()
                .status,
            "Enter submits this command"
        );
        choices.set_selection(Some("anthropic"), Some("claude-sonnet-5"), None);
        assert!(completion_suffix("/model gpt", &choices).is_none());
        choices.set_catalog("openai", &entries, false);
        assert!(completion_suffix("/model gpt", &choices).is_none());
        choices.set_selection(Some("custom:local"), Some("owned-model"), None);
        let entries = [ModelEntry {
            id: "owned-model".into(),
            context_window_tokens: None,
            runnable: false,
            efforts: vec![Effort::High],
        }];
        choices.set_catalog("custom:local", &entries, true);
        for (draft, suffix) in [
            ("/model owned-m", "odel"),
            ("/model owned-model hig", "h"),
            ("/model owned-model def", "ault"),
        ] {
            assert_eq!(completion_suffix(draft, &choices), Some(suffix.into()));
        }
        assert!(completion_suffix("/model owned-model low", &choices).is_none());
        assert!(completion_suffix("/model different-model hig", &choices).is_none());
        assert!(completion_suffix("/model different-model def", &choices).is_none());
        choices.set_selection(Some("custom:unloaded"), Some("owned-model"), None);
        choices.set_catalog("custom:unloaded", &entries, false);
        assert!(completion_suffix("/model owned-model hig", &choices).is_none());
        assert!(completion_suffix("/model owned-model def", &choices).is_none());
        choices.set_selection(Some("openai"), Some("gpt-unreviewed"), None);
        let invalid = [
            "",
            "bad\nmodel",
            "\u{1b}[31mmodel",
            "módel",
            &"x".repeat(129),
        ]
        .into_iter()
        .map(|id| ModelEntry {
            id: id.into(),
            context_window_tokens: None,
            runnable: false,
            efforts: vec![],
        })
        .collect::<Vec<_>>();
        choices.set_catalog("openai", &invalid, false);
        for prefix in ["/model bad", "/model m", "/model x"] {
            assert_eq!(completion_suffix(prefix, &choices), None);
        }
    }
}

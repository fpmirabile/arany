use crate::session::{
    AgentStatus, Event, EventEnvelope, ProviderCallDisposition, ProviderCallRecord, RunStatus,
    RunView, SessionView,
};
use serde::Serialize;
use std::borrow::Cow;
use unicode_width::UnicodeWidthStr;

mod agents;
mod model;
pub(crate) use agents::AgentInspectorModel;
pub(crate) use model::{DraftAction, PresentationModel};
pub(crate) use model::{linear_session_lines, model_id_preview, safe_truncate};

impl std::fmt::Display for crate::session::CollaborationPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Single => f.write_str("single"),
            Self::Auto {
                max_active_children,
            } => write!(f, "auto · up to {max_active_children} children"),
            Self::Team {
                max_active_children,
            } => write!(f, "team · up to {max_active_children} children"),
        }
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(model::run_state_label(Some(*self)))
    }
}

impl std::fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(model::agent_state_label(*self))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Output {
    Text,
    Jsonl,
}

pub(crate) fn tool_approval_lines(intent: &crate::tools::EffectIntent) -> Vec<String> {
    use crate::tools::ToolCall;
    let mut lines = vec!["Approve this action?".to_owned()];
    match &intent.call {
        ToolCall::Write {
            path,
            expected_digest,
            content,
        } => {
            lines.push(format!(
                "{} {}",
                if expected_digest.is_some() {
                    "Replace"
                } else {
                    "Create"
                },
                escape_terminal(path)
            ));
            lines.push("Content:".into());
            lines.extend(
                content
                    .split('\n')
                    .map(|line| format!("  {}", escape_terminal(line))),
            );
        }
        ToolCall::Edit { path, old, new, .. } => {
            lines.push(format!(
                "Edit {} · only if the file is unchanged",
                escape_terminal(path)
            ));
            lines.push("Remove:".into());
            lines.extend(
                old.split('\n')
                    .map(|line| format!("- {}", escape_terminal(line))),
            );
            lines.push("Insert:".into());
            lines.extend(
                new.split('\n')
                    .map(|line| format!("+ {}", escape_terminal(line))),
            );
        }
        ToolCall::Mkdir { path } => lines.push(format!("Create folder {}", escape_terminal(path))),
        ToolCall::Command { program, args, cwd } => {
            lines.push(format!(
                "Run {} in {}",
                escape_terminal(program),
                if cwd.is_empty() {
                    "project root".into()
                } else {
                    escape_terminal(cwd)
                }
            ));
            lines.push("Offline isolated copy · command changes are discarded".into());
            for (index, argument) in args.iter().enumerate() {
                lines.push(format!(
                    "Argument {}: {}",
                    index + 1,
                    escape_terminal(&serde_json::to_string(argument).expect("argument string"))
                ));
            }
        }
        ToolCall::McpCall {
            server,
            tool,
            arguments,
            ..
        } => {
            lines.push(format!(
                "Call {} / {}",
                escape_terminal(server),
                escape_terminal(tool)
            ));
            lines.push(format!("Arguments: {}", escape_terminal(arguments)));
        }
        _ => lines.push(escape_terminal(
            &serde_json::to_string(&intent.call).expect("typed action"),
        )),
    }
    lines.push(String::new());
    lines.push("Allow once for this exact action · expires within one minute".into());
    lines
}

pub(crate) fn workspace_permission_lines(path: &std::path::Path) -> Vec<String> {
    vec![
        "Do you trust this folder?".into(),
        escape_terminal(&path.to_string_lossy()),
        String::new(),
        "Arany can read and automatically edit this project.".into(),
        "Commands run offline in an isolated copy and ask first.".into(),
        "Trust is remembered for this folder. Shift+Tab changes approval mode.".into(),
    ]
}

pub fn render_run_feedback(run: &RunView) -> Option<String> {
    match run.status {
        RunStatus::Failed => {
            let reason = run
                .agents
                .iter()
                .filter(|agent| agent.status == AgentStatus::Failed)
                .filter_map(|agent| agent.provider_calls.last())
                .filter(|call| call.phase != crate::provider::AgentPhase::ToolReview)
                .find_map(provider_failure_notice);
            if let Some(reason) = reason {
                return Some(format!(
                    "Error: Run failed. {reason} No answer was committed. Use /agents to inspect this Run."
                ));
            }
            if run.tools.last().is_some_and(|tool| {
                tool.observation.as_ref().is_some_and(|observation| {
                    observation.disposition == crate::tools::ToolDisposition::Failed
                        && observation.guard.is_none()
                })
            }) {
                return Some("Error: Native Tool protection failed before the action started. That action did not run; earlier completed actions remain. Use /agents to inspect the Guard stage before submitting again.".into());
            }
            if run.tools.last().is_some_and(|tool| {
                tool.observation.as_ref().is_some_and(|observation| {
                    observation.disposition == crate::tools::ToolDisposition::Denied
                        && observation.guard.is_none()
                })
            }) {
                return Some("Action denied or approval expired. No further action ran; earlier completed actions remain. Use /permissions or Shift+Tab to review approval settings.".into());
            }
            if let Some(tool) = run.tools.last()
                && tool.observation.as_ref().is_none_or(|observation| {
                    matches!(
                        observation.disposition,
                        crate::tools::ToolDisposition::Uncertain
                            | crate::tools::ToolDisposition::Cancelled
                    )
                })
            {
                return Some("Error: Tool completion is uncertain. Files or effects may have changed; inspect them before retrying. No answer was committed.".into());
            }
            if run.primary_tool_limit_reached() {
                return Some("Error: Tool continuation budget exhausted. No answer was committed; completed Tool observations remain in the Session.".into());
            }
            Some(
                "Error: Run failed. No answer was committed. Use /agents to inspect this Run."
                    .into(),
            )
        }
        RunStatus::Cancelled => {
            Some("Run cancelled. No answer was committed. You can submit a new task.".to_owned())
        }
        _ => None,
    }
}

fn provider_failure_notice(call: &ProviderCallRecord) -> Option<&'static str> {
    use crate::provider::ProviderFailureReason;
    match (call.disposition, call.failure_reason) {
        (ProviderCallDisposition::Rejected, Some(ProviderFailureReason::AccountAccess)) => {
            return Some(
                "Provider denied account access. Check the selected account and model permissions.",
            );
        }
        (ProviderCallDisposition::Rejected, Some(ProviderFailureReason::UsageLimit)) => {
            return Some(
                "Provider reported a usage limit. Check the selected account's usage before trying again.",
            );
        }
        (
            ProviderCallDisposition::Unavailable,
            Some(ProviderFailureReason::UsageTemporarilyUnavailable),
        ) => {
            return Some(
                "Provider usage is temporarily unavailable. Check provider status before trying again.",
            );
        }
        (ProviderCallDisposition::Unavailable, Some(ProviderFailureReason::ServiceUnavailable)) => {
            return Some(
                "Provider service unavailable. Check provider status before trying again.",
            );
        }
        (ProviderCallDisposition::InvalidResponse, Some(ProviderFailureReason::StreamProtocol)) => {
            return Some(
                "Provider stream was incomplete or malformed. No answer was accepted; plan usage may have occurred.",
            );
        }
        (
            ProviderCallDisposition::InvalidResponse,
            Some(ProviderFailureReason::ResponseContract),
        ) => {
            return Some(
                "Provider response did not match the selected model, completed-message, or usage contract. No answer was accepted.",
            );
        }
        (
            ProviderCallDisposition::InvalidResponse,
            Some(ProviderFailureReason::OutcomeContract),
        ) => {
            return Some(
                "Provider output did not match Arany's structured outcome contract. No answer was accepted.",
            );
        }
        (
            ProviderCallDisposition::InvalidResponse,
            Some(ProviderFailureReason::LocalOutputLimit),
        ) => {
            return Some(
                "Provider reported output above Arany's local token limit. No answer was accepted; this limit does not cap remote plan usage.",
            );
        }
        _ => {}
    }
    Some(match call.disposition {
        ProviderCallDisposition::Unavailable => {
            "Provider unavailable. Check the connection and provider status."
        }
        ProviderCallDisposition::Rejected => {
            "Provider rejected the request. Check the selected account, model, and usage limits."
        }
        ProviderCallDisposition::InvalidResponse => {
            "Provider returned an incompatible response. Check the selected model and effort."
        }
        ProviderCallDisposition::OutputLimit => "Provider response exceeded Arany's output limit.",
        ProviderCallDisposition::TimedOut => "Provider call timed out.",
        ProviderCallDisposition::TaskPanic => "Provider task failed internally.",
        ProviderCallDisposition::Finished
        | ProviderCallDisposition::Delegated
        | ProviderCallDisposition::ToolRequested
        | ProviderCallDisposition::Cancelled => return None,
    })
}

#[derive(Serialize)]
struct EventLine {
    sequence: u64,
    session_id: String,
    run_id: Option<String>,
    agent_run_id: Option<String>,
    kind: &'static str,
    event_version: u32,
    payload: serde_json::Value,
    created_at_ms: i64,
}

pub fn render_session(view: &SessionView, events: &[EventEnvelope], output: Output) -> String {
    let mut rendered = String::new();
    if output == Output::Text {
        rendered.push_str(&format!(
            "Session: {}\nTitle: {}\n",
            view.id,
            escape_terminal(&view.conversation_title())
        ));
    }
    for envelope in events {
        let detail = match &envelope.event {
            Event::SessionStarted { title, .. } | Event::SessionRenamed { title } => title.clone(),
            Event::MessageAccepted { text, images, .. } if output == Output::Text && !images.is_empty() => {
                serde_json::json!({ "text": text, "images": images.iter().enumerate().map(|(index, image)| image.description(index)).collect::<Vec<_>>() }).to_string()
            }
            _ => envelope.event.payload().expect("validated Event"),
        };
        match output {
            Output::Text => rendered.push_str(&format!(
                "{} {} {}\n",
                envelope.sequence,
                envelope.event.kind(),
                escape_terminal(&detail)
            )),
            Output::Jsonl => {
                append_jsonl(&mut rendered, envelope);
            }
        }
    }
    rendered
}

pub fn render_exec(
    run: &RunView,
    events: &[EventEnvelope],
    output: Output,
    new_session: bool,
) -> String {
    match output {
        Output::Text => render_answer(run, "Answer:"),
        Output::Jsonl => {
            let mut rendered = String::new();
            for envelope in events {
                if envelope.run_id == Some(run.id)
                    || new_session && matches!(envelope.event, Event::SessionStarted { .. })
                {
                    append_jsonl(&mut rendered, envelope);
                }
            }
            rendered
        }
    }
}

pub(crate) fn render_answer(run: &RunView, heading: &str) -> String {
    let Some(message) = &run.assistant_message else {
        return String::new();
    };
    let mut rendered = format!("{heading}\n");
    for line in message.split('\n') {
        rendered.push_str("  ");
        rendered.push_str(&escape_terminal(line));
        rendered.push('\n');
    }
    rendered
}

pub(crate) fn user_message_lines(objective: &str, width: usize) -> Vec<String> {
    let mut rendered = vec![safe_truncate("You:", width)];
    let mut lines = objective.split('\n');
    for line in lines.by_ref().take(4) {
        rendered.push(safe_truncate(&format!("  {line}"), width));
    }
    if lines.next().is_some() {
        rendered.pop();
        rendered.push(safe_truncate("  … (more in Session history)", width));
    }
    rendered
}

pub(crate) fn image_message_text<'a>(
    objective: &'a str,
    images: &[crate::provider::ImageAttachment],
) -> Cow<'a, str> {
    if images.is_empty() {
        return Cow::Borrowed(objective);
    }
    let mut text = objective.to_owned();
    for (index, image) in images.iter().enumerate() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&image_description(image, index));
    }
    Cow::Owned(text)
}

pub(crate) fn image_message_lines(
    images: &[crate::provider::ImageAttachment],
    width: usize,
) -> Vec<String> {
    images
        .iter()
        .enumerate()
        .map(|(index, image)| {
            safe_truncate(&format!("  {}", image_description(image, index)), width)
        })
        .collect()
}

fn image_description(image: &crate::provider::ImageAttachment, index: usize) -> String {
    let (width, height) = image.dimensions();
    format!(
        "Image {}: PNG {width}x{height}, {:.1} KB",
        index + 1,
        image.byte_len() as f64 / 1_000.0
    )
}

fn append_jsonl(rendered: &mut String, envelope: &EventEnvelope) {
    let line = EventLine {
        sequence: envelope.sequence,
        session_id: envelope.session_id.to_string(),
        run_id: envelope.run_id.map(|id| id.to_string()),
        agent_run_id: envelope.agent_run_id.map(|id| id.to_string()),
        kind: envelope.event.kind(),
        event_version: envelope.event.version() as u32,
        payload: serde_json::from_str(&envelope.event.payload().expect("validated Event"))
            .expect("valid Event JSON"),
        created_at_ms: envelope.created_at_ms,
    };
    let json = serde_json::to_string(&line).expect("serializable Event");
    rendered.push_str(&escape_json_controls(&json));
    rendered.push('\n');
}

/// Escapes terminal control and bidirectional formatting characters in untrusted text.
pub fn escape_terminal(value: &str) -> String {
    let mut safe = String::with_capacity(value.len());
    for character in value.chars() {
        let code = character as u32;
        if unsafe_display_character(character) {
            safe.push_str(&format!("\\u{{{code:04x}}}"));
        } else {
            safe.push(character);
        }
    }
    safe
}

pub(crate) fn escaped_terminal_width(value: &str) -> usize {
    let mut characters = value.chars();
    if let Some(character) = characters.next()
        && characters.next().is_none()
    {
        return if unsafe_display_character(character) {
            character.escape_unicode().count().max(8)
        } else {
            UnicodeWidthStr::width(value)
        };
    }
    if value.chars().any(unsafe_display_character) {
        UnicodeWidthStr::width(escape_terminal(value).as_str())
    } else {
        UnicodeWidthStr::width(value)
    }
}

fn escape_json_controls(value: &str) -> String {
    let mut safe = String::with_capacity(value.len());
    for character in value.chars() {
        if unsafe_display_character(character) {
            safe.push_str(&format!("\\u{:04x}", character as u32));
        } else {
            safe.push(character);
        }
    }
    safe
}

fn unsafe_display_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character as u32,
            0x202a..=0x202e | 0x2066..=0x2069 | 0x061c | 0x200e | 0x200f | 0x2028 | 0x2029
        )
}

pub(crate) fn session_access_label(defaults: &crate::session::SessionDefaults) -> &str {
    match defaults.provider.as_deref() {
        Some("chatgpt") => "ChatGPT plan",
        Some("openai") => "OpenAI API",
        Some("anthropic") => "Anthropic API",
        Some(profile) => profile,
        None => "User default",
    }
}

#[cfg(test)]
mod tests {
    use super::user_message_lines;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn user_message_preview_is_labeled_bounded_and_inert() {
        let lines = user_message_lines("first\u{1b}[2J\nsecond\nthird\nfourth\nfifth", 20);
        assert_eq!(lines[0], "You:");
        assert_eq!(lines[1], "  first\\u{001b}[2J");
        assert_eq!(lines[2], "  second");
        assert_eq!(lines[3], "  third");
        assert!(lines[4].contains('…'));
        assert_eq!(lines.len(), 5);
        assert!(
            lines
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) <= 20)
        );
        assert!(!lines.join("\n").contains('\u{1b}'));
        assert_eq!(user_message_lines("你好", 5), ["You:", "  你…"]);
    }
}

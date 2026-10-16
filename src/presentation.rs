//! Pure over committed `SessionView`/`RunView` facts and the CLI-owned unsaved conversation
//! projection: a zero creation sequence denotes that local projection, never a replayed Session;
//! label it unsaved and expose no temporary ID. Engine imports no terminal libraries or
//! presentation state.
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
pub(crate) use model::{
    linear_session_lines, model_id_preview, safe_truncate, tool_capacity_notice,
};

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

pub(crate) fn tool_summary(tool: &crate::session::ToolView) -> String {
    use crate::tools::{ToolCall, ToolDisposition};
    let operation = match &tool.intent.call {
        ToolCall::List { path } => format!("List {path}"),
        ToolCall::Read { path, .. } => format!("Read {path}"),
        ToolCall::Search { path, .. } => format!("Search {path}"),
        ToolCall::Mkdir { path } => format!("Create folder {path}"),
        ToolCall::Write { path, .. } => format!("Write {path}"),
        ToolCall::Edit { path, .. } => format!("Edit {path}"),
        ToolCall::Command { program, .. } => format!("Command {program}"),
        ToolCall::ApplyChangeset { changeset, .. } => {
            format!("Publish {} command file changes", changeset.files.len())
        }
        ToolCall::Skill { name, .. } => format!("Skill {name}"),
        ToolCall::McpList { server } => format!("MCP list {server}"),
        ToolCall::McpCall { server, tool, .. } => format!("MCP {server} / {tool}"),
        ToolCall::GitStatus { path } => format!("Git status {path}"),
        ToolCall::GitDiff { path, .. } => format!("Git diff {path}"),
        ToolCall::GitLog { path, .. } => format!("Git log {path}"),
    };
    let state = match tool
        .observation
        .as_ref()
        .map(|observation| observation.disposition)
    {
        None => "pending",
        Some(ToolDisposition::Succeeded) => "succeeded",
        Some(ToolDisposition::Denied) => "denied",
        Some(ToolDisposition::Failed) => "failed",
        Some(ToolDisposition::Conflict) => "conflict",
        Some(ToolDisposition::Limit) => "limit",
        Some(ToolDisposition::Cancelled) => "cancelled",
        Some(ToolDisposition::Uncertain) => "uncertain",
    };
    let cache_update = tool
        .observation
        .as_ref()
        .and_then(|observation| cache_update_label(observation.cache_update));
    match cache_update {
        Some(cache_update) => format!(
            "Tool · {} · {state} · {cache_update}",
            safe_truncate(&operation, 160)
        ),
        None => format!("Tool · {} · {state}", safe_truncate(&operation, 160)),
    }
}

fn cache_update_label(update: crate::tools::CacheUpdate) -> Option<&'static str> {
    use crate::tools::CacheUpdate;
    match update {
        CacheUpdate::NotApplicable => None,
        CacheUpdate::NotAttempted => Some("cache not attempted"),
        CacheUpdate::Published => Some("cache saved"),
        CacheUpdate::Rejected => Some("cache not saved (limit)"),
        CacheUpdate::Failed => Some("cache update failed"),
        CacheUpdate::Uncertain => Some("cache outcome uncertain"),
    }
}

pub fn render_cache_update_feedback(run: &RunView) -> Option<&'static str> {
    use crate::tools::CacheUpdate;
    let updates = run.tools.iter().filter_map(|tool| {
        tool.observation
            .as_ref()
            .map(|observation| observation.cache_update)
    });
    let mut rejected = false;
    let mut failed = false;
    for update in updates {
        match update {
            CacheUpdate::Uncertain => {
                return Some(
                    "Build cache outcome is uncertain; inspect Session history before retrying.",
                );
            }
            CacheUpdate::Rejected => rejected = true,
            CacheUpdate::Failed => failed = true,
            CacheUpdate::NotApplicable | CacheUpdate::NotAttempted | CacheUpdate::Published => {}
        }
    }
    if failed {
        Some("A build cache update failed; inspect Tool history for details.")
    } else if rejected {
        Some(
            "A build cache update was rejected by its size or quota limit; the previous cache was kept.",
        )
    } else {
        None
    }
}

pub(crate) fn tool_detail_text(tool: &crate::session::ToolView) -> String {
    let arguments = serde_json::to_string(&tool.intent.call).expect("typed Tool call");
    format!(
        "{}\nArguments: {arguments}\nResult:\n{}",
        tool_summary(tool),
        tool.observation
            .as_ref()
            .map_or("Awaiting result", |observation| observation.output.as_str())
    )
}

pub(crate) fn display_answer(run: &RunView) -> Option<String> {
    Some(display_answer_chunks(run)?.collect())
}

pub(crate) fn display_answer_chunks(run: &RunView) -> Option<impl Iterator<Item = &str>> {
    let call = run
        .display
        .iter()
        .filter(|chunk| chunk.category == crate::session::DisplayCategory::Answer)
        .map(|chunk| chunk.call_index)
        .max()?;
    if !display_call_visible(run, call) {
        return None;
    }
    Some(
        run.display
            .iter()
            .filter(move |chunk| {
                chunk.call_index == call
                    && chunk.category == crate::session::DisplayCategory::Answer
            })
            .map(|chunk| chunk.text.as_str()),
    )
}

pub(crate) fn display_call_visible(run: &RunView, call: u32) -> bool {
    run.agents
        .first()
        .and_then(|agent| {
            usize::try_from(call)
                .ok()
                .and_then(|index| agent.provider_calls.get(index))
        })
        .is_none_or(|record| {
            matches!(
                record.disposition,
                ProviderCallDisposition::Finished
                    | ProviderCallDisposition::Delegated
                    | ProviderCallDisposition::ToolRequested
                    | ProviderCallDisposition::Unavailable
                    | ProviderCallDisposition::TimedOut
                    | ProviderCallDisposition::Cancelled
                    | ProviderCallDisposition::TaskPanic
            )
        })
}

pub(crate) fn display_commentary(run: &RunView) -> Vec<(u32, u8, String)> {
    let mut items: Vec<(u32, u8, String)> = Vec::new();
    for chunk in &run.display {
        if chunk.category != crate::session::DisplayCategory::Commentary
            || !display_call_visible(run, chunk.call_index)
        {
            continue;
        }
        if let Some((_, _, text)) = items
            .iter_mut()
            .find(|(call, item, _)| *call == chunk.call_index && *item == chunk.item)
        {
            text.push_str(&chunk.text);
        } else {
            items.push((chunk.call_index, chunk.item, chunk.text.clone()));
        }
    }
    items
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
            lines.push("Offline isolated copy · changes are validated before publication".into());
            for (index, argument) in args.iter().enumerate() {
                lines.push(format!(
                    "Argument {}: {}",
                    index + 1,
                    escape_terminal(&serde_json::to_string(argument).expect("argument string"))
                ));
            }
        }
        ToolCall::ApplyChangeset { changeset, .. } => {
            lines.push(format!(
                "Publish {} command file changes",
                changeset.files.len()
            ));
            for file in &changeset.files {
                let operation = match (&file.before, &file.after) {
                    (None, Some(_)) => "Create",
                    (Some(_), None) => "Delete",
                    _ => "Replace",
                };
                lines.push(format!("{operation} {}", escape_terminal(&file.path)));
                if let Some(before) = &file.before {
                    changeset_version_lines(&mut lines, "Before", "-", before);
                }
                if let Some(after) = &file.after {
                    changeset_version_lines(&mut lines, "After", "+", after);
                }
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

fn changeset_version_lines(
    lines: &mut Vec<String>,
    label: &str,
    prefix: &str,
    version: &crate::tools::types::FileVersion,
) {
    let Some(bytes) = version.bytes() else {
        lines.push(format!("{label}: invalid file version"));
        return;
    };
    lines.push(format!(
        "{label}: {} bytes · sha256 {} · executable {}",
        bytes.len(),
        version.sha256,
        version.executable
    ));
    if let Ok(text) = std::str::from_utf8(&bytes)
        && text
            .chars()
            .all(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
    {
        lines.extend(
            text.split('\n')
                .map(|line| format!("{prefix} {}", escape_terminal(line))),
        );
    } else {
        lines.push("Binary content".into());
    }
}

pub(crate) fn workspace_permission_lines(path: &std::path::Path) -> Vec<String> {
    vec![
        "Do you trust this folder?".into(),
        escape_terminal(&path.to_string_lossy()),
        String::new(),
        "Trust allows supported project edits, commands and Skills.".into(),
        "Tools require native OS protection; unavailable Tools stay disabled.".into(),
        "Supported commands run offline in an isolated copy and ask first.".into(),
        "Trust is remembered for this folder. Shift+Tab changes supported approval modes.".into(),
    ]
}

pub fn render_run_feedback(run: &RunView) -> Option<String> {
    match run.status {
        RunStatus::Finished => render_cache_update_feedback(run).map(str::to_owned),
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
                    observation.disposition == crate::tools::ToolDisposition::Succeeded
                        && observation.cache_update == crate::tools::CacheUpdate::Uncertain
                })
            }) {
                return Some("Error: The command succeeded but its build cache update is uncertain; the Run stopped without retrying. No answer was committed.".into());
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
                    ) || observation.cache_update == crate::tools::CacheUpdate::Uncertain
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

/// Read-only status of the Session's current or last Goal, shown by `/goal`. A paused Goal that is
/// not that Goal is named on its own line, since `/goal resume` continues it.
pub fn goal_status(view: &SessionView) -> String {
    let Some(goal) = view.goals.last() else {
        return "No Goal in this Session; send a message or use /goal <objective>".into();
    };
    let budget = goal.effective_budget();
    let mut text = format!(
        "Goal {} · {} · {} · {} · Runs {}/{} · calls {}/{} · tokens {}/{}{} · active {}/{}",
        goal.id.to_string().chars().take(8).collect::<String>(),
        goal.status.as_str(),
        goal_phase_word(goal.phase),
        if goal.autonomous {
            "autonomous"
        } else {
            "single Run"
        },
        goal.run_ids.len(),
        budget.max_runs,
        goal.provider_calls,
        budget
            .max_provider_calls
            .map_or_else(|| "unlimited".to_owned(), |limit| limit.to_string()),
        goal.known_tokens,
        budget
            .max_tokens
            .map_or_else(|| "unlimited".to_owned(), |limit| limit.to_string()),
        if goal.token_usage_unknown {
            " (some unknown)"
        } else {
            ""
        },
        goal_clock(goal.active_ms),
        budget
            .max_active_ms
            .map_or_else(|| "unlimited".to_owned(), goal_clock),
    );
    if goal.status == crate::session::GoalStatus::Interrupted {
        text.push_str(if goal.admits_resume(view) {
            " · Use /goal resume to continue"
        } else {
            " · out of budget"
        });
    }
    if let Some(paused) = view.interrupted_goal_id().filter(|id| *id != goal.id) {
        let short = paused.to_string().chars().take(8).collect::<String>();
        if view
            .goal(paused)
            .is_some_and(|paused| paused.admits_resume(view))
        {
            text.push_str(&format!(
                "\nPaused Goal {short} can be resumed with /goal resume"
            ));
        } else {
            text.push_str(&format!("\nPaused Goal {short} is out of budget"));
        }
    }
    text
}

fn goal_phase_word(phase: crate::session::GoalPhase) -> &'static str {
    match phase {
        crate::session::GoalPhase::Research => "research",
        crate::session::GoalPhase::Plan => "plan",
        crate::session::GoalPhase::Implementation => "implementation",
    }
}

fn goal_clock(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{}m{:02}s", seconds / 60, seconds % 60)
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
            let goal_id = events.iter().find_map(|envelope| match &envelope.event {
                Event::GoalRunLinked {
                    goal_id,
                    run_id: linked_run,
                    ..
                } if *linked_run == run.id => Some(*goal_id),
                _ => None,
            });
            for envelope in events {
                let task_goal_event = goal_id.is_some_and(|goal_id| match &envelope.event {
                    Event::GoalStarted {
                        goal_id: event_goal,
                        ..
                    }
                    | Event::GoalResumed {
                        goal_id: event_goal,
                    }
                    | Event::GoalFinished {
                        goal_id: event_goal,
                        ..
                    } => *event_goal == goal_id,
                    _ => false,
                });
                if envelope.run_id == Some(run.id)
                    || task_goal_event
                    || new_session && matches!(envelope.event, Event::SessionStarted { .. })
                {
                    append_jsonl(&mut rendered, envelope);
                }
            }
            rendered
        }
    }
}

#[cfg(test)]
mod exec_jsonl_tests {
    use super::*;
    use crate::session::{GoalBudget, GoalId, GoalPhase, GoalReport, GoalStatus, SessionId};

    fn envelope(
        sequence: u64,
        session_id: SessionId,
        run_id: Option<crate::session::RunId>,
        event: Event,
    ) -> EventEnvelope {
        EventEnvelope {
            sequence,
            session_id,
            run_id,
            agent_run_id: None,
            event,
            created_at_ms: 0,
        }
    }

    fn goal_started(goal_id: GoalId, objective: &str) -> Event {
        Event::GoalStarted {
            goal_id,
            objective: objective.into(),
            workspace_identity: (1, 2),
            budget: GoalBudget::default(),
            phase: GoalPhase::Implementation,
            autonomous: false,
            resource_settings: crate::tools::ToolResourceSettings::default(),
        }
    }

    fn goal_finished(goal_id: GoalId, summary: &str) -> Event {
        Event::GoalFinished {
            goal_id,
            status: GoalStatus::Completed,
            active_ms: 0,
            report: GoalReport {
                summary: summary.into(),
                changes: Vec::new(),
                evidence: Vec::new(),
                unresolved: Vec::new(),
                cached_input_tokens: 0,
            },
        }
    }

    fn message(run_id: crate::session::RunId, text: &str) -> Event {
        Event::MessageAccepted {
            run_id,
            text: text.into(),
            images: Vec::new(),
        }
    }

    #[test]
    fn exec_jsonl_selects_latest_run_and_matching_goal_lifecycle() {
        let session_id = SessionId::new();
        let goal_id = GoalId::new();
        let earlier_goal = GoalId::new();
        let earlier_run = crate::session::RunId::new();
        let current_run = crate::session::RunId::new();
        let events = vec![
            envelope(
                1,
                session_id,
                None,
                goal_started(earlier_goal, "Earlier task"),
            ),
            envelope(
                2,
                session_id,
                None,
                goal_finished(earlier_goal, "Earlier report"),
            ),
            envelope(3, session_id, None, goal_started(goal_id, "Current task")),
            envelope(
                4,
                session_id,
                Some(earlier_run),
                Event::GoalRunLinked {
                    goal_id,
                    run_id: earlier_run,
                    phase: GoalPhase::Research,
                },
            ),
            envelope(
                5,
                session_id,
                Some(earlier_run),
                message(earlier_run, "Earlier Run"),
            ),
            envelope(6, session_id, None, Event::GoalResumed { goal_id }),
            envelope(
                7,
                session_id,
                Some(current_run),
                Event::GoalRunLinked {
                    goal_id,
                    run_id: current_run,
                    phase: GoalPhase::Implementation,
                },
            ),
            envelope(
                8,
                session_id,
                Some(current_run),
                message(current_run, "Current Run"),
            ),
            envelope(
                9,
                session_id,
                None,
                goal_finished(goal_id, "Current report"),
            ),
        ];
        let run = RunView {
            id: current_run,
            objective: "Current Run".into(),
            images: Vec::new(),
            config: None,
            agents: Vec::new(),
            assistant_message: None,
            status: RunStatus::Finished,
            accepted_sequence: 8,
            finished_sequence: Some(8),
            tools: Vec::new(),
            tool_compaction_summary: None,
            tool_compaction_covered_count: 0,
            display: Vec::new(),
            steering_notes: Vec::new(),
        };

        let output = render_exec(&run, &events, Output::Jsonl, false);
        let lines: Vec<serde_json::Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSONL Event"))
            .collect();
        assert_eq!(
            lines
                .iter()
                .map(|line| line["kind"].as_str().expect("Event kind"))
                .collect::<Vec<_>>(),
            vec![
                "GoalStarted",
                "GoalResumed",
                "GoalRunLinked",
                "MessageAccepted",
                "GoalFinished",
            ]
        );
        assert_eq!(
            lines
                .iter()
                .map(|line| line["sequence"].as_u64().expect("Event sequence"))
                .collect::<Vec<_>>(),
            vec![3, 6, 7, 8, 9]
        );
        assert_eq!(lines[0]["payload"]["goal_id"], goal_id.to_string());
        assert_eq!(lines[4]["payload"]["goal_id"], goal_id.to_string());
        assert!(lines[0]["run_id"].is_null());
        assert!(lines[1]["run_id"].is_null());
        assert!(lines[4]["run_id"].is_null());
        assert!(
            lines[2..4]
                .iter()
                .all(|line| line["run_id"] == current_run.to_string())
        );
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

pub(crate) fn image_description(image: &crate::provider::ImageAttachment, index: usize) -> String {
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

pub(crate) fn escaped_terminal_bytes(value: &str) -> usize {
    value
        .chars()
        .map(|character| {
            if unsafe_display_character(character) {
                character.escape_unicode().count().max(8)
            } else {
                character.len_utf8()
            }
        })
        .sum()
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
    use super::{escape_terminal, escaped_terminal_bytes, user_message_lines};
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

    #[test]
    fn escaped_terminal_byte_count_matches_safe_text_without_materializing_it() {
        let text = "a\u{001b}\u{202e}é";
        assert_eq!(escaped_terminal_bytes(text), escape_terminal(text).len());
    }
}

#[cfg(test)]
mod goal_status_tests {
    use super::*;
    use crate::session::{GoalBudget, GoalId, GoalPhase, GoalStatus};

    fn session_with_goal(autonomous: bool) -> SessionView {
        let session_id = crate::session::SessionId::new();
        let events = [
            Event::SessionStarted {
                title: "Goals".into(),
                workspace_identity: (1, 2),
            },
            Event::GoalStarted {
                goal_id: GoalId::new(),
                objective: "Ship the parser".into(),
                workspace_identity: (1, 2),
                budget: GoalBudget::default(),
                phase: GoalPhase::Implementation,
                autonomous,
                resource_settings: crate::tools::ToolResourceSettings::default(),
            },
        ]
        .into_iter()
        .zip(1..)
        .map(|(event, sequence)| EventEnvelope {
            sequence,
            session_id,
            run_id: None,
            agent_run_id: None,
            event,
            created_at_ms: 0,
        })
        .collect::<Vec<_>>();
        SessionView::replay(session_id, &events)
            .expect("valid Goal history")
            .expect("Session")
    }

    #[test]
    fn goal_status_reports_limits_and_the_resume_hint_only_when_interrupted() {
        // An unfinished Goal reopens as Interrupted, so the running case is set explicitly.
        let mut view = session_with_goal(true);
        view.goals[0].status = GoalStatus::Running;
        let running = goal_status(&view);
        assert!(running.starts_with("Goal "), "{running}");
        assert!(
            running.contains(" · running · implementation · autonomous · Runs 0/16 · calls 0/64 · tokens 0/unlimited · active 0m00s/30m00s"),
            "{running}"
        );
        assert!(!running.contains("/goal resume"), "{running}");

        view.goals[0].status = GoalStatus::Interrupted;
        view.goals[0].token_usage_unknown = true;
        let interrupted = goal_status(&view);
        assert!(interrupted.contains(" · interrupted · "), "{interrupted}");
        assert!(
            interrupted.contains("tokens 0/unlimited (some unknown)"),
            "{interrupted}"
        );
        assert!(
            interrupted.ends_with(" · Use /goal resume to continue"),
            "{interrupted}"
        );

        let single = goal_status(&session_with_goal(false));
        assert!(single.contains(" · single Run · "), "{single}");
    }

    #[test]
    fn goal_status_names_a_paused_goal_that_is_not_the_last_one() {
        let mut view = session_with_goal(true);
        view.goals[0].status = GoalStatus::Interrupted;
        let paused = view.goals[0]
            .id
            .to_string()
            .chars()
            .take(8)
            .collect::<String>();
        let mut newer = view.goals[0].clone();
        newer.id = crate::session::GoalId::new();
        newer.status = GoalStatus::Completed;
        let newer_short = newer.id.to_string().chars().take(8).collect::<String>();
        view.goals.push(newer);
        let status = goal_status(&view);
        assert!(
            status.starts_with(&format!("Goal {newer_short} · completed")),
            "{status}"
        );
        assert!(
            status.ends_with(&format!(
                "\nPaused Goal {paused} can be resumed with /goal resume"
            )),
            "{status}"
        );
        assert!(!status.contains("Use /goal resume to continue"), "{status}");

        view.goals[1].status = GoalStatus::Interrupted;
        let last_paused = goal_status(&view);
        assert!(!last_paused.contains("Paused Goal"), "{last_paused}");
    }

    #[test]
    fn goal_status_shows_out_of_budget_instead_of_a_resume_hint() {
        let mut view = session_with_goal(true);
        view.goals[0].status = GoalStatus::Interrupted;
        view.goals[0].provider_calls = view.goals[0]
            .effective_budget()
            .max_provider_calls
            .unwrap_or(0);
        let exhausted = goal_status(&view);
        assert!(exhausted.ends_with(" · out of budget"), "{exhausted}");
        assert!(!exhausted.contains("/goal resume"), "{exhausted}");

        let mut newer = view.goals[0].clone();
        newer.id = crate::session::GoalId::new();
        newer.status = GoalStatus::Completed;
        view.goals.push(newer);
        let status = goal_status(&view);
        assert!(!status.contains("/goal resume"), "{status}");
        assert!(status.contains("\nPaused Goal "), "{status}");
        assert!(status.ends_with(" is out of budget"), "{status}");
    }

    #[test]
    fn goal_status_hint_matches_resume_admission_for_unaccepted_links_and_spent_time() {
        // A trailing GoalRunLinked without its accepted Run is not a Run, so the hint still offers resume.
        let mut view = session_with_goal(true);
        view.goals[0].status = GoalStatus::Interrupted;
        view.goals[0].budget.max_runs = 1;
        view.goals[0].run_ids = vec![crate::session::RunId::new()];
        let linked = goal_status(&view);
        assert!(
            linked.ends_with(" · Use /goal resume to continue"),
            "{linked}"
        );

        let mut view = session_with_goal(true);
        view.goals[0].status = GoalStatus::Interrupted;
        view.goals[0].active_ms = view.goals[0].effective_budget().max_active_ms.unwrap_or(0);
        let spent = goal_status(&view);
        assert!(spent.ends_with(" · out of budget"), "{spent}");

        // An unsettled active-time reservation that resume would settle as spent is counted.
        let mut view = session_with_goal(true);
        view.goals[0].status = GoalStatus::Interrupted;
        let limit = view.goals[0]
            .effective_budget()
            .max_active_ms
            .expect("finite active limit");
        view.goals[0].active_ms = limit - 1;
        view.goals[0].reserved_active_run = Some(crate::session::RunId::new());
        view.goals[0].reserved_active_ms = Some(1);
        let reserved = goal_status(&view);
        assert!(reserved.ends_with(" · out of budget"), "{reserved}");
    }

    #[test]
    fn goal_status_without_a_goal_is_a_short_notice() {
        let mut view = session_with_goal(false);
        view.goals.clear();
        assert_eq!(
            goal_status(&view),
            "No Goal in this Session; send a message or use /goal <objective>"
        );
    }
}

#[cfg(test)]
mod git_summary_tests {
    use super::tool_summary;
    use crate::session::{AgentRunId, RunId, ToolView};
    use crate::tools::{EffectIntent, ToolCall, ToolLimits};
    use uuid::Uuid;

    #[test]
    fn git_reads_summarize_their_paths() {
        for (call, expected) in [
            (
                ToolCall::GitStatus { path: "src".into() },
                "Tool · Git status src · pending",
            ),
            (
                ToolCall::GitDiff {
                    path: "src".into(),
                    base: None,
                    head: None,
                    staged: false,
                },
                "Tool · Git diff src · pending",
            ),
            (
                ToolCall::GitLog {
                    path: ".".into(),
                    revision: None,
                    max_count: 1,
                },
                "Tool · Git log . · pending",
            ),
        ] {
            let tool = ToolView {
                intent: EffectIntent {
                    id: Uuid::now_v7(),
                    run_id: RunId::new(),
                    agent_run_id: AgentRunId::new(),
                    policy_digest: [1; 32],
                    enforcement_digest: [2; 32],
                    workspace_device: 1,
                    workspace_inode: 2,
                    call,
                    limits: ToolLimits::default(),
                    expires_at_ms: 1,
                    use_count: 1,
                },
                observation: None,
                review_call_index: None,
                finished_sequence: None,
            };
            assert_eq!(tool_summary(&tool), expected);
        }
    }
}

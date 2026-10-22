use crate::protocol::EventLine;
use arany::{
    AgentStatus, Event, EventEnvelope, ProviderCallDisposition, ProviderCallRecord, RunStatus,
    RunView, SessionView,
};
use std::borrow::Cow;
use unicode_width::UnicodeWidthStr;

mod agents;
mod model;
pub mod rewind;
pub mod storage;
pub(crate) mod usage;
#[cfg(test)]
pub(crate) use agents::tests::team_view;
pub(crate) use agents::{
    AgentFocus, AgentInspectorModel, AgentQuery, AgentRow, AgentTarget, AgentTranscript, Appended,
    MAX_AGENT_ROWS, MAX_AGENT_SEARCH, MAX_RECENT_RUNS, STEERING_NOTE_HEADING, TranscriptPart,
    agent_targets, run_targets,
};
pub(crate) use model::{DraftAction, PresentationModel};
pub(crate) use model::{
    linear_session_lines, model_id_preview, safe_truncate, tool_capacity_notice,
};
pub use usage::usage_pages;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Output {
    Text,
    Jsonl,
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64;
    let mut unit = "B";
    for next in ["KB", "MB", "GB", "TB"] {
        size /= 1024.0;
        unit = next;
        if (size * 10.0).round() / 10.0 < 1024.0 {
            break;
        }
    }
    format!("{size:.1} {unit}")
}

pub fn format_duration(milliseconds: u64) -> String {
    let mut parts = Vec::with_capacity(3);
    let hours = milliseconds / 3_600_000;
    let minutes = milliseconds / 60_000 % 60;
    let remaining_ms = milliseconds % 60_000;
    if hours > 0 {
        parts.push(format!("{hours} h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes} min"));
    }
    if remaining_ms > 0 || parts.is_empty() {
        let fraction = remaining_ms % 1000;
        if fraction == 0 {
            parts.push(format!("{} s", remaining_ms / 1000));
        } else {
            let value = format!("{}.{fraction:03}", remaining_ms / 1000);
            parts.push(format!("{} s", value.trim_end_matches('0')));
        }
    }
    parts.join(" ")
}

pub(crate) fn supervisor_line(verdict: &arany::supervisor::SupervisorVerdict) -> String {
    use arany::supervisor::SignalKind;
    let signals = verdict
        .signals
        .iter()
        .map(|signal| match signal.signal {
            SignalKind::Loop => "possible loop",
            SignalKind::Probing => "possible probing",
            SignalKind::Disproportion => "possibly disproportionate changes",
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Supervisor (shadow): {signals}; no action taken{}",
        if verdict.capped {
            "; verdict limit reached"
        } else {
            ""
        }
    )
}

pub(crate) fn supervisor_lines(run: &RunView) -> impl Iterator<Item = String> + '_ {
    run.supervisor
        .verdicts()
        .iter()
        .map(|(_, verdict)| supervisor_line(verdict))
}

#[cfg(test)]
pub(crate) fn supervisor_fixture(
    actor: arany::AgentRunId,
    count: usize,
) -> arany::supervisor::SupervisorState {
    use arany::supervisor::*;
    SupervisorState::with_verdicts_for_test(
        (0..count)
            .map(|index| {
                (
                    actor,
                    SupervisorVerdict {
                        subject_sequence: 1,
                        site: Site::ToolOutcome,
                        evaluator: 1,
                        mode: Mode::Shadow,
                        signals: vec![
                            Signal {
                                signal: SignalKind::Loop,
                                level: Level::Notice,
                                would_response: None,
                                features: Features::Loop(Default::default()),
                                evidence: vec![1],
                            },
                            Signal {
                                signal: SignalKind::Probing,
                                level: Level::WouldNarrow,
                                would_response: Some(Response::Revoke),
                                features: Features::Probing(Default::default()),
                                evidence: vec![1],
                            },
                            Signal {
                                signal: SignalKind::Disproportion,
                                level: Level::WouldNarrow,
                                would_response: Some(Response::RequireApproval),
                                features: Features::Disproportion(Default::default()),
                                evidence: vec![1],
                            },
                        ],
                        capped: index == 255,
                    },
                )
            })
            .collect(),
    )
}

pub(crate) fn tool_summary(tool: &arany::ToolView) -> String {
    use arany::{ToolCall, ToolDisposition};
    let operation = match &tool.intent.call {
        ToolCall::ApplyChangeset {
            source: arany::worker::ChangesetSource::Rewind { .. },
        } => "Rewind publication()".into(),
        ToolCall::PrepareWorker { .. } => "Worker data preparation()".into(),
        ToolCall::ApplyChangeset {
            source: arany::worker::ChangesetSource::WorkerHandover { .. },
        } => "Worker changeset publication()".into(),
        ToolCall::ParseAgentDefinition { .. } => "Parse agent definition()".into(),
        ToolCall::List { path } => format!("List({path})"),
        ToolCall::Read { path, .. } => format!("Read({path})"),
        ToolCall::Search {
            path,
            query,
            regex,
            include,
            case_sensitive,
            offset,
            limit,
        } => {
            let mut options = String::new();
            if *regex {
                options.push_str(", regex");
            }
            if let Some(include) = include {
                options.push_str(&format!(", include {include}"));
            }
            if !case_sensitive {
                options.push_str(", ignore case");
            }
            if *offset > 0 {
                options.push_str(&format!(", offset {offset}"));
            }
            if *limit != arany::MAX_SEARCH_ROWS {
                options.push_str(&format!(", limit {limit}"));
            }
            format!("Search({query}, {path}{options})")
        }
        ToolCall::Glob { pattern, path, .. } => format!("Glob({pattern}, {path})"),
        ToolCall::Mkdir { path } => format!("Create folder({path})"),
        ToolCall::Write { path, .. } => format!("Write({path})"),
        ToolCall::Edit { path, .. } => format!("Edit({path})"),
        ToolCall::MultiEdit {
            file_path, edits, ..
        } => format!("MultiEdit({file_path}, {} edits)", edits.len()),
        ToolCall::Command { program, args, .. } => {
            format!("Command({program} {})", args.join(" "))
        }
        ToolCall::CommandUnguarded { program, args, .. } => {
            format!("Unguarded command({program} {})", args.join(" "))
        }
        ToolCall::ApplyChangeset {
            source: arany::worker::ChangesetSource::CommandReceipt { changeset, .. },
        } => format!("Publish({} command file changes)", changeset.files.len()),
        ToolCall::Skill { name, .. } => format!("Skill({name})"),
        ToolCall::McpList { server } => format!("MCP list({server})"),
        ToolCall::McpCall { server, tool, .. } => format!("MCP({server} / {tool})"),
        ToolCall::GitStatus { path } => format!("Git status({path})"),
        ToolCall::GitDiff { path, .. } => format!("Git diff({path})"),
        ToolCall::GitLog { path, .. } => format!("Git log({path})"),
        ToolCall::GitCommit {
            paths,
            staged: true,
            ..
        } if paths.is_empty() => "Git commit(staged)".into(),
        ToolCall::GitCommit { paths, .. } => format!("Git commit({})", paths.join(", ")),
        ToolCall::GitBranch { name: None, .. } => "Git branches()".into(),
        ToolCall::GitBranch {
            name: Some(name),
            start_point,
            ..
        } => match start_point {
            Some(start) => format!("Git branch({name} from {start})"),
            None => format!("Git branch({name})"),
        },
        ToolCall::GitSwitch { branch } => format!("Git switch({branch})"),
        ToolCall::GitPush { remote, branch } => format!("host Git push({branch} → {remote})"),
        ToolCall::GhPrCreate { repository, .. } => format!("host PR create({repository})"),
    };
    let result = match tool.observation.as_ref() {
        None => "pending".into(),
        Some(observation) => match observation.disposition {
            ToolDisposition::Limit => format!("limit: {}", limit_summary(observation)),
            ToolDisposition::Succeeded => match tool.intent.call {
                ToolCall::Write { .. }
                | ToolCall::Edit { .. }
                | ToolCall::MultiEdit { .. }
                | ToolCall::Mkdir { .. }
                | ToolCall::ApplyChangeset { .. } => "applied".into(),
                ToolCall::Command { .. } | ToolCall::CommandUnguarded { .. } => {
                    command_result_summary(observation)
                        .unwrap_or_else(|| read_result_summary(std::slice::from_ref(tool)))
                }
                ToolCall::GitCommit { .. }
                | ToolCall::GitBranch { name: Some(_), .. }
                | ToolCall::GitSwitch { .. } => git_write_summary(observation),
                ToolCall::GitPush { .. } | ToolCall::GhPrCreate { .. } => {
                    safe_truncate(&observation.output, 160)
                }
                _ => read_result_summary(std::slice::from_ref(tool)),
            },
            disposition => {
                let state = match disposition {
                    ToolDisposition::Denied => "denied",
                    ToolDisposition::Failed => "Error",
                    ToolDisposition::Conflict => "conflict",
                    ToolDisposition::Limit => "limit",
                    ToolDisposition::Cancelled => "cancelled",
                    ToolDisposition::Uncertain => "uncertain",
                    ToolDisposition::Succeeded => {
                        unreachable!("success has its own result projection")
                    }
                };
                if observation.output.is_empty() {
                    state.into()
                } else {
                    match (disposition, &tool.intent.call) {
                        // An uncertain Git write may lead with locks the owner must remove by hand.
                        (
                            ToolDisposition::Uncertain,
                            ToolCall::GitCommit { .. }
                            | ToolCall::GitBranch { .. }
                            | ToolCall::GitSwitch { .. },
                        ) => format!("{state}: {}", wrapped_rows(&observation.output)),
                        _ => format!("{state}: {}", safe_truncate(&observation.output, 160)),
                    }
                }
            }
        },
    };
    let mut summary = format!("⏺ {}\n  ⎿ {result}", safe_truncate(&operation, 160));
    if let Some(label) = tool
        .observation
        .as_ref()
        .and_then(|observation| cache_update_label(observation.cache_update))
    {
        summary.push_str(" · ");
        summary.push_str(label);
    }
    if let Some(checkpoint) = &tool.checkpoint {
        if let Some(notice) = checkpoint.notice() {
            summary.push_str(" · ");
            summary.push_str(notice);
        }
        if let Some(run) = checkpoint.pruned_before_run {
            summary.push_str(&format!(
                " · Older checkpoints pruned; code restore unavailable before Run {run}"
            ));
        }
    }
    if tool.ran_unguarded() {
        summary.push_str(" · UNGUARDED");
    }
    if let Some(cause) = tool
        .observation
        .as_ref()
        .and_then(|value| value.enforcement_cause)
    {
        summary.push_str(" · ");
        summary.push_str(cause.diagnostic());
    }
    if let Some(verdict) = &tool.permission_verdict
        && verdict.decision == arany::permissions::Decision::Deny
    {
        summary.push_str(match verdict.source {
            arany::permissions::PermissionSource::Rule => " · permission deny rule",
            arany::permissions::PermissionSource::TypedPrefixRule => " · typed Git deny rule",
            arany::permissions::PermissionSource::PinnedGrant => " · pinned grant denial",
            arany::permissions::PermissionSource::Hook => " · hook denial",
            _ => " · permission mode denial",
        });
    }
    let hook_results = tool.hooks.iter().filter_map(|hook| hook.outcome.as_ref());
    let (mut unknown, mut failed) = (false, false);
    for outcome in hook_results {
        if matches!(
            outcome.observation.disposition,
            arany::ToolDisposition::Uncertain | arany::ToolDisposition::Cancelled
        ) || outcome.observation.disposition == arany::ToolDisposition::Failed
            && outcome.observation.attestation.is_none()
        {
            unknown = true;
        } else if matches!(
            outcome.result,
            arany::permissions::HookResult::Ignored {
                why: arany::permissions::HookIgnoreReason::NonZeroExit
                    | arany::permissions::HookIgnoreReason::InvalidJson
                    | arany::permissions::HookIgnoreReason::NotCompleted
            }
        ) {
            failed = true;
        }
    }
    if unknown {
        summary.push_str(" · hook outcome unknown");
    } else if failed {
        summary.push_str(" · hook failed (non-blocking)");
    }
    summary
}

fn limit_summary(observation: &arany::ToolObservation) -> String {
    use arany::LimitCause;
    match observation.limit_cause.unwrap_or(LimitCause::Resource) {
        LimitCause::Timeout => format!(
            "timed out after {}{}",
            format_duration(u64::from(observation.intent.effective_timeout_ms())),
            if matches!(
                observation.intent.call,
                arany::ToolCall::Command { .. } | arany::ToolCall::CommandUnguarded { .. }
            ) {
                "; /settings raises it"
            } else {
                ""
            }
        ),
        LimitCause::OutputCeiling => {
            format!(
                "output passed {}; stopped",
                format_bytes(1024 * 1024 * 1024)
            )
        }
        LimitCause::ContextFull => "no room left in this Run's context".into(),
        LimitCause::ReadCapacity => "too many reads at once; nothing was read".into(),
        LimitCause::RequestedTime => "asked for more time than the pinned maximum".into(),
        LimitCause::Resource => "resource limit reached".into(),
    }
}

// The committed receipt, not a later read, is the evidence of what was published.
fn git_write_summary(observation: &arany::ToolObservation) -> String {
    let result: serde_json::Value = serde_json::from_str(&observation.output).unwrap_or_default();
    let text = |key: &str| safe_truncate(result[key].as_str().unwrap_or("?"), 160);
    let short = |key: &str| text(key).chars().take(12).collect::<String>();
    match &observation.intent.call {
        arany::ToolCall::GitCommit { .. } => {
            format!("committed {} on {}", short("commit"), text("branch"))
        }
        arany::ToolCall::GitSwitch { .. } if result["switched"] == false => {
            format!("already on {}", text("branch"))
        }
        arany::ToolCall::GitSwitch { .. } => {
            let files = result["files"].as_u64().unwrap_or_default();
            format!(
                "switched to {} · {files} {}",
                text("branch"),
                if files == 1 { "file" } else { "files" }
            )
        }
        _ => format!("created {} at {}", text("branch"), short("oid")),
    }
}

fn command_result_summary(observation: &arany::ToolObservation) -> Option<String> {
    let result: serde_json::Value = serde_json::from_str(&observation.output).ok()?;
    let exit = result.get("exit_code")?.as_i64()?;
    let stdout: arany::OutputTotal =
        serde_json::from_value(result.get("stdout_total")?.clone()).ok()?;
    let stderr: arany::OutputTotal =
        serde_json::from_value(result.get("stderr_total")?.clone()).ok()?;
    let truncated = [(stdout, "stdout"), (stderr, "stderr")]
        .into_iter()
        .any(|(total, name)| {
            result[name].as_str().is_some_and(|text| {
                total.bytes > text.len() as u64 || total.lines != text.lines().count() as u64
            })
        });
    let lines = stdout.lines.saturating_add(stderr.lines);
    let digits = lines.to_string();
    let mut count = String::new();
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            count.push(',');
        }
        count.push(character);
    }
    Some(format!(
        "exit {exit} · {count} {}{}",
        if lines == 1 { "line" } else { "lines" },
        if truncated { " (truncated)" } else { "" }
    ))
}

fn read_result_summary(tools: &[arany::ToolView]) -> String {
    let mut counts = [0usize; 3];
    let mut units = [false; 3];
    let mut truncated = false;
    for tool in tools {
        let Some(observation) = &tool.observation else {
            continue;
        };
        let output = &observation.output;
        let value = if output.len() <= 64 * 1024 {
            serde_json::from_str::<serde_json::Value>(output).ok()
        } else {
            None
        };
        if let Some(text) = value
            .as_ref()
            .and_then(|v| v.get("text"))
            .and_then(|v| v.as_str())
        {
            counts[1] += text.lines().count();
            units[1] = true;
        } else if let Some(entries) = value
            .as_ref()
            .and_then(|v| v.get("entries"))
            .and_then(|v| v.as_array())
        {
            counts[0] += entries.len();
            units[0] = true;
        } else {
            counts[2] += output.len();
            units[2] = true;
        }
        truncated |= value.as_ref().is_some_and(|v| {
            v.get("truncated").and_then(|v| v.as_bool()) == Some(true)
                || v.get("may_be_truncated").and_then(|v| v.as_bool()) == Some(true)
        });
    }
    let mut summary = counts
        .into_iter()
        .enumerate()
        .filter(|(unit, _)| units[*unit])
        .map(|(unit, count)| match unit {
            0 => format!("{count} {}", if count == 1 { "entry" } else { "entries" }),
            1 => format!("{count} {}", if count == 1 { "line" } else { "lines" }),
            _ => format_bytes(count as u64),
        })
        .collect::<Vec<_>>()
        .join(", ");
    if summary.is_empty() {
        summary = format_bytes(0);
    }
    if truncated {
        summary.push_str(" (truncated)");
    }
    summary
}

fn read_operation(tool: &arany::ToolView) -> bool {
    use arany::ToolCall;
    matches!(
        tool.intent.call,
        ToolCall::List { .. }
            | ToolCall::Read { .. }
            | ToolCall::Search { .. }
            | ToolCall::Glob { .. }
            | ToolCall::GitStatus { .. }
            | ToolCall::GitDiff { .. }
            | ToolCall::GitLog { .. }
            | ToolCall::GitBranch { name: None, .. }
            | ToolCall::Skill { .. }
            | ToolCall::McpList { .. }
    )
}

fn collapsible_read(tool: &arany::ToolView) -> bool {
    read_operation(tool)
        && tool.observation.as_ref().is_some_and(|o| {
            o.disposition == arany::ToolDisposition::Succeeded
                && o.enforcement_cause.is_none()
                && o.cache_update == arany::CacheUpdate::NotApplicable
        })
        && tool.checkpoint.is_none()
        && tool.hooks.is_empty()
        && tool
            .permission_verdict
            .as_ref()
            .is_none_or(|v| v.decision != arany::permissions::Decision::Deny)
}

fn group_label(tools: &[arany::ToolView]) -> String {
    use arany::ToolCall;
    type Kind = (
        fn(&ToolCall) -> bool,
        &'static str,
        &'static str,
        &'static str,
    );
    let kinds: [Kind; 6] = [
        (
            |c| matches!(c, ToolCall::Search { .. } | ToolCall::Glob { .. }),
            "searched for",
            "pattern",
            "patterns",
        ),
        (
            |c| matches!(c, ToolCall::Read { .. }),
            "read",
            "file",
            "files",
        ),
        (
            |c| matches!(c, ToolCall::List { .. }),
            "listed",
            "directory",
            "directories",
        ),
        (
            |c| {
                matches!(
                    c,
                    ToolCall::GitStatus { .. }
                        | ToolCall::GitDiff { .. }
                        | ToolCall::GitLog { .. }
                        | ToolCall::GitBranch { name: None, .. }
                )
            },
            "inspected",
            "Git query",
            "Git queries",
        ),
        (
            |c| matches!(c, ToolCall::Skill { .. }),
            "loaded",
            "Skill",
            "Skills",
        ),
        (
            |c| matches!(c, ToolCall::McpList { .. }),
            "listed",
            "MCP server",
            "MCP servers",
        ),
    ];
    let mut label = kinds
        .iter()
        .filter_map(|(matches, verb, one, many)| {
            let count = tools.iter().filter(|t| matches(&t.intent.call)).count();
            (count > 0).then(|| format!("{verb} {count} {}", if count == 1 { one } else { many }))
        })
        .collect::<Vec<_>>()
        .join(", ");
    label[..1].make_ascii_uppercase();
    label
}

pub(crate) fn tool_groups(
    tools: &[arany::ToolView],
) -> impl Iterator<Item = (std::ops::Range<usize>, String)> + '_ {
    let mut start = 0;
    std::iter::from_fn(move || {
        let first = tools.get(start)?;
        let count = if collapsible_read(first) {
            tools[start..]
                .iter()
                .take_while(|tool| {
                    tool.intent.agent_run_id == first.intent.agent_run_id && collapsible_read(tool)
                })
                .count()
        } else {
            1
        };
        let range = start..start + count;
        let summary = if count > 1 {
            format!(
                "⏺ {}\n  ⎿ {}",
                group_label(&tools[range.clone()]),
                read_result_summary(&tools[range.clone()])
            )
        } else {
            tool_summary(first)
        };
        start += count;
        Some((range, summary))
    })
}

pub(crate) fn settled_tool_groups(
    run: &RunView,
) -> impl Iterator<Item = (std::ops::Range<usize>, String)> + '_ {
    tool_groups(&run.tools).filter(|(range, _)| {
        run.tools[range.clone()]
            .iter()
            .all(|tool| tool.observation.is_some())
            && !(run.status == RunStatus::Active
                && collapsible_read(&run.tools[range.start])
                && run.tools.get(range.end).is_none_or(|next| {
                    next.observation.is_none()
                        && read_operation(next)
                        && next.intent.agent_run_id == run.tools[range.start].intent.agent_run_id
                }))
    })
}

pub(crate) fn plain_tool_summary(summary: &str) -> String {
    summary
        .strip_prefix("⏺ ")
        .unwrap_or(summary)
        .replace("\n  ⎿ ", "\nResult: ")
}

fn cache_update_label(update: arany::CacheUpdate) -> Option<&'static str> {
    use arany::CacheUpdate;
    match update {
        CacheUpdate::NotApplicable
        | CacheUpdate::Discarded
        | CacheUpdate::NotAttempted
        | CacheUpdate::Published => None,
        CacheUpdate::Rejected => Some("cache not saved (limit)"),
        CacheUpdate::Failed => Some("cache update failed"),
        CacheUpdate::Uncertain => Some("cache outcome uncertain"),
    }
}

pub fn render_cache_update_feedback(run: &RunView) -> Option<&'static str> {
    use arany::CacheUpdate;
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
            CacheUpdate::NotApplicable
            | CacheUpdate::NotAttempted
            | CacheUpdate::Published
            | CacheUpdate::Discarded => {}
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

pub(crate) fn tool_detail_text(tool: &arany::ToolView) -> String {
    let (head, output) = tool_detail_parts(tool);
    head + output
}

// The bounded summary and arguments, then the borrowed result, so callers can measure before joining.
pub(crate) fn tool_detail_parts(tool: &arany::ToolView) -> (String, &str) {
    let arguments = serde_json::to_string(&tool.intent.call).expect("typed Tool call");
    (
        format!("{}\nArguments: {arguments}\nResult:\n", tool_summary(tool)),
        tool.observation
            .as_ref()
            .map_or("Awaiting result", |observation| observation.output.as_str()),
    )
}

pub(crate) fn display_answer(run: &RunView) -> Option<String> {
    Some(display_answer_chunks(run)?.collect())
}

pub(crate) fn display_answer_chunks(run: &RunView) -> Option<impl Iterator<Item = &str>> {
    let call = run
        .display
        .iter()
        .filter(|chunk| chunk.category == arany::DisplayCategory::Answer)
        .map(|chunk| chunk.call_index)
        .max()?;
    if !display_call_visible(run, call) {
        return None;
    }
    Some(
        run.display
            .iter()
            .filter(move |chunk| {
                chunk.call_index == call && chunk.category == arany::DisplayCategory::Answer
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
        if chunk.category != arany::DisplayCategory::Commentary
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolApprovalChoice {
    Yes,
    AutoEdits,
    ExactCommand,
    GitPrefix(arany::permissions::TypedGitPrefix),
    No,
}

impl ToolApprovalChoice {
    pub fn label(self) -> &'static str {
        match self {
            Self::Yes => "Yes",
            Self::AutoEdits => "Yes, and don't ask again for: file edits",
            Self::ExactCommand => {
                "Yes, and don't ask again for: this identical command in this live session (unguarded)"
            }
            Self::GitPrefix(prefix) => match prefix {
                arany::permissions::TypedGitPrefix::Commit => {
                    "Yes, and allow git commit in this folder from the next Run"
                }
                arany::permissions::TypedGitPrefix::Branch => {
                    "Yes, and allow git branch in this folder from the next Run"
                }
                arany::permissions::TypedGitPrefix::Switch => {
                    "Yes, and allow git switch in this folder from the next Run"
                }
                arany::permissions::TypedGitPrefix::Push
                | arany::permissions::TypedGitPrefix::PrCreate => "No remembered rule offered",
            },
            Self::No => "No",
        }
    }

    pub fn reply(self) -> arany::ApprovalReply {
        match self {
            Self::Yes | Self::AutoEdits => arany::ApprovalReply::AllowOnce,
            Self::ExactCommand => arany::ApprovalReply::AllowForSession,
            Self::GitPrefix(_) => arany::ApprovalReply::AllowPrefixInFolder,
            Self::No => arany::ApprovalReply::Deny,
        }
    }
}

pub fn tool_approval_choices(approval: &arany::ToolApproval) -> Vec<ToolApprovalChoice> {
    let mut choices = vec![ToolApprovalChoice::Yes];
    if approval
        .unguarded
        .as_ref()
        .is_some_and(|prompt| prompt.remember)
    {
        choices.push(ToolApprovalChoice::ExactCommand);
    } else if approval.unguarded.is_none() && !approval.intent.call.is_host() {
        if approval.allow_auto_edits {
            choices.push(ToolApprovalChoice::AutoEdits);
        }
        if let Some(prefix) = approval.remember_prefix {
            choices.push(ToolApprovalChoice::GitPrefix(prefix));
        }
    }
    choices.push(ToolApprovalChoice::No);
    choices
}

pub fn unguarded_approval_lines(
    intent: &arany::EffectIntent,
    prompt: &arany::UnguardedPrompt,
) -> Vec<String> {
    let arany::ToolCall::CommandUnguarded {
        program,
        args,
        cwd,
        justification,
    } = &intent.call
    else {
        return tool_approval_lines(intent);
    };
    let mut lines = vec![
        "Run outside the Guard? (unguarded)".to_owned(),
        "The primary asks to run this command on this machine with no sandbox.".into(),
        format!(
            "Program: {} → {}",
            escape_terminal(program),
            escape_terminal(&prompt.executable)
        ),
    ];
    if args.is_empty() {
        lines.push("Args: none".into());
    } else {
        lines.push("Args:".into());
        lines.extend(
            args.iter()
                .map(|argument| format!("  {}", escape_terminal(argument))),
        );
    }
    lines.push(if let Some(pin) = &intent.host_command {
        format!(
            "Directory: {}",
            escape_terminal(
                &std::path::Path::new(&pin.workspace)
                    .join(cwd)
                    .display()
                    .to_string()
            )
        )
    } else if cwd.is_empty() {
        "Directory: ./ (the real project folder)".into()
    } else {
        format!(
            "Directory: ./{} (inside the real project folder)",
            escape_terminal(cwd)
        )
    });
    lines.push(format!(
        "Model's reason: {}",
        escape_terminal(justification)
    ));
    if prompt.sensitive {
        lines.push("SENSITIVE PATH: this command names private credentials or Arany state; output may be sent to the Provider.".into());
    }
    if prompt.inactive_rules {
        lines.push(
            "Previous exact project approvals are inactive; fresh approval is required.".into(),
        );
    }
    lines.push(match prompt.attribution {
        Some(cause) => format!("Guard attribution: {}", cause.diagnostic()),
        None => {
            "Host access requires explicit authority; no prior Guard failure is required.".into()
        }
    });
    lines.extend(
        [
            "Risk: it runs as you, with your home directory, network and every file you can reach.",
            "Changes land directly in the project without changeset validation, and Arany cannot",
            "guarantee that processes it leaves behind are stopped. This run is recorded as unguarded.",
            "It can execute repository configuration, modify Arany permissions, Store and journal, and disclose private data to the Provider.",
            "Host effects cannot be rewound; same-user state tampering is not prevented or reliably detected.",
            "Shell/interpreter inline programs are opaque beyond a lexical privilege-tool check; it is not a security boundary.",
            "File scripts, obfuscation and other payloads remain uninspected. Host children have no controlling terminal, so terminal privilege prompts cannot appear.",
            "Allow only if you would type this exact command yourself.",
        ]
        .map(String::from),
    );
    lines
}

pub fn tool_approval_lines(intent: &arany::EffectIntent) -> Vec<String> {
    use arany::ToolCall;
    let mut lines = vec!["Proposed action".to_owned()];
    if intent.call.is_host() {
        lines.push("host · one human-approved remote operation · no automatic retry".into());
        if let Some(host) = &intent.host
            && matches!(intent.call, ToolCall::GitPush { .. })
        {
            lines.push(format!(
                "Source commit: {}",
                escape_terminal(&host.source_oid)
            ));
            lines.push(format!("Refspec: {}", escape_terminal(&host.refspec)));
        }
        match &intent.call {
            ToolCall::GitPush { remote, branch } => {
                lines.push(format!("Destination: {}", escape_terminal(remote)));
                lines.push(format!("Branch: {}", escape_terminal(branch)));
            }
            ToolCall::GhPrCreate {
                repository,
                base,
                head,
                title,
                body,
                draft,
            } => {
                lines.push(
                    "PR uses the remote head branch at creation, not a fixed local commit.".into(),
                );
                for (label, value) in [
                    ("Repository", repository),
                    ("Base", base),
                    ("Head", head),
                    ("Title", title),
                ] {
                    lines.push(format!("{label}: {}", escape_terminal(value)));
                }
                lines.push(format!("Draft: {draft}"));
                lines.push("Body:".into());
                lines.extend(
                    body.lines()
                        .map(|line| format!("  {}", escape_terminal(line))),
                );
            }
            _ => {}
        }
        return lines;
    }
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
        ToolCall::MultiEdit {
            file_path, edits, ..
        } => {
            lines.push(format!(
                "MultiEdit {} · only if the file is unchanged",
                escape_terminal(file_path)
            ));
            for (index, edit) in edits.iter().enumerate() {
                lines.push(format!(
                    "Edit {} · {}",
                    index + 1,
                    if edit.replace_all {
                        "all matches"
                    } else {
                        "one match"
                    }
                ));
                lines.push("Remove:".into());
                lines.extend(
                    edit.old_string
                        .split('\n')
                        .map(|line| format!("- {}", escape_terminal(line))),
                );
                lines.push("Insert:".into());
                lines.extend(
                    edit.new_string
                        .split('\n')
                        .map(|line| format!("+ {}", escape_terminal(line))),
                );
            }
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
        ToolCall::ApplyChangeset {
            source: arany::worker::ChangesetSource::CommandReceipt { changeset, .. },
        } => {
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
        ToolCall::GitCommit {
            message,
            paths,
            staged,
        } => {
            lines.push(if *staged {
                "Commit the staged changes to the current branch".into()
            } else {
                "Commit the working-tree contents of these paths to the current branch:".into()
            });
            lines.extend(
                paths
                    .iter()
                    .map(|path| format!("  {}", escape_terminal(path))),
            );
            lines.push("Message:".into());
            lines.extend(
                message
                    .split('\n')
                    .map(|line| format!("  {}", escape_terminal(line))),
            );
            lines.push("Repository hooks and configuration do not run".into());
        }
        ToolCall::GitBranch {
            name: Some(name),
            start_point,
            ..
        } => lines.push(format!(
            "Create branch {} at {} without switching",
            escape_terminal(name),
            escape_terminal(start_point.as_deref().unwrap_or("HEAD"))
        )),
        ToolCall::GitSwitch { branch } => {
            lines.push(format!("Switch to branch {}", escape_terminal(branch)));
            lines.push(
                "Changed Workspace files are replaced; local changes they would overwrite refuse"
                    .into(),
            );
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
    version: &arany::FileVersion,
) {
    let Some(bytes) = version.bytes() else {
        lines.push(format!("{label}: invalid file version"));
        return;
    };
    lines.push(format!(
        "{label}: {} · sha256 {} · executable {}",
        format_bytes(bytes.len() as u64),
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
        "Arany can read this folder already. Trusting it also lets Arany edit project files, run commands and use project Skills.".into(),
        "Every edit and command runs in an isolated copy under native OS protection and publishes only reviewed changes; without that protection Tools stay disabled.".into(),
        "Commands run offline and ask first unless you change the approval mode later with Shift+Tab.".into(),
        "Prior versions of files changed by Arany are saved unencrypted in private StateRoot storage.".into(),
        "Trust is remembered for this exact folder only. Read only keeps today's behavior.".into(),
    ]
}

pub(crate) const RUNTIME_PROFILE_LINES: [&str; 4] = [
    "Optional runtime profiles (read only)",
    "Check a toolchain to let commands use that exact version; its bin joins PATH. Check a cache to let commands read it. At most one version per runtime. Nothing is selected until you check it, and checking switches the choice to Trust.",
    "For offline Cargo tests, select Rust and each required Cargo archive, source and index cache separately; macOS also needs its compiler and SDK. Build requests use one compilation job. Missing dependencies fail without downloading or expanding access.",
    "Caches may hold private package data or credential URLs; a command can print them into bounded output that is journaled and sent to the Provider. Package-manager configuration and credential files stay excluded.",
];

// `/` is not abbreviated: every absolute path is under it.
pub fn abbreviate_home(path: &std::path::Path, home: Option<&std::path::Path>) -> String {
    let Some(home) = home.filter(|home| home.is_absolute() && home.parent().is_some()) else {
        return path.to_string_lossy().into_owned();
    };
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.to_string_lossy()),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

pub fn render_provider_attempts(run: &RunView) -> Option<String> {
    render_goal_provider_attempts([run])
}

pub fn render_goal_provider_attempts<'a>(
    runs: impl IntoIterator<Item = &'a RunView>,
) -> Option<String> {
    let notices: Vec<_> = runs
        .into_iter()
        .flat_map(|run| {
            let retries = run.provider_retries.iter().map(|(_, retry)| retry.notice());
            retries.chain(
                run.provider_fallbacks
                    .iter()
                    .map(|(_, fallback)| fallback.notice()),
            )
        })
        .map(|notice| escape_terminal(&notice))
        .collect();
    (!notices.is_empty()).then(|| notices.join("\n"))
}

pub fn run_feedback(run: &RunView) -> Option<String> {
    match run.status {
        RunStatus::Finished => render_cache_update_feedback(run).map(str::to_owned),
        RunStatus::Failed => {
            let budget_stop = run.failure == Some(arany::RunFailure::BudgetExhausted);
            if let Some(reason) = run.failure.and_then(run_failure_notice) {
                let last_refusal = run
                    .agents
                    .iter()
                    .find(|agent| agent.role == arany::AgentRole::Primary)
                    .and_then(|agent| agent.provider_calls.last())
                    .and_then(|call| call.refusal.as_ref())
                    .map(|refusal| refusal_notice(refusal.cause));
                let detail = match (run.failure, last_refusal) {
                    (Some(arany::RunFailure::ProposalRefused), Some(cause)) => cause.to_owned(),
                    (Some(arany::RunFailure::RefusalLimit), Some(cause)) => {
                        format!("{reason} {cause}")
                    }
                    _ => reason.to_owned(),
                };
                return Some(format!(
                    "Error: Run failed. {detail} No answer was committed. Use /agents to inspect this Run."
                ));
            }
            let reason = run
                .agents
                .iter()
                .filter(|agent| agent.status == AgentStatus::Failed)
                .filter_map(|agent| agent.provider_calls.last())
                .filter(|call| call.phase != arany::AgentPhase::ToolReview)
                .filter(|_| !budget_stop)
                .find_map(provider_failure_notice);
            if let Some(reason) = reason {
                return Some(format!(
                    "Error: Run failed. {reason} No answer was committed. Use /agents to inspect this Run."
                ));
            }
            if let Some(tool) = run.tools.last().filter(|tool| {
                tool.observation.as_ref().is_some_and(|observation| {
                    observation.disposition == arany::ToolDisposition::Failed
                        && observation.attestation.is_none()
                })
            }) {
                if tool.intent.call.site() == arany::Site::InProcess {
                    return Some("Error: A Workspace read stopped before it started because the admitted Workspace or its intent no longer verified. Nothing was read; earlier completed actions remain. Use /agents to inspect this Run.".into());
                }
                return Some("Error: Native Tool protection failed before the action started. That action did not run; earlier completed actions remain. Use /agents to inspect the Guard stage before submitting again.".into());
            }
            if run.tools.last().is_some_and(|tool| {
                tool.observation.as_ref().is_some_and(|observation| {
                    observation.disposition == arany::ToolDisposition::Succeeded
                        && observation.cache_update == arany::CacheUpdate::Uncertain
                })
            }) {
                return Some("Error: The command succeeded but its build cache update is uncertain; the Run stopped without retrying. No answer was committed.".into());
            }
            if run.tools.last().is_some_and(|tool| {
                tool.observation.as_ref().is_some_and(|observation| {
                    observation.disposition == arany::ToolDisposition::Denied
                        && observation.attestation.is_none()
                })
            }) {
                return Some("Action denied or approval expired. No further action ran; earlier completed actions remain. Use /permissions or Shift+Tab to review approval settings.".into());
            }
            if let Some(tool) = run.tools.last()
                && tool.observation.as_ref().is_none_or(|observation| {
                    matches!(
                        observation.disposition,
                        arany::ToolDisposition::Uncertain | arany::ToolDisposition::Cancelled
                    ) || observation.cache_update == arany::CacheUpdate::Uncertain
                })
            {
                return Some("Error: Tool completion is uncertain. Files or effects may have changed; inspect them before retrying. No answer was committed.".into());
            }
            if run.primary_tool_limit_reached() {
                return Some("Error: Tool continuation budget exhausted. No answer was committed; completed Tool observations remain in the Session.".into());
            }
            Some(format!(
                "Error: Run failed. {}No answer was committed. Use /agents to inspect this Run.",
                if budget_stop {
                    "The Goal budget refused the next action. "
                } else {
                    ""
                }
            ))
        }
        RunStatus::StoppedByHook => Some(match run.hook_stop_reason().flatten() {
            Some(reason) => format!("Run stopped by hook: {}", escape_terminal(reason)),
            None => "Run stopped by hook".into(),
        }),
        RunStatus::Cancelled => {
            Some("Run cancelled. No answer was committed. You can submit a new task.".to_owned())
        }
        _ => None,
    }
}

fn run_failure_notice(cause: arany::RunFailure) -> Option<&'static str> {
    use arany::RunFailure;
    Some(match cause {
        RunFailure::ProviderCall | RunFailure::ToolStopped | RunFailure::BudgetExhausted => {
            return None;
        }
        RunFailure::ProposalRefused => "Arany refused the proposed operation before dispatch.",
        RunFailure::DefinitionInvalid => "An agent definition could not be loaded.",
        RunFailure::PlanAdmission => {
            "Arany could not admit the proposed plan. The proposed action did not run."
        }
        RunFailure::ToolContextFull => "Tool continuation could not fit in the available context.",
        RunFailure::WorkersUnverified => "Worker results have not passed verification.",
        RunFailure::CriterionUnmet => "The plan's success criterion was not met.",
        RunFailure::RefusalLimit => "The same proposed operation was refused three times.",
    })
}

fn refusal_notice(cause: arany::ProposalRefusalCause) -> &'static str {
    use arany::ProposalRefusalCause as Cause;
    match cause {
        Cause::InvalidArguments => {
            "Arany refused invalid operation arguments. The proposed action did not run."
        }
        Cause::Phase => {
            "Arany refused an operation unavailable in this phase. The proposed action did not run."
        }
        Cause::PlanInvalid => {
            "Arany refused an invalid or changed plan. The proposed action did not run."
        }
        Cause::PlanBudgetOutsideGoal => {
            "Arany refused a plan budget outside the Goal budget. The proposed action did not run."
        }
        Cause::PlanBudgetSpent => {
            "Arany refused a plan budget already spent by this Run. The proposed action did not run."
        }
        Cause::PlanScopeExcludesCall => {
            "Arany refused an operation outside the plan scope. The proposed action did not run."
        }
        Cause::PlanOutsideGrant => {
            "Arany refused a plan outside the admitted grant. The proposed action did not run."
        }
        Cause::ArgumentBytes => {
            "Arany refused operation arguments above the byte limit. The proposed action did not run."
        }
        Cause::LeaseOverlap => {
            "Arany refused an operation overlapping an active worker lease. The proposed action did not run."
        }
        Cause::NotOffered => {
            "Arany refused an operation that was not offered. The proposed action did not run."
        }
        Cause::AnswerInvalid => "Arany refused an invalid text answer.",
        Cause::DelegateInvalid => {
            "Arany refused an invalid delegation. No children were started for it."
        }
    }
}

fn provider_failure_notice(call: &ProviderCallRecord) -> Option<&'static str> {
    use arany::ProviderFailureReason;
    match (call.disposition, call.failure_reason) {
        (
            ProviderCallDisposition::InvalidResponse,
            Some(ProviderFailureReason::ProposalRefused),
        ) => {
            return call
                .refusal
                .as_ref()
                .map(|refusal| refusal_notice(refusal.cause));
        }
        (
            ProviderCallDisposition::InvalidResponse,
            Some(
                ProviderFailureReason::WorkerAnswerDeferred
                | ProviderFailureReason::WorkerDelegateRefused
                | ProviderFailureReason::WorkerToolRefused
                | ProviderFailureReason::CriterionNotMetInView,
            ),
        ) => {
            return Some("Arany refused the proposed worker coordination outcome.");
        }
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
        (ProviderCallDisposition::Rejected, Some(ProviderFailureReason::RateLimited)) => {
            return Some("Provider rate limited the request. Wait before trying again.");
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
            "Arany rejected the response during outcome validation."
        }
        ProviderCallDisposition::OutputLimit => "Provider response exceeded Arany's output limit.",
        ProviderCallDisposition::TimedOut => "Provider call timed out.",
        ProviderCallDisposition::TaskPanic => "Provider task failed internally.",
        ProviderCallDisposition::Finished
        | ProviderCallDisposition::Delegated
        | ProviderCallDisposition::ToolRequested
        | ProviderCallDisposition::PlanRecorded
        | ProviderCallDisposition::Cancelled => return None,
    })
}

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
        format_duration(goal.active_ms),
        budget
            .max_active_ms
            .map_or_else(|| "unlimited".to_owned(), format_duration),
    );
    let unguarded: usize = view
        .runs
        .iter()
        .filter(|run| goal.run_ids.contains(&run.id))
        .map(RunView::unguarded_tools)
        .sum();
    if unguarded > 0 {
        text.push_str(&format!(" · unguarded effects: {unguarded}"));
    }
    let host: usize = view
        .runs
        .iter()
        .filter(|run| goal.run_ids.contains(&run.id))
        .map(RunView::host_tools)
        .sum();
    if host > 0 {
        text.push_str(&format!(" · host effects: {host}"));
    }
    if goal.status == arany::GoalStatus::Interrupted {
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

fn goal_phase_word(phase: arany::GoalPhase) -> &'static str {
    match phase {
        arany::GoalPhase::Research => "research",
        arany::GoalPhase::Plan => "plan",
        arany::GoalPhase::Implementation => "implementation",
    }
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
                serde_json::json!({ "text": text, "images": images.iter().enumerate().map(|(index, image)| image_description(image, index)).collect::<Vec<_>>() }).to_string()
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
    if output == Output::Text {
        for worker in view.runs.iter().flat_map(RunView::worker_history) {
            rendered.push_str(&escape_terminal(&worker.description()));
            rendered.push('\n');
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
    use arany::{
        GoalBudget, GoalId, GoalPhase, GoalReport, GoalStatus, SessionId, WorkspaceIdentity,
    };

    fn envelope(
        sequence: u64,
        session_id: SessionId,
        run_id: Option<arany::RunId>,
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
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
            budget: GoalBudget::default(),
            phase: GoalPhase::Implementation,
            autonomous: false,
            resource_settings: arany::ToolResourceSettings::default(),
            continues: None,
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
            exhausted: false,
        }
    }

    fn message(run_id: arany::RunId, text: &str) -> Event {
        Event::MessageAccepted {
            skill: None,
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
        let earlier_run = arany::RunId::new();
        let current_run = arany::RunId::new();
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
            skill: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: current_run,
            objective: "Current Run".into(),
            images: Vec::new(),
            config: None,
            agents: Vec::new(),
            assistant_message: None,
            status: RunStatus::Finished,
            accepted_sequence: 8,
            finished_sequence: Some(8),
            failure: None,
            tools: Vec::new(),
            tool_compaction_summary: None,
            tool_compaction_covered_count: 0,
            tool_results_cleared_count: 0,
            definitions: Default::default(),
            display: Vec::new(),
            steering_notes: Vec::new(),
            lifecycle_hooks: Vec::new(),
            lifecycle_hook_sequence: None,
            provider_sequences: Default::default(),
            provider_fallbacks: Vec::new(),
            provider_retries: Vec::new(),
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
    let unguarded = run.unguarded_tools();
    let host = run.host_tools();
    let mut effects = Vec::new();
    if unguarded > 0 {
        effects.push(format!("unguarded effects: {unguarded}"));
    }
    if host > 0 {
        effects.push(format!("host effects: {host}"));
    }
    let mut rendered = match heading.strip_suffix(':') {
        Some(base) if !effects.is_empty() => format!("{base} ({}):\n", effects.join("; ")),
        _ => format!("{heading}\n"),
    };
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
    images: &[arany::ImageAttachment],
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

pub(crate) fn image_message_lines(images: &[arany::ImageAttachment], width: usize) -> Vec<String> {
    images
        .iter()
        .enumerate()
        .map(|(index, image)| {
            safe_truncate(&format!("  {}", image_description(image, index)), width)
        })
        .collect()
}

pub(crate) fn image_description(image: &arany::ImageAttachment, index: usize) -> String {
    let (width, height) = image.dimensions();
    format!(
        "Image {}: PNG {width}x{height}, {}",
        index + 1,
        format_bytes(image.byte_len() as u64)
    )
}

fn append_jsonl(rendered: &mut String, envelope: &EventEnvelope) {
    let line = EventLine::from(envelope);
    let json = serde_json::to_string(&line).expect("serializable Event");
    rendered.push_str(&escape_json_controls(&json));
    rendered.push('\n');
}

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

const WRAPPED_ROW_CHARS: usize = 120;
const MAX_WRAPPED_ROWS: usize = 8;

// The escaped text across at most eight continuation rows, then an explicit remainder count.
fn wrapped_rows(value: &str) -> String {
    let escaped: Vec<char> = escape_terminal(value).chars().collect();
    let mut rows: Vec<String> = escaped
        .chunks(WRAPPED_ROW_CHARS)
        .take(MAX_WRAPPED_ROWS)
        .map(|row| row.iter().collect())
        .collect();
    let hidden = escaped
        .len()
        .saturating_sub(WRAPPED_ROW_CHARS * MAX_WRAPPED_ROWS);
    if hidden > 0 {
        rows.push(format!("… {hidden} more characters"));
    }
    rows.join("\n    ")
}

fn unsafe_display_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character as u32,
            0x00ad
                | 0x061c
                | 0x180e
                // Zero-width joiners stay: emoji and many scripts need them.
                | 0x200b
                | 0x200e
                | 0x200f
                | 0x2028..=0x202e
                | 0x2060..=0x2069
                | 0xfeff
        )
}

pub(crate) fn session_access_label(defaults: &arany::SessionDefaults) -> &str {
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
    use super::{
        abbreviate_home, escape_terminal, escaped_terminal_bytes, user_message_lines,
        workspace_permission_lines,
    };
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn byte_sizes_use_binary_units_for_human_output() {
        for (bytes, expected) in [
            (0, "0 B"),
            (1, "1 B"),
            (512, "512 B"),
            (1023, "1023 B"),
            (1024, "1.0 KB"),
            (1536, "1.5 KB"),
            (39_240, "38.3 KB"),
            (1024 * 1024 - 1024, "1023.0 KB"),
            (1024 * 1024 - 1, "1.0 MB"),
            (1024 * 1024, "1.0 MB"),
            (1024 * 1024 * 1024 - 1, "1.0 GB"),
            (1024 * 1024 * 1024, "1.0 GB"),
            (1024_u64.pow(4), "1.0 TB"),
            (u64::MAX, "16777216.0 TB"),
        ] {
            assert_eq!(super::format_bytes(bytes), expected, "{bytes}");
        }
    }

    #[test]
    fn durations_use_exact_hours_minutes_and_seconds_for_human_output() {
        for (milliseconds, expected) in [
            (0, "0 s"),
            (1, "0.001 s"),
            (999, "0.999 s"),
            (1000, "1 s"),
            (1500, "1.5 s"),
            (59_999, "59.999 s"),
            (60_000, "1 min"),
            (60_001, "1 min 0.001 s"),
            (90_000, "1 min 30 s"),
            (3_599_999, "59 min 59.999 s"),
            (3_600_000, "1 h"),
            (3_600_001, "1 h 0.001 s"),
            (5_400_000, "1 h 30 min"),
            (u64::MAX, "5124095576030 h 25 min 51.615 s"),
        ] {
            assert_eq!(
                super::format_duration(milliseconds),
                expected,
                "{milliseconds}"
            );
        }
    }

    #[test]
    fn goal_fallback_notices_collect_every_run_once_in_order() {
        use arany::{
            ProviderCallDisposition, ProviderFallback, ProviderIdentity, ProviderRole, RunId,
            RunStatus,
        };
        fn fallback(model: &str) -> (arany::AgentRunId, ProviderFallback) {
            (
                arany::AgentRunId::new(),
                ProviderFallback {
                    rate_limits: None,
                    role: ProviderRole::Primary,
                    call_index: 0,
                    failure: ProviderCallDisposition::Unavailable,
                    from: ProviderIdentity::new("openai", model, arany::Provenance::native()),
                    to: ProviderIdentity::new("openai", "fallback", arany::Provenance::native()),
                },
            )
        }
        let run = |fallbacks: Vec<_>| super::RunView {
            skill: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: RunId::new(),
            objective: String::new(),
            images: Vec::new(),
            config: None,
            agents: Vec::new(),
            assistant_message: None,
            status: RunStatus::Finished,
            accepted_sequence: 0,
            finished_sequence: None,
            failure: None,
            tools: Vec::new(),
            tool_compaction_summary: None,
            tool_compaction_covered_count: 0,
            tool_results_cleared_count: 0,
            definitions: Default::default(),
            display: Vec::new(),
            steering_notes: Vec::new(),
            lifecycle_hooks: Vec::new(),
            lifecycle_hook_sequence: None,
            provider_sequences: Default::default(),
            provider_fallbacks: fallbacks,
            provider_retries: Vec::new(),
        };
        let earlier = run(vec![fallback("first")]);
        let latest = run(vec![fallback("second")]);
        let notices = super::render_goal_provider_attempts([&earlier, &latest]).unwrap();
        let lines: Vec<_> = notices.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("openai/first ") && lines[1].contains("openai/second "));
        assert_eq!(
            super::render_goal_provider_attempts([&latest]).unwrap(),
            super::render_provider_attempts(&latest).unwrap()
        );
        assert_eq!(
            super::render_goal_provider_attempts([&run(Vec::new())]),
            None
        );
    }

    #[test]
    fn abbreviate_home_replaces_only_the_home_prefix() {
        use std::path::Path;

        let home = Path::new("/home/u");
        assert_eq!(
            abbreviate_home(Path::new("/home/u/.npm/_cacache"), Some(home)),
            "~/.npm/_cacache"
        );
        assert_eq!(abbreviate_home(home, Some(home)), "~");
        assert_eq!(
            abbreviate_home(Path::new("/home/user2/x"), Some(home)),
            "/home/user2/x"
        );
        assert_eq!(abbreviate_home(Path::new("/opt/x"), None), "/opt/x");
        assert_eq!(
            abbreviate_home(Path::new("/opt/x"), Some(Path::new("/"))),
            "/opt/x"
        );
        assert_eq!(
            abbreviate_home(Path::new("/home/u/x"), Some(Path::new("relative"))),
            "/home/u/x"
        );
    }

    #[test]
    fn workspace_permission_lines_keep_the_question_and_escaped_path() {
        use std::path::Path;

        let lines = workspace_permission_lines(Path::new("/projects/\u{1b}[31mred"));
        assert_eq!(lines[0], "Do you trust this folder?");
        assert_eq!(lines[1], escape_terminal("/projects/\u{1b}[31mred"));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("saved unencrypted in private StateRoot storage"))
        );
        assert!(
            lines
                .join("\n")
                .contains("Read only keeps today's behavior")
        );
    }

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
    use arany::{GoalBudget, GoalId, GoalPhase, GoalStatus, WorkspaceIdentity};

    fn session_with_goal(autonomous: bool) -> SessionView {
        let session_id = arany::SessionId::new();
        let events = [
            Event::SessionStarted {
                title: "Goals".into(),
                workspace_identity: WorkspaceIdentity {
                    device: 1,
                    inode: 2,
                },
            },
            Event::GoalStarted {
                goal_id: GoalId::new(),
                objective: "Ship the parser".into(),
                workspace_identity: WorkspaceIdentity {
                    device: 1,
                    inode: 2,
                },
                budget: GoalBudget {
                    max_provider_calls: Some(64),
                    max_tokens: None,
                    max_active_ms: Some(30 * 60 * 1000),
                    max_runs: 16,
                },
                phase: GoalPhase::Implementation,
                autonomous,
                resource_settings: arany::ToolResourceSettings::default(),
                continues: None,
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
            running.contains(" · running · implementation · autonomous · Runs 0/16 · calls 0/64 · tokens 0/unlimited · active 0 s/30 min"),
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
        newer.id = arany::GoalId::new();
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
        newer.id = arany::GoalId::new();
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
        view.goals[0].run_ids = vec![arany::RunId::new()];
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

        // A started Run that never settled charges its deadline, which resume counts as spent.
        let mut view = session_with_goal(true);
        view.goals[0].status = GoalStatus::Interrupted;
        let limit = view.goals[0]
            .effective_budget()
            .max_active_ms
            .expect("finite active limit");
        view.goals[0].active_ms = limit - 1;
        view.goals[0].active_deadline = Some((arany::RunId::new(), None));
        view.goals[0].active_ceiling_ms = 1;
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
pub(crate) fn tool_fixture(call: arany::ToolCall, output: &str) -> arany::ToolView {
    let intent = arany::EffectIntent {
        host: None,
        host_command: None,
        target: arany::worker::EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id: arany::RunId::new(),
        agent_run_id: arany::AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call,
        limits: arany::ToolLimits::default(),
        expires_at_ms: 1,
        use_count: 1,
    };
    arany::ToolView {
        checkpoint: None,
        observation: Some(arany::ToolObservation {
            intent: intent.clone(),
            disposition: arany::ToolDisposition::Succeeded,
            output: output.into(),
            attestation: None,
            cache_update: arany::CacheUpdate::NotApplicable,
            enforcement_cause: None,
            limit_cause: None,
        }),
        intent,
        review_call_index: None,
        permission_verdict: None,
        hooks: Vec::new(),
        hook_sequence: None,
        finished_sequence: Some(1),
    }
}

#[cfg(test)]
mod peer_row_tests {
    use super::*;
    use arany::{ToolCall, ToolDisposition};

    #[test]
    fn named_limit_rows_and_cache_problem_markers_have_the_same_linear_copy() {
        use arany::{CacheUpdate, LimitCause};
        let mut tool = tool_fixture(
            ToolCall::Command {
                program: "sh".into(),
                args: vec![],
                cwd: String::new(),
            },
            "untrusted timeout text\u{1b}[2J",
        );
        tool.intent.limits.runtime_ms = 120_000;
        tool.observation.as_mut().unwrap().intent.limits.runtime_ms = 120_000;
        for (cause, copy) in [
            (
                LimitCause::Timeout,
                "timed out after 2 min; /settings raises it",
            ),
            (LimitCause::OutputCeiling, "output passed 1.0 GB; stopped"),
            (
                LimitCause::ContextFull,
                "no room left in this Run's context",
            ),
            (
                LimitCause::ReadCapacity,
                "too many reads at once; nothing was read",
            ),
            (
                LimitCause::RequestedTime,
                "asked for more time than the pinned maximum",
            ),
            (LimitCause::Resource, "resource limit reached"),
        ] {
            let observation = tool.observation.as_mut().unwrap();
            observation.disposition = ToolDisposition::Limit;
            observation.limit_cause = Some(cause);
            for (cache, marker) in [
                (CacheUpdate::NotApplicable, ""),
                (CacheUpdate::NotAttempted, ""),
                (CacheUpdate::Discarded, ""),
                (CacheUpdate::Published, ""),
                (CacheUpdate::Rejected, " · cache not saved (limit)"),
                (CacheUpdate::Failed, " · cache update failed"),
                (CacheUpdate::Uncertain, " · cache outcome uncertain"),
            ] {
                tool.observation.as_mut().unwrap().cache_update = cache;
                let summary = tool_summary(&tool);
                assert_eq!(
                    summary,
                    format!("⏺ Command(sh )\n  ⎿ limit: {copy}{marker}")
                );
                assert_eq!(
                    plain_tool_summary(&summary),
                    format!("Command(sh )\nResult: limit: {copy}{marker}")
                );
            }
        }
        let observation = tool.observation.as_mut().unwrap();
        observation.disposition = ToolDisposition::Uncertain;
        observation.limit_cause = None;
        observation.output = "effects unknown; never replay".into();
        assert_eq!(
            tool_summary(&tool),
            "⏺ Command(sh )\n  ⎿ uncertain: effects unknown; never replay · cache outcome uncertain"
        );
    }

    #[test]
    fn git_write_rows_show_their_receipt_and_approvals_the_exact_action() {
        let oid = "a".repeat(40);
        for (call, output, expected) in [
            (
                ToolCall::GitCommit {
                    message: "Fix\nbody".into(),
                    paths: vec!["src/a.rs".into(), "README.md".into()],
                    staged: false,
                },
                serde_json::json!({"commit": oid, "branch": "main"}),
                "⏺ Git commit(src/a.rs, README.md)\n  ⎿ committed aaaaaaaaaaaa on main",
            ),
            (
                ToolCall::GitCommit {
                    message: "Fix".into(),
                    paths: Vec::new(),
                    staged: true,
                },
                serde_json::json!({"commit": oid, "branch": "main"}),
                "⏺ Git commit(staged)\n  ⎿ committed aaaaaaaaaaaa on main",
            ),
            (
                ToolCall::GitBranch {
                    name: Some("topic".into()),
                    start_point: Some("HEAD~1".into()),
                    scope: None,
                    base: None,
                    max_count: None,
                },
                serde_json::json!({"branch": "topic", "oid": oid}),
                "⏺ Git branch(topic from HEAD~1)\n  ⎿ created topic at aaaaaaaaaaaa",
            ),
            (
                ToolCall::GitSwitch {
                    branch: "topic".into(),
                },
                serde_json::json!({"branch": "topic", "switched": true, "files": 1}),
                "⏺ Git switch(topic)\n  ⎿ switched to topic · 1 file",
            ),
            (
                ToolCall::GitSwitch {
                    branch: "topic".into(),
                },
                serde_json::json!({"branch": "topic", "switched": false}),
                "⏺ Git switch(topic)\n  ⎿ already on topic",
            ),
        ] {
            assert_eq!(
                tool_summary(&tool_fixture(call, &output.to_string())),
                expected
            );
        }
        // Receipt names are repository data: escaped, and bounded to their row.
        let hostile = format!("\u{202e}evil\u{1b}[2J{}", "b".repeat(400));
        let created = tool_summary(&tool_fixture(
            ToolCall::GitBranch {
                name: Some("topic".into()),
                start_point: None,
                scope: None,
                base: None,
                max_count: None,
            },
            &serde_json::json!({"branch": hostile, "oid": oid}).to_string(),
        ));
        assert!(
            created.contains("created \\u{202e}evil\\u{001b}[2J"),
            "{created}"
        );
        assert!(!created.contains('\u{202e}') && !created.contains('\u{1b}'));
        assert!(
            created.lines().nth(1).unwrap().chars().count() < 200,
            "{created}"
        );
        let long = format!(".git/refs/heads/{}.lock", "deeply/nested/branch".repeat(6));
        let locks = format!(
            "Git locks left by an interrupted write: .git/index.lock, {long}, .git/packed-refs.lock; remove them after checking that no git process is running. Guard execution unconfirmed; effects may have occurred. Do not retry automatically."
        );
        let uncertain = |output: &str| {
            let mut tool = tool_fixture(
                ToolCall::GitSwitch {
                    branch: "topic".into(),
                },
                output,
            );
            tool.observation.as_mut().unwrap().disposition = ToolDisposition::Uncertain;
            tool_summary(&tool)
        };
        let summary = uncertain(&locks);
        let rows: Vec<&str> = summary.lines().collect();
        assert!(
            rows.len() > 3,
            "the report wraps instead of being cut: {summary}"
        );
        let joined: String = rows[1..]
            .iter()
            .map(|row| {
                row.trim_start_matches("  ⎿ uncertain: ")
                    .trim_start_matches("    ")
            })
            .collect();
        assert_eq!(joined, locks, "every path and the instruction stay visible");
        let flood = "x".repeat(2_000);
        let summary = uncertain(&flood);
        assert_eq!(summary.lines().count(), 1 + 8 + 1);
        assert!(summary.ends_with("… 1040 more characters"), "{summary}");
        assert!(uncertain("\u{202e}gol.kcol").contains("\\u{202e}"));
        let mut failed = tool_fixture(
            ToolCall::GitSwitch {
                branch: "topic".into(),
            },
            r#"{"failed":"Switching would overwrite local changes"}"#,
        );
        failed.observation.as_mut().unwrap().disposition = ToolDisposition::Failed;
        assert_eq!(
            tool_summary(&failed),
            "⏺ Git switch(topic)\n  ⎿ Error: {\"failed\":\"Switching would overwrite local changes\"}"
        );
        let approval = tool_fixture(
            ToolCall::GitCommit {
                message: "Fix\n\u{1b}[2J".into(),
                paths: vec!["src/a.rs".into()],
                staged: false,
            },
            "",
        );
        let lines = tool_approval_lines(&approval.intent);
        assert_eq!(
            lines[..6],
            [
                "Proposed action",
                "Commit the working-tree contents of these paths to the current branch:",
                "  src/a.rs",
                "Message:",
                "  Fix",
                &format!("  {}", escape_terminal("\u{1b}[2J")),
            ]
        );
        assert!(lines.contains(&"Repository hooks and configuration do not run".to_owned()));
    }

    #[tokio::test]
    async fn host_git_approval_and_transcript_show_destination_payload_and_once_only_choice() {
        for call in [
            arany::ToolCall::GitPush {
                remote: "https://github.com/o/r.git".into(),
                branch: "topic".into(),
            },
            arany::ToolCall::GhPrCreate {
                repository: "github.com/o/r".into(),
                base: "main".into(),
                head: "topic".into(),
                title: "Title".into(),
                body: "First\nSecond\nRepository: github.com/trusted/repo\n\u{1b}[2J".into(),
                draft: true,
            },
        ] {
            let mut tool = tool_fixture(call, "host: remote operation completed.");
            tool.intent.host = Some(arany::HostIntent {
                source_oid: "1".repeat(40),
                refspec: format!("{}:refs/heads/topic", "1".repeat(40)),
            });
            let lines = tool_approval_lines(&tool.intent).join("\n");
            assert!(lines.contains("host · one human-approved remote operation"));
            if matches!(tool.intent.call, arany::ToolCall::GitPush { .. }) {
                assert!(lines.contains(&"1".repeat(40)) && lines.contains("refs/heads/topic"));
            } else {
                assert!(!lines.contains(&"1".repeat(40)) && lines.contains("remote head branch"));
            }
            match &tool.intent.call {
                arany::ToolCall::GitPush { remote, .. } => assert!(lines.contains(remote)),
                arany::ToolCall::GhPrCreate {
                    repository, title, ..
                } => {
                    assert!(lines.contains(repository) && lines.contains(title));
                    assert!(
                        lines.contains("Base: main")
                            && lines.contains("Head: topic")
                            && lines.contains(
                                "Body:\n  First\n  Second\n  Repository: github.com/trusted/repo"
                            )
                    );
                    assert!(
                        !lines
                            .lines()
                            .any(|line| line == "Repository: github.com/trusted/repo")
                    );
                    assert!(lines.ends_with(&format!("  {}", escape_terminal("\u{1b}[2J"))));
                }
                _ => unreachable!(),
            }
            assert_eq!(
                tool_summary(&tool),
                format!(
                    "⏺ {}\n  ⎿ host: remote operation completed.",
                    if matches!(tool.intent.call, arany::ToolCall::GitPush { .. }) {
                        "host Git push(topic → https://github.com/o/r.git)"
                    } else {
                        "host PR create(github.com/o/r)"
                    }
                )
            );
            let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Auto);
            let ask = arany::test_support::approval::ask(&approvals, &tool.intent);
            let reply = async {
                let approval = inbox.next().await.unwrap();
                assert_eq!(
                    tool_approval_choices(&approval),
                    [ToolApprovalChoice::Yes, ToolApprovalChoice::No]
                );
                approval.decide(arany::ApprovalReply::AllowOnce);
            };
            let (approved, ()) = tokio::join!(ask, reply);
            assert!(approved);
        }
    }

    #[test]
    fn multiedit_summary_and_approval_keep_one_outcome_and_every_ordered_replacement() {
        let tool = tool_fixture(
            ToolCall::MultiEdit {
                file_path: "src/a".into(),
                expected_digest: "0".repeat(64),
                edits: vec![
                    arany::LiteralEdit {
                        old_string: "old\nline".into(),
                        new_string: "middle".into(),
                        replace_all: true,
                    },
                    arany::LiteralEdit {
                        old_string: "middle".into(),
                        new_string: "\u{1b}[2J".into(),
                        replace_all: false,
                    },
                ],
            },
            "{}",
        );
        assert_eq!(
            tool_summary(&tool),
            "⏺ MultiEdit(src/a, 2 edits)\n  ⎿ applied"
        );
        assert_eq!(
            tool_approval_lines(&tool.intent),
            [
                "Proposed action",
                "MultiEdit src/a · only if the file is unchanged",
                "Edit 1 · all matches",
                "Remove:",
                "- old",
                "- line",
                "Insert:",
                "+ middle",
                "Edit 2 · one match",
                "Remove:",
                "- middle",
                "Insert:",
                "+ \\u{001b}[2J",
                "",
                "Allow once for this exact action · expires within one minute",
            ]
        );
    }

    #[test]
    fn timeout_rows_use_operation_caps_and_only_offer_settings_for_commands() {
        for (call, operation, cap, guidance) in [
            (
                ToolCall::GitStatus { path: ".".into() },
                "Git status(.)",
                20_000,
                "",
            ),
            (
                ToolCall::GitDiff {
                    path: ".".into(),
                    base: None,
                    head: None,
                    staged: false,
                },
                "Git diff(.)",
                20_000,
                "",
            ),
            (
                ToolCall::GitLog {
                    path: ".".into(),
                    revision: None,
                    max_count: 10,
                },
                "Git log(.)",
                20_000,
                "",
            ),
            (
                ToolCall::GitSwitch {
                    branch: "topic".into(),
                },
                "Git switch(topic)",
                20_000,
                "",
            ),
            (
                ToolCall::McpList {
                    server: "local".into(),
                },
                "MCP list(local)",
                40_000,
                "",
            ),
            (
                ToolCall::McpCall {
                    server: "local".into(),
                    tool: "inspect".into(),
                    schema_digest: "a".repeat(64),
                    arguments: "{}".into(),
                },
                "MCP(local / inspect)",
                40_000,
                "",
            ),
            (
                ToolCall::Command {
                    program: "sh".into(),
                    args: vec![],
                    cwd: String::new(),
                },
                "Command(sh )",
                60_000,
                "; /settings raises it",
            ),
            (
                ToolCall::CommandUnguarded {
                    program: "sh".into(),
                    args: vec![],
                    cwd: String::new(),
                    justification: "Run the timed-out command".into(),
                },
                "Unguarded command(sh )",
                60_000,
                "; /settings raises it",
            ),
        ] {
            for runtime_ms in [60_000, 1_500] {
                let mut tool = tool_fixture(call.clone(), "untrusted timeout text");
                let observation = tool.observation.as_mut().unwrap();
                observation.intent.limits.runtime_ms = runtime_ms;
                observation.disposition = ToolDisposition::Limit;
                observation.limit_cause = Some(arany::LimitCause::Timeout);
                let duration = match runtime_ms.min(cap) {
                    1500 => "1.5 s",
                    20_000 => "20 s",
                    40_000 => "40 s",
                    60_000 => "1 min",
                    other => panic!("unexpected timeout fixture: {other}"),
                };
                let marker = if tool.ran_unguarded() {
                    " · UNGUARDED"
                } else {
                    ""
                };
                let summary = tool_summary(&tool);
                assert_eq!(
                    summary,
                    format!(
                        "⏺ {operation}\n  ⎿ limit: timed out after {duration}{guidance}{marker}"
                    )
                );
                assert_eq!(
                    plain_tool_summary(&summary),
                    format!(
                        "{operation}\nResult: limit: timed out after {duration}{guidance}{marker}"
                    )
                );
            }
        }
    }

    #[test]
    fn command_rows_use_typed_line_totals_and_mark_truncation() {
        let output = serde_json::json!({
            "exit_code": 0,
            "stdout": "head\n…[123398 bytes, 12338 lines omitted; 123408 bytes, 12340 lines total]…\ntail\n",
            "stderr": "", "stdout_total": {"bytes":123408,"lines":12340},
            "stderr_total": {"bytes":0,"lines":0}, "changeset":{"files":[]}
        }).to_string();
        let tool = tool_fixture(
            ToolCall::Command {
                program: "sh".into(),
                args: vec![],
                cwd: String::new(),
            },
            &output,
        );
        assert_eq!(
            tool_summary(&tool),
            "⏺ Command(sh )\n  ⎿ exit 0 · 12,340 lines (truncated)"
        );
        assert_eq!(
            plain_tool_summary(&tool_summary(&tool)),
            "Command(sh )\nResult: exit 0 · 12,340 lines (truncated)"
        );
        let mut tool = tool;
        tool.observation.as_mut().unwrap().output = serde_json::json!({
            "exit_code": 0,
            "stdout": "x lines omitted; 45 bytes, 1 lines total]…\n",
            "stderr": "", "stdout_total": {"bytes":45,"lines":1},
            "stderr_total": {"bytes":0,"lines":0}, "changeset":{"files":[]}
        })
        .to_string();
        assert_eq!(tool_summary(&tool), "⏺ Command(sh )\n  ⎿ exit 0 · 1 line");
        assert_eq!(
            plain_tool_summary(&tool_summary(&tool)),
            "Command(sh )\nResult: exit 0 · 1 line"
        );
    }

    #[test]
    fn tool_rows_show_bounded_results_and_inert_failures() {
        for (call, output, expected) in [
            (
                ToolCall::List { path: ".".into() },
                r#"{"entries":["a","b"],"may_be_truncated":false}"#,
                "⏺ List(.)\n  ⎿ 2 entries",
            ),
            (
                ToolCall::List { path: ".".into() },
                r#"{"entries":[]}"#,
                "⏺ List(.)\n  ⎿ 0 entries",
            ),
            (
                ToolCall::Glob {
                    pattern: "**/*.rs".into(),
                    path: "src".into(),
                    limit: 1,
                },
                r#"{"entries":[{"bytes":3,"path":"src/a.rs"}],"limit":1,"may_be_truncated":true}"#,
                "⏺ Glob(**/*.rs, src)\n  ⎿ 1 entry (truncated)",
            ),
            (
                ToolCall::Read {
                    path: "src/界.rs".into(),
                    offset: None,
                    limit: Some(1024),
                },
                r#"{"text":"one\ntwo\n","truncated":true}"#,
                "⏺ Read(src/界.rs)\n  ⎿ 2 lines (truncated)",
            ),
            (
                ToolCall::Search {
                    path: "src".into(),
                    query: "fn \\w+".into(),
                    regex: true,
                    include: Some("**/*.rs".into()),
                    case_sensitive: false,
                    offset: 100,
                    limit: 100,
                },
                r#"{"entries":[{"path":"src/a.rs","line":1,"text":"fn a"}],"limit":100,"may_be_truncated":true,"next_offset":101}"#,
                "⏺ Search(fn \\w+, src, regex, include **/*.rs, ignore case, offset 100)\n  ⎿ 1 entry (truncated)",
            ),
            (
                ToolCall::Search {
                    path: "src".into(),
                    query: "x".into(),
                    regex: false,
                    include: None,
                    case_sensitive: true,
                    offset: 0,
                    limit: 10,
                },
                r#"{"entries":[],"limit":10,"may_be_truncated":false,"next_offset":null}"#,
                "⏺ Search(x, src, limit 10)\n  ⎿ 0 entries",
            ),
            (
                ToolCall::Skill {
                    name: "review".into(),
                    resource: None,
                },
                "abc",
                "⏺ Skill(review)\n  ⎿ 3 B",
            ),
            (
                ToolCall::Write {
                    path: "test.txt".into(),
                    expected_digest: None,
                    content: "hello".into(),
                },
                "{}",
                "⏺ Write(test.txt)\n  ⎿ applied",
            ),
        ] {
            let tool = tool_fixture(call, output);
            assert_eq!(tool_summary(&tool), expected);
            assert!(!expected.contains("succeeded"));
        }
        let skill = tool_fixture(
            ToolCall::Skill {
                name: "review".into(),
                resource: None,
            },
            &"x".repeat(39_240),
        );
        assert_eq!(tool_summary(&skill), "⏺ Skill(review)\n  ⎿ 38.3 KB");
        let mut tools = vec![
            tool_fixture(
                ToolCall::List { path: ".".into() },
                r#"{"entries":["a","b"]}"#,
            ),
            tool_fixture(
                ToolCall::Read {
                    path: "a".into(),
                    offset: None,
                    limit: None,
                },
                r#"{"text":"one\ntwo\n"}"#,
            ),
            skill,
        ];
        let actor = tools[0].intent.agent_run_id;
        for tool in &mut tools {
            tool.intent.agent_run_id = actor;
            tool.observation.as_mut().unwrap().intent.agent_run_id = actor;
        }
        let grouped = tool_groups(&tools).collect::<Vec<_>>();
        assert_eq!(grouped, vec![(0..3, "⏺ Read 1 file, listed 1 directory, loaded 1 Skill\n  ⎿ 2 entries, 2 lines, 38.3 KB".into())]);
        assert_eq!(
            plain_tool_summary(&grouped[0].1),
            "Read 1 file, listed 1 directory, loaded 1 Skill\nResult: 2 entries, 2 lines, 38.3 KB"
        );
        assert_eq!(
            read_result_summary(&[tools[2].clone(), tools[2].clone()]),
            "76.6 KB"
        );
        let mut missing = tool_fixture(
            ToolCall::Read {
                path: "agents/skills/ponytail/SKILL.md".into(),
                offset: None,
                limit: None,
            },
            "File or directory not found; List the parent directory to check the path",
        );
        missing.observation.as_mut().unwrap().disposition = ToolDisposition::Failed;
        assert_eq!(
            tool_summary(&missing),
            "⏺ Read(agents/skills/ponytail/SKILL.md)\n  ⎿ Error: File or directory not found; List the parent directory to check the path"
        );
        let mut tool = tool_fixture(
            ToolCall::Read {
                path: "bad\u{1b}[2J".into(),
                offset: None,
                limit: Some(100),
            },
            "blocked\u{1b}]52;forged\u{7}\nRun: forged",
        );
        for (disposition, label) in [
            (ToolDisposition::Denied, "denied"),
            (ToolDisposition::Failed, "Error"),
            (ToolDisposition::Conflict, "conflict"),
            (ToolDisposition::Cancelled, "cancelled"),
            (ToolDisposition::Uncertain, "uncertain"),
        ] {
            tool.observation.as_mut().unwrap().disposition = disposition;
            assert_eq!(
                tool_summary(&tool),
                format!(
                    "⏺ Read(bad\\u{{001b}}[2J)\n  ⎿ {label}: blocked\\u{{001b}}]52;forged\\u{{0007}}\\u{{000a}}Run: forged"
                )
            );
        }
        tool.observation.as_mut().unwrap().output = "界".repeat(4096);
        assert!(tool_summary(&tool).len() < 1024);
        use arany::permissions::{Decision, PermissionSource, PermissionVerdict};
        tool.observation.as_mut().unwrap().disposition = ToolDisposition::Denied;
        for (source, marker) in [
            (PermissionSource::Rule, "permission deny rule"),
            (PermissionSource::PinnedGrant, "pinned grant denial"),
            (PermissionSource::Hook, "hook denial"),
            (PermissionSource::Mode, "permission mode denial"),
        ] {
            tool.permission_verdict = Some(PermissionVerdict {
                host: None,
                intent_digest: [0; 32],
                decision: Decision::Deny,
                mode: None,
                basis: source,
                source,
                approved: false,
                hook_restriction: None,
                hooks: 0,
                narrowed: false,
            });
            assert!(tool_summary(&tool).ends_with(marker));
        }
    }
}

#[cfg(test)]
mod git_summary_tests {
    use super::tool_summary;
    use arany::ToolCall;

    #[test]
    fn git_reads_summarize_their_paths() {
        for (call, expected) in [
            (
                ToolCall::GitStatus { path: "src".into() },
                "⏺ Git status(src)\n  ⎿ pending",
            ),
            (
                ToolCall::GitDiff {
                    path: "src".into(),
                    base: None,
                    head: None,
                    staged: false,
                },
                "⏺ Git diff(src)\n  ⎿ pending",
            ),
            (
                ToolCall::GitLog {
                    path: ".".into(),
                    revision: None,
                    max_count: 1,
                },
                "⏺ Git log(.)\n  ⎿ pending",
            ),
        ] {
            let mut tool = super::tool_fixture(call, "");
            tool.observation = None;
            assert_eq!(tool_summary(&tool), expected);
        }
    }
}

#[cfg(test)]
mod enforcement_tests {
    #[test]
    fn tool_summary_preserves_the_guards_protected_path_diagnostic() {
        let intent = arany::EffectIntent {
            host: None,
            host_command: None,
            target: arany::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: arany::RunId::new(),
            agent_run_id: arany::AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: arany::ToolCall::Write {
                path: "file".into(),
                expected_digest: None,
                content: "text".into(),
            },
            limits: arany::ToolLimits::default(),
            expires_at_ms: 1,
            use_count: 1,
        };
        let mut tool = arany::ToolView {
            checkpoint: None,
            intent: intent.clone(),
            observation: Some(arany::ToolObservation {
                intent,
                disposition: arany::ToolDisposition::Denied,
                output: String::new(),
                attestation: None,
                cache_update: arany::CacheUpdate::NotApplicable,
                enforcement_cause: Some(arany::EnforcementCause::FilesystemDenied),
                limit_cause: None,
            }),
            permission_verdict: None,
            hooks: Vec::new(),
            hook_sequence: None,
            review_call_index: None,
            finished_sequence: Some(1),
        };
        assert!(
            super::tool_summary(&tool)
                .ends_with(" · Guard enforcement: protected path or set-ID entry refused")
        );
        use sha2::Digest as _;
        if let arany::ToolCall::Write {
            expected_digest, ..
        } = &mut tool.intent.call
        {
            *expected_digest = Some("a".repeat(64));
        }
        let observation = tool.observation.as_mut().unwrap();
        observation.intent = tool.intent.clone();
        observation.disposition = arany::ToolDisposition::Succeeded;
        observation.enforcement_cause = None;
        observation.attestation = Some(arany::Attestation::Guard(arany::GuardReceipt {
            contract_version: 1,
            intent_digest: sha2::Sha256::digest(serde_json::to_vec(&tool.intent).unwrap()).into(),
            enforcement_digest: tool.intent.enforcement_digest,
            limits: tool.intent.limits.clone(),
        }));
        tool.checkpoint = Some(arany::Checkpoint {
            status: arany::CheckpointStatus::Skipped,
            reason: Some(arany::CheckpointReason::Quota),
            pruned_before_run: Some(tool.intent.run_id),
        });
        assert_eq!(
            super::tool_summary(&tool),
            format!(
                "⏺ Write(file)\n  ⎿ applied · This change cannot be rewound (checkpoint storage full) · Older checkpoints pruned; code restore unavailable before Run {}",
                tool.intent.run_id
            )
        );
    }
}

#[cfg(test)]
mod committed_tests;

#[cfg(test)]
mod unguarded_tests {
    use super::{render_answer, tool_summary, unguarded_approval_lines};
    use arany::{
        AgentRunId, Attestation, CacheUpdate, EffectIntent, RunId, ToolCall, ToolDisposition,
        ToolLimits, ToolObservation, ToolView, UnguardedPrompt,
    };

    fn intent(args: &[&str], cwd: &str) -> EffectIntent {
        EffectIntent {
            host: None,
            host_command: None,
            target: arany::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: RunId::new(),
            agent_run_id: AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: ToolCall::CommandUnguarded {
                program: "sh".into(),
                args: args.iter().map(|arg| (*arg).into()).collect(),
                cwd: cwd.into(),
                justification: "It needs the network\u{1b}[31m".into(),
            },
            limits: ToolLimits::default(),
            expires_at_ms: 1,
            use_count: 1,
        }
    }

    #[test]
    fn the_unguarded_prompt_carries_the_plan_copy_and_escapes_model_text() {
        let prompt = UnguardedPrompt {
            sensitive: false,
            inactive_rules: false,
            executable: "/usr/bin/bash".into(),
            attribution: None,
            remember: true,
        };
        let lines = unguarded_approval_lines(&intent(&["-c", "cargo test"], ""), &prompt);
        assert_eq!(
            lines,
            [
                "Run outside the Guard? (unguarded)",
                "The primary asks to run this command on this machine with no sandbox.",
                "Program: sh → /usr/bin/bash",
                "Args:",
                "  -c",
                "  cargo test",
                "Directory: ./ (the real project folder)",
                "Model's reason: It needs the network\\u{001b}[31m",
                "Host access requires explicit authority; no prior Guard failure is required.",
                "Risk: it runs as you, with your home directory, network and every file you can reach.",
                "Changes land directly in the project without changeset validation, and Arany cannot",
                "guarantee that processes it leaves behind are stopped. This run is recorded as unguarded.",
                "It can execute repository configuration, modify Arany permissions, Store and journal, and disclose private data to the Provider.",
                "Host effects cannot be rewound; same-user state tampering is not prevented or reliably detected.",
                "Shell/interpreter inline programs are opaque beyond a lexical privilege-tool check; it is not a security boundary.",
                "File scripts, obfuscation and other payloads remain uninspected. Host children have no controlling terminal, so terminal privilege prompts cannot appear.",
                "Allow only if you would type this exact command yourself.",
            ]
        );
        assert!(lines.iter().all(|line| !line.contains('\u{1b}')));
        let nested = unguarded_approval_lines(&intent(&[], "sub"), &prompt);
        assert!(nested.contains(&"Args: none".to_owned()));
        assert!(nested.contains(&"Directory: ./sub (inside the real project folder)".to_owned()));
    }

    fn host_tool(disposition: ToolDisposition, attested: bool) -> ToolView {
        let intent = intent(&["-c", "exit 0"], "");
        ToolView {
            checkpoint: None,
            observation: Some(ToolObservation {
                enforcement_cause: None,
                limit_cause: (disposition == arany::ToolDisposition::Limit)
                    .then_some(arany::LimitCause::Resource),
                attestation: attested.then(|| {
                    Attestation::Unguarded(arany::UnguardedReceipt {
                        contract_version: 1,
                        intent_digest: [0; 32],
                        executable_sha256: "0".repeat(64),
                        pid: 1,
                        exit_code: Some(0),
                        descendants_confirmed_stopped: true,
                    })
                }),
                intent: intent.clone(),
                disposition,
                output: String::new(),
                cache_update: CacheUpdate::NotApplicable,
            }),
            intent,
            review_call_index: None,
            permission_verdict: None,
            hooks: Vec::new(),
            hook_sequence: None,
            finished_sequence: None,
        }
    }

    #[test]
    fn host_runs_are_marked_unguarded_everywhere_they_are_summarized() {
        assert_eq!(
            tool_summary(&host_tool(ToolDisposition::Succeeded, true)),
            "⏺ Unguarded command(sh -c exit 0)\n  ⎿ 0 B · UNGUARDED"
        );
        assert_eq!(
            tool_summary(&host_tool(ToolDisposition::Uncertain, false)),
            "⏺ Unguarded command(sh -c exit 0)\n  ⎿ uncertain · UNGUARDED"
        );
        assert_eq!(
            tool_summary(&host_tool(ToolDisposition::Denied, false)),
            "⏺ Unguarded command(sh -c exit 0)\n  ⎿ denied"
        );
    }

    #[test]
    fn answer_headings_count_unguarded_and_host_effects() {
        let mut run = arany::RunView {
            skill: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: RunId::new(),
            objective: "Run".into(),
            images: Vec::new(),
            config: None,
            provider_fallbacks: Vec::new(),
            provider_retries: Vec::new(),
            agents: Vec::new(),
            assistant_message: Some("Done".into()),
            status: arany::RunStatus::Finished,
            accepted_sequence: 1,
            finished_sequence: Some(2),
            failure: None,
            tools: Vec::new(),
            tool_compaction_summary: None,
            tool_compaction_covered_count: 0,
            tool_results_cleared_count: 0,
            definitions: Default::default(),
            display: Vec::new(),
            steering_notes: Vec::new(),
            lifecycle_hooks: Vec::new(),
            lifecycle_hook_sequence: None,
            provider_sequences: Default::default(),
        };
        assert_eq!(render_answer(&run, "Answer:"), "Answer:\n  Done\n");
        run.tools.push(host_tool(ToolDisposition::Succeeded, true));
        assert_eq!(
            render_answer(&run, "Arany · Answer:"),
            "Arany · Answer (unguarded effects: 1):\n  Done\n"
        );
        assert_eq!(
            render_answer(&run, "Answer:"),
            "Answer (unguarded effects: 1):\n  Done\n"
        );
        let mut host = host_tool(ToolDisposition::Uncertain, false);
        host.intent.call = ToolCall::GitPush {
            remote: "https://github.com/example/project".into(),
            branch: "main".into(),
        };
        run.tools.push(host);
        assert_eq!(
            render_answer(&run, "Answer:"),
            "Answer (unguarded effects: 1; host effects: 1):\n  Done\n"
        );
        run.tools.remove(0);
        assert_eq!(
            render_answer(&run, "Arany · Answer:"),
            "Arany · Answer (host effects: 1):\n  Done\n"
        );
    }
}

use super::usage::grouped_count;
use super::{escape_terminal, format_bytes, format_duration, safe_truncate};
use arany::{RunStatus, RunView};
use std::borrow::Cow;

mod approval;
mod summary;

const SUMMARY_TEXT_WIDTH: usize = 160;

pub(crate) fn tool_summary(tool: &arany::ToolView) -> String {
    let operation = summary::operation(&tool.intent.call);
    let result = tool_result(tool);
    let mut summary = format!(
        "⏺ {}\n  ⎿ {result}",
        safe_truncate(&operation, SUMMARY_TEXT_WIDTH)
    );
    if let Some(label) = tool
        .observation
        .as_ref()
        .and_then(|observation| cache_update_label(observation.cache_update))
    {
        summary.push_str(" · ");
        summary.push_str(label);
    }
    push_execution_notes(&mut summary, tool);
    if tool.ran_unguarded() {
        summary.push_str(" · UNGUARDED");
    }
    if let Some(cause) = tool
        .observation
        .as_ref()
        .and_then(|value| value.enforcement_diagnostic())
    {
        summary.push_str(" · ");
        summary.push_str(cause);
    }
    if let Some(stage) = tool
        .observation
        .as_ref()
        .and_then(|value| value.guard_stage_diagnostic())
    {
        summary.push_str(" · ");
        summary.push_str(&stage);
    }
    if let Some(note) = deny_note(tool) {
        summary.push_str(note);
    }
    if let Some(note) = hook_note(tool) {
        summary.push_str(note);
    }
    if let Some(spill) = tool
        .observation
        .as_ref()
        .and_then(|observation| observation.spill.as_ref())
    {
        summary.push_str(&format!(
            "\n  Output spill: spill:{}; {}; truncated: {}",
            safe_truncate(&spill.r#ref, SUMMARY_TEXT_WIDTH),
            format_bytes(spill.bytes),
            spill.truncated
        ));
    }
    summary
}

fn tool_result(tool: &arany::ToolView) -> String {
    use arany::ToolDisposition;
    use summary::ResultKind;
    match tool.observation.as_ref() {
        None => "pending".into(),
        Some(observation) => match observation.disposition {
            ToolDisposition::Limit => format!("limit: {}", limit_summary(observation)),
            ToolDisposition::Succeeded => match summary::result_kind(&tool.intent.call) {
                ResultKind::Applied => "applied".into(),
                ResultKind::Command => command_result_summary(observation)
                    .unwrap_or_else(|| read_result_summary(std::slice::from_ref(tool))),
                ResultKind::GitCommit | ResultKind::GitSwitch | ResultKind::GitWrite => {
                    git_write_summary(observation)
                }
                ResultKind::Host => safe_truncate(&observation.output, SUMMARY_TEXT_WIDTH),
                ResultKind::Read => read_result_summary(std::slice::from_ref(tool)),
            },
            disposition => unsuccessful_result(tool, observation, disposition),
        },
    }
}

fn unsuccessful_result(
    tool: &arany::ToolView,
    observation: &arany::ToolObservation,
    disposition: arany::ToolDisposition,
) -> String {
    use arany::ToolDisposition;
    use summary::ResultKind;
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
    if disposition == ToolDisposition::Denied {
        match tool.model_denial_label() {
            Some(label) => format!("{state}: {label}"),
            None if observation.output.is_empty() => state.into(),
            None => format!(
                "{state}: {}",
                safe_truncate(&observation.output, SUMMARY_TEXT_WIDTH)
            ),
        }
    } else if observation.output.is_empty() {
        state.into()
    } else {
        match disposition {
            ToolDisposition::Failed
                if matches!(summary::result_kind(&tool.intent.call), ResultKind::Command) =>
            {
                format!(
                    "{state}: {}",
                    command_result_summary(observation).unwrap_or_else(|| {
                        safe_truncate(&observation.output, SUMMARY_TEXT_WIDTH)
                    })
                )
            }
            // An uncertain Git write may lead with locks the owner must remove by hand.
            ToolDisposition::Uncertain if summary::uncertain_git_write(&tool.intent.call) => {
                format!("{state}: {}", wrapped_rows(&observation.output))
            }
            _ => format!(
                "{state}: {}",
                safe_truncate(&observation.output, SUMMARY_TEXT_WIDTH)
            ),
        }
    }
}

fn push_execution_notes(summary: &mut String, tool: &arany::ToolView) {
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
    for fact in &tool.intent.rewrite {
        summary.push_str(" · rewrite: ");
        summary.push_str(fact.stage.label());
    }
    if let Some(wrap) = &tool.intent.wrap {
        summary.push_str(" · wrap: ");
        summary.push_str(match wrap.stage {
            arany::WrapStage::Wrapped => "wrapped",
            arany::WrapStage::Direct => "direct wrapper",
            arany::WrapStage::Probe => "probe",
            arany::WrapStage::PinRefused => "unavailable (needs re-pin)",
        });
        summary.push_str(" · ");
        summary.push_str(&escape_terminal(&wrap.tool.executable));
    }
}

fn deny_note(tool: &arany::ToolView) -> Option<&'static str> {
    let verdict = tool
        .permission_verdict
        .as_ref()
        .filter(|verdict| verdict.decision == arany::permissions::Decision::Deny)?;
    Some(match verdict.source {
        arany::permissions::PermissionSource::Rule => " · permission deny rule",
        arany::permissions::PermissionSource::TypedPrefixRule => " · typed Git deny rule",
        arany::permissions::PermissionSource::HostPrefixRule => " · host command deny rule",
        arany::permissions::PermissionSource::PinnedGrant => " · pinned grant denial",
        arany::permissions::PermissionSource::Hook => " · hook denial",
        _ => " · permission mode denial",
    })
}

// Any hook with an unknown outcome outranks a hook that failed without blocking.
fn hook_note(tool: &arany::ToolView) -> Option<&'static str> {
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
        Some(" · hook outcome unknown")
    } else if failed {
        Some(" · hook failed (non-blocking)")
    } else {
        None
    }
}

pub(super) fn limit_summary(observation: &arany::ToolObservation) -> String {
    use arany::LimitCause;
    match observation.limit_cause.unwrap_or(LimitCause::Resource) {
        LimitCause::Timeout => format!(
            "timed out after {}{}",
            format_duration(u64::from(observation.intent.effective_timeout_ms())),
            if matches!(
                summary::result_kind(&observation.intent.call),
                summary::ResultKind::Command
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
    let text = |key: &str| safe_truncate(result[key].as_str().unwrap_or("?"), SUMMARY_TEXT_WIDTH);
    let short = |key: &str| text(key).chars().take(12).collect::<String>();
    match summary::result_kind(&observation.intent.call) {
        summary::ResultKind::GitCommit => {
            format!("committed {} on {}", short("commit"), text("branch"))
        }
        summary::ResultKind::GitSwitch if result["switched"] == false => {
            format!("already on {}", text("branch"))
        }
        summary::ResultKind::GitSwitch => {
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
    let stderr_text = result.get("stderr")?.as_str()?;
    let truncated = [
        (stdout, result.get("stdout")?.as_str()?),
        (stderr, stderr_text),
    ]
    .into_iter()
    .any(|(total, text)| {
        total.bytes > text.len() as u64 || total.lines != text.lines().count() as u64
    });
    let lines = stdout.lines.saturating_add(stderr.lines);
    let count = grouped_count(lines);
    let selection = result.get("selection")?;
    let omissions = if selection.is_null() {
        " · selection counts unavailable".into()
    } else {
        let selection_count = |key: &str| selection[key].as_u64();
        let counts = [
            selection_count("ignored_entries")?,
            selection_count("pruned_directories")?,
            selection_count("oversized_inputs")?,
            selection_count("discarded_outputs")?,
        ];
        let noun = |count: u64, singular: &str, plural: &str| {
            format!(
                "{} {}",
                grouped_count(count),
                if count == 1 { singular } else { plural }
            )
        };
        let [ignored, pruned, oversized, discarded] = counts;
        let mut parts = Vec::new();
        match (ignored, pruned) {
            (0, 0) => {}
            (0, _) => parts.push(noun(pruned, "pruned root", "pruned roots")),
            (_, 0) => parts.push(noun(ignored, "ignored entry", "ignored entries")),
            _ => parts.push(format!(
                "{} ({})",
                noun(ignored, "ignored entry", "ignored entries"),
                noun(pruned, "pruned root", "pruned roots")
            )),
        }
        if oversized > 0 {
            parts.push(noun(oversized, "oversized input", "oversized inputs"));
        }
        if discarded > 0 {
            parts.push(noun(discarded, "discarded output", "discarded outputs"));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" · {}", parts.join(", "))
        }
    };
    let diagnostic = if observation.disposition == arany::ToolDisposition::Failed
        && !stderr_text.trim().is_empty()
    {
        format!(" · stderr: {}", safe_truncate(stderr_text.trim(), 80))
    } else {
        String::new()
    };
    Some(format!(
        "exit {exit} · {count} {}{}{omissions}{diagnostic}",
        if lines == 1 { "line" } else { "lines" },
        if truncated { " (truncated)" } else { "" }
    ))
}

fn cache_update_label(update: arany::CacheUpdate) -> Option<&'static str> {
    use arany::CacheUpdate;
    match update {
        CacheUpdate::NotApplicable | CacheUpdate::NotAttempted | CacheUpdate::Published => None,
        CacheUpdate::Rejected => Some("cache not saved (limit)"),
        CacheUpdate::Failed => Some("cache update failed"),
        CacheUpdate::Uncertain => Some("cache outcome uncertain"),
    }
}

pub(crate) const HOST_SENSITIVE_PATH_WARNING: &str = "SENSITIVE PATH: this command names private credentials or Arany state; output may be sent to the Provider.";

const UNGUARDED_RISK: [&str; 8] = [
    "Risk: it runs as you, with your home directory, network and every file you can reach.",
    "Changes land directly in the project without changeset validation, and Arany cannot",
    "guarantee that processes it leaves behind are stopped. This run is recorded as unguarded.",
    "It can execute repository configuration, modify Arany permissions, Store and journal, and disclose private data to the Provider.",
    "Host effects cannot be rewound; same-user state tampering is not prevented or reliably detected.",
    "Shell/interpreter inline programs are opaque beyond a lexical privilege-tool check; it is not a security boundary.",
    "File scripts, obfuscation and other payloads remain uninspected. Host children have no controlling terminal, so terminal privilege prompts cannot appear.",
    "Allow only if you would type this exact command yourself.",
];

pub fn unguarded_approval_lines(
    intent: &arany::EffectIntent,
    prompt: &arany::UnguardedPrompt,
) -> Vec<String> {
    let arany::ToolCall::CommandUnguarded {
        program,
        args,
        cwd,
        justification,
        ..
    } = &intent.call
    else {
        return tool_approval_lines(intent);
    };
    let mut lines = Vec::new();
    if prompt.sensitive {
        lines.push(HOST_SENSITIVE_PATH_WARNING.into());
    }
    lines.extend([
        "Run outside the Guard? (unguarded)".to_owned(),
        "The primary asks to run this command on this machine with no sandbox.".into(),
        format!(
            "Program: {} → {}",
            escape_terminal(program),
            escape_terminal(&prompt.executable)
        ),
    ]);
    if args.is_empty() {
        lines.push("Args: none".into());
    } else {
        lines.push("Args:".into());
        lines.extend(
            args.iter()
                .map(|argument| format!("  {}", escape_terminal(argument))),
        );
    }
    lines.push(host_directory(intent, cwd));
    lines.push(format!(
        "Model's reason: {}",
        escape_terminal(justification)
    ));
    if prompt.inactive_rules {
        lines.push(
            "Previous exact project approvals were archived intact under archive/inactive-host-commands; fresh approval is required.".into(),
        );
    }
    if let Some(prefix) = host_prefix_proposal(intent, prompt) {
        lines.push(format!(
            "Remember literal argv prefix (JSON): {}",
            escape_terminal(&serde_json::to_string(prefix).expect("prefix tokens"))
        ));
        lines.push("Every eligible suffix is authorized host behavior, not confinement. Saving approves this action once; the rule applies next Run. The first save creates default permission settings for other Tools.".into());
    }
    lines.push(match prompt.attribution {
        Some(cause) => format!("Guard attribution: {}", cause.diagnostic()),
        None => {
            "Host access requires explicit authority; no prior Guard failure is required.".into()
        }
    });
    lines.extend(UNGUARDED_RISK.map(String::from));
    lines
}

// A pinned host command names its absolute directory; otherwise the directory is project-relative.
fn host_directory(intent: &arany::EffectIntent, cwd: &str) -> String {
    if let Some(pin) = &intent.host_command {
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
    }
}

pub fn tool_approval_lines(intent: &arany::EffectIntent) -> Vec<String> {
    let mut lines = approval_header(intent);
    lines.extend(approval_call_lines(&intent.call));
    // Git pushes to the URL the user's insteadOf/pushInsteadOf rewrites produce, pinned at proposal.
    if let (arany::ToolCall::GitPush { .. }, Some(host)) = (&intent.call, &intent.host) {
        lines.push(format!("Pushes to: {}", escape_terminal(&host.destination)));
    }
    lines.extend(approval_footer(intent));
    lines
}

fn approval_header(intent: &arany::EffectIntent) -> Vec<String> {
    use arany::ToolCall;
    let mut lines = vec!["Proposed action".to_owned()];
    if intent.call.is_host() {
        lines.push("host · remote operation · no automatic retry".into());
        if let Some(host) = &intent.host
            && matches!(intent.call, ToolCall::GitPush { .. })
        {
            lines.push(format!(
                "Source commit: {}",
                escape_terminal(&host.source_oid)
            ));
            lines.push(format!("Refspec: {}", escape_terminal(&host.refspec)));
        }
    }
    lines
}

// Keep approval details as one exhaustive per-variant table.
fn approval_call_lines(call: &arany::ToolCall) -> Vec<String> {
    approval::call_lines(call)
}

fn approval_footer(intent: &arany::EffectIntent) -> Vec<String> {
    if intent.call.is_host() {
        Vec::new()
    } else {
        vec![String::new(), "Allow once for this exact action".into()]
    }
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
    summary::read_group(&tool.intent.call).is_some()
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
            |c| summary::read_group(c) == Some(summary::ReadGroup::Pattern),
            "searched for",
            "pattern",
            "patterns",
        ),
        (
            |c| summary::read_group(c) == Some(summary::ReadGroup::File),
            "read",
            "file",
            "files",
        ),
        (
            |c| summary::read_group(c) == Some(summary::ReadGroup::Directory),
            "listed",
            "directory",
            "directories",
        ),
        (
            |c| summary::read_group(c) == Some(summary::ReadGroup::Git),
            "inspected",
            "Git query",
            "Git queries",
        ),
        (
            |c| summary::read_group(c) == Some(summary::ReadGroup::Skill),
            "loaded",
            "Skill",
            "Skills",
        ),
        (
            |c| summary::read_group(c) == Some(summary::ReadGroup::Mcp),
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

pub(crate) fn tool_detail_text(tool: &arany::ToolView) -> String {
    let (head, output) = tool_detail_parts(tool);
    head + output.as_ref()
}

// The bounded summary and arguments, then the projected result, for selection and copy.
pub(crate) fn tool_detail_parts(tool: &arany::ToolView) -> (String, Cow<'_, str>) {
    let arguments = serde_json::to_string(&tool.intent.call).expect("typed Tool call");
    let output = match tool.model_denial_output() {
        Some(output) => Cow::Borrowed(output),
        None => tool
            .observation
            .as_ref()
            .map_or(Cow::Borrowed("Awaiting result"), |observation| {
                Cow::Borrowed(observation.output.as_str())
            }),
    };
    (
        format!("{}\nArguments: {arguments}\nResult:\n", tool_summary(tool)),
        output,
    )
}

pub(crate) const HOST_SESSION_APPROVAL_HINT: &str =
    "Shift+Tab to Request offers session approval for later eligible commands";
// Linear input has no mode cycle, so it names the visual-mode route instead.
pub(crate) const HOST_SESSION_APPROVAL_LINEAR_HINT: &str = "Session approval for later eligible commands needs Request approvals, selected with Shift+Tab in a visual mode";

#[derive(Clone)]
pub struct ToolApprovalPrompt {
    pub intent_id: uuid::Uuid,
    pub lines: Vec<String>,
    pub choices: Vec<ToolApprovalChoice>,
    pub unguarded: bool,
}

impl ToolApprovalPrompt {
    pub fn for_mode(&self, mode: Option<arany::ApprovalMode>) -> Cow<'_, Self> {
        if !self.unguarded
            || mode != Some(arany::ApprovalMode::AutoEdits)
            || self.choices.contains(&ToolApprovalChoice::ExactCommand)
        {
            return Cow::Borrowed(self);
        }
        let mut prompt = self.clone();
        prompt.lines.push(HOST_SESSION_APPROVAL_HINT.into());
        Cow::Owned(prompt)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolApprovalChoice {
    Yes,
    AutoEdits,
    ExactCommand,
    HostPrefix,
    GitPrefix(arany::permissions::TypedGitPrefix),
    No,
}

impl ToolApprovalChoice {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Yes => "Yes",
            Self::AutoEdits => "Yes, and don't ask again for: file edits",
            Self::ExactCommand => "Yes, for this session",
            Self::HostPrefix => "Yes, and allow this host prefix in this folder from the next Run",
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
                arany::permissions::TypedGitPrefix::PrList => {
                    "Yes, and allow gh pr list in this folder from the next Run"
                }
                arany::permissions::TypedGitPrefix::PrView => {
                    "Yes, and allow gh pr view in this folder from the next Run"
                }
            },
            Self::No => "No",
        }
    }

    pub fn reply(self) -> arany::ApprovalReply {
        match self {
            Self::Yes | Self::AutoEdits => arany::ApprovalReply::AllowOnce,
            Self::ExactCommand => arany::ApprovalReply::AllowForSession,
            Self::GitPrefix(_) | Self::HostPrefix => arany::ApprovalReply::AllowPrefixInFolder,
            Self::No => arany::ApprovalReply::Deny,
        }
    }
}

fn host_prefix_proposal<'a>(
    intent: &'a arany::EffectIntent,
    prompt: &arany::UnguardedPrompt,
) -> Option<&'a [String]> {
    match &intent.call {
        arany::ToolCall::CommandUnguarded {
            prefix_rule: Some(prefix),
            ..
        } if prompt.prefix => Some(prefix),
        _ => None,
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
    } else if approval.unguarded.is_none()
        && (!approval.intent.call.is_host()
            || matches!(
                approval.intent.call,
                arany::ToolCall::GhPrList { .. } | arany::ToolCall::GhPrView { .. }
            ))
    {
        if approval.allow_auto_edits && !approval.intent.call.is_host() {
            choices.push(ToolApprovalChoice::AutoEdits);
        }
        if let Some(prefix) = approval.remember_prefix {
            choices.push(ToolApprovalChoice::GitPrefix(prefix));
        }
    }
    if approval
        .unguarded
        .as_ref()
        .and_then(|prompt| host_prefix_proposal(&approval.intent, prompt))
        .is_some()
    {
        choices.push(ToolApprovalChoice::HostPrefix);
    }
    choices.push(ToolApprovalChoice::No);
    choices
}

#[cfg(test)]
mod tests;

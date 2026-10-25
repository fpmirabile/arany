use super::usage::grouped_count;
use super::{escape_terminal, format_bytes, format_duration, safe_truncate};
use arany::{RunStatus, RunView};
use std::borrow::Cow;

pub(crate) fn tool_summary(tool: &arany::ToolView) -> String {
    use arany::{ToolCall, ToolDisposition};
    // Keep compact row labels as one exhaustive variant table.
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
        ToolCall::Delete { path, .. } => format!("Delete({path})"),
        ToolCall::Move {
            source,
            destination,
            ..
        } => format!("Move({source} → {destination})"),
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
        ToolCall::GhPrList { repository, .. } => format!("host PR list({repository})"),
    };
    let result = match tool.observation.as_ref() {
        None => "pending".into(),
        Some(observation) => match observation.disposition {
            ToolDisposition::Limit => format!("limit: {}", limit_summary(observation)),
            ToolDisposition::Succeeded => match tool.intent.call {
                ToolCall::Write { .. }
                | ToolCall::Edit { .. }
                | ToolCall::MultiEdit { .. }
                | ToolCall::Delete { .. }
                | ToolCall::Move { .. }
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
                ToolCall::GhPrList { .. } => safe_truncate(&observation.output, 160),
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
                if disposition == ToolDisposition::Denied {
                    match tool.model_denial_label() {
                        Some(label) => format!("{state}: {label}"),
                        None if observation.output.is_empty() => state.into(),
                        None => format!("{state}: {}", safe_truncate(&observation.output, 160)),
                    }
                } else if observation.output.is_empty() {
                    state.into()
                } else {
                    match (disposition, &tool.intent.call) {
                        (
                            ToolDisposition::Failed,
                            ToolCall::Command { .. } | ToolCall::CommandUnguarded { .. },
                        ) => format!(
                            "{state}: {}",
                            command_result_summary(observation)
                                .unwrap_or_else(|| safe_truncate(&observation.output, 160))
                        ),
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
            arany::permissions::PermissionSource::HostPrefixRule => " · host command deny rule",
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
        if counts.iter().any(|count| *count > 0) {
            format!(
                " · {} ({}), {}, {}",
                noun(counts[0], "ignored entry", "ignored entries"),
                noun(counts[1], "pruned root", "pruned roots"),
                noun(counts[2], "oversized input", "oversized inputs"),
                noun(counts[3], "discarded output", "discarded outputs")
            )
        } else {
            String::new()
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
        CacheUpdate::NotApplicable
        | CacheUpdate::Discarded
        | CacheUpdate::NotAttempted
        | CacheUpdate::Published => None,
        CacheUpdate::Rejected => Some("cache not saved (limit)"),
        CacheUpdate::Failed => Some("cache update failed"),
        CacheUpdate::Uncertain => Some("cache outcome uncertain"),
    }
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
        ..
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
    let mut lines = approval_header(intent);
    lines.extend(approval_call_lines(&intent.call));
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
    use arany::ToolCall;
    let mut lines = Vec::new();
    if call.is_host() {
        match call {
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
            ToolCall::GhPrList {
                repository,
                state,
                base,
                head,
                limit,
            } => {
                lines.push("Private GitHub PR data may be sent to the current Provider.".into());
                lines.push(format!("Repository: {}", escape_terminal(repository)));
                lines.push(format!("State: {}", state.as_str()));
                lines.push(format!(
                    "Base: {}",
                    escape_terminal(base.as_deref().unwrap_or("any"))
                ));
                lines.push(format!(
                    "Head: {}",
                    escape_terminal(head.as_deref().unwrap_or("any"))
                ));
                lines.push(format!("Limit: {limit}"));
            }
            _ => {}
        }
        return lines;
    }
    match call {
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
        ToolCall::Delete { path, .. } => lines.push(format!(
            "Delete {} · only if the file is unchanged",
            escape_terminal(path)
        )),
        ToolCall::Move {
            source,
            destination,
            ..
        } => {
            lines.push(format!(
                "Move {} → {}",
                escape_terminal(source),
                escape_terminal(destination)
            ));
            lines.push("Only if the source is unchanged and the destination is absent".into());
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
            &serde_json::to_string(call).expect("typed action"),
        )),
    }
    lines
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
            Self::ExactCommand => {
                "Yes, and don't ask again for: this identical command in this live session (unguarded)"
            }
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
                    "Yes, and allow gh pr list from the next Run"
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
            || matches!(approval.intent.call, arany::ToolCall::GhPrList { .. }))
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

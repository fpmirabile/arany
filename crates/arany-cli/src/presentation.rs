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
pub(crate) use agents::AgentInspectorModel;
pub(crate) use model::{DraftAction, PresentationModel};
pub(crate) use model::{
    linear_session_lines, model_id_preview, safe_truncate, tool_capacity_notice,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Output {
    Text,
    Jsonl,
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
        } => "Rewind publication".into(),
        ToolCall::PrepareWorker { .. } => "Worker data preparation".into(),
        ToolCall::ApplyChangeset {
            source: arany::worker::ChangesetSource::WorkerHandover { .. },
        } => "Worker changeset publication".into(),
        ToolCall::ParseAgentDefinition { .. } => "Parse agent definition".into(),
        ToolCall::List { path } => format!("List {path}"),
        ToolCall::Read { path, .. } => format!("Read {path}"),
        ToolCall::Search { path, .. } => format!("Search {path}"),
        ToolCall::Mkdir { path } => format!("Create folder {path}"),
        ToolCall::Write { path, .. } => format!("Write {path}"),
        ToolCall::Edit { path, .. } => format!("Edit {path}"),
        ToolCall::Command { program, .. } => format!("Command {program}"),
        ToolCall::CommandUnguarded { program, .. } => format!("Unguarded command {program}"),
        ToolCall::ApplyChangeset {
            source: arany::worker::ChangesetSource::CommandReceipt { changeset, .. },
        } => {
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
    let mut summary = match cache_update {
        Some(cache_update) => format!(
            "Tool · {} · {state} · {cache_update}",
            safe_truncate(&operation, 160)
        ),
        None => format!("Tool · {} · {state}", safe_truncate(&operation, 160)),
    };
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

fn cache_update_label(update: arany::CacheUpdate) -> Option<&'static str> {
    use arany::CacheUpdate;
    match update {
        CacheUpdate::NotApplicable => None,
        CacheUpdate::Discarded => Some("private cache discarded"),
        CacheUpdate::NotAttempted => Some("cache not attempted"),
        CacheUpdate::Published => Some("cache saved"),
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
        "This command failed inside the Guard. The model asks to run it again on this machine with no sandbox.".into(),
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
    lines.push(if cwd.is_empty() {
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
    lines.push(match prompt.attribution {
        Some(cause) => format!("Guard attribution: {}", cause.diagnostic()),
        None => {
            "Guard attribution: none. Arany could not verify that the Guard caused the failure."
                .into()
        }
    });
    lines.extend(
        [
            "Risk: it runs as you, with your home directory, network and every file you can reach.",
            "Changes land directly in the project without changeset validation, and Arany cannot",
            "guarantee that processes it leaves behind are stopped. This run is recorded as unguarded.",
            "Allow only if you would type this exact command yourself.",
        ]
        .map(String::from),
    );
    lines
}

pub fn tool_approval_lines(intent: &arany::EffectIntent) -> Vec<String> {
    use arany::ToolCall;
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
        "Arany can read this folder already. Trusting it also lets Arany edit project files, run commands and use project Skills.".into(),
        "Every edit and command runs in an isolated copy under native OS protection and publishes only reviewed changes; without that protection Tools stay disabled.".into(),
        "Commands run offline and ask first unless you change the approval mode later with Shift+Tab.".into(),
        "Prior versions of files changed by Arany are saved unencrypted in private StateRoot storage.".into(),
        "Trust is remembered for this exact folder only. Read only keeps today's behavior.".into(),
    ]
}

pub(crate) const RUNTIME_PROFILE_LINES: [&str; 3] = [
    "Optional runtime profiles (read only)",
    "Check a toolchain to let commands use that exact version; its bin joins PATH. Check a cache to let commands read it. At most one version per runtime. Nothing is selected until you check it, and checking switches the choice to Trust.",
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

pub fn render_provider_fallbacks(run: &RunView) -> Option<String> {
    render_goal_provider_fallbacks([run])
}

pub fn render_goal_provider_fallbacks<'a>(
    runs: impl IntoIterator<Item = &'a RunView>,
) -> Option<String> {
    let notices: Vec<_> = runs
        .into_iter()
        .flat_map(|run| run.provider_fallbacks.iter())
        .map(|(_, fallback)| escape_terminal(&fallback.notice()))
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
        goal_clock(goal.active_ms),
        budget
            .max_active_ms
            .map_or_else(|| "unlimited".to_owned(), goal_clock),
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
            definitions: Default::default(),
            display: Vec::new(),
            steering_notes: Vec::new(),
            lifecycle_hooks: Vec::new(),
            lifecycle_hook_sequence: None,
            provider_sequences: Default::default(),
            provider_fallbacks: Vec::new(),
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
    let mut rendered = match heading.strip_suffix(':') {
        Some(base) if unguarded > 0 => format!("{base} (unguarded effects: {unguarded}):\n"),
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
        "Image {}: PNG {width}x{height}, {:.1} KB",
        index + 1,
        image.byte_len() as f64 / 1_000.0
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

fn unsafe_display_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character as u32,
            0x202a..=0x202e | 0x2066..=0x2069 | 0x061c | 0x200e | 0x200f | 0x2028 | 0x2029
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
    fn goal_fallback_notices_collect_every_run_once_in_order() {
        use arany::{
            ProviderCallDisposition, ProviderFallback, ProviderIdentity, ProviderRole, RunId,
            RunStatus,
        };
        fn fallback(model: &str) -> (arany::AgentRunId, ProviderFallback) {
            (
                arany::AgentRunId::new(),
                ProviderFallback {
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
            definitions: Default::default(),
            display: Vec::new(),
            steering_notes: Vec::new(),
            lifecycle_hooks: Vec::new(),
            lifecycle_hook_sequence: None,
            provider_sequences: Default::default(),
            provider_fallbacks: fallbacks,
        };
        let earlier = run(vec![fallback("first")]);
        let latest = run(vec![fallback("second")]);
        let notices = super::render_goal_provider_fallbacks([&earlier, &latest]).unwrap();
        let lines: Vec<_> = notices.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("openai/first ") && lines[1].contains("openai/second "));
        assert_eq!(
            super::render_goal_provider_fallbacks([&latest]).unwrap(),
            super::render_provider_fallbacks(&latest).unwrap()
        );
        assert_eq!(
            super::render_goal_provider_fallbacks([&run(Vec::new())]),
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
                budget: GoalBudget::default(),
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
mod git_summary_tests {
    use super::tool_summary;
    use arany::{AgentRunId, RunId, ToolView};
    use arany::{EffectIntent, ToolCall, ToolLimits};
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
                checkpoint: None,
                intent: EffectIntent {
                    target: arany::worker::EffectTarget::Workspace,
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
                permission_verdict: None,
                hooks: Vec::new(),
                hook_sequence: None,
                finished_sequence: None,
            };
            assert_eq!(tool_summary(&tool), expected);
        }
    }
}

#[cfg(test)]
mod enforcement_tests {
    #[test]
    fn tool_summary_preserves_the_guards_protected_path_diagnostic() {
        let intent = arany::EffectIntent {
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
                "Tool · Write file · succeeded · This change cannot be rewound (checkpoint storage full) · Older checkpoints pruned; code restore unavailable before Run {}",
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
            executable: "/usr/bin/bash".into(),
            attribution: None,
            remember: true,
        };
        let lines = unguarded_approval_lines(&intent(&["-c", "cargo test"], ""), &prompt);
        assert_eq!(
            lines,
            [
                "Run outside the Guard? (unguarded)",
                "This command failed inside the Guard. The model asks to run it again on this machine with no sandbox.",
                "Program: sh → /usr/bin/bash",
                "Args:",
                "  -c",
                "  cargo test",
                "Directory: ./ (the real project folder)",
                "Model's reason: It needs the network\\u{001b}[31m",
                "Guard attribution: none. Arany could not verify that the Guard caused the failure.",
                "Risk: it runs as you, with your home directory, network and every file you can reach.",
                "Changes land directly in the project without changeset validation, and Arany cannot",
                "guarantee that processes it leaves behind are stopped. This run is recorded as unguarded.",
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
            "Tool · Unguarded command sh · succeeded · UNGUARDED"
        );
        assert_eq!(
            tool_summary(&host_tool(ToolDisposition::Uncertain, false)),
            "Tool · Unguarded command sh · uncertain · UNGUARDED"
        );
        assert_eq!(
            tool_summary(&host_tool(ToolDisposition::Denied, false)),
            "Tool · Unguarded command sh · denied"
        );
    }

    #[test]
    fn answer_headings_count_unguarded_effects() {
        let mut run = arany::RunView {
            skill: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: RunId::new(),
            objective: "Run".into(),
            images: Vec::new(),
            config: None,
            provider_fallbacks: Vec::new(),
            agents: Vec::new(),
            assistant_message: Some("Done".into()),
            status: arany::RunStatus::Finished,
            accepted_sequence: 1,
            finished_sequence: Some(2),
            failure: None,
            tools: Vec::new(),
            tool_compaction_summary: None,
            tool_compaction_covered_count: 0,
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
    }
}

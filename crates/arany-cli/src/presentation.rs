use crate::protocol::EventLine;
use arany::{
    AgentStatus, Event, EventEnvelope, ProviderCallDisposition, ProviderCallRecord, ProviderKind,
    RunStatus, RunView, SessionView,
};
use std::borrow::Cow;
use unicode_width::UnicodeWidthStr;

mod agents;
mod exec;
mod model;
pub mod rewind;
pub mod storage;
mod tool_rows;
pub(crate) mod usage;
#[cfg(test)]
pub(crate) use agents::tests::team_view;
pub(crate) use agents::{
    AgentFocus, AgentInspectorModel, AgentQuery, AgentRow, AgentTarget, AgentTranscript, Appended,
    MAX_AGENT_ROWS, MAX_AGENT_SEARCH, MAX_RECENT_RUNS, STEERING_NOTE_HEADING, TranscriptPart,
    agent_targets, run_targets,
};
pub use exec::render_exec;
pub(crate) use model::{DraftAction, MAX_ROW_WIDTH, PresentationModel};
pub(crate) use model::{
    linear_session_lines, model_id_preview, safe_truncate, tool_capacity_notice,
};
#[cfg(test)]
pub(crate) use tool_rows::tool_summary;
pub use tool_rows::{
    ToolApprovalChoice, tool_approval_choices, tool_approval_lines, unguarded_approval_lines,
};
pub(crate) use tool_rows::{
    plain_tool_summary, settled_tool_groups, tool_detail_parts, tool_detail_text, tool_groups,
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

pub fn format_tokens(tokens: u32) -> String {
    if tokens < 1_000 {
        return format!("{tokens} tokens");
    }
    let (divisor, unit) = if tokens < 1_000_000 {
        (1_000, "k")
    } else {
        (1_000_000, "M")
    };
    let value = f64::from(tokens) / f64::from(divisor);
    let value = format!("{value:.1}");
    format!("{}{unit} tokens", value.trim_end_matches(".0"))
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

pub fn session_run_feedback(view: &SessionView, run: &RunView) -> Option<String> {
    let Some(goal) = view.budget_stop_goal(run) else {
        return run_feedback(run);
    };
    let mut text = String::from("Arany status: ");
    if let Some(summary) = arany::budget_refusal_summary(goal) {
        text.push_str(&summary);
        if let Some(report) = &goal.report {
            for (index, fact) in report.unresolved.iter().enumerate().skip(1) {
                let fact = escape_terminal(fact);
                if text.len() + fact.len() + 160 > arany::MAX_GOAL_REPORT_BYTES {
                    text.push_str(&format!(
                        "\n{} additional report fact(s) omitted.",
                        report.unresolved.len() - index
                    ));
                    break;
                }
                text.push('\n');
                text.push_str(&fact);
            }
        }
    } else {
        let budget = goal.effective_budget();
        let (dimension, used, limit) = match view.budget_stop_limit(run)? {
            arany::GoalLimit::Calls => (
                "Provider calls",
                goal.provider_calls.to_string(),
                budget.max_provider_calls.map(|calls| calls.to_string()),
            ),
            arany::GoalLimit::Tokens => (
                "tokens",
                goal.known_tokens.to_string(),
                budget.max_tokens.map(|tokens| tokens.to_string()),
            ),
            arany::GoalLimit::ActiveTime => (
                "active time",
                format_duration(goal.active_ms),
                budget.max_active_ms.map(format_duration),
            ),
            arany::GoalLimit::Runs => (
                "Runs",
                goal.run_ids.len().to_string(),
                Some(budget.max_runs.to_string()),
            ),
        };
        text.push_str(&format!("Run stopped at a budget boundary: {dimension}; committed usage {used}, pinned limit {}. Committed refused reservation detail is unavailable.", limit?));
        if goal.token_usage_unknown {
            text.push_str(" Token usage is unknown.");
        }
        if goal.uncertain_active_ms > 0 {
            text.push_str(" Active time includes conservative crash charges.");
        }
    }
    text.push_str(if run.assistant_message.is_some() {
        "\nThe committed answer is preserved."
    } else {
        "\nNo answer was committed; completed actions remain in the Session."
    });
    Some(text)
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
            if let Some(observation) = run
                .tools
                .last()
                .and_then(|tool| tool.observation.as_ref())
                .filter(|observation| {
                    observation.disposition == arany::ToolDisposition::Denied
                        && observation.attestation.is_none()
                })
            {
                let disposition = match observation.approval_cause {
                    Some(arany::ApprovalCause::Cancelled) => "Run cancelled.",
                    Some(arany::ApprovalCause::Paused) => "Goal paused.",
                    Some(arany::ApprovalCause::ChannelUnavailable) => {
                        "Approval channel unavailable."
                    }
                    Some(arany::ApprovalCause::ChildTimeout) => "Child approval wait timed out.",
                    Some(arany::ApprovalCause::Declined) | None => "Action denied.",
                };
                let recovery = if observation.approval_cause
                    == Some(arany::ApprovalCause::ChannelUnavailable)
                {
                    "Start a new request from an interactive approval channel."
                } else {
                    "Use /permissions or Shift+Tab to review approval settings."
                };
                return Some(format!(
                    "{disposition} No further action ran; earlier completed actions remain. {recovery}"
                ));
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
            let notice = match run
                .tools
                .last()
                .and_then(|tool| tool.observation.as_ref())
                .and_then(|observation| observation.approval_cause)
            {
                Some(arany::ApprovalCause::Paused) => "Goal paused.",
                Some(
                    arany::ApprovalCause::Cancelled
                    | arany::ApprovalCause::ChannelUnavailable
                    | arany::ApprovalCause::Declined
                    | arany::ApprovalCause::ChildTimeout,
                )
                | None => "Run cancelled.",
            };
            Some(format!(
                "{notice} No answer was committed. You can submit a new task."
            ))
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
        Cause::InvalidPath => {
            "Arany refused an invalid or protected Workspace path. The proposed action did not run."
        }
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
        (ProviderCallDisposition::Rejected, Some(ProviderFailureReason::ProposalRefused)) => {
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
    if goal.status == arany::GoalStatus::Interrupted && goal.include_paths.is_none() {
        text.push_str(&format!(
            " · {}",
            arany::EngineError::GoalIncludesUnavailable
        ));
    } else if goal.status == arany::GoalStatus::Interrupted {
        text.push_str(if goal.admits_resume(view) {
            " · Use /goal resume to continue"
        } else {
            " · out of budget"
        });
    }
    if let Some(paused) = view
        .interrupted_goal_id()
        .and_then(|id| view.goal(id))
        .filter(|paused| paused.id != goal.id)
    {
        let short = paused.id.to_string().chars().take(8).collect::<String>();
        if paused.include_paths.is_none() {
            text.push_str(&format!(
                "\nPaused Goal {short}: {}",
                arany::EngineError::GoalIncludesUnavailable
            ));
        } else if paused.admits_resume(view) {
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
        for run in &view.runs {
            if view.budget_stop_goal(run).is_some()
                && let Some(feedback) = session_run_feedback(view, run)
            {
                rendered.push_str(&feedback);
                rendered.push('\n');
            }
        }
        for worker in view.runs.iter().flat_map(RunView::worker_history) {
            rendered.push_str(&escape_terminal(&worker.description()));
            rendered.push('\n');
        }
    }
    rendered
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
    arany::escape_status_text(value).into_owned()
}

fn unsafe_display_character(character: char) -> bool {
    let mut bytes = [0; 4];
    matches!(
        arany::escape_status_text(character.encode_utf8(&mut bytes)),
        std::borrow::Cow::Owned(_)
    )
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

pub(crate) fn session_access_label(defaults: &arany::SessionDefaults) -> &str {
    match defaults.provider.as_deref().and_then(ProviderKind::parse) {
        Some(ProviderKind::ChatGpt) => "ChatGPT plan",
        Some(ProviderKind::OpenAi) => "OpenAI API",
        Some(ProviderKind::Anthropic) => "Anthropic API",
        Some(ProviderKind::Custom) | None => defaults.provider.as_deref().unwrap_or("User default"),
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
                include_paths: Some(Vec::new()),
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
    fn goal_status_hints_distinguish_unknown_includes_from_exhausted_budgets() {
        for includes_known in [true, false] {
            for exhausted in [true, false] {
                let mut view = session_with_goal(true);
                view.goals[0].status = GoalStatus::Interrupted;
                view.goals[0].include_paths = includes_known.then(Vec::new);
                if exhausted {
                    view.goals[0].provider_calls = view.goals[0]
                        .effective_budget()
                        .max_provider_calls
                        .expect("finite call budget");
                }
                let guidance =
                    "Goal include paths are unknown; start a new Goal with explicit inputs";
                let current_suffix = if !includes_known {
                    format!(" · {guidance}")
                } else if exhausted {
                    " · out of budget".into()
                } else {
                    " · Use /goal resume to continue".into()
                };
                let current = goal_status(&view);
                assert!(current.ends_with(&current_suffix), "{current}");
                assert_eq!(
                    current.contains("/goal resume"),
                    includes_known && !exhausted
                );
                assert_eq!(
                    current.contains("out of budget"),
                    includes_known && exhausted
                );

                let paused = view.goals[0]
                    .id
                    .to_string()
                    .chars()
                    .take(8)
                    .collect::<String>();
                let mut newer = view.goals[0].clone();
                newer.id = arany::GoalId::new();
                newer.status = GoalStatus::Completed;
                view.goals.push(newer);
                let paused_suffix = if !includes_known {
                    format!("\nPaused Goal {paused}: {guidance}")
                } else if exhausted {
                    format!("\nPaused Goal {paused} is out of budget")
                } else {
                    format!("\nPaused Goal {paused} can be resumed with /goal resume")
                };
                let status = goal_status(&view);
                assert!(status.ends_with(&paused_suffix), "{status}");
                assert_eq!(
                    status.contains("/goal resume"),
                    includes_known && !exhausted
                );
                assert_eq!(
                    status.contains("out of budget"),
                    includes_known && exhausted
                );
            }
        }
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

    #[test]
    fn budget_feedback_uses_the_refusal_record_and_labels_missing_detail() {
        let mut view = session_with_goal(true);
        let mut run = crate::presentation::team_view().runs.remove(0);
        run.status = RunStatus::Failed;
        run.failure = Some(arany::RunFailure::BudgetExhausted);
        run.assistant_message = None;
        let goal = &mut view.goals[0];
        goal.run_ids = vec![run.id];
        goal.provider_calls = 64;
        goal.status = GoalStatus::Failed;
        goal.budget_closed = true;
        view.runs = vec![run];
        assert_eq!(
            session_run_feedback(&view, &view.runs[0]).unwrap(),
            "Arany status: Run stopped at a budget boundary: Provider calls; committed usage 64, pinned limit 64. Committed refused reservation detail is unavailable.\nNo answer was committed; completed actions remain in the Session."
        );
        let mut active_view = view.clone();
        let active_goal = &mut active_view.goals[0];
        active_goal.budget.max_provider_calls = None;
        active_goal.budget.max_active_ms = Some(90_500);
        active_goal.active_ms = 90_501;
        assert_eq!(
            session_run_feedback(&active_view, &active_view.runs[0]).unwrap(),
            "Arany status: Run stopped at a budget boundary: active time; committed usage 1 min 30.501 s, pinned limit 1 min 30.5 s. Committed refused reservation detail is unavailable.\nNo answer was committed; completed actions remain in the Session."
        );
        let goal = &mut view.goals[0];
        goal.budget_refusal = Some(arany::BudgetRefusal {
            dimension: arany::GoalLimit::Calls,
            cause: arany::BudgetRefusalCause::Exhausted,
            action: arany::BudgetAction::Provider,
            run_id: Some(view.runs[0].id),
            agent_run_id: Some(view.runs[0].agents[0].id),
            used: 64,
            limit: 64,
            requested: Some(1),
            required_minimum: Some(1),
            token_usage_unknown: false,
            charge_basis: arany::BudgetChargeBasis::Observed,
        });
        goal.report = Some(arany::GoalReport {
            summary: String::new(),
            changes: Vec::new(),
            evidence: Vec::new(),
            unresolved: vec!["Misleading budget prose".into()],
            cached_input_tokens: 0,
        });
        view.runs[0].status = RunStatus::Finished;
        view.runs[0].assistant_message = Some("Committed answer".into());
        assert_eq!(
            session_run_feedback(&view, &view.runs[0]).unwrap(),
            "Arany status: Goal budget refused Provider: Provider calls; exhausted; used at refusal 64, pinned limit 64; requested 1; required minimum 1; final settled usage 64.\nThe committed answer is preserved."
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
        use_count: 1,
    };
    arany::ToolView {
        checkpoint: None,
        observation: Some(arany::ToolObservation {
            intent: intent.clone(),
            disposition: arany::ToolDisposition::Succeeded,
            output: output.into(),
            attestation: None,
            approval_cause: None,
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
mod committed_tests;

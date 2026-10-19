use crate::session::{GoalReport, RunView};

pub(super) fn goal_report(
    view: &crate::session::SessionView,
    run: &RunView,
    goal: &crate::session::GoalView,
) -> GoalReport {
    let raw_summary = run
        .primary()
        .and_then(|primary| primary.summary.as_deref())
        .filter(|summary| !summary.is_empty())
        .unwrap_or(match run.status {
            crate::session::RunStatus::Finished => "Goal completed.",
            crate::session::RunStatus::Failed => "Goal failed before completion.",
            _ => "Goal did not reach a terminal Run boundary.",
        });
    let mut report = GoalReport {
        summary: String::new(),
        changes: Vec::new(),
        evidence: Vec::new(),
        unresolved: Vec::new(),
        cached_input_tokens: goal.known_cached_tokens,
    };
    let mut omitted_facts = false;
    let uncertain_calls = goal.unobserved_provider_calls.len();
    if uncertain_calls > 0 {
        push_goal_report_item(
            &mut report,
            GoalReportSection::Unresolved,
            &format!(
                "{} reserved Provider call(s) have no committed outcome; their usage remains unknown and charged.",
                uncertain_calls
            ),
            &mut omitted_facts,
        );
    }
    if goal.token_usage_unknown {
        push_goal_report_item(
            &mut report,
            GoalReportSection::Unresolved,
            "Provider token usage is incomplete.",
            &mut omitted_facts,
        );
    }
    if goal.uncertain_active_ms > 0 {
        push_goal_report_item(
            &mut report,
            GoalReportSection::Unresolved,
            &format!(
                "{} ms of active time were conservatively charged after interruption.",
                goal.uncertain_active_ms
            ),
            &mut omitted_facts,
        );
    }
    let mut workers: Vec<_> = view
        .runs
        .iter()
        .filter(|r| goal.run_ids.contains(&r.id))
        .flat_map(|r| r.worker_history())
        .collect();
    workers.sort_by_key(|w| {
        use crate::worker::WorkerPublication;
        if w.publication == WorkerPublication::Unknown || w.private_outcome_unknown {
            0
        } else if w.publication == WorkerPublication::Confirmed && !w.verified {
            1
        } else if w.publication == WorkerPublication::Unpublished {
            2
        } else {
            3
        }
    });
    let (critical, workers): (Vec<_>, Vec<_>) = workers.into_iter().partition(|w| {
        w.publication == crate::worker::WorkerPublication::Unknown || w.private_outcome_unknown
    });
    for worker in &critical {
        push_goal_report_item(
            &mut report,
            GoalReportSection::Unresolved,
            &worker.description(),
            &mut omitted_facts,
        );
    }
    push_cache_facts(&mut report, view, goal, &mut omitted_facts);
    push_unguarded_facts(&mut report, view, goal, &mut omitted_facts);
    push_checkpoint_facts(&mut report, view, goal, &mut omitted_facts);
    for (index, (_, fallback)) in view
        .runs
        .iter()
        .filter(|run| goal.run_ids.contains(&run.id))
        .flat_map(|run| &run.provider_fallbacks)
        .take(9)
        .enumerate()
    {
        if index == 8 {
            omitted_facts = true;
            break;
        }
        push_goal_report_item(
            &mut report,
            GoalReportSection::Unresolved,
            &fallback.notice(),
            &mut omitted_facts,
        );
    }
    push_settled_workers(&mut report, workers, &mut omitted_facts);
    // The serialized-size loop below is the only truncation; it starts from the full summary.
    let mut summary_bytes = raw_summary.len();
    let mut summary_notice_added = false;
    report.summary = raw_summary.to_owned();
    while !goal_report_fits_with_omission(&mut report) {
        summary_bytes = summary_bytes.saturating_sub(128);
        if summary_bytes < raw_summary.len() && !summary_notice_added {
            let summary = std::mem::take(&mut report.summary);
            push_goal_report_item(
                &mut report,
                GoalReportSection::Unresolved,
                "The primary summary was shortened to fit the durable Goal report.",
                &mut omitted_facts,
            );
            report.summary = summary;
            summary_notice_added = true;
        }
        report.summary = crate::tools::types::truncate_utf8(raw_summary, summary_bytes).to_owned();
        assert!(summary_bytes > 0 || goal_report_fits_with_omission(&mut report));
    }
    let protected_unresolved_count = report.unresolved.len();
    push_unresolved_tool_facts(&mut report, view, goal, &mut omitted_facts);
    push_success_facts(&mut report, view, goal, &mut omitted_facts);
    if omitted_facts {
        append_goal_report_omission(&mut report, protected_unresolved_count);
    }
    report
}

fn push_checkpoint_facts(
    report: &mut GoalReport,
    view: &crate::session::SessionView,
    goal: &crate::session::GoalView,
    omitted_facts: &mut bool,
) {
    let mut checkpoint_skipped = false;
    let mut checkpoints_pruned = false;
    for checkpoint in view
        .runs
        .iter()
        .filter(|run| goal.run_ids.contains(&run.id))
        .flat_map(|run| &run.tools)
        .filter_map(|tool| tool.checkpoint.as_ref())
    {
        if !checkpoint_skipped && let Some(notice) = checkpoint.notice() {
            push_goal_report_item(report, GoalReportSection::Unresolved, notice, omitted_facts);
            checkpoint_skipped = true;
        }
        if !checkpoints_pruned && let Some(run) = checkpoint.pruned_before_run {
            push_goal_report_item(
                report,
                GoalReportSection::Unresolved,
                &format!("Older checkpoints pruned; code restore unavailable before Run {run}"),
                omitted_facts,
            );
            checkpoints_pruned = true;
        }
    }
}

fn push_cache_facts(
    report: &mut GoalReport,
    view: &crate::session::SessionView,
    goal: &crate::session::GoalView,
    omitted_facts: &mut bool,
) {
    let mut cache_rejected = false;
    let mut cache_failed = false;
    let mut cache_uncertain = false;
    for task_run in view
        .runs
        .iter()
        .filter(|task_run| goal.run_ids.contains(&task_run.id))
    {
        for tool in &task_run.tools {
            if let Some(observation) = tool.observation.as_ref() {
                match observation.cache_update {
                    crate::tools::CacheUpdate::Rejected => cache_rejected = true,
                    crate::tools::CacheUpdate::Failed => cache_failed = true,
                    crate::tools::CacheUpdate::Uncertain => cache_uncertain = true,
                    crate::tools::CacheUpdate::NotApplicable
                    | crate::tools::CacheUpdate::NotAttempted
                    | crate::tools::CacheUpdate::Published
                    | crate::tools::CacheUpdate::Discarded => {}
                }
            }
        }
    }
    if cache_rejected {
        push_goal_report_item(
            report,
            GoalReportSection::Unresolved,
            "One or more Commands completed, but the build cache update was rejected; the previous cache was kept.",
            omitted_facts,
        );
    }
    if cache_failed {
        push_goal_report_item(
            report,
            GoalReportSection::Unresolved,
            "One or more Commands completed, but the build cache update failed; the previous cache was kept.",
            omitted_facts,
        );
    }
    if cache_uncertain {
        push_goal_report_item(
            report,
            GoalReportSection::Unresolved,
            "One or more Commands have an uncertain build cache publication; inspect Session history before retrying.",
            omitted_facts,
        );
    }
}

fn push_settled_workers(
    report: &mut GoalReport,
    workers: Vec<crate::worker::WorkerHistory>,
    omitted_facts: &mut bool,
) {
    let mut verified = Vec::new();
    for worker in workers {
        if worker.verified {
            verified.push(worker.assignment_id);
            continue;
        }
        push_goal_report_item(
            report,
            GoalReportSection::Unresolved,
            &worker.description(),
            omitted_facts,
        );
        if worker.publication == crate::worker::WorkerPublication::Confirmed {
            push_goal_report_item(
                report,
                GoalReportSection::Unresolved,
                &format!(
                    "Worker {}: confirmed Workspace publication needs fresh primary verification.",
                    worker.assignment_id
                ),
                omitted_facts,
            );
        }
    }
    for id in verified {
        push_goal_report_item(
            report,
            GoalReportSection::Changes,
            &format!("Worker {id}: settled and verified."),
            omitted_facts,
        );
    }
}

fn push_unguarded_facts(
    report: &mut GoalReport,
    view: &crate::session::SessionView,
    goal: &crate::session::GoalView,
    omitted_facts: &mut bool,
) {
    let count = view
        .runs
        .iter()
        .filter(|task_run| goal.run_ids.contains(&task_run.id))
        .flat_map(|task_run| &task_run.tools)
        .filter(|tool| tool.ran_unguarded())
        .count();
    if count > 0 {
        push_goal_report_item(
            report,
            GoalReportSection::Unresolved,
            &format!(
                "{count} unguarded host run(s) executed outside the Guard; their effects were not validated as a changeset."
            ),
            omitted_facts,
        );
    }
}

fn push_unresolved_tool_facts(
    report: &mut GoalReport,
    view: &crate::session::SessionView,
    goal: &crate::session::GoalView,
    omitted_facts: &mut bool,
) {
    for task_run in view
        .runs
        .iter()
        .filter(|task_run| goal.run_ids.contains(&task_run.id))
    {
        for tool in &task_run.tools {
            let call = &tool.intent.call;
            let label = goal_report_call_label(call);
            let Some(observation) = tool.observation.as_ref() else {
                if matches!(
                    task_run.status,
                    crate::session::RunStatus::Cancelled
                        | crate::session::RunStatus::Failed
                        | crate::session::RunStatus::Interrupted
                ) {
                    push_goal_report_item(
                        report,
                        GoalReportSection::Unresolved,
                        &format!("{label} has no committed Tool outcome."),
                        omitted_facts,
                    );
                }
                continue;
            };
            match observation.disposition {
                crate::tools::ToolDisposition::Succeeded
                    if observation.attested_for(&tool.intent) => {}
                crate::tools::ToolDisposition::Denied
                | crate::tools::ToolDisposition::Failed
                | crate::tools::ToolDisposition::Conflict => push_goal_report_item(
                    report,
                    GoalReportSection::Unresolved,
                    &format!("{label} failed; its effect was not recorded as successful."),
                    omitted_facts,
                ),
                crate::tools::ToolDisposition::Limit => {
                    let timeout_limit = match call {
                        crate::tools::ToolCall::Command { .. } => {
                            let requested = goal
                                .plan_contract
                                .as_ref()
                                .and_then(|contract| contract.command_timeout_ms)
                                .unwrap_or(goal.resource_settings.default_command_timeout_ms);
                            (requested > goal.resource_settings.max_command_timeout_ms).then_some((
                                requested,
                                goal.resource_settings.max_command_timeout_ms,
                            ))
                        }
                        _ => None,
                    };
                    let fact = timeout_limit.map_or_else(
                        || format!("{label} failed; its effect was not recorded as successful."),
                        |(requested, maximum)| {
                            crate::tools::types::command_timeout_limit_message(requested, maximum)
                        },
                    );
                    push_goal_report_item(
                        report,
                        GoalReportSection::Unresolved,
                        &fact,
                        omitted_facts,
                    );
                }
                crate::tools::ToolDisposition::Uncertain
                | crate::tools::ToolDisposition::Cancelled => {
                    if observation.cache_update != crate::tools::CacheUpdate::Uncertain {
                        push_goal_report_item(
                            report,
                            GoalReportSection::Unresolved,
                            &format!(
                                "{label} has an uncertain or cancelled outcome; inspect Workspace state before retrying."
                            ),
                            omitted_facts,
                        );
                    }
                }
                crate::tools::ToolDisposition::Succeeded => push_goal_report_item(
                    report,
                    GoalReportSection::Unresolved,
                    &format!("{label} reported success without a valid attestation."),
                    omitted_facts,
                ),
            }
        }
    }
}

fn push_success_facts(
    report: &mut GoalReport,
    view: &crate::session::SessionView,
    goal: &crate::session::GoalView,
    omitted_facts: &mut bool,
) {
    for task_run in view
        .runs
        .iter()
        .filter(|task_run| goal.run_ids.contains(&task_run.id))
    {
        push_definition_diagnostics(report, task_run, omitted_facts);
        for tool in &task_run.tools {
            let Some(observation) = tool.observation.as_ref() else {
                continue;
            };
            if observation.disposition != crate::tools::ToolDisposition::Succeeded
                || !observation.attested_for(&tool.intent)
            {
                continue;
            }
            let call = &tool.intent.call;
            match call {
                crate::tools::ToolCall::PrepareWorker { .. } => {}
                crate::tools::ToolCall::ApplyChangeset {
                    source: crate::session::worker::ChangesetSource::WorkerHandover { .. },
                } => {}
                crate::tools::ToolCall::ParseAgentDefinition { .. } => {}
                crate::tools::ToolCall::Write { path, .. }
                | crate::tools::ToolCall::Edit { path, .. } => {
                    push_goal_report_item(
                        report,
                        GoalReportSection::Changes,
                        &format!("{} {path}.", call.label()),
                        omitted_facts,
                    );
                }
                crate::tools::ToolCall::Mkdir { path } => {
                    push_goal_report_item(
                        report,
                        GoalReportSection::Changes,
                        &format!("Created directory {path}."),
                        omitted_facts,
                    );
                }
                crate::tools::ToolCall::ApplyChangeset {
                    source:
                        crate::session::worker::ChangesetSource::CommandReceipt { changeset, .. },
                } => {
                    for change in &changeset.files {
                        let action = if change.after.is_none() {
                            "removed"
                        } else if change.before.is_none() {
                            "created"
                        } else {
                            "updated"
                        };
                        push_goal_report_item(
                            report,
                            GoalReportSection::Changes,
                            &format!("Command {action} {}.", change.path),
                            omitted_facts,
                        );
                    }
                }
                _ => {}
            }
            push_goal_report_item(
                report,
                GoalReportSection::Evidence,
                &goal_report_evidence(call, &observation.output, observation.attestation.as_ref()),
                omitted_facts,
            );
        }
    }
}

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, PartialEq, Eq)]
enum GoalReportSection {
    Changes,
    Evidence,
    Unresolved,
}

const GOAL_REPORT_ITEM_LIMIT: usize = 32;
const GOAL_REPORT_OMISSION: &str =
    "Some lower-priority Goal facts were omitted to fit the report limits.";

fn push_goal_report_item(
    report: &mut GoalReport,
    section: GoalReportSection,
    text: &str,
    omitted_facts: &mut bool,
) {
    let count = match section {
        GoalReportSection::Changes => report.changes.len(),
        GoalReportSection::Evidence => report.evidence.len(),
        GoalReportSection::Unresolved => report.unresolved.len(),
    };
    if count >= GOAL_REPORT_ITEM_LIMIT - usize::from(section == GoalReportSection::Unresolved) {
        *omitted_facts = true;
        return;
    }
    let item = crate::tools::types::truncate_utf8(text, 512).to_owned();
    match section {
        GoalReportSection::Changes => report.changes.push(item),
        GoalReportSection::Evidence => report.evidence.push(item),
        GoalReportSection::Unresolved => report.unresolved.push(item),
    }
    if serde_json::to_vec(report).map_or(true, |bytes| {
        bytes.len() > crate::session::MAX_GOAL_REPORT_BYTES
    }) || section == GoalReportSection::Unresolved && !goal_report_fits_with_omission(report)
    {
        match section {
            GoalReportSection::Changes => report.changes.pop(),
            GoalReportSection::Evidence => report.evidence.pop(),
            GoalReportSection::Unresolved => report.unresolved.pop(),
        };
        *omitted_facts = true;
    }
}

fn append_goal_report_omission(report: &mut GoalReport, protected_unresolved_count: usize) {
    while !goal_report_fits_with_omission(report) {
        if report.unresolved.len() >= GOAL_REPORT_ITEM_LIMIT
            && report.unresolved.len() > protected_unresolved_count
        {
            report.unresolved.pop();
            continue;
        }
        if report.evidence.pop().is_some() || report.changes.pop().is_some() {
            continue;
        }
        if report.unresolved.len() > protected_unresolved_count {
            report.unresolved.pop();
            continue;
        }
        if !report.summary.is_empty() {
            let summary_bytes = report.summary.len().saturating_sub(128);
            report.summary =
                crate::tools::types::truncate_utf8(&report.summary, summary_bytes).to_owned();
            continue;
        }
        unreachable!("priority Goal report facts and omission notice fit their bounds");
    }
    report.unresolved.push(GOAL_REPORT_OMISSION.to_owned());
}

fn goal_report_fits_with_omission(report: &mut GoalReport) -> bool {
    if report.unresolved.len() >= GOAL_REPORT_ITEM_LIMIT {
        return false;
    }
    report.unresolved.push(GOAL_REPORT_OMISSION.to_owned());
    let fits = serde_json::to_vec(report)
        .is_ok_and(|bytes| bytes.len() <= crate::session::MAX_GOAL_REPORT_BYTES);
    report.unresolved.pop();
    fits
}

fn goal_report_call_label(call: &crate::tools::ToolCall) -> String {
    match call {
        crate::tools::ToolCall::PrepareWorker { .. } => "Worker data preparation".into(),
        crate::tools::ToolCall::ApplyChangeset {
            source: crate::session::worker::ChangesetSource::WorkerHandover { .. },
        } => "Worker changeset publication".into(),
        crate::tools::ToolCall::ParseAgentDefinition { .. } => {
            "Agent definition preparation".into()
        }
        crate::tools::ToolCall::Read { path, .. }
        | crate::tools::ToolCall::List { path }
        | crate::tools::ToolCall::Search { path, .. }
        | crate::tools::ToolCall::Mkdir { path }
        | crate::tools::ToolCall::Write { path, .. }
        | crate::tools::ToolCall::Edit { path, .. } => format!("{} {path}", call.label()),
        crate::tools::ToolCall::Command { program, .. } => format!("Command {program}"),
        crate::tools::ToolCall::CommandUnguarded { program, .. } => {
            format!("Unguarded command {program}")
        }
        crate::tools::ToolCall::ApplyChangeset { .. } => "Command changeset publication".into(),
        crate::tools::ToolCall::Skill { name, .. } => format!("Skill {name}"),
        crate::tools::ToolCall::McpList { server } => format!("MCP catalog {server}"),
        crate::tools::ToolCall::McpCall { server, tool, .. } => format!("MCP {server}.{tool}"),
        crate::tools::ToolCall::GitStatus { path } => format!("Git status {path}"),
        crate::tools::ToolCall::GitDiff { path, .. } => format!("Git diff {path}"),
        crate::tools::ToolCall::GitLog { path, .. } => format!("Git log {path}"),
    }
}

fn goal_report_evidence(
    call: &crate::tools::ToolCall,
    output: &str,
    attestation: Option<&crate::tools::Attestation>,
) -> String {
    let attester = match attestation {
        Some(crate::tools::Attestation::InProcess { .. }) => "The Engine",
        Some(crate::tools::Attestation::Unguarded(_)) => "An unguarded host run",
        _ => "Guard",
    };
    match call {
        crate::tools::ToolCall::PrepareWorker { .. } => "Worker data preparation".into(),
        crate::tools::ToolCall::ApplyChangeset {
            source: crate::session::worker::ChangesetSource::WorkerHandover { .. },
        } => "Worker changeset publication".into(),
        crate::tools::ToolCall::ParseAgentDefinition { .. } => {
            "Agent definition preparation".into()
        }
        crate::tools::ToolCall::Command { program, .. } => {
            serde_json::from_str::<crate::tools::types::CommandResult>(output)
                .ok()
                .filter(crate::tools::types::CommandResult::valid)
                .map_or_else(
                    || format!("Command {program} returned an attested result."),
                    |result| {
                        format!(
                            "Command {program} exited {}.",
                            result
                                .exit_code
                                .map_or_else(|| "without a code".into(), |code| code.to_string())
                        )
                    },
                )
        }
        crate::tools::ToolCall::ApplyChangeset {
            source: crate::session::worker::ChangesetSource::CommandReceipt { changeset, .. },
        } => {
            format!(
                "{attester} attested publication of {} command file change(s).",
                changeset.files.len()
            )
        }
        crate::tools::ToolCall::Read { path, .. } => {
            serde_json::from_str::<serde_json::Value>(output)
                .ok()
                .and_then(|value| {
                    value
                        .get("sha256")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .map_or_else(
                    || format!("{attester} attested a read of {path}."),
                    |digest| format!("{attester} attested a read of {path} (SHA-256 {digest})."),
                )
        }
        crate::tools::ToolCall::McpCall { server, tool, .. } => {
            format!("{attester} attested MCP call {server}.{tool}.")
        }
        _ => format!("{attester} attested {}.", goal_report_call_label(call)),
    }
}

fn push_definition_diagnostics(
    report: &mut GoalReport,
    run: &crate::session::RunView,
    omitted_facts: &mut bool,
) {
    let Some(config) = run.config.as_ref() else {
        return;
    };
    let Some(pin) = config.agent_definitions.as_ref() else {
        return;
    };
    let outputs: Vec<_> = run
        .tools
        .iter()
        .filter(|tool| {
            matches!(
                tool.intent.call,
                crate::tools::ToolCall::ParseAgentDefinition { .. }
            )
        })
        .filter_map(|tool| tool.observation.as_ref())
        .filter(|observation| observation.attested_for(&observation.intent))
        .map(|observation| observation.output.as_str())
        .collect();
    let child_model = config
        .providers
        .as_ref()
        .map_or(config.model.as_str(), |routes| {
            routes.child[0].model.as_str()
        });
    let Ok(registry) = crate::tools::definitions::registry(pin, outputs.into_iter(), child_model)
    else {
        return;
    };
    for note in registry.diagnostics {
        push_goal_report_item(
            report,
            GoalReportSection::Evidence,
            &format!("Agent definition {note}."),
            omitted_facts,
        );
    }
}

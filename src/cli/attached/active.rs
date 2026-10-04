use super::{Admission, controls::ActiveSubmission, emit_outcome, run};
use arany::{
    AttachedTerminal, CompactionFailure, CompactionRecord, CompactionStatus, Composer,
    ComposerEdit, RunCancellation, RunOutcome, RunProgress, RunRequest, SessionView,
    ShutdownSignal, TerminalError, TerminalInput,
};
use std::time::Duration;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

pub(super) enum RunSubmission {
    Completed(Box<RunOutcome>, bool),
    InterruptedBeforeRun,
    Shutdown(String),
}

pub(super) enum CompactionOutcome {
    Notice(String),
    Unavailable(String),
    Interrupted(String),
    Shutdown(ShutdownSignal),
}

pub(super) async fn submit_objective(
    terminal: &mut AttachedTerminal,
    admission: &Admission,
    view: &mut SessionView,
    composer: &mut Composer,
    objective: String,
) -> Result<RunSubmission, String> {
    let profile = view
        .defaults
        .provider
        .clone()
        .ok_or("select /provider and /model before submitting")?;
    let model = view
        .defaults
        .model
        .clone()
        .ok_or("select /model before submitting")?;
    let starting_sequence = view.last_sequence;
    let cancellation = RunCancellation::new();
    let mut progress = RunProgress::new();
    let request = RunRequest {
        session_id: Some(view.id),
        title: None,
        objective,
        images: composer.take_images(),
        workspace: admission.workspace.clone(),
        include_paths: admission.include_paths.clone(),
        policy: view.defaults.policy,
    };
    draw_active_snapshot(terminal, view, composer, false, false, None)
        .map_err(|error| error.to_string())?;
    let future = run::run_selected(
        &admission.state_dir,
        &admission.telemetry,
        run::Selection {
            workspace: &admission.workspace,
            profile: &profile,
            model: &model,
            effort: view.defaults.effort,
            account_id: view.defaults.account_id,
        },
        request,
        cancellation.clone(),
        progress.clone(),
        admission.tools,
    );
    tokio::pin!(future);
    let mut cancelling = false;
    let mut user_recorded = false;
    let shutdown_reason = loop {
        tokio::select! {
            result = &mut future => {
                if terminal.close_agents_at_run_end().is_err() {
                    return Ok(RunSubmission::Shutdown(
                        "terminal input cleanup failed; Run may have committed; check Session history".into(),
                    ));
                }
                if result.is_err()
                    && terminal.restore_draft_input(composer.text().len()).is_err()
                {
                    return Ok(RunSubmission::Shutdown("terminal input cleanup failed".into()));
                }
                return match result {
                    Ok(outcome) => Ok(RunSubmission::Completed(Box::new(outcome), user_recorded)),
                    Err(run::RunAttemptError::InterruptedBeforeRun) => Ok(RunSubmission::InterruptedBeforeRun),
                    Err(run::RunAttemptError::Failed(error)) => Err(error),
                };
            },
            update = progress.changed() => {
                if update.sequence <= view.last_sequence {
                    continue;
                }
                if !user_recorded {
                    if terminal.record_user_run(&update.run).is_err() {
                        break "terminal rendering failed".to_owned();
                    }
                    user_recorded = true;
                }
                if let Some(last) = view.runs.last_mut()
                    && last.id == update.run.id
                {
                    *last = update.run;
                } else {
                    view.runs.push(update.run);
                }
                view.last_sequence = update.sequence;
                if draw_active_snapshot(terminal, view, composer, user_recorded, cancelling, None).is_err() {
                    break "terminal rendering failed".to_owned();
                }
            }
            input = terminal.next_input() => {
                let input = match input {
                    Ok(input) => input,
                    Err(_) => break "terminal input failed".to_owned(),
                };
                if terminal.help_open() {
                    match input {
                        TerminalInput::Interrupt => terminal.close_help(),
                        TerminalInput::Suspend | TerminalInput::Shutdown(_) => {}
                        _ => {
                            terminal.help_input(input);
                            if draw_active_snapshot(terminal, view, composer, user_recorded, cancelling, None).is_err() {
                                break "terminal rendering failed".to_owned();
                            }
                            continue;
                        }
                    }
                }
                if terminal.agents_open()
                    && !matches!(input, TerminalInput::Interrupt | TerminalInput::Suspend | TerminalInput::Shutdown(_))
                {
                    if terminal.agent_input(view, input).is_err() {
                        break "terminal rendering failed".to_owned();
                    }
                    if !terminal.agents_open()
                        && draw_active_snapshot(terminal, view, composer, user_recorded, cancelling, None).is_err()
                    {
                        break "terminal rendering failed".to_owned();
                    }
                    continue;
                }
                if terminal.agents_open()
                    && matches!(input, TerminalInput::Interrupt | TerminalInput::Shutdown(_))
                    && terminal.close_agents().is_err()
                {
                    break "terminal input cleanup failed".to_owned();
                }
                if terminal.handle_history_input(input, view) {
                    if draw_active_snapshot(terminal, view, composer, user_recorded, cancelling, None).is_err() {
                        break "terminal rendering failed".to_owned();
                    }
                    continue;
                }
                let notice = match input {
                    TerminalInput::ClipboardPaste => Some(match terminal.request_clipboard_paste() {
                        Ok(()) => "Reading clipboard... draft retained".to_owned(),
                        Err(error) => format!("Error: {error}; draft unchanged"),
                    }),
                    TerminalInput::Paste => match terminal.paste_into(composer) {
                        Err(error) => Some(format!("Error: {error}; draft unchanged")),
                        Ok(ComposerEdit::Changed) => {
                            if draw_active_snapshot(terminal, view, composer, user_recorded, cancelling, None).is_err() {
                                break "terminal rendering failed".to_owned();
                            }
                            None
                        }
                        Ok(_) => None,
                    },
                    input @ (TerminalInput::Character(_)
                    | TerminalInput::Newline
                    | TerminalInput::Backspace
                    | TerminalInput::Delete
                    | TerminalInput::Left
                    | TerminalInput::Right
                    | TerminalInput::WordLeft
                    | TerminalInput::WordRight
                    | TerminalInput::BackspaceWord
                    | TerminalInput::Up
                    | TerminalInput::Down
                    | TerminalInput::Home
                    | TerminalInput::End
                    | TerminalInput::Tab
                    | TerminalInput::Escape) => match composer.apply(input) {
                        ComposerEdit::AtCapacity => Some("Error: input exceeds 8 KiB; draft unchanged".to_owned()),
                        ComposerEdit::Changed if !terminal.is_linear() => {
                            if draw_active_snapshot(terminal, view, composer, user_recorded, cancelling, None).is_err() {
                                break "terminal rendering failed".to_owned();
                            }
                            None
                        }
                        ComposerEdit::Unchanged => None,
                        ComposerEdit::Changed => None,
                    },
                    TerminalInput::LineRejected => {
                        Some("Error: invalid or overlong terminal line; draft unchanged".to_owned())
                    }
                    TerminalInput::LineContinued => {
                        Some(format!("Draft: {} of 8192 bytes; Enter retains until Run ends", composer.text().len()))
                    }
                    TerminalInput::QuickActions => {
                        Some("Quick actions are unavailable during a Run; /agents opens details".to_owned())
                    }
                    TerminalInput::Submit if terminal.clipboard_loading() => {
                        if terminal.restore_draft_input(composer.text().len()).is_err() {
                            break "terminal input failed".to_owned();
                        }
                        Some("Clipboard loading; press Enter after paste to submit".to_owned())
                    }
                    TerminalInput::Submit if !terminal.is_linear()
                        && composer.apply(TerminalInput::Submit) == ComposerEdit::Changed => {
                        if draw_active_snapshot(terminal, view, composer, user_recorded, cancelling, None).is_err() {
                            break "terminal rendering failed".to_owned();
                        }
                        None
                    }
                    TerminalInput::Submit => {
                        match super::controls::handle_active_submission(composer, view, starting_sequence) {
                            ActiveSubmission::RetainedDraft => {
                                if terminal.restore_draft_input(composer.text().len()).is_err() {
                                    break "terminal input failed".to_owned();
                                }
                                Some("Draft retained; press Enter after this Run to submit".to_owned())
                            }
                            ActiveSubmission::Rejected(notice) => {
                                if terminal.is_linear() {
                                    composer.take();
                                }
                                Some(format!("Error: {notice}"))
                            }
                            ActiveSubmission::Notice(notice) => Some(notice),
                            ActiveSubmission::ReadClipboard => Some(match terminal.request_clipboard_paste() {
                                Ok(()) => "Reading clipboard... draft retained".to_owned(),
                                Err(error) => format!("Error: {error}; draft unchanged"),
                            }),
                            ActiveSubmission::OpenAgents => {
                                if terminal.open_agents(view, true).is_err() {
                                    break "terminal rendering failed".to_owned();
                                }
                                None
                            }
                            ActiveSubmission::OpenHelp => {
                                if terminal.open_help().is_err() {
                                    break "terminal rendering failed".to_owned();
                                }
                                None
                            }
                        }
                    }
                    TerminalInput::Interrupt => {
                        if cancelling {
                            break "forced shutdown after second Ctrl+C".to_owned();
                        }
                        cancelling = true;
                        cancellation.cancel();
                        None
                    }
                    TerminalInput::Suspend => {
                        if terminal.suspend_and_resume(composer.text().len()).is_err() {
                            break "terminal resume failed".to_owned();
                        }
                        if draw_active_snapshot(terminal, view, composer, user_recorded, cancelling, None).is_err() {
                            break "terminal rendering failed".to_owned();
                        }
                        None
                    }
                    TerminalInput::Shutdown(signal) => {
                        break format!("terminated by {}", signal.name());
                    }
                    TerminalInput::Resize => {
                        if draw_active_snapshot(terminal, view, composer, user_recorded, cancelling, None).is_err() {
                            break "terminal rendering failed".to_owned();
                        }
                        None
                    }
                    TerminalInput::EndOfInput if !composer.is_empty() => {
                        Some("Draft retained; press Enter after this Run to submit".to_owned())
                    }
                    _ => None,
                };
                if (notice.is_some() || matches!(input, TerminalInput::Interrupt))
                    && draw_active_snapshot(terminal, view, composer, user_recorded, cancelling, notice.as_deref()).is_err()
                {
                    break "terminal rendering failed".to_owned();
                }
            }
        }
    };
    cancellation.cancel();
    let restored = terminal.restore();
    let mut reason = shutdown_reason;
    match tokio::time::timeout(SHUTDOWN_GRACE, future.as_mut()).await {
        Ok(Ok(outcome)) if restored.is_ok() => {
            if emit_outcome(&admission.state_dir, &outcome, user_recorded)
                .await
                .is_err()
            {
                reason.push_str("; committed receipt unavailable; check Session history");
            }
        }
        Ok(Ok(_)) => reason.push_str("; Run committed; check Session history"),
        Ok(Err(run::RunAttemptError::InterruptedBeforeRun)) => {
            reason
                .push_str("; no Run started for this submission; account work may still complete");
        }
        Ok(Err(_)) => reason.push_str("; Run outcome unavailable; check Session history"),
        Err(_) => reason.push_str("; Run may be interrupted; check Session history"),
    }
    if restored.is_err() {
        reason.push_str("; terminal restoration failed");
    }
    Ok(RunSubmission::Shutdown(reason))
}

fn draw_active_snapshot(
    terminal: &mut AttachedTerminal,
    view: &SessionView,
    composer: &Composer,
    current_run_started: bool,
    cancelling: bool,
    notice: Option<&str>,
) -> Result<(), TerminalError> {
    if !current_run_started {
        terminal.draw_preparing(
            view,
            composer,
            notice.or(cancelling.then_some("Stopping request...")),
        )
    } else if cancelling {
        terminal.draw_progress_with_draft(view, composer, notice.unwrap_or("Cancelling Run..."))
    } else if terminal.is_linear() {
        terminal.draw_progress_with_draft(
            view,
            composer,
            notice.unwrap_or("Run in progress... Ctrl+C cancels"),
        )
    } else {
        terminal.draw(view, composer, notice)
    }
}

pub(super) async fn compact_current(
    terminal: &mut AttachedTerminal,
    admission: &Admission,
    view: &SessionView,
    composer: &mut Composer,
    pinned: Option<(&arany::RunConfig, arany::RunId)>,
) -> Result<CompactionOutcome, String> {
    let (profile, model, effort, account_id) = if let Some((config, _)) = pinned {
        (
            config.provider.as_str(),
            config.model.as_str(),
            config.effort,
            config.saved_api_account_id.or_else(|| {
                config
                    .chatgpt_provenance
                    .as_ref()
                    .map(|provenance| provenance.account_id)
            }),
        )
    } else {
        let Some(profile) = view.defaults.provider.as_deref() else {
            return Ok(CompactionOutcome::Unavailable(
                "select /provider and /model before compacting".into(),
            ));
        };
        let Some(model) = view.defaults.model.as_deref() else {
            return Ok(CompactionOutcome::Unavailable(
                "select /model before compacting".into(),
            ));
        };
        (
            profile,
            model,
            view.defaults.effort,
            view.defaults.account_id,
        )
    };
    let selection = run::Selection {
        workspace: &admission.workspace,
        profile,
        model,
        effort,
        account_id,
    };
    let progress_text = if pinned.is_some() {
        "Automatically compacting Session... Ctrl+C interrupts"
    } else {
        "Compacting Session... Ctrl+C interrupts"
    };
    terminal
        .draw_busy(view, composer, Some(progress_text))
        .map_err(|error| error.to_string())?;
    let future = run::compact_selected(
        &admission.state_dir,
        &admission.telemetry,
        selection,
        view.id,
        &admission.workspace,
        pinned.map(|(_, run_id)| run_id),
    );
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return Ok(match result {
                Ok(record) => CompactionOutcome::Notice(record.as_ref().map_or_else(
                    || "Automatic compaction skipped; Run boundary changed or already compacted".into(),
                    compaction_notice,
                )),
                Err(error) => CompactionOutcome::Unavailable(error),
            }),
            input = terminal.next_input() => {
                let input = input.map_err(|error| error.to_string())?;
                if terminal.handle_history_input(input, view) {
                    terminal.draw_busy(view, composer, None)
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                let notice = match input {
                    TerminalInput::ClipboardPaste => Some(match terminal.request_clipboard_paste() {
                        Ok(()) => "Reading clipboard... draft retained".to_owned(),
                        Err(error) => format!("Error: {error}; draft unchanged"),
                    }),
                    TerminalInput::Paste => match terminal.paste_into(composer) {
                        Err(error) => Some(format!("Error: {error}; draft unchanged")),
                        Ok(ComposerEdit::Changed) => {
                            terminal.draw_busy(view, composer, None)
                                .map_err(|error| error.to_string())?;
                            None
                        }
                        Ok(_) => None,
                    },
                    TerminalInput::Interrupt => {
                        return Ok(CompactionOutcome::Interrupted(
                            "Compaction interrupted; check Session history".into(),
                        ));
                    }
                    TerminalInput::Resize => {
                        terminal.draw_busy(view, composer, None)
                            .map_err(|error| error.to_string())?;
                        None
                    }
                    TerminalInput::Suspend => {
                        terminal.suspend_and_resume(composer.text().len())
                            .map_err(|error| error.to_string())?;
                        terminal.draw_busy(view, composer, None)
                            .map_err(|error| error.to_string())?;
                        None
                    }
                    TerminalInput::Shutdown(signal) => {
                        return Ok(CompactionOutcome::Shutdown(signal));
                    }
                    TerminalInput::Submit | TerminalInput::EndOfInput => {
                        terminal.restore_draft_input(composer.text().len())
                            .map_err(|error| error.to_string())?;
                        Some("Draft retained; press Enter after compaction to submit".to_owned())
                    }
                    TerminalInput::LineRejected => Some(
                        "Error: invalid or overlong terminal line; draft unchanged".to_owned(),
                    ),
                    TerminalInput::LineContinued => Some(format!(
                        "Draft: {} of 8192 bytes; Enter retains until compaction ends",
                        composer.text().len(),
                    )),
                    TerminalInput::QuickActions => Some(
                        "Quick actions are unavailable during compaction; draft retained".to_owned(),
                    ),
                    input => match composer.apply(input) {
                        ComposerEdit::AtCapacity => Some(
                            "Error: input exceeds 8 KiB; draft unchanged".to_owned(),
                        ),
                        ComposerEdit::Changed if !terminal.is_linear() => Some(progress_text.to_owned()),
                        ComposerEdit::Changed | ComposerEdit::Unchanged => None,
                    },
                };
                if let Some(notice) = notice {
                    terminal.draw_busy(view, composer, Some(&notice))
                        .map_err(|error| error.to_string())?;
                }
            }
        }
    }
}

fn compaction_notice(record: &CompactionRecord) -> String {
    let usage = match (record.input_tokens, record.output_tokens) {
        (Some(input), Some(output)) => format!("{input} input / {output} output tokens"),
        _ => "usage unavailable".into(),
    };
    let bound = if record.output_token_bound == arany::OutputTokenBound::LocalAcceptanceOnly {
        "; remote usage not capped"
    } else {
        ""
    };
    match &record.status {
        CompactionStatus::Succeeded { .. } => format!("Compaction saved; {usage}{bound}"),
        CompactionStatus::Failed { reason } => {
            let reason = match reason {
                CompactionFailure::ProviderUnavailable => "Provider unavailable",
                CompactionFailure::ProviderRejected => "Provider rejected",
                CompactionFailure::InvalidOutcome => "invalid Provider outcome",
                CompactionFailure::TimedOut => "timed out",
            };
            format!("Error: Compaction failed: {reason}; {usage}{bound}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_notice_reports_committed_status_without_exposing_summary() {
        let summary = "\u{1b}[31muntrusted summary";
        let summary_bytes = summary.len() as u32;
        let mut record = CompactionRecord {
            covered_run_id: arany::RunId::new(),
            covered_sequence: 1,
            source_digest: [0; 32],
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            prompt_version: 1,
            schema_version: 1,
            compiler_version: 1,
            source_bytes: 4,
            source_tokens_estimate: 1,
            input_tokens: Some(12),
            output_tokens: Some(3),
            response_id: None,
            wire_provenance: None,
            chatgpt_provenance: None,
            output_token_bound: arany::OutputTokenBound::ProviderEnforced,
            status: CompactionStatus::Succeeded {
                summary: summary.into(),
                content_digest: [0; 32],
                summary_bytes,
                summary_tokens_estimate: summary_bytes.div_ceil(4),
            },
        };
        assert_eq!(
            compaction_notice(&record),
            "Compaction saved; 12 input / 3 output tokens"
        );
        record.input_tokens = None;
        record.output_tokens = None;
        record.status = CompactionStatus::Failed {
            reason: CompactionFailure::ProviderRejected,
        };
        assert_eq!(
            compaction_notice(&record),
            "Error: Compaction failed: Provider rejected; usage unavailable"
        );
        record.output_token_bound = arany::OutputTokenBound::LocalAcceptanceOnly;
        assert_eq!(
            compaction_notice(&record),
            "Error: Compaction failed: Provider rejected; usage unavailable; remote usage not capped"
        );
        for (reason, label) in [
            (
                CompactionFailure::ProviderUnavailable,
                "Provider unavailable",
            ),
            (CompactionFailure::ProviderRejected, "Provider rejected"),
            (
                CompactionFailure::InvalidOutcome,
                "invalid Provider outcome",
            ),
            (CompactionFailure::TimedOut, "timed out"),
        ] {
            record.status = CompactionStatus::Failed { reason };
            assert_eq!(
                compaction_notice(&record),
                format!(
                    "Error: Compaction failed: {label}; usage unavailable; remote usage not capped"
                )
            );
        }
    }
}

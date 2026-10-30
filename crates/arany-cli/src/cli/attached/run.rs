use super::{Admission, Feedback, emit_outcome, load_confirmed_outcome};
use crate::cli::provider::select::{Selection, select_provider};
use crate::cli::run_outcome::{SHUTDOWN_GRACE, stop_after_answer, stop_text};
use crate::cli::serve::embedded::{self, Controls, classify_run_error};
pub(in crate::cli) use crate::cli::serve::embedded::{RunAttemptError, RunFailure};
use crate::terminal::{
    AgentChatFollow, AttachedTerminal, Composer, ComposerDraft, TerminalError, TerminalInput,
    TerminalNotice,
};
use arany::{
    CompactionRecord, Engine, RunId, RunOutcome, RunRequest, RunResult, SessionId, SessionView,
};
use arany_cli::protocol::{methods::*, rpc::*};
use std::path::Path;

mod approval;
mod input;

#[cfg(test)]
#[path = "run/tests.rs"]
mod focused_tests;

use approval::{ApprovalPage, ApprovalUi};
use input::{InputFailure, InputRoute, STEERING_SENT, settle_recorded_note};

pub(super) async fn preflight_selected(
    state_dir: &Path,
    selection: Selection<'_>,
) -> Result<(), String> {
    select_provider(state_dir, selection)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

pub(super) fn finish_attempt(
    result: Result<RunResult, arany::EngineError>,
    pause_requested: bool,
) -> Result<RunOutcome, RunAttemptError> {
    let RunResult {
        run,
        stopped,
        continuation_error,
    } = result.map_err(classify_run_error)?;
    // A Run that could not settle has no committed outcome to show, only its projected stop.
    if !run.run.status.is_outcome() {
        return Err(RunAttemptError::Failed(RunFailure::Unsettled(
            stopped.map_or("Run did not reach a terminal state", stop_text),
        )));
    }
    match stop_after_answer(
        run.run.status,
        stopped,
        continuation_error.as_ref(),
        pause_requested,
    ) {
        Some(reason) => Err(RunAttemptError::Stopped {
            outcome: Box::new(run),
            reason,
        }),
        None => Ok(run),
    }
}

pub(super) async fn compact_selected(
    engine: &Engine,
    state_dir: &Path,
    selection: Selection<'_>,
    session_id: SessionId,
    workspace: &Path,
    expected_run_id: Option<RunId>,
    pinned_roster: Option<&arany::RunConfig>,
) -> Result<Option<CompactionRecord>, String> {
    engine
        .readmit_state()
        .map_err(|_| "state directory unavailable or unsafe")?;
    let providers = if let Some(config) = pinned_roster {
        tokio::time::timeout(
            std::time::Duration::from_secs(120),
            crate::cli::provider::roster::pinned_compaction(state_dir, workspace, config),
        )
        .await
        .map_err(|_| "compaction admission exceeded 120 seconds")??
    } else {
        let provider = select_provider(state_dir, selection)
            .await
            .map_err(|error| error.to_string())?;
        engine
            .readmit_state()
            .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
        crate::cli::provider::routing::configured_providers(state_dir, workspace, provider)
            .await
            .map_err(|error| error.to_string())?
    };
    match expected_run_id {
        Some(expected_run_id) => {
            engine
                .auto_compact_session(
                    &providers,
                    session_id,
                    workspace.to_path_buf(),
                    expected_run_id,
                )
                .await
        }
        None => engine
            .compact_session(&providers, session_id, workspace.to_path_buf())
            .await
            .map(Some),
    }
    .map_err(|error| error.to_string())
}

pub(super) enum RunSubmission {
    Completed(Box<RunOutcome>, bool, Option<String>),
    InterruptedBeforeRun,
    Shutdown(String),
}

fn draft_retained_notice(autonomous: bool) -> &'static str {
    if autonomous {
        "Draft retained; press Enter after this Goal to submit"
    } else {
        "Draft retained; press Enter after this Run to submit"
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ActiveStop {
    Running,
    Pausing,
    Cancelling,
}

fn run_settled(view: &SessionView, starting_sequence: u64) -> bool {
    view.runs.last().is_some_and(|run| {
        run.accepted_sequence > starting_sequence
            && !matches!(
                run.status,
                arany::RunStatus::Pending | arany::RunStatus::Active
            )
    })
}

// Once the current Run's outcome is committed, /exit waits for the Run to return, as Ctrl+D does.
fn exits_after_settled_run(
    composer: &Composer,
    view: &SessionView,
    starting_sequence: u64,
) -> bool {
    matches!(
        composer.submission(),
        Ok(crate::terminal::Submission::Command {
            command: crate::terminal::InteractiveCommand::Exit,
            argument: None,
        })
    ) && run_settled(view, starting_sequence)
}

// /exit drops held drafts and notes so the deferred Ctrl+D exits on an empty composer.
async fn abandon_drafts_for_exit(
    composer: &mut Composer,
    held: &mut Option<HeldDraft>,
    controls: &Controls,
) {
    composer.take();
    composer.take_images();
    *held = None;
    let _ = controls.withdraw_note().await;
}

fn active_escape(
    input: TerminalInput,
    menu_open: bool,
    clipboard_loading: bool,
    stop: ActiveStop,
) -> TerminalInput {
    match input {
        TerminalInput::Escape
            if !clipboard_loading && !menu_open && stop != ActiveStop::Cancelling =>
        {
            TerminalInput::Interrupt
        }
        input => input,
    }
}

fn run_end_notice(sent: bool, note_returned: bool, restored: bool) -> Option<&'static str> {
    if !restored {
        return Some(
            "Drafts exceed the combined limit; both kept; submit or clear this draft, then press Up",
        );
    }
    if sent {
        Some(STEERING_SENT)
    } else if note_returned {
        Some("Steering note withdrawn; draft retained")
    } else {
        None
    }
}

type HeldDraft = ComposerDraft;

fn hold_draft(composer: &mut Composer) -> Option<HeldDraft> {
    (!composer.is_empty()).then(|| composer.take_draft())
}

fn release_held_draft(composer: &mut Composer, draft: HeldDraft) -> bool {
    composer.restore_draft(draft)
}

pub(super) struct PreparedSubmission<'a> {
    pub(super) engine: &'a std::sync::Arc<arany::Engine>,
    pub(super) profile: String,
    pub(super) model: String,
    pub(super) roster: Option<crate::cli::provider::roster::SelectedRoster>,
    pub(super) manifest: Option<String>,
    pub(super) request: RunRequest,
    pub(super) progress: RunProgress<'a>,
    pub(super) inbox: embedded::Bootstrap,
}

pub(super) struct RunUi<'a> {
    pub(super) terminal: &'a mut AttachedTerminal,
    pub(super) view: &'a mut SessionView,
    pub(super) composer: &'a mut Composer,
}

#[derive(Default)]
struct Drafts {
    offered: Option<String>,
    linear_offers: Vec<ComposerDraft>,
    held: Option<HeldDraft>,
}

pub(super) struct RunProgress<'a> {
    admission: &'a Admission,
    starting_sequence: u64,
    autonomous: bool,
    submitted_goal: Option<arany::GoalId>,
    controls: Controls,
    approval_ui: ApprovalUi,
    stop: ActiveStop,
    user_recorded: bool,
    drafts: Drafts,
    seen_delivered: u64,
    shown_retries: (Option<arany::RunId>, usize),
    exit_after_run: bool,
}

enum Iteration {
    Armed(ApprovalPage),
    Redraw,
}

enum Completion {
    Finished(Result<RunSubmission, String>),
    Shutdown(String),
}

impl<'a> RunProgress<'a> {
    pub(super) fn new(
        admission: &'a Admission,
        ui: &RunUi<'_>,
        starting_sequence: u64,
        autonomous: bool,
        submitted_goal: Option<arany::GoalId>,
    ) -> (Self, embedded::Bootstrap) {
        let (controls, inbox) = Controls::new(
            ui.composer.approval_mode().unwrap_or_default(),
            !ui.terminal.is_linear(),
        );
        (
            Self {
                admission,
                starting_sequence,
                autonomous,
                submitted_goal,
                controls,
                approval_ui: ApprovalUi::default(),
                stop: ActiveStop::Running,
                user_recorded: false,
                drafts: Drafts::default(),
                seen_delivered: 0,
                shown_retries: (None, 0),
                exit_after_run: false,
            },
            inbox,
        )
    }

    fn draw(
        &self,
        ui: &mut RunUi<'_>,
        notice: Option<TerminalNotice<'_>>,
    ) -> Result<(), TerminalError> {
        ui.draw(self.user_recorded, self.stop, notice)
    }

    async fn prepare_iteration(&mut self, ui: &mut RunUi<'_>) -> Result<Iteration, String> {
        if self.approval_ui.pending.is_none()
            && ui
                .terminal
                .restore_selected_draft_input(ui.composer.input_capacity())
                .is_err()
        {
            return Err("terminal input cleanup failed".to_owned());
        }
        let approval_more = match &self.approval_ui.pending {
            Some(approval) => match ui.terminal.draw_tool_approval(
                &approval.prompt.for_mode(ui.composer.approval_mode()),
                ui.view,
                self.approval_ui.page,
                (!self.approval_ui.unarmed).then_some(self.approval_ui.choice),
                self.approval_ui.review_notice,
            ) {
                Ok(more) => more,
                Err(_) => return Err("terminal rendering failed".to_owned()),
            },
            None => false,
        };
        // Only bytes the reader reads after this barrier reach the armed redraw.
        if std::mem::take(&mut self.approval_ui.unarmed) {
            if ui.terminal.discard_approval_typeahead().await.is_err() {
                return Err("terminal input admission failed".to_owned());
            }
            return Ok(Iteration::Redraw);
        }
        Ok(Iteration::Armed(if approval_more {
            ApprovalPage::More
        } else {
            ApprovalPage::Last
        }))
    }

    async fn settle(
        &mut self,
        ui: &mut RunUi<'_>,
        result: Result<Box<RunOutcome>, RunAttemptError>,
    ) -> Completion {
        let caught_up = !ui.terminal.child_notes_pending()
            || match self.controls.view(ui.view.id).await {
                Ok(view) => {
                    *ui.view = view;
                    true
                }
                Err(_) => false,
            };
        ui.terminal
            .settle_child_notes(ui.view, self.controls.child_withdrawal(), true)
            .await;
        let child_follow = if !caught_up {
            false
        } else {
            match ui.terminal.follow_agent_chat(ui.view).await {
                Ok(follow) => follow != AgentChatFollow::Unchanged,
                Err(_) => return Completion::Shutdown("terminal input cleanup failed".into()),
            }
        };
        ui.terminal.set_waiting_tool(None);
        ui.terminal.set_preview(None);
        ui.composer.set_file_candidates(None, Vec::new());
        if ui.terminal.close_agents_at_run_end().is_err() {
            return Completion::Finished(Ok(RunSubmission::Shutdown(
                "terminal input cleanup failed; Run may have committed; check Session history"
                    .into(),
            )));
        }
        if let Err(reason) = self.restore_drafts(ui, result.is_err(), child_follow).await {
            return Completion::Finished(Ok(RunSubmission::Shutdown(reason)));
        }
        Completion::Finished(self.submission_result(ui, result).await)
    }

    async fn restore_drafts(
        &mut self,
        ui: &mut RunUi<'_>,
        failed: bool,
        child_follow: bool,
    ) -> Result<(), String> {
        // An undelivered note ends the Run as an ordinary retained draft.
        let had_offer = self.drafts.offered.is_some();
        let sent = settle_recorded_note(
            self.controls.delivered_notes().await?,
            &mut self.seen_delivered,
            &mut self.drafts.offered,
            ui.composer,
        );
        let had_held = self.drafts.held.is_some();
        // The undelivered note predates the held draft, so restore it ahead of that draft.
        let note = self
            .controls
            .withdraw_note()
            .await?
            .filter(|_| ui.terminal.is_linear());
        let withdrawn = note.is_some();
        let mut restored = self
            .drafts
            .held
            .take()
            .is_none_or(|draft| release_held_draft(ui.composer, draft));
        if let Some(note) = note {
            restored &= ui.composer.restore_draft(ComposerDraft::from_steering_note(
                note,
                &self.drafts.linear_offers,
            ));
        }
        if (failed || had_offer || sent || had_held)
            && ui
                .terminal
                .restore_draft_input(ui.composer.input_capacity())
                .is_err()
        {
            return Err("terminal input cleanup failed".into());
        }
        let notice = run_end_notice(sent, withdrawn, restored);
        if (child_follow || notice.is_some())
            && self.draw(ui, notice.map(TerminalNotice::History)).is_err()
        {
            return Err("terminal rendering failed".into());
        }
        Ok(())
    }

    async fn submission_result(
        &self,
        ui: &RunUi<'_>,
        result: Result<Box<RunOutcome>, RunAttemptError>,
    ) -> Result<RunSubmission, String> {
        match result {
            Ok(outcome) => Ok(RunSubmission::Completed(outcome, self.user_recorded, None)),
            Err(RunAttemptError::Stopped { outcome, reason }) => Ok(RunSubmission::Completed(
                outcome,
                self.user_recorded,
                Some(reason),
            )),
            Err(RunAttemptError::InterruptedBeforeRun) => Ok(RunSubmission::InterruptedBeforeRun),
            Err(RunAttemptError::Failed(RunFailure::Engine(
                arany::EngineError::GoalBudgetExhausted,
            ))) => {
                let reloaded = self.controls.view(ui.view.id).await?;
                let feedback =
                    crate::presentation::budget_refusal_feedback(&reloaded, self.submitted_goal);
                Err(feedback
                    .unwrap_or_else(|| crate::presentation::BUDGET_REFUSAL_UNCOMMITTED.into()))
            }
            Err(RunAttemptError::Failed(error)) => Err(error.to_string()),
        }
    }

    fn retry_notice(&mut self, ui: &RunUi<'_>) -> Option<String> {
        ui.view
            .runs
            .last()
            .filter(|run| run.accepted_sequence > self.starting_sequence)
            .and_then(|run| {
                let shown = if self.shown_retries.0 == Some(run.id) {
                    self.shown_retries.1
                } else {
                    0
                };
                self.shown_retries = (Some(run.id), run.provider_retries.len());
                run.provider_retries.get(shown..)?.last()
            })
            .map(|(_, retry)| crate::presentation::escape_terminal(&retry.notice()))
    }

    async fn on_events(&mut self, ui: &mut RunUi<'_>) -> Result<(), String> {
        if !self.user_recorded
            && let Some(run) = ui
                .view
                .runs
                .last()
                .filter(|run| run.accepted_sequence > self.starting_sequence)
        {
            if ui.terminal.record_user_run(run).is_err() {
                return Err("terminal rendering failed".to_owned());
            }
            self.user_recorded = true;
        }
        let sent = settle_recorded_note(
            self.controls.delivered_notes().await?,
            &mut self.seen_delivered,
            &mut self.drafts.offered,
            ui.composer,
        );
        if sent
            && ui
                .terminal
                .restore_draft_input(ui.composer.input_capacity())
                .is_err()
        {
            return Err("terminal input failed".to_owned());
        }
        let retry = self.retry_notice(ui);
        let notice = if sent {
            Some(TerminalNotice::History(STEERING_SENT))
        } else {
            retry.as_deref().map(TerminalNotice::Transient)
        };
        let child_changed = ui
            .terminal
            .settle_child_notes(
                ui.view,
                self.controls.child_withdrawal(),
                self.stop != ActiveStop::Running,
            )
            .await;
        if child_changed
            && ui
                .terminal
                .restore_selected_draft_input(ui.composer.input_capacity())
                .is_err()
        {
            return Err("terminal input failed".to_owned());
        }
        let follow = match ui.terminal.follow_agent_chat(ui.view).await {
            Ok(follow) => follow,
            Err(_) => return Err("terminal rendering failed".to_owned()),
        };
        let notice = match follow {
            AgentChatFollow::Closed(text) => Some(TerminalNotice::History(text)),
            AgentChatFollow::Unchanged | AgentChatFollow::Redraw => notice,
        };
        if self.approval_ui.pending.is_none()
            && (child_changed || follow != AgentChatFollow::Unchanged || notice.is_some())
            && self.draw(ui, notice).is_err()
        {
            return Err("terminal rendering failed".to_owned());
        }
        Ok(())
    }

    fn draw_hint(&self, ui: &mut RunUi<'_>) -> Result<(), String> {
        if self.approval_ui.pending.is_none() && !ui.terminal.agent_chat_open() {
            self.draw(ui, None)
                .map_err(|_| "terminal rendering failed".to_owned())?;
        }
        Ok(())
    }

    async fn control_input(
        &mut self,
        ui: &mut RunUi<'_>,
        input: TerminalInput,
    ) -> Result<Option<Feedback>, InputFailure> {
        if input == TerminalInput::Interrupt {
            if self.stop == ActiveStop::Cancelling {
                return Err(InputFailure::Shutdown(
                    "forced shutdown after second Ctrl+C".to_owned(),
                ));
            }
            self.stop = ActiveStop::Cancelling;
            ui.terminal
                .settle_child_notes(ui.view, self.controls.child_withdrawal(), true)
                .await;
            self.controls.cancel().await?;
        }
        if input == TerminalInput::Suspend {
            ui.terminal
                .suspend_and_resume(ui.composer.input_capacity())
                .map_err(|_| InputFailure::Shutdown("terminal resume failed".to_owned()))?;
            self.draw(ui, None)
                .map_err(|_| InputFailure::Shutdown("terminal rendering failed".to_owned()))?;
        }
        if let TerminalInput::Shutdown(signal) = input {
            return Err(InputFailure::Shutdown(format!(
                "terminated by {}",
                signal.name()
            )));
        }
        if input == TerminalInput::Resize {
            self.draw(ui, None)
                .map_err(|_| InputFailure::Shutdown("terminal rendering failed".to_owned()))?;
        }
        if input == TerminalInput::EndOfInput {
            if !ui.composer.is_empty() || self.drafts.held.is_some() {
                return Ok(Some(Feedback::Transient(
                    draft_retained_notice(self.autonomous).into(),
                )));
            }
            // The screen already shows the outcome, so exit once the Run returns.
            if run_settled(ui.view, self.starting_sequence) {
                ui.terminal.defer_input(TerminalInput::EndOfInput);
                self.exit_after_run = true;
            }
        }
        Ok(None)
    }
}

enum RunEvent {
    Server(Result<embedded::Event, String>),
    Input(Result<TerminalInput, TerminalError>),
}

pub(super) async fn run_attached(
    prepared: PreparedSubmission<'_>,
    ui: &mut RunUi<'_>,
) -> Result<RunSubmission, String> {
    let PreparedSubmission {
        engine,
        profile,
        model,
        roster,
        manifest,
        request,
        mut progress,
        inbox,
    } = prepared;
    progress.draw(ui, None).map_err(|error| error.to_string())?;
    if let Some(manifest) = &manifest {
        ui.terminal
            .draw_busy_notice(
                ui.view,
                ui.composer,
                Some(TerminalNotice::History(manifest)),
            )
            .map_err(|error| error.to_string())?;
    }
    let admission = embedded::Admission {
        state: progress.admission.state_dir.clone(),
        profile,
        model,
        effort: ui.view.defaults.effort,
        account_id: ui.view.defaults.account_id,
        request,
        configured_tools: progress.admission.tools,
        permissions: progress.admission.workspace_permissions.clone(),
        roster,
    };
    let admitted_engine = engine.clone();
    let mut connection = embedded::Connection::start(
        engine.clone(),
        admission.state.clone(),
        progress.controls.clone(),
        inbox,
        move |controls, inbox| admission.admit(admitted_engine, controls, inbox),
    );
    let completion = run_client(&mut progress, ui, &mut connection).await;
    match completion {
        Completion::Finished(result) => match connection.close().await {
            Ok(()) => result,
            Err(mut reason) => {
                if matches!(result, Ok(RunSubmission::Completed(..))) {
                    reason.push_str("; Run committed; check Session history");
                }
                if ui.terminal.restore().is_err() {
                    reason.push_str("; terminal restoration failed");
                }
                Ok(RunSubmission::Shutdown(reason))
            }
        },
        Completion::Shutdown(reason) => finish_shutdown(&progress, ui, connection, reason).await,
    }
}

async fn run_client(
    progress: &mut RunProgress<'_>,
    ui: &mut RunUi<'_>,
    connection: &mut embedded::Connection,
) -> Completion {
    loop {
        let page = match progress.prepare_iteration(ui).await {
            Ok(Iteration::Armed(page)) => page,
            Ok(Iteration::Redraw) => continue,
            Err(reason) => return Completion::Shutdown(reason),
        };
        let event = tokio::select! {
            event = connection.next() => RunEvent::Server(event),
            input = async {
                if progress.approval_ui.pending.is_some() { ui.terminal.next_input().await }
                else { ui.terminal.next_chat_input().await }
            }, if !progress.exit_after_run => RunEvent::Input(input),
        };
        let result = match event {
            RunEvent::Server(Ok(embedded::Event::Finished(result))) => {
                let result = (*result)
                    .and_then(|result| {
                        finish_attempt(Ok(result), progress.stop != ActiveStop::Running)
                    })
                    .map(Box::new);
                return progress.settle(ui, result).await;
            }
            RunEvent::Server(Ok(embedded::Event::Committed(event))) => {
                match connection.reduce(ui.view, event).await {
                    Ok(()) => progress.on_events(ui).await,
                    Err(error) => Err(error),
                }
            }
            RunEvent::Server(Ok(embedded::Event::Message(message))) => {
                progress.on_server_message(ui, message).await
            }
            RunEvent::Server(Err(error)) => Err(error),
            RunEvent::Input(input) => match progress.on_input(ui, input, page).await {
                Ok(()) => Ok(()),
                Err(InputFailure::Shutdown(reason)) => Err(reason),
                Err(InputFailure::Return(error)) => return Completion::Finished(Err(error)),
            },
        };
        if let Err(reason) = result {
            return Completion::Shutdown(reason);
        }
    }
}

impl RunProgress<'_> {
    async fn on_server_message(
        &mut self,
        ui: &mut RunUi<'_>,
        message: Message,
    ) -> Result<(), String> {
        match message {
            Message::Request(request) => {
                self.approval_ui.pending = Some(embedded::Approval::from_request(request)?);
                self.receive_approval(ui);
            }
            Message::Notification(note) if note.method == CANCEL_REQUEST_METHOD => {
                let withdrawn = note
                    .params
                    .and_then(|params| serde_json::from_value::<CancelRequestParams>(params).ok());
                if withdrawn.is_some_and(|params| {
                    self.approval_ui
                        .pending
                        .as_ref()
                        .is_some_and(|approval| approval.request_id == params.request_id)
                }) {
                    self.approval_ui.pending.take();
                    self.approval_ui.answer.clear();
                    ui.terminal
                        .restore_draft_input(ui.composer.input_capacity())
                        .map_err(|error| error.to_string())?;
                    self.draw(ui, None)
                        .map_err(|_| "terminal rendering failed".to_owned())?;
                }
            }
            Message::Notification(note) if note.method == "turn/display" => {
                let display: TurnDisplayNotification =
                    serde_json::from_value(note.params.unwrap_or_default())
                        .map_err(|_| "invalid Server display".to_owned())?;
                let changed = match display.update {
                    arany_cli::protocol::methods::DisplayUpdate::Waiting { intent_id } => {
                        ui.terminal.set_waiting_tool(intent_id);
                        true
                    }
                    arany_cli::protocol::methods::DisplayUpdate::Preview { preview } => ui
                        .terminal
                        .set_preview(preview.map(|preview| preview.into_update(display.turn_id))),
                };
                if changed {
                    self.draw_hint(ui)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

async fn finish_shutdown(
    progress: &RunProgress<'_>,
    ui: &mut RunUi<'_>,
    mut connection: embedded::Connection,
    shutdown_reason: String,
) -> Result<RunSubmission, String> {
    ui.terminal
        .settle_child_notes(ui.view, progress.controls.child_withdrawal(), true)
        .await;
    let _ = progress.controls.cancel().await;
    let linear = ui.terminal.is_linear();
    let mode = (!linear).then_some(ui.terminal.mode());
    let preview_printed = if linear {
        progress.user_recorded
    } else {
        ui.view
            .runs
            .last()
            .is_some_and(|run| ui.terminal.user_run_recorded(run.id))
    };
    let restored = ui.terminal.restore();
    let mut reason = shutdown_reason;
    let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
    let finish = async {
        let result = loop {
            if let embedded::Event::Finished(result) = connection
                .next()
                .await
                .map_err(|_| embedded::failed("Run event stream closed"))?
            {
                break *result;
            }
        };
        result.and_then(|result| finish_attempt(Ok(result), true))
    };
    let finished = tokio::time::timeout_at(deadline, finish).await;
    let cleanup = tokio::time::timeout_at(deadline, connection.close()).await;
    match finished {
        Ok(Ok(outcome)) if restored.is_ok() => {
            let emitted = match load_confirmed_outcome(&progress.admission.state_dir, &outcome)
                .await
            {
                Ok(confirmed) => emit_outcome(&confirmed, &outcome, preview_printed, !linear, mode),
                Err(error) => Err(error),
            };
            if emitted.is_err() {
                reason.push_str("; committed receipt unavailable; check Session history");
            }
        }
        Ok(Ok(_)) => reason.push_str("; Run committed; check Session history"),
        Ok(Err(RunAttemptError::InterruptedBeforeRun)) => {
            reason.push_str("; no Run started for this submission; account work may still complete")
        }
        Ok(Err(RunAttemptError::Stopped { .. } | RunAttemptError::Failed(_))) => {
            reason.push_str("; Run outcome unavailable; check Session history")
        }
        Err(_) => reason.push_str("; Run may be interrupted; check Session history"),
    }
    if !matches!(cleanup, Ok(Ok(()))) {
        reason.push_str("; Server shutdown failed");
    }
    if restored.is_err() {
        reason.push_str("; terminal restoration failed");
    }
    Ok(RunSubmission::Shutdown(reason))
}

impl RunUi<'_> {
    fn draw(
        &mut self,
        current_run_started: bool,
        stop: ActiveStop,
        notice: Option<TerminalNotice<'_>>,
    ) -> Result<(), TerminalError> {
        if !current_run_started {
            let status = match stop {
                ActiveStop::Running => None,
                ActiveStop::Pausing => Some("Pausing at the next step... Ctrl+C cancels now"),
                ActiveStop::Cancelling => Some("Stopping request..."),
            };
            self.terminal.draw_preparing_notice(
                self.view,
                self.composer,
                notice.or(status.map(TerminalNotice::Transient)),
            )
        } else if stop != ActiveStop::Running || self.terminal.is_linear() {
            let notice = notice.unwrap_or(TerminalNotice::Transient(match stop {
                ActiveStop::Cancelling => "Cancelling Run...",
                ActiveStop::Pausing => "Pausing at the next step... Ctrl+C cancels now",
                ActiveStop::Running => "Run is active; Ctrl+C cancels",
            }));
            if self.terminal.is_linear() {
                self.terminal
                    .draw_busy_notice(self.view, self.composer, Some(notice))
            } else {
                self.terminal
                    .draw_notice(self.view, self.composer, Some(notice))
            }
        } else {
            self.terminal.draw_notice(self.view, self.composer, notice)
        }
    }
}

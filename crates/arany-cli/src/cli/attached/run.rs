use super::{Admission, Feedback, emit_outcome, load_confirmed_outcome};
use crate::cli::provider::routing::RoutingError;
use crate::cli::provider::select::{Selection, SelectionError, select_provider};
use crate::cli::run_outcome::{SHUTDOWN_GRACE, stop_after_answer, stop_text};
use crate::terminal::{
    AgentChatFollow, AttachedTerminal, Composer, ComposerDraft, TerminalError, TerminalInput,
    TerminalNotice,
};
use arany::{
    AdmittedProviders, CompactionRecord, Engine, RunCancellation, RunControls, RunHandle, RunHints,
    RunId, RunOutcome, RunRequest, RunResult, RunSteering, SessionId, SessionView, ToolGrant,
    native_protection_supported,
};
use std::path::Path;
use std::time::Duration;

mod approval;
mod input;

use approval::{ApprovalPage, ApprovalUi, withdraw_abandoned_approval};
use input::{InputFailure, InputRoute, STEERING_SENT, settle_recorded_note};

#[derive(Clone, Copy)]
pub(super) struct ToolAccess<'a> {
    pub configured: bool,
    pub permissions: Option<&'a arany::WorkspacePermissions>,
    pub approvals: &'a arany::ToolApprovals,
}

#[derive(Debug)]
pub(super) enum RunAttemptError {
    InterruptedBeforeRun,
    Stopped {
        outcome: Box<arany::RunOutcome>,
        reason: String,
    },
    Failed(RunFailure),
}

#[derive(Debug, thiserror::Error)]
pub(super) enum RunFailure {
    #[error(transparent)]
    Selection(SelectionError),
    #[error("{0}")]
    Roster(String),
    #[error(transparent)]
    Routing(RoutingError),
    #[error("state directory unavailable or unsafe")]
    StateUnavailable,
    #[error("{}", super::permissions::UNAVAILABLE_NOTICE)]
    ToolsUnavailable,
    #[error(transparent)]
    Engine(arany::EngineError),
    #[error("{0}")]
    Unsettled(&'static str),
}

pub(super) async fn preflight_selected(
    state_dir: &Path,
    selection: Selection<'_>,
) -> Result<(), String> {
    select_provider(state_dir, selection)
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
}

struct SelectedSubmission<'a> {
    engine: &'a Engine,
    state_dir: &'a Path,
    selection: Selection<'a>,
    request: RunRequest,
    cancellation: RunCancellation,
    hints: RunHints,
    tools: ToolAccess<'a>,
    steering: RunSteering,
    roster: Option<crate::cli::provider::roster::SelectedRoster>,
}

async fn submit_selected<'a>(
    submission: SelectedSubmission<'a>,
) -> Result<RunHandle<'a>, RunAttemptError> {
    let SelectedSubmission {
        engine,
        state_dir,
        selection,
        request,
        cancellation,
        hints,
        tools,
        steering,
        roster,
    } = submission;
    let mut admission_cancellation = cancellation.clone();
    let admit = async {
        engine
            .readmit_state()
            .map_err(|_| RunFailure::StateUnavailable)?;
        if let Some(roster) = &roster {
            roster
                .matches_selection(selection)
                .map_err(RunFailure::Roster)?;
            return roster
                .admit(state_dir, &request.workspace)
                .await
                .map_err(RunFailure::Roster);
        }
        let provider = select_provider(state_dir, selection)
            .await
            .map_err(RunFailure::Selection)?;
        // Fail closed before route resolution can recreate a removed State directory.
        engine
            .readmit_state()
            .map_err(|_| RunFailure::StateUnavailable)?;
        crate::cli::provider::routing::configured_providers(state_dir, &request.workspace, provider)
            .await
            .map_err(RunFailure::Routing)
    };
    let providers = tokio::select! {
        biased;
        () = admission_cancellation.cancelled() => return Err(RunAttemptError::InterruptedBeforeRun),
        result = admit => result.map_err(RunAttemptError::Failed)?,
    };
    submit_with_providers(
        engine,
        providers,
        request,
        cancellation,
        hints,
        tools,
        steering,
    )
}

fn submit_with_providers<'a>(
    engine: &'a Engine,
    providers: AdmittedProviders,
    request: RunRequest,
    cancellation: RunCancellation,
    hints: RunHints,
    tools: ToolAccess<'_>,
    steering: RunSteering,
) -> Result<RunHandle<'a>, RunAttemptError> {
    // Each submission fails closed when the held State became unsafe or its path was swapped.
    engine
        .readmit_state()
        .map_err(|_| RunAttemptError::Failed(RunFailure::StateUnavailable))?;
    let trusted_tools =
        super::permissions::supports_tools(Some(&providers.primary[0].identity().profile));
    let grant = if tools.configured {
        if !native_protection_supported() {
            return Err(RunAttemptError::Failed(RunFailure::ToolsUnavailable));
        }
        ToolGrant::Configured
    } else {
        match tools
            .permissions
            .filter(|permissions| trusted_tools && permissions.is_trusted())
        {
            Some(permissions) => ToolGrant::Trusted(permissions.clone()),
            None => ToolGrant::None,
        }
    };
    let grants_tools = !matches!(grant, ToolGrant::None);
    let handle = engine
        .submit(
            providers,
            request,
            RunControls {
                tools: grant,
                approvals: Some(tools.approvals.clone()),
                steering: Some(steering),
                cancellation,
                hints: Some(hints),
            },
        )
        .map_err(|error| {
            if grants_tools {
                arany::record_development_failure(arany::DevelopmentFailure::ToolAdmission);
            }
            RunAttemptError::Failed(RunFailure::Engine(error))
        })?;
    Ok(handle)
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

fn classify_run_error(error: arany::EngineError) -> RunAttemptError {
    match error {
        arany::EngineError::CancelledBeforeStart => RunAttemptError::InterruptedBeforeRun,
        error => RunAttemptError::Failed(RunFailure::Engine(error)),
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

enum RunStep<'a> {
    Admitted(arany::RunHandle<'a>),
    Completed(Result<Box<RunOutcome>, RunAttemptError>),
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
fn abandon_drafts_for_exit(
    composer: &mut Composer,
    held: &mut Option<HeldDraft>,
    steering: &RunSteering,
) {
    composer.take();
    composer.take_images();
    *held = None;
    let _ = steering.withdraw_note();
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
    pub(super) engine: &'a arany::Engine,
    pub(super) profile: String,
    pub(super) model: String,
    pub(super) roster: Option<crate::cli::provider::roster::SelectedRoster>,
    pub(super) manifest: Option<String>,
    pub(super) request: RunRequest,
    pub(super) progress: RunProgress<'a>,
    pub(super) inbox: arany::ApprovalInbox,
}

pub(super) struct RunUi<'a> {
    pub(super) terminal: &'a mut AttachedTerminal,
    pub(super) view: &'a mut SessionView,
    pub(super) composer: &'a mut Composer,
}

pub(super) struct RunProgress<'a> {
    admission: &'a Admission,
    starting_sequence: u64,
    autonomous: bool,
    submitted_goal: Option<arany::GoalId>,
    cancellation: RunCancellation,
    hints: RunHints,
    approvals: arany::ToolApprovals,
    steering: RunSteering,
    approval_ui: ApprovalUi,
    stop: ActiveStop,
    user_recorded: bool,
    offered: Option<String>,
    linear_offers: Vec<ComposerDraft>,
    held: Option<HeldDraft>,
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
    ) -> (Self, arany::ApprovalInbox) {
        let cancellation = RunCancellation::new();
        let hints = if ui.terminal.is_linear() {
            RunHints::new()
        } else {
            RunHints::with_previews()
        };
        let (approvals, inbox) =
            arany::ToolApprovals::new(ui.composer.approval_mode().unwrap_or_default());
        (
            Self {
                admission,
                starting_sequence,
                autonomous,
                submitted_goal,
                cancellation,
                hints,
                approvals,
                steering: RunSteering::new(),
                approval_ui: ApprovalUi::default(),
                stop: ActiveStop::Running,
                user_recorded: false,
                offered: None,
                linear_offers: Vec::new(),
                held: None,
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
                approval,
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
            || catch_up(&self.admission.state_dir, ui.view).await.is_ok();
        ui.terminal
            .settle_child_notes(ui.view, &self.steering, true);
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
        if let Err(reason) = self.restore_drafts(ui, result.is_err(), child_follow) {
            return Completion::Finished(Ok(RunSubmission::Shutdown(reason)));
        }
        Completion::Finished(self.submission_result(ui, result).await)
    }

    fn restore_drafts(
        &mut self,
        ui: &mut RunUi<'_>,
        failed: bool,
        child_follow: bool,
    ) -> Result<(), String> {
        // An undelivered note ends the Run as an ordinary retained draft.
        let had_offer = self.offered.is_some();
        let sent = settle_recorded_note(
            self.steering.delivered_notes(),
            &mut self.seen_delivered,
            &mut self.offered,
            ui.composer,
        );
        let had_held = self.held.is_some();
        // The undelivered note predates the held draft, so restore it ahead of that draft.
        let note = self
            .steering
            .withdraw_note()
            .filter(|_| ui.terminal.is_linear());
        let withdrawn = note.is_some();
        let mut restored = self
            .held
            .take()
            .is_none_or(|draft| release_held_draft(ui.composer, draft));
        if let Some(note) = note {
            restored &= ui
                .composer
                .restore_draft(ComposerDraft::from_steering_note(note, &self.linear_offers));
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
                let reloaded = super::load_view(&self.admission.state_dir, ui.view.id).await?;
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
            self.steering.delivered_notes(),
            &mut self.seen_delivered,
            &mut self.offered,
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
        let child_changed = ui.terminal.settle_child_notes(
            ui.view,
            &self.steering,
            self.stop != ActiveStop::Running,
        );
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

    fn control_input(
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
                .settle_child_notes(ui.view, &self.steering, true);
            self.cancellation.cancel();
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
            if !ui.composer.is_empty() || self.held.is_some() {
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

enum RunEvent<'a> {
    ApprovalWithdrawn,
    Approval,
    Step(RunStep<'a>),
    Preview(Option<arany::DisplayUpdate>),
    Events(Result<(), String>),
    Waiting(Option<uuid::Uuid>),
    Input(Result<TerminalInput, TerminalError>),
}

struct RunDriver<'a, 'f, F> {
    future: std::pin::Pin<&'f mut F>,
    handle: Option<arany::RunHandle<'a>>,
    inbox: arany::ApprovalInbox,
    previews: arany::RunPreview,
    waiting_hints: RunHints,
}

impl<'a, F> RunDriver<'a, '_, F>
where
    F: std::future::Future<Output = Result<arany::RunHandle<'a>, RunAttemptError>>,
{
    async fn poll(&mut self, progress: &mut RunProgress<'_>, ui: &mut RunUi<'_>) -> RunEvent<'a> {
        let (result, mut events) = self
            .handle
            .as_mut()
            .map(|handle| (&mut handle.result, &mut handle.events))
            .unzip();
        let approval_pending = progress.approval_ui.pending.is_some();
        let stop = progress.stop;
        tokio::select! {
            () = withdraw_abandoned_approval(&mut progress.approval_ui.pending, &mut progress.approval_ui.answer) => RunEvent::ApprovalWithdrawn,
            approval = self.inbox.next(), if !approval_pending => { progress.approval_ui.pending = approval; RunEvent::Approval },
            step = async {
                match result {
                    Some(result) => RunStep::Completed(finish_attempt(result.await, !matches!(stop, ActiveStop::Running)).map(Box::new)),
                    None => match self.future.as_mut().await {
                        Ok(handle) => RunStep::Admitted(handle),
                        Err(error) => RunStep::Completed(Err(error)),
                    },
                }
            } => RunEvent::Step(step),
            preview = self.previews.changed() => RunEvent::Preview(preview),
            event = async {
                match events.as_deref_mut() {
                    Some(events) => events.next().await,
                    None => std::future::pending().await,
                }
            } => {
                let ready = async || match events.as_deref_mut() {
                    Some(events) => tokio::time::timeout(Duration::ZERO, events.next()).await.ok(),
                    None => None,
                };
                RunEvent::Events(apply_burst(&progress.admission.state_dir, ui.view, event, ready).await)
            },
            waiting = self.waiting_hints.waiting_changed() => RunEvent::Waiting(waiting),
            input = async {
                if approval_pending { ui.terminal.next_input().await } else { ui.terminal.next_chat_input().await }
            }, if !progress.exit_after_run => RunEvent::Input(input),
        }
    }

    async fn finish(self) -> Result<RunOutcome, RunAttemptError> {
        let handle = match self.handle {
            Some(handle) => handle,
            None => self.future.await?,
        };
        finish_attempt(handle.await, true)
    }
}

pub(super) async fn drive_run(
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
    let approvals = progress.approvals.clone();
    let admission = progress.admission;
    let future = submit_selected(SelectedSubmission {
        engine,
        state_dir: &admission.state_dir,
        selection: Selection {
            workspace: &admission.workspace,
            profile: &profile,
            model: &model,
            effort: ui.view.defaults.effort,
            account_id: ui.view.defaults.account_id,
        },
        request,
        cancellation: progress.cancellation.clone(),
        hints: progress.hints.clone(),
        tools: ToolAccess {
            configured: admission.tools,
            permissions: admission.workspace_permissions.as_ref(),
            approvals: &approvals,
        },
        steering: progress.steering.clone(),
        roster,
    });
    tokio::pin!(future);
    let mut driver = RunDriver {
        future: future.as_mut(),
        handle: None,
        inbox,
        previews: progress.hints.previews(),
        waiting_hints: progress.hints.clone(),
    };
    match drive_loop(&mut progress, ui, &mut driver).await {
        Completion::Finished(result) => result,
        Completion::Shutdown(reason) => finish_shutdown(&progress, ui, driver, reason).await,
    }
}

async fn drive_loop<'a, F>(
    progress: &mut RunProgress<'_>,
    ui: &mut RunUi<'_>,
    driver: &mut RunDriver<'a, '_, F>,
) -> Completion
where
    F: std::future::Future<Output = Result<arany::RunHandle<'a>, RunAttemptError>>,
{
    loop {
        let page = match progress.prepare_iteration(ui).await {
            Ok(Iteration::Armed(page)) => page,
            Ok(Iteration::Redraw) => continue,
            Err(reason) => return Completion::Shutdown(reason),
        };
        let result = match driver.poll(progress, ui).await {
            RunEvent::ApprovalWithdrawn => {
                if let Err(error) = ui
                    .terminal
                    .restore_draft_input(ui.composer.input_capacity())
                {
                    return Completion::Finished(Err(error.to_string()));
                }
                progress
                    .draw(ui, None)
                    .map_err(|_| "terminal rendering failed".to_owned())
            }
            RunEvent::Approval => {
                progress.receive_approval(ui);
                Ok(())
            }
            RunEvent::Step(RunStep::Admitted(handle)) => {
                driver.handle = Some(handle);
                continue;
            }
            RunEvent::Step(RunStep::Completed(result)) => {
                return progress.settle(ui, result).await;
            }
            RunEvent::Preview(preview) => {
                ui.terminal.set_preview(preview);
                progress.draw_hint(ui)
            }
            RunEvent::Events(result) => match result {
                Ok(()) => progress.on_events(ui).await,
                Err(reason) => Err(reason),
            },
            RunEvent::Waiting(waiting) => {
                ui.terminal.set_waiting_tool(waiting);
                progress.draw_hint(ui)
            }
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

async fn finish_shutdown<'a, F>(
    progress: &RunProgress<'_>,
    ui: &mut RunUi<'_>,
    driver: RunDriver<'a, '_, F>,
    shutdown_reason: String,
) -> Result<RunSubmission, String>
where
    F: std::future::Future<Output = Result<arany::RunHandle<'a>, RunAttemptError>>,
{
    ui.terminal
        .settle_child_notes(ui.view, &progress.steering, true);
    progress.cancellation.cancel();
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
    match tokio::time::timeout(SHUTDOWN_GRACE, driver.finish()).await {
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
            reason
                .push_str("; no Run started for this submission; account work may still complete");
        }
        Ok(Err(RunAttemptError::Stopped { .. } | RunAttemptError::Failed(_))) => {
            reason.push_str("; Run outcome unavailable; check Session history")
        }
        Err(_) => reason.push_str("; Run may be interrupted; check Session history"),
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

const EVENT_DRAIN_LIMIT: usize = 64;

// Bounded so input is polled again before the next burst.
async fn apply_burst(
    state_dir: &std::path::Path,
    view: &mut SessionView,
    first: Option<arany::Tapped>,
    mut ready: impl AsyncFnMut() -> Option<Option<arany::Tapped>>,
) -> Result<(), String> {
    let mut event = first;
    for drained in 1..=EVENT_DRAIN_LIMIT {
        let Some(tapped) = event else {
            return Err("Run event stream closed".to_owned());
        };
        if apply_tapped(state_dir, view, tapped).await.is_err() {
            return Err("Run history unavailable; check Session history".to_owned());
        }
        if drained == EVENT_DRAIN_LIMIT {
            break;
        }
        match ready().await {
            Some(next) => event = next,
            None => break,
        }
    }
    Ok(())
}

async fn apply_tapped(
    state_dir: &std::path::Path,
    view: &mut SessionView,
    tapped: arany::Tapped,
) -> Result<(), String> {
    match tapped {
        arany::Tapped::Committed(event) => {
            if event.session_id != view.id || event.sequence <= view.last_sequence {
                return Ok(());
            }
            if event.sequence != view.last_sequence + 1 {
                return catch_up(state_dir, view).await;
            }
            match arany::reduce_into(view, &event) {
                Ok(()) => Ok(()),
                Err(_) => catch_up(state_dir, view).await,
            }
        }
        arany::Tapped::Resync { .. } => catch_up(state_dir, view).await,
    }
}

async fn catch_up(state_dir: &std::path::Path, view: &mut SessionView) -> Result<(), String> {
    let state = arany::StateRoot::open_existing(state_dir).map_err(|error| error.to_string())?;
    let store = arany::Store::open_read_only(state).map_err(|error| error.to_string())?;
    let loaded = store
        .load_open_view(view.id)
        .await
        .map_err(|error| error.to_string());
    store.close().await.map_err(|error| error.to_string())?;
    *view = loaded?.ok_or_else(|| "Session history unavailable".to_owned())?;
    Ok(())
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use arany::StateRoot;
    use arany::test_support::{
        engine::open,
        goals::{assert_long_turn, facts_workspace, long_turn_provider, replayed},
        scenario::request,
    };
    use std::{os::unix::fs::PermissionsExt, sync::Arc};

    #[tokio::test]
    async fn attached_defaults_complete_past_64_calls_and_30_active_minutes() {
        for finite in [false, true] {
            let temp = tempfile::tempdir().expect("private root");
            let workspace = facts_workspace(temp.path(), "Fact");
            let state = temp.path().join("state");
            let engine = open(
                StateRoot::admit(&state).expect("State"),
                arany::EngineConfig {
                    guard_executable: Some(std::env::current_exe().expect("test executable")),
                    ..arany::EngineConfig::default()
                },
            )
            .expect("Engine");
            let config = state.join("tools.json");
            std::fs::write(&config, br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#).expect("config");
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
                .expect("private config");
            if finite {
                let preferences = serde_json::json!({"version":6,"preferences":{
                    "completion":"tabs","view":"fullscreen",
                    "goal_budget":arany::GoalBudget { max_provider_calls:Some(1), ..Default::default() },
                    "resources":arany::ToolResourceSettings::default(),"copy_on_select":true,"newline":"shift_enter"
                }});
                StateRoot::open_existing(&state)
                    .expect("State")
                    .record(arany::RecordKind::UiPreferences)
                    .replace(&serde_json::to_vec(&preferences).expect("settings"))
                    .expect("finite settings");
            }
            let id = engine
                .create_session(workspace.clone(), None)
                .await
                .expect("Session");
            let (_, view) = replayed(&state, id).await;
            let settings = super::super::controls::restore_settings_at(&state)
                .await
                .expect("missing settings");
            let mut input = request(&workspace, "Inspect the folder");
            input.session_id = Some(id);
            input.title = None;
            input.goal = super::super::active::goal_request(
                &view,
                super::super::active::GoalStart::Plain,
                settings,
            )
            .expect("attached request");
            let provider = Arc::new(long_turn_provider());
            let (approvals, _inbox) = arany::ToolApprovals::new(arany::ApprovalMode::AutoEdits);
            let result = submit_with_providers(
                &engine,
                AdmittedProviders::single(provider.clone()),
                input,
                RunCancellation::new(),
                RunHints::new(),
                ToolAccess {
                    configured: true,
                    permissions: None,
                    approvals: &approvals,
                },
                RunSteering::new(),
            )
            .expect("attached admission")
            .await;
            let outcome = finish_attempt(result, false).expect("attached completion");
            assert_eq!(
                outcome.run.assistant_message.as_deref(),
                if finite {
                    None
                } else {
                    Some("Long turn completed")
                }
            );
            assert_eq!(
                outcome.run.status,
                if finite {
                    arany::RunStatus::Failed
                } else {
                    arany::RunStatus::Finished
                }
            );
            engine.close().await.expect("shutdown");
            let (events, view) = replayed(&state, id).await;
            if finite {
                assert_eq!(
                    crate::presentation::run_feedback(&outcome.run).as_deref(),
                    Some(
                        "Error: Run failed. The Goal budget refused the next action. No answer was committed. Use /agents to inspect this Run."
                    )
                );
                assert_eq!(view.goals[0].budget.max_provider_calls, Some(1));
                assert_eq!(view.goals[0].provider_calls, 1);
                assert!(matches!(
                    events.last().expect("receipt").event,
                    arany::Event::GoalFinished {
                        exhausted: true,
                        ..
                    }
                ));
                assert!(view.runs[0].tools.is_empty());
            } else {
                assert_long_turn(&events, &view);
                assert!(provider.responses.lock().expect("script").is_empty());
            }
            assert_eq!(
                provider.requests.lock().expect("requests").len(),
                if finite { 1 } else { 65 }
            );
        }
    }
}

// Linux release builds have no tests in this module: each one is debug-only or macOS-only.
#[cfg(all(test, unix, any(debug_assertions, target_os = "macos")))]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    use crate::terminal::{AttachedTerminal, Composer};

    #[cfg(debug_assertions)]
    use arany::RecordKind;
    #[cfg(target_os = "macos")]
    use arany::{
        AgentPhase, Answer, ApprovalMode, ChatGptAdmission, ChatGptProvenance, CollaborationPolicy,
        OutputTokenBound, Provenance, Provider, ProviderError, ProviderIdentity, ProviderOutcome,
        ProviderRequest, ProviderResponse, RunStatus,
    };
    use arany::{Effort, SessionView, StateRoot, Store};
    #[cfg(target_os = "macos")]
    use std::sync::{Arc, Mutex};
    use std::{
        io::Write,
        os::unix::fs::{MetadataExt, PermissionsExt},
        os::unix::process::CommandExt,
        process::{Child, Command, Stdio},
        time::Duration,
    };
    use uuid::Uuid;

    #[test]
    fn engine_cancellation_before_admission_is_an_interrupted_attempt() {
        assert!(matches!(
            classify_run_error(arany::EngineError::CancelledBeforeStart),
            RunAttemptError::InterruptedBeforeRun
        ));
        assert!(matches!(
            classify_run_error(arany::EngineError::CoordinatorFailed),
            RunAttemptError::Failed(_)
        ));
    }

    struct OwnedChild(Child);

    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn saved_access_is_authorized_before_chat_without_exposing_secrets() {
        enum StartupInput {
            Reply(&'static str),
            Interrupt,
        }
        use StartupInput::{Interrupt, Reply};
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Case {
            ChatGpt,
            Locked,
            Missing,
            Secret,
            Native,
            NativeCached,
            Replaced,
            PrivateFile,
            Unconfigured,
            InvalidChatgpt,
            InvalidChatgptReset,
            InvalidNative,
            InvalidNativeReset,
            InvalidPreferences,
            InvalidPreferencesReset,
            InvalidRegistration,
            InitialSetupFailure,
            InvalidHistory,
            InvalidHistorySameRoot,
            InvalidHistorySetup,
            InvalidHistorySetupQuit,
            CorruptHistory,
            CorruptHistoryCancel,
            StaleEventHistory,
            UnsafeHistory,
            ResumeLocked,
            ContinueMissing,
            Cancel,
        }
        impl Case {
            const ALL: [Self; 28] = [
                Self::ChatGpt,
                Self::Locked,
                Self::Missing,
                Self::Secret,
                Self::Native,
                Self::NativeCached,
                Self::Replaced,
                Self::PrivateFile,
                Self::Unconfigured,
                Self::InvalidChatgpt,
                Self::InvalidChatgptReset,
                Self::InvalidNative,
                Self::InvalidNativeReset,
                Self::InvalidPreferences,
                Self::InvalidPreferencesReset,
                Self::InvalidRegistration,
                Self::InitialSetupFailure,
                Self::InvalidHistory,
                Self::InvalidHistorySameRoot,
                Self::InvalidHistorySetup,
                Self::InvalidHistorySetupQuit,
                Self::CorruptHistory,
                Self::CorruptHistoryCancel,
                Self::StaleEventHistory,
                Self::UnsafeHistory,
                Self::ResumeLocked,
                Self::ContinueMissing,
                Self::Cancel,
            ];

            fn name(self) -> &'static str {
                match self {
                    Self::ChatGpt => "chatgpt",
                    Self::Locked => "locked",
                    Self::Missing => "missing",
                    Self::Secret => "secret",
                    Self::Native => "native",
                    Self::NativeCached => "native_cached",
                    Self::Replaced => "replaced",
                    Self::PrivateFile => "private_file",
                    Self::Unconfigured => "unconfigured",
                    Self::InvalidChatgpt => "invalid_chatgpt",
                    Self::InvalidChatgptReset => "invalid_chatgpt_reset",
                    Self::InvalidNative => "invalid_native",
                    Self::InvalidNativeReset => "invalid_native_reset",
                    Self::InvalidPreferences => "invalid_preferences",
                    Self::InvalidPreferencesReset => "invalid_preferences_reset",
                    Self::InvalidRegistration => "invalid_registration",
                    Self::InitialSetupFailure => "initial_setup_failure",
                    Self::InvalidHistory => "invalid_history",
                    Self::InvalidHistorySameRoot => "invalid_history_same_root",
                    Self::CorruptHistory => "corrupt_history",
                    Self::CorruptHistoryCancel => "corrupt_history_cancel",
                    Self::StaleEventHistory => "stale_event_history",
                    Self::InvalidHistorySetup => "invalid_history_setup",
                    Self::InvalidHistorySetupQuit => "invalid_history_setup_quit",
                    Self::UnsafeHistory => "unsafe_history",
                    Self::ResumeLocked => "resume_locked",
                    Self::ContinueMissing => "continue_missing",
                    Self::Cancel => "cancel",
                }
            }

            fn archives_history(self) -> bool {
                matches!(
                    self,
                    Self::InvalidHistory
                        | Self::InvalidHistorySameRoot
                        | Self::InvalidHistorySetup
                        | Self::InvalidHistorySetupQuit
                        | Self::StaleEventHistory
                )
            }

            fn parse(name: &str) -> Self {
                Self::ALL
                    .into_iter()
                    .find(|case| case.name() == name)
                    .expect("synthetic startup case")
            }
        }
        impl std::fmt::Display for Case {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.name())
            }
        }

        const ROOT: &str = "ARANY_TEST_SAVED_ACCESS_ROOT";
        const CASE: &str = "ARANY_TEST_SAVED_ACCESS_CASE";
        const TEST: &str = "cli::attached::run::tests::saved_access_is_authorized_before_chat_without_exposing_secrets";
        let Some(root) = std::env::var_os(ROOT) else {
            for case in Case::ALL {
                let dialogue: &[StartupInput] = match case {
                    Case::InvalidChatgpt | Case::InvalidRegistration => &[
                        Reply("/setup\n"),
                        Reply("ChatGPT plan\n"),
                        Reply("\n"),
                        Reply("/exit\n"),
                    ],
                    Case::InvalidChatgptReset => &[
                        Reply("/setup\n"),
                        Reply("ChatGPT plan\n"),
                        Reply("Reset local setup\n"),
                        Reply("\n"),
                        Reply("/exit\n"),
                    ],
                    Case::InvalidNative => &[
                        Reply("/setup\n"),
                        Reply("API key\n"),
                        Reply("\n"),
                        Reply("/exit\n"),
                    ],
                    Case::InvalidNativeReset => &[
                        Reply("/setup\n"),
                        Reply("API key\n"),
                        Reply("Reset local setup\n"),
                        Reply("\n"),
                        Reply("/exit\n"),
                    ],
                    Case::InvalidPreferences => &[Reply("/setup\n"), Reply("\n"), Reply("/exit\n")],
                    Case::InvalidPreferencesReset => &[
                        Reply("/setup\n"),
                        Reply("Reset local setup\n"),
                        Interrupt,
                        Reply("/exit\n"),
                    ],
                    Case::InitialSetupFailure | Case::InvalidHistorySetup => {
                        &[Reply("ChatGPT plan\n"), Reply("/exit\n")]
                    }
                    Case::InvalidHistorySetupQuit => &[Interrupt],
                    Case::CorruptHistory => &[
                        Reply("Reset history\n"),
                        Reply("Accept\n"),
                        Reply("/exit\n"),
                    ],
                    Case::CorruptHistoryCancel => {
                        &[Reply("Reset history\n"), Reply("\n"), Reply("Exit\n")]
                    }
                    Case::UnsafeHistory => &[Reply("Exit\n")],
                    Case::ResumeLocked | Case::ContinueMissing | Case::Cancel => &[],
                    _ => &[Reply("/exit\n")],
                };
                let mut inputs = dialogue.iter();
                let temp = tempfile::tempdir().unwrap();
                let (master, slave) = crate::test_process::cloexec_pty(None);
                let settings = nix::sys::termios::tcgetattr(&master).unwrap();
                let mut input = master.try_clone().unwrap();
                let mut answered_bytes = 0;
                let listener = if case == Case::Cancel {
                    let listener =
                        std::os::unix::net::UnixListener::bind(temp.path().join("authorization"))
                            .unwrap();
                    listener.set_nonblocking(true).unwrap();
                    Some(listener)
                } else {
                    None
                };
                let mut connection = None;
                let mut helper_pid = None;
                let mut interrupted = false;
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .env_clear()
                    .env(ROOT, temp.path())
                    .env(CASE, case.name())
                    .env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"))
                    .env("ARANY_TEST_EXE", temp.path().join("credential-helper"))
                    .env("HOME", temp.path().join("fixture-home"))
                    .env("XDG_STATE_HOME", temp.path().join("fixture-state"))
                    .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
                    .env("TERM", "dumb")
                    .args(["--exact", TEST, "--nocapture"])
                    .current_dir(temp.path())
                    .process_group(0)
                    .stdin(Stdio::from(slave.try_clone().unwrap()))
                    .stdout(Stdio::piped())
                    .stderr(Stdio::from(slave));
                let mut child = OwnedChild(command.spawn().unwrap());
                let product_pid =
                    rustix::process::Pid::from_raw(child.0.id().try_into().unwrap()).unwrap();
                drop(command);
                let output = crate::test_process::capture_terminal(
                    &mut child.0,
                    master,
                    Duration::from_secs(20),
                    64 * 1024,
                    |bytes| {
                        if !interrupted && let Some(listener) = &listener {
                            match listener.accept() {
                                Ok((mut ready, _)) => {
                                    use std::io::Read;
                                    ready.set_nonblocking(false).unwrap();
                                    ready
                                        .set_read_timeout(Some(Duration::from_secs(4)))
                                        .unwrap();
                                    let mut pid = [0; 4];
                                    ready.read_exact(&mut pid).unwrap();
                                    helper_pid = rustix::process::Pid::from_raw(
                                        u32::from_be_bytes(pid).try_into().unwrap(),
                                    );
                                    connection = Some(ready);
                                    rustix::process::kill_process(
                                        product_pid,
                                        rustix::process::Signal::INT,
                                    )
                                    .unwrap();
                                    interrupted = true;
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                                Err(error) => panic!("authorization readiness: {error}"),
                            }
                        }
                        let fresh =
                            String::from_utf8_lossy(&bytes[answered_bytes..]).replace("\r\n", "\n");
                        if fresh.contains("Input:\n")
                            && let Some(action) = inputs.next()
                        {
                            match action {
                                Reply(line) => input.write_all(line.as_bytes()).unwrap(),
                                Interrupt => {
                                    rustix::process::kill_process(
                                        product_pid,
                                        rustix::process::Signal::INT,
                                    )
                                    .unwrap();
                                }
                            }
                            answered_bytes = bytes.len();
                        }
                    },
                );
                let transcript = String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n");
                assert!(
                    output.status.success(),
                    "saved access case {case}: {}; terminal {}",
                    String::from_utf8_lossy(&output.stdout),
                    crate::presentation::escape_terminal(
                        &transcript.replace("synthetic-startup-secret", "[redacted]")
                    )
                );
                crate::test_process::assert_single_test_completed(
                    &output.stdout,
                    &format!("saved access case {case}"),
                );
                let expects_prompt = !matches!(
                    case,
                    Case::ResumeLocked | Case::ContinueMissing | Case::Cancel
                );
                assert_eq!(
                    transcript.contains("Input:\n"),
                    expects_prompt,
                    "startup input ownership for {case}"
                );
                assert!(
                    inputs.as_slice().is_empty(),
                    "startup dialogue must complete for {case}"
                );
                assert_eq!(
                    nix::sys::termios::tcgetattr(&input).unwrap(),
                    settings,
                    "startup must restore exact terminal settings"
                );
                if case == Case::Cancel {
                    use std::io::Read;
                    assert!(interrupted);
                    let mut byte = [0];
                    assert_eq!(
                        connection.as_mut().unwrap().read(&mut byte).unwrap(),
                        0,
                        "cancelled credential helper EOF"
                    );
                    assert_eq!(
                        rustix::process::test_kill_process(helper_pid.unwrap()),
                        Err(rustix::io::Errno::SRCH),
                        "credential helper must be reaped before exit"
                    );
                }
                assert!(
                    !transcript.contains("synthetic-startup-secret"),
                    "credential escaped to presentation"
                );
                let marker = temp.path().join("helper-access");
                if case.archives_history() {
                    assert!(
                        !transcript.contains("Setup: History recovery"),
                        "incompatible history must not need a manual reset: {case}"
                    );
                    // Setup replaces earlier startup notices; the archive notice must survive it.
                    assert_eq!(
                        transcript
                            .lines()
                            .filter(|line| line.starts_with("Notice: ")
                                && line.contains(
                                    "History from an incompatible version archived intact to "
                                ))
                            .count(),
                        usize::from(case != Case::InvalidHistorySetupQuit),
                        "archive notice for {case}: {}",
                        crate::presentation::escape_terminal(&transcript)
                    );
                    if case == Case::InvalidHistorySameRoot {
                        assert!(transcript.contains("Initial task was not submitted"));
                    }
                    if case == Case::InvalidHistorySetup {
                        assert!(transcript.contains("Setup could not be completed"));
                    }
                    let calls = std::fs::read_to_string(marker).unwrap_or_default();
                    assert_eq!(
                        calls.lines().count(),
                        usize::from(!case.name().contains("_setup")),
                        "archived history keeps access"
                    );
                } else if case.name().contains("history") {
                    assert!(
                        transcript.contains("Setup: History recovery"),
                        "history case {case}: {}",
                        crate::presentation::escape_terminal(&transcript)
                    );
                    assert!(transcript.contains("empty Enter selects Retry"));
                    if case == Case::UnsafeHistory {
                        assert!(!transcript.contains("Reset history"));
                        assert!(transcript.contains("unsafe or inaccessible"));
                    } else {
                        assert!(transcript.contains("Reset deletes all conversations"));
                        assert!(transcript.contains("Reset history"));
                        assert!(transcript.contains("This cannot be undone."));
                        assert!(transcript.contains("empty Enter selects Back"));
                    }
                    let calls = std::fs::read_to_string(marker).unwrap_or_default();
                    assert_eq!(
                        calls.lines().count(),
                        usize::from(
                            !case.name().ends_with("_cancel") && case != Case::UnsafeHistory
                        )
                    );
                } else if case.name().starts_with("invalid_") || case == Case::InitialSetupFailure {
                    assert!(transcript.contains("Setup: "));
                    if !matches!(case, Case::InvalidRegistration | Case::InitialSetupFailure) {
                        assert!(transcript.contains("Saved access could not be restored"));
                    }
                    if case == Case::InitialSetupFailure {
                        assert!(transcript.contains("Setup could not be completed"));
                    } else {
                        assert!(
                            transcript.contains("empty Enter selects Back"),
                            "reset must initially decline"
                        );
                        assert!(
                            transcript.contains("Reset local setup"),
                            "reset choice must not be clipped"
                        );
                        if !case.name().starts_with("invalid_preferences") {
                            assert!(transcript.contains("no remote sign-out"));
                        }
                    }
                    let expected = usize::from(matches!(
                        case,
                        Case::InvalidRegistration
                            | Case::InvalidChatgptReset
                            | Case::InvalidNativeReset
                    ));
                    let calls = std::fs::read_to_string(marker).unwrap_or_default();
                    assert_eq!(
                        calls.lines().count(),
                        expected,
                        "only explicit setup probes or valid startup authorization may use the helper"
                    );
                } else if matches!(case, Case::PrivateFile | Case::Unconfigured) {
                    assert!(
                        !marker.exists(),
                        "unconfigured/private file must not access the OS store"
                    );
                    assert!(!transcript.contains("Authorize saved access"));
                } else {
                    let phase = transcript.find("Approve this Arany credential in your OS password store; Esc or Ctrl+C exits\n").expect("authorization phase");
                    if expects_prompt {
                        assert!(phase < transcript.find("Input:\n").expect("composer"));
                    }
                    let calls = std::fs::read_to_string(marker).unwrap();
                    assert_eq!(
                        calls.lines().count(),
                        1,
                        "startup must not retry or inspect another item"
                    );
                    match case {
                        Case::Locked | Case::ResumeLocked => {
                            assert!(transcript.contains("OS credential store is locked"))
                        }
                        Case::Missing | Case::ContinueMissing => assert!(
                            transcript
                                .contains("Saved sign-in is missing from the OS password store")
                        ),
                        Case::Secret => {
                            assert!(transcript.contains("OS credential store unavailable"))
                        }
                        Case::Replaced => assert!(transcript.contains("invalid saved account")),
                        Case::Cancel => {
                            assert!(transcript.contains("Startup authorization cancelled"))
                        }
                        _ => assert!(
                            !transcript.contains("Error:"),
                            "authorization failed in {case}"
                        ),
                    }
                }
            }
            return;
        };
        let root = std::path::PathBuf::from(root);
        let case = Case::parse(&std::env::var(CASE).unwrap());
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let session_path = root.join(if case == Case::InvalidHistorySameRoot {
            "account"
        } else {
            "session"
        });
        let state = StateRoot::admit(&root.join("account")).unwrap();
        let metadata = std::fs::metadata(&workspace).unwrap();
        let trust = serde_json::to_vec(&serde_json::json!({
            "version": 2, "workspaces": [{ "path": workspace,
                "device": metadata.dev(), "inode": metadata.ino(), "mode": "auto_edits",
                "runtime_grants": [] }]
        }))
        .unwrap();
        let trust_path = state.path().join("workspace-permissions.json");
        std::fs::write(&trust_path, trust).unwrap();
        std::fs::set_permissions(&trust_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let native = matches!(
            case,
            Case::Native
                | Case::NativeCached
                | Case::Replaced
                | Case::PrivateFile
                | Case::InvalidNative
                | Case::InvalidNativeReset
        );
        let account = crate::accounts::credentials::SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(Effort::Low),
            "synthetic-startup-secret".into(),
        )
        .unwrap();
        let id = if native { account.id } else { Uuid::now_v7() };
        let profile = if native { "openai" } else { "chatgpt" };
        let (operation, slot, response) = if case == Case::Unconfigured {
            ("forbidden", "forbidden".into(), "exit 1".into())
        } else if native {
            if case == Case::PrivateFile {
                crate::accounts::credentials::save(
                    &workspace,
                    account,
                    crate::accounts::credentials::AccountStorage::PrivateFile,
                )
                .await
                .unwrap();
                ("forbidden", "forbidden".into(), "exit 1".into())
            } else {
                state
                    .record(RecordKind::SavedAccount)
                    .replace(br#"{"schema":1,"storage":"keyring","account":null}"#)
                    .unwrap();
                let record = serde_json::to_string(&account)
                    .unwrap()
                    .replace('\'', "'\\''");
                if case == Case::Native {
                    (
                        "read",
                        "default-native-api-account".into(),
                        format!("printf '%s' '{record}'"),
                    )
                } else {
                    (
                        "authorize",
                        "default-native-api-account".into(),
                        if case == Case::Replaced {
                            "exit 4"
                        } else {
                            "exit 0"
                        }
                        .into(),
                    )
                }
            }
        } else {
            state
                .record(RecordKind::ChatgptAccounts).replace(
                    &serde_json::to_vec(&serde_json::json!({
                        "schema": 1, "selected": id, "accounts": [{
                            "id": id, "host_id": "11111111-1111-4111-8111-111111111111",
                            "client_id": "synthetic-client", "subject": "synthetic-subject",
                            "storage": "keyring", "renewal_pending": false, "signout_pending": false,
                            "disconnected": false, "plan_permission_missing": false, "token": null
                        }], "model_checks": []
                    }))
                    .unwrap(),
                )
                .unwrap();
            let slot = crate::accounts::chatgpt::selected_keyring_slot(id)
                .unwrap()
                .unwrap();
            let response = match case {
                Case::ChatGpt => "exit 0",
                Case::Locked | Case::ResumeLocked => "exit 3",
                Case::Missing | Case::ContinueMissing => "exit 2",
                Case::Secret => "printf synthetic-startup-secret",
                Case::Cancel => "held-helper",
                Case::InvalidRegistration
                | Case::InvalidHistorySameRoot
                | Case::InvalidHistory
                | Case::CorruptHistory
                | Case::CorruptHistoryCancel
                | Case::StaleEventHistory
                | Case::UnsafeHistory => "exit 0",
                Case::InitialSetupFailure
                | Case::InvalidHistorySetup
                | Case::InvalidHistorySetupQuit
                | Case::InvalidChatgpt
                | Case::InvalidChatgptReset
                | Case::InvalidPreferences
                | Case::InvalidPreferencesReset => "exit 1",
                _ => panic!("invalid synthetic startup case"),
            }
            .into();
            ("authorize", slot, response)
        };
        if !matches!(
            case,
            Case::Native | Case::Unconfigured | Case::InvalidNative | Case::InvalidNativeReset
        ) {
            state
                .record(RecordKind::ModelPreferences)
                .replace(
                    &serde_json::to_vec(&serde_json::json!({
                        "version": 1, "selected": { "profile": profile, "account_id": id },
                        "sources": [{ "profile": profile, "account_id": id, "model": "gpt-5.4",
                            "effort": "low", "catalog": ["gpt-5.4"] }]
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        if case.name().starts_with("invalid_chatgpt") {
            let mut record: serde_json::Value = serde_json::from_slice(
                &state
                    .record(RecordKind::ChatgptAccounts)
                    .read()
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            record["accounts"][0]
                .as_object_mut()
                .unwrap()
                .remove("plan_permission_missing");
            state
                .record(RecordKind::ChatgptAccounts)
                .replace(&serde_json::to_vec(&record).unwrap())
                .unwrap();
        }
        if case.name().starts_with("invalid_native") {
            state
                .record(RecordKind::SavedAccount)
                .replace(b"unsupported-api-setup")
                .unwrap();
        }
        if case.name().starts_with("invalid_preferences") {
            state
                .record(RecordKind::ModelPreferences)
                .replace(b"unsupported-model-preferences")
                .unwrap();
        }
        if case == Case::InvalidRegistration {
            state
                .record(RecordKind::ChatgptRegistration)
                .replace(b"unsupported-registration")
                .unwrap();
        }
        if matches!(case, Case::InitialSetupFailure | Case::InvalidHistorySetup) {
            nix::unistd::mkfifo(
                &state.path().join("chatgpt-registration.json"),
                nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
            )
            .unwrap();
        }
        let recovery = case.name().starts_with("invalid_")
            || case.name().contains("_history")
            || case == Case::InitialSetupFailure;
        let before_accounts = if recovery {
            Some((
                state.record(RecordKind::SavedAccount).read().unwrap(),
                state.record(RecordKind::ChatgptAccounts).read().unwrap(),
                state.record(RecordKind::ModelPreferences).read().unwrap(),
            ))
        } else {
            None
        };
        let marker = root
            .join("helper-access")
            .to_str()
            .unwrap()
            .replace('\'', "'\\''");
        let identity = if native && operation == "authorize" {
            format!("[ \"$#\" -eq 5 ] && [ \"$4\" = '{id}' ] && [ \"$5\" = 'openai' ] || exit 1")
        } else {
            "[ \"$#\" -eq 3 ] || exit 1".into()
        };
        let response = if case == Case::Cancel {
            let socket = root
                .join("authorization")
                .to_str()
                .unwrap()
                .replace('\'', "'\\''");
            let executable = std::env::current_exe()
                .unwrap()
                .to_str()
                .unwrap()
                .replace('\'', "'\\''");
            format!(
                "export ARANY_TEST_AUTHORIZATION_SOCKET='{socket}'\nexec '{executable}' --exact accounts::credentials::keyring_helper::tests::held_authorization_helper --ignored --nocapture"
            )
        } else {
            response
        };
        let script = format!(
            "#!/bin/sh\n[ \"$1\" = --internal-credential-helper ] || exit 1\nprintf '%s:%s\\n' \"$2\" \"$3\" >> '{marker}'\nif [ \"$2\" = probe ]; then exit 1; fi\n{identity}\n[ \"$2\" = '{operation}' ] && [ \"$3\" = '{slot}' ] || exit 1\n{response}\n"
        );
        let helper = root.join("credential-helper");
        std::fs::write(&helper, script).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut args = super::super::AttachedArgs {
            workspace: Some(workspace.clone()),
            state_dir: Some(session_path.clone()),
            screen_reader: true,
            ..Default::default()
        };
        args.setup = case == Case::InitialSetupFailure || case.name().contains("_setup");
        if case == Case::InvalidHistorySameRoot {
            args.prompt = Some("synthetic task must remain unsubmitted".into());
            args.continue_session = true;
        }
        let before_history = if recovery {
            let session = arany::create_session(
                StateRoot::admit(&session_path).unwrap(),
                workspace.clone(),
                None,
            )
            .await
            .unwrap();
            let store =
                Store::open_read_only(StateRoot::open_existing(&session_path).unwrap()).unwrap();
            let events = store.load_session(session).await.unwrap();
            store.close().await.unwrap();
            Some((session, events))
        } else {
            None
        };
        if case.name().starts_with("invalid_history") {
            let connection =
                rusqlite::Connection::open(session_path.join("events.sqlite3")).unwrap();
            connection.execute_batch("PRAGMA user_version=3").unwrap();
        }
        if case == Case::StaleEventHistory {
            let connection =
                rusqlite::Connection::open(session_path.join("events.sqlite3")).unwrap();
            connection
                .execute_batch(
                    "UPDATE events SET event_version = event_version - 1 WHERE kind = 'SessionStarted'",
                )
                .unwrap();
        }
        if case.name().starts_with("corrupt_history") {
            std::fs::write(
                session_path.join("events.sqlite3"),
                b"synthetic-broken-history",
            )
            .unwrap();
        }
        let outside = root.join("outside-history");
        if case == Case::UnsafeHistory {
            std::fs::write(&outside, b"outside-stays").unwrap();
            std::os::unix::fs::symlink(&outside, session_path.join("events.sqlite3-journal"))
                .unwrap();
        }
        let before_database = if case.name().contains("history") {
            Some(std::fs::read(session_path.join("events.sqlite3")).unwrap())
        } else {
            None
        };
        if matches!(case, Case::ResumeLocked | Case::ContinueMissing) {
            let session = arany::create_session(
                StateRoot::admit(&session_path).unwrap(),
                workspace.clone(),
                None,
            )
            .await
            .unwrap();
            arany::set_session_defaults(
                StateRoot::open_existing(&session_path).unwrap(),
                workspace.clone(),
                session,
                arany::SessionDefaults {
                    provider: Some("chatgpt".into()),
                    model: Some("gpt-5.4".into()),
                    effort: Some(Effort::Low),
                    account_id: Some(id),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let store =
                Store::open_read_only(StateRoot::open_existing(&session_path).unwrap()).unwrap();
            let before = store.load_session(session).await.unwrap();
            store.close().await.unwrap();
            args.prompt = Some("synthetic task must not be submitted".into());
            if case == Case::ResumeLocked {
                args.resume = Some(Some(session.to_string()));
            } else {
                args.continue_session = true;
            }
            let error = super::super::run(args, arany::Telemetry::disabled())
                .await
                .unwrap_err();
            assert!(error.contains(if case == Case::ResumeLocked {
                "OS credential store is locked"
            } else {
                "Saved sign-in is missing"
            }));
            eprintln!("Startup rejected: {error}");
            let store =
                Store::open_read_only(StateRoot::open_existing(&session_path).unwrap()).unwrap();
            assert_eq!(
                store.load_session(session).await.unwrap(),
                before,
                "denied startup must preserve the closed Event prefix"
            );
            store.close().await.unwrap();
            return;
        }
        super::super::run(args, arany::Telemetry::disabled())
            .await
            .unwrap();
        if case == Case::Cancel {
            assert!(
                !session_path.exists(),
                "cancelled startup must not create Session state"
            );
            eprintln!("Startup authorization cancelled");
            return;
        }
        if let Some((session, events)) = before_history {
            if let Some(database) = before_database {
                if case.name().ends_with("_cancel") || case == Case::UnsafeHistory {
                    assert!(
                        std::fs::read(session_path.join("events.sqlite3")).unwrap() == database
                    );
                    if case == Case::UnsafeHistory {
                        assert_eq!(std::fs::read(outside).unwrap(), b"outside-stays");
                    }
                } else {
                    let store =
                        Store::open_read_only(StateRoot::open_existing(&session_path).unwrap())
                            .unwrap();
                    assert!(store.load_session(session).await.unwrap().is_empty());
                    store.close().await.unwrap();
                    assert!(
                        arany::list_sessions(
                            StateRoot::open_existing(&session_path).unwrap(),
                            workspace.clone(),
                        )
                        .await
                        .unwrap()
                        .is_empty()
                    );
                    let archive = session_path.join("archive");
                    if case.archives_history() {
                        let archived = std::fs::read_dir(&archive)
                            .unwrap()
                            .map(|entry| entry.unwrap().path())
                            .filter(|path| {
                                path.file_name()
                                    .unwrap()
                                    .to_str()
                                    .unwrap()
                                    .starts_with("history-")
                            })
                            .collect::<Vec<_>>();
                        assert_eq!(archived.len(), 1, "one archive for {case}");
                        assert!(
                            std::fs::read(archived[0].join("events.sqlite3")).unwrap() == database,
                            "archived history stays intact"
                        );
                        // Only a drawn archive notice is acknowledged into the manifest.
                        let drawn = case != Case::InvalidHistorySetupQuit;
                        assert_eq!(archived[0].join("manifest.json").exists(), drawn, "{case}");
                        assert_eq!(archive.join("pending.json").exists(), !drawn, "{case}");
                    } else {
                        assert!(!archive.exists(), "manual reset never archives");
                    }
                }
                assert!(
                    state.record(RecordKind::ChatgptAccounts).read().unwrap()
                        == before_accounts.unwrap().1
                );
                // Startup re-remembers the selection, so only presence is stable here.
                assert!(
                    state
                        .record(RecordKind::ModelPreferences)
                        .present()
                        .unwrap()
                );
                assert!(trust_path.exists());
                return;
            }
            let store =
                Store::open_read_only(StateRoot::open_existing(&session_path).unwrap()).unwrap();
            let current = store.load_session(session).await.unwrap();
            assert_eq!(current, events, "recovery preserves closed history");
            assert!(
                SessionView::replay(session, &current)
                    .unwrap()
                    .unwrap()
                    .runs
                    .is_empty()
            );
            store.close().await.unwrap();
            let (native_before, chatgpt_before, models_before) = before_accounts.unwrap();
            let reset = case.name().ends_with("_reset");
            assert!(
                state.record(RecordKind::SavedAccount).read().unwrap()
                    == if reset && native { None } else { native_before }
            );
            assert!(
                state.record(RecordKind::ChatgptAccounts).read().unwrap()
                    == if reset && case.name().starts_with("invalid_chatgpt") {
                        None
                    } else {
                        chatgpt_before
                    }
            );
            if case == Case::InvalidRegistration {
                assert!(
                    state
                        .record(RecordKind::ChatgptRegistration)
                        .read()
                        .unwrap()
                        == Some(b"unsupported-registration".to_vec())
                );
            } else {
                assert!(
                    state.record(RecordKind::ModelPreferences).read().unwrap()
                        == if reset { None } else { models_before }
                );
            }
            return;
        }
        assert!(
            arany::list_sessions(StateRoot::open_existing(&session_path).unwrap(), workspace,)
                .await
                .unwrap()
                .is_empty(),
            "startup and quit must create no canonical Session or Run"
        );
    }

    #[cfg(target_os = "macos")]
    struct Greeting {
        identity: ProviderIdentity,
        observed: Arc<Mutex<Vec<ProviderRequest>>>,
    }

    #[cfg(target_os = "macos")]
    impl Greeting {
        fn new(
            profile: &'static str,
            account_id: Uuid,
            observed: Arc<Mutex<Vec<ProviderRequest>>>,
        ) -> Self {
            let mut identity =
                ProviderIdentity::new(profile, "synthetic-model", arany::Provenance::native());
            identity.effort = Some(Effort::Low);
            if profile == "chatgpt" {
                identity.provenance = Provenance::ChatGpt(ChatGptProvenance {
                    account_id,
                    evidence_fingerprint: [1; 32],
                    admission: ChatGptAdmission::AccountConsent,
                });
                identity.limits.output_token_bound = OutputTokenBound::LocalAcceptanceOnly;
            }
            Self { identity, observed }
        }
    }

    #[cfg(target_os = "macos")]
    #[async_trait::async_trait]
    impl Provider for Greeting {
        fn identity(&self) -> &ProviderIdentity {
            &self.identity
        }
        async fn invoke(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            self.observed.lock().unwrap().push(request);
            Ok(ProviderResponse {
                turn: arany::ProviderTurn::scripted(),
                outcome: ProviderOutcome::Answer(Answer {
                    text: "Hola".into(),
                }),
                response_id: None,
                usage: arany::Usage {
                    input_tokens: Some(1),
                    output_tokens: Some(1),
                    ..Default::default()
                },
                wire_provenance: None,
            })
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn native_chat_preserves_turns_with_guarded_tools_and_rejects_invalid_admission() {
        const FIXTURE: &str = "ARANY_TEST_NATIVE_CHAT_ROOT";
        const TEST: &str = "cli::attached::run::tests::native_chat_preserves_turns_with_guarded_tools_and_rejects_invalid_admission";
        let Some(root) = std::env::var_os(FIXTURE) else {
            let temp = tempfile::tempdir().unwrap();
            let (master, slave) = crate::test_process::cloexec_pty(None);
            let mut input = master.try_clone().unwrap();
            let mut cancelled = false;
            let mut recovery_prompts = 0;
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .env_clear()
                .env(FIXTURE, temp.path())
                .env("HOME", temp.path().join("fixture-home"))
                .env("XDG_STATE_HOME", temp.path().join("fixture-state"))
                .env("XDG_DATA_HOME", temp.path().join("fixture-data"))
                .env("TERM", "dumb")
                .current_dir(temp.path())
                .args(["--exact", TEST, "--nocapture"])
                .process_group(0)
                .stdin(Stdio::from(slave.try_clone().unwrap()))
                .stdout(Stdio::piped())
                .stderr(Stdio::from(slave));
            command.env("ARANY_TEST_ACCOUNT_ROOT", temp.path().join("account"));
            let mut child = OwnedChild(command.spawn().unwrap());
            drop(command);
            let output = crate::test_process::capture_terminal(
                &mut child.0,
                master,
                Duration::from_secs(30),
                64 * 1024,
                |bytes| {
                    let text = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
                    let prompts = text.matches("Submission recovery").count();
                    if prompts > recovery_prompts {
                        recovery_prompts = prompts;
                        match prompts {
                            1 => input.write_all(b"\n").unwrap(),
                            2 => {
                                std::fs::rename(
                                    temp.path().join("recovery-state"),
                                    temp.path().join("openai"),
                                )
                                .unwrap();
                                input.write_all(b"\n").unwrap();
                            }
                            3 => input.write_all(b"Exit (discard local draft)\n").unwrap(),
                            _ => panic!("unexpected submission recovery prompt"),
                        }
                        return;
                    }
                    if cancelled {
                        return;
                    }
                    // Release builds reject the account override, so startup offers recovery instead of consent.
                    if text.contains(crate::test_process::READ_ONLY_CONSENT_PROMPT) {
                        input.write_all(b"\x04").unwrap();
                        cancelled = true;
                    } else if text.contains("Folder trust recovery") {
                        // Linear mode reads whole lines, so a lone Escape never arrives.
                        input.write_all(b"\x04").unwrap();
                        cancelled = true;
                    }
                },
            );
            assert!(
                output.status.success(),
                "native chat regression: stdout {}; terminal {}",
                crate::presentation::escape_terminal(&String::from_utf8_lossy(&output.stdout)),
                crate::presentation::escape_terminal(&String::from_utf8_lossy(&output.stderr))
            );
            crate::test_process::assert_single_test_completed(
                &output.stdout,
                "native chat regression",
            );
            assert!(cancelled, "startup must reach consent or recovery and exit");
            assert_eq!(recovery_prompts, 3, "history retry, recovery, and exit");
            return;
        };
        let root = std::path::PathBuf::from(root);
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let account = StateRoot::admit(&root.join("account")).unwrap();
        let metadata = std::fs::metadata(&workspace).unwrap();
        let remembered = serde_json::to_vec(&serde_json::json!({
            "version": 2, "workspaces": [{ "path": workspace,
                "device": metadata.dev(), "inode": metadata.ino(), "mode": "auto_edits",
                "runtime_grants": [] }]
        }))
        .unwrap();
        let trust_record = account.path().join("workspace-permissions.json");
        std::fs::write(&trust_record, &remembered).unwrap();
        std::fs::set_permissions(&trust_record, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut terminal = AttachedTerminal::acquire_with_mode(
            true,
            crate::terminal::TerminalMode::default(),
            Default::default(),
        )
        .unwrap();
        let mut composer = Composer::default();
        composer.apply(crate::terminal::TerminalInput::Character('x'));
        composer.set_approval_mode(Some(ApprovalMode::AutoEdits));
        let choice =
            super::super::permissions::admit(&mut terminal, &workspace, &mut composer, false).await;
        #[cfg(debug_assertions)]
        assert!(
            matches!(&choice, Ok(super::super::permissions::PermissionChoice::Selected(access)) if access.is_trusted())
        );
        #[cfg(not(debug_assertions))]
        assert!(
            matches!(
                &choice,
                Ok(super::super::permissions::PermissionChoice::Exit)
            ),
            "optimized account override must end at recovery before host access"
        );
        let permissions = match &choice {
            Ok(super::super::permissions::PermissionChoice::Selected(access)) => Some(access),
            _ => None,
        };
        let (approvals, _inbox) = arany::ToolApprovals::new(ApprovalMode::AutoEdits);
        for profile in ["chatgpt", "openai", "anthropic"] {
            let state = root.join(profile);
            let account_id = Uuid::now_v7();
            let observed = Arc::new(Mutex::new(Vec::new()));
            let engine = arany::test_support::engine::open(
                StateRoot::admit(&state).unwrap(),
                arany::EngineConfig {
                    guard_executable: std::env::current_exe().ok(),
                    ..Default::default()
                },
            )
            .unwrap();
            let mut session = None;
            for _ in 0..2 {
                let outcome = submit_with_providers(
                    &engine,
                    AdmittedProviders::single(Arc::new(Greeting::new(
                        profile,
                        account_id,
                        observed.clone(),
                    ))),
                    RunRequest {
                        skill: None,
                        session_id: session,
                        title: None,
                        objective: "hola hola".into(),
                        images: Vec::new(),
                        workspace: workspace.clone(),
                        launch_directory: None,
                        include_paths: Vec::new(),
                        policy: CollaborationPolicy::Single,
                        goal: arany::GoalRequest::default(),
                    },
                    RunCancellation::new(),
                    RunHints::new(),
                    ToolAccess {
                        configured: false,
                        permissions,
                        approvals: &approvals,
                    },
                    RunSteering::new(),
                )
                .expect("ordinary greeting retains the admitted native file capability")
                .await
                .map(|result| result.run)
                .expect("ordinary greeting Run finishes");
                session = Some(outcome.session_id);
                assert_eq!(outcome.run.status, RunStatus::Finished);
            }
            {
                let requests = observed.lock().unwrap();
                assert_eq!(requests.len(), 2, "exact greeting call count");
                assert!(
                    requests
                        .iter()
                        .all(|request| request.phase == AgentPhase::RootPlan
                            && request.input.objective == "hola hola"
                            && request.input.tools.is_some() == permissions.is_some()
                            && request.collaboration == CollaborationPolicy::Single
                            && request.model == "synthetic-model"
                            && request.input.workspace_guidance.is_empty()
                            && request.input.includes.is_empty()
                            && request.input.images.is_empty()
                            && request.input.child_results.is_empty()
                            && request.input.context_summary.is_none())
                );
                assert!(requests[0].input.history.is_empty());
                assert_eq!(
                    requests[1].input.history.len(),
                    1,
                    "resumed greeting retains its accepted turn"
                );
                assert_eq!(requests[1].input.history[0].user, "hola hola");
                assert_eq!(requests[1].input.history[0].assistant, "Hola");
            }
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            let events = store.load_session(session.unwrap()).await.unwrap();
            let view = SessionView::replay(session.unwrap(), &events)
                .unwrap()
                .unwrap();
            assert_eq!(view.runs.len(), 2);
            assert!(view.runs.iter().all(|run| run.status == RunStatus::Finished
                && run.objective == "hola hola"
                && run.assistant_message.as_deref() == Some("Hola")
                && run.tools.is_empty()));
            store.close().await.unwrap();
            let private = state.join("tools.json");
            std::fs::write(&private, serde_json::to_vec(&serde_json::json!({"version":1,"workspace_paths":["."],"write":true,
                "commands":[{"name":"sh","executable":"/bin/sh","sha256":"0".repeat(64),"interpreter":true,"inputs":[]}],"skills":[],"mcp":[]})).unwrap()).unwrap();
            std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600)).unwrap();
            let error = match submit_with_providers(
                &engine,
                AdmittedProviders::single(Arc::new(Greeting::new(
                    profile,
                    account_id,
                    observed.clone(),
                ))),
                RunRequest {
                    skill: None,
                    session_id: session,
                    title: None,
                    objective: "explicit Tools".into(),
                    images: Vec::new(),
                    workspace: workspace.clone(),
                    launch_directory: None,
                    include_paths: Vec::new(),
                    policy: CollaborationPolicy::Single,
                    goal: arany::GoalRequest::default(),
                },
                RunCancellation::new(),
                RunHints::new(),
                ToolAccess {
                    configured: true,
                    permissions: None,
                    approvals: &approvals,
                },
                RunSteering::new(),
            ) {
                Ok(handle) => finish_attempt(handle.await, false)
                    .expect_err("explicit Tool admission must reject before inference"),
                Err(error) => error,
            };
            #[cfg(debug_assertions)]
            assert!(
                matches!(
                    error,
                    RunAttemptError::Failed(RunFailure::Engine(arany::EngineError::Tool(
                        arany::ToolError::ChangedInput
                    )))
                ),
                "the admitted private account fixture must reach stale executable-pin rejection"
            );
            #[cfg(not(debug_assertions))]
            assert!(
                matches!(
                    error,
                    RunAttemptError::Failed(RunFailure::Engine(arany::EngineError::Store(
                        arany::StoreError::InvalidStateDirectory
                    )))
                ),
                "the optimized account override must reject from the Workspace lease before Tool discovery or Provider use"
            );
            assert_eq!(
                observed.lock().unwrap().len(),
                2,
                "rejection must make no Provider call"
            );
            let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
            assert_eq!(
                store.load_session(session.unwrap()).await.unwrap(),
                events,
                "explicit rejection preserves the committed prefix"
            );
            store.close().await.unwrap();
            engine.close().await.unwrap();
            if profile == "openai" {
                let mut recovery_view = view.clone();
                let mut recovery_composer = Composer::default();
                recovery_composer
                    .insert_paste(&"payload ".repeat(200))
                    .unwrap();
                recovery_composer.insert_paste(" @visible.txt").unwrap();
                recovery_composer
                    .attach_image(arany::test_support::test_image())
                    .unwrap();
                let submitted = recovery_composer.take_draft();
                let backup = root.join("recovery-state");
                std::fs::rename(&state, &backup).unwrap();
                super::super::reconcile_failed_submission(
                    &mut terminal,
                    &state,
                    &mut recovery_view,
                    &mut recovery_composer,
                    submitted.clone(),
                    view.last_sequence,
                )
                .await
                .unwrap();
                assert_eq!(recovery_composer.take_draft(), submitted);
                super::super::reconcile_failed_submission(
                    &mut terminal,
                    &state,
                    &mut recovery_view,
                    &mut recovery_composer,
                    submitted.clone(),
                    0,
                )
                .await
                .unwrap();
                assert!(
                    recovery_composer.is_empty(),
                    "accepted draft cannot be retried"
                );
                std::fs::rename(&state, &backup).unwrap();
                let error = super::super::reconcile_failed_submission(
                    &mut terminal,
                    &state,
                    &mut recovery_view,
                    &mut recovery_composer,
                    submitted,
                    view.last_sequence,
                )
                .await
                .unwrap_err();
                assert!(error.contains("Acceptance is unknown"));
                assert!(
                    recovery_composer.is_empty(),
                    "unknown draft cannot be retried"
                );
                std::fs::rename(&backup, &state).unwrap();
                assert_eq!(
                    observed.lock().unwrap().len(),
                    2,
                    "recovery makes no Provider call"
                );
                assert_eq!(recovery_view.runs.len(), 2);
            }
        }
        let choice = match arany::WorkspacePermissions::open(&workspace) {
            Ok(access) => Some(
                tokio::time::timeout(
                    Duration::from_secs(3),
                    super::super::permissions::choose(
                        &mut terminal,
                        access.prepare_profile_selection(),
                        &mut composer,
                        false,
                    ),
                )
                .await
                .unwrap(),
            ),
            Err(_) => None,
        };
        #[cfg(debug_assertions)]
        assert!(matches!(
            choice,
            Some(Ok(super::super::permissions::PermissionChoice::Exit))
        ));
        #[cfg(not(debug_assertions))]
        assert!(choice.is_none(), "release rejects the test account root");
        assert_eq!(
            super::super::permissions::cycle(&mut composer),
            if permissions.is_some() {
                "Request approvals · Shift+Tab changes mode"
            } else {
                "Read only · use /permissions to trust this folder"
            }
        );
        assert_eq!(composer.text(), "x");
        assert_eq!(
            composer.approval_mode(),
            permissions.map(|_| ApprovalMode::Request)
        );
        assert_eq!(std::fs::read(&trust_record).unwrap(), remembered);
        drop(terminal);
    }
}

#[cfg(test)]
mod burst_tests {
    use super::*;
    use arany::test_support::{
        engine::{open, run_to_end},
        scenario::{ScriptedProvider, request},
    };
    use arany::{
        Answer, ChildAssignment, CollaborationPolicy, Delegate, EngineConfig, ProviderOutcome,
        StateRoot, Store, Tapped,
    };
    use std::{collections::VecDeque, sync::Arc};

    // A team Run with two children, then single Runs, so the journal outgrows one burst.
    async fn committed_team() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        Vec<arany::EventEnvelope>,
    ) {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(workspace.join("notes")).unwrap();
        std::fs::write(workspace.join("notes/facts.txt"), "Fact").unwrap();
        let state_dir = root.path().join("state");
        let engine = open(
            StateRoot::admit(&state_dir).unwrap(),
            EngineConfig::default(),
        )
        .unwrap();
        let answer = |text: &str| ProviderOutcome::Answer(Answer { text: text.into() });
        let child = |objective: &str| ChildAssignment {
            editor: None,
            objective: objective.into(),
            profile: None,
        };
        let mut responses = vec![
            ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: vec![child("first task"), child("second task")],
            }),
            answer("first result"),
            answer("second result"),
            answer("team answer"),
        ];
        responses.extend((0..8).map(|_| answer("later answer")));
        let mut provider = ScriptedProvider::new(responses);
        // One call at a time, so each child takes its scripted answer in spawn order.
        provider.identity.limits.concurrency = 1;
        let provider = Arc::new(provider);
        let mut team = request(&workspace, "team task");
        team.policy = CollaborationPolicy::Team {
            max_active_children: 2,
        };
        let session = run_to_end(&engine, &provider, team)
            .await
            .unwrap()
            .session_id;
        for _ in 0..8 {
            let mut later = request(&workspace, "later task");
            later.session_id = Some(session);
            later.title = None;
            run_to_end(&engine, &provider, later).await.unwrap();
        }
        engine.close().await.unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&state_dir).unwrap()).unwrap();
        let events = store.load_session(session).await.unwrap();
        store.close().await.unwrap();
        (root, state_dir, events)
    }

    fn tapped(envelope: &arany::EventEnvelope) -> Tapped {
        Tapped::Committed(Arc::new(envelope.clone()))
    }

    fn replay(events: &[arany::EventEnvelope]) -> SessionView {
        SessionView::replay(events[0].session_id, events)
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn committed_events_apply_in_bounded_in_order_bursts_and_gaps_reload_the_view() {
        let (_root, state_dir, events) = committed_team().await;
        let full = replay(&events);

        // Commit order, one Event at a time, reaches the strictly replayed view.
        let mut view = replay(&events[..1]);
        for envelope in &events[1..] {
            apply_tapped(&state_dir, &mut view, tapped(envelope))
                .await
                .unwrap();
            assert_eq!(view.last_sequence, envelope.sequence);
        }
        assert_eq!(view.runs, full.runs);

        // One burst applies at most EVENT_DRAIN_LIMIT ready Events and leaves the rest queued.
        let mut queue = events[1..].iter().map(tapped).collect::<VecDeque<_>>();
        assert!(queue.len() > EVENT_DRAIN_LIMIT);
        let mut view = replay(&events[..1]);
        let first = queue.pop_front();
        apply_burst(&state_dir, &mut view, first, async || {
            queue.pop_front().map(Some)
        })
        .await
        .unwrap();
        assert_eq!(view.last_sequence, events[EVENT_DRAIN_LIMIT].sequence);
        assert_eq!(queue.len(), events.len() - 1 - EVENT_DRAIN_LIMIT);
        // Nothing else ready ends the burst after its first Event.
        let first = queue.pop_front();
        apply_burst(&state_dir, &mut view, first, async || None)
            .await
            .unwrap();
        assert_eq!(view.last_sequence, events[EVENT_DRAIN_LIMIT + 1].sequence);

        // A sequence gap or a resync reloads the whole validated committed view.
        let gap = tapped(&events[EVENT_DRAIN_LIMIT + 3]);
        for missed in [Some(gap), Some(Tapped::Resync { missed: 3 })] {
            let mut view = replay(&events[..=EVENT_DRAIN_LIMIT]);
            apply_burst(&state_dir, &mut view, missed, async || None)
                .await
                .unwrap();
            assert_eq!(view.last_sequence, full.last_sequence);
            assert_eq!(view.runs, full.runs);
        }
        // A closed stream or unreadable history stops the Run view instead of inventing progress.
        let mut view = replay(&events[..1]);
        for (state, event, expected) in [
            (state_dir.as_path(), None, "Run event stream closed"),
            (
                std::path::Path::new("/nonexistent-arany-state"),
                Some(Tapped::Resync { missed: 1 }),
                "Run history unavailable; check Session history",
            ),
        ] {
            assert_eq!(
                apply_burst(state, &mut view, event, async || None).await,
                Err(expected.to_owned())
            );
        }
        assert_eq!(view.last_sequence, events[0].sequence);
    }
}

#[cfg(test)]
mod escape_tests {
    use super::*;

    #[test]
    fn run_end_notice_never_claims_a_dropped_note_was_kept() {
        assert_eq!(
            run_end_notice(false, true, false),
            Some(
                "Drafts exceed the combined limit; both kept; submit or clear this draft, then press Up"
            )
        );
        assert_eq!(
            run_end_notice(false, false, false),
            Some(
                "Drafts exceed the combined limit; both kept; submit or clear this draft, then press Up"
            )
        );
        assert_eq!(
            run_end_notice(false, true, true),
            Some("Steering note withdrawn; draft retained")
        );
        assert_eq!(run_end_notice(true, false, true), Some(STEERING_SENT));
        assert_eq!(run_end_notice(false, false, true), None);
    }

    #[test]
    fn escape_cancels_only_when_nothing_else_claims_it() {
        let idle = |input, menu, clipboard, stop| active_escape(input, menu, clipboard, stop);
        assert_eq!(
            idle(TerminalInput::Escape, false, false, ActiveStop::Running),
            TerminalInput::Interrupt
        );
        assert_eq!(
            idle(TerminalInput::Escape, true, false, ActiveStop::Running),
            TerminalInput::Escape,
            "an open menu closes first"
        );
        assert_eq!(
            idle(TerminalInput::Escape, false, true, ActiveStop::Running),
            TerminalInput::Escape,
            "a pending clipboard read is cancelled, not the Run"
        );
        assert_eq!(
            idle(TerminalInput::Escape, false, false, ActiveStop::Cancelling),
            TerminalInput::Escape,
            "a second Escape never forces shutdown"
        );
        assert_eq!(
            idle(TerminalInput::Interrupt, false, false, ActiveStop::Running),
            TerminalInput::Interrupt
        );
    }
}

#[cfg(test)]
mod approval_reply_tests {
    use super::approval::{linear_approval_choice, withdraw_abandoned_approval};
    use crate::presentation::ToolApprovalChoice::{
        AutoEdits, ExactCommand, GitPrefix, HostPrefix, No, Yes,
    };
    use crate::terminal::{Composer, TerminalInput};
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn child_timeout_withdraws_the_dialog_and_discards_its_partial_answer() {
        tokio::time::timeout(Duration::from_secs(60), async {
            let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
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
                call: arany::ToolCall::Mkdir {
                    path: "directory".into(),
                },
                limits: arany::ToolLimits::default(),
                use_count: 1,
            };
            let child_approvals = approvals.clone();
            let child_intent = intent.clone();
            let child = tokio::spawn(async move {
                tokio::time::timeout(
                    Duration::from_secs(30),
                    arany::test_support::approval::ask(&child_approvals, &child_intent),
                )
                .await
            });
            let mut pending = Some(inbox.next().await.expect("child approval"));
            let mut answer = Composer::default();
            answer.apply(TerminalInput::Character('y'));
            assert!(
                tokio::time::timeout(
                    Duration::ZERO,
                    withdraw_abandoned_approval(&mut pending, &mut answer),
                )
                .await
                .is_err(),
                "a live approval stays answerable"
            );
            assert!(pending.is_some());
            assert_eq!(answer.text(), "y");
            tokio::time::advance(Duration::from_secs(31)).await;
            assert!(child.await.expect("child waiter").is_err());
            withdraw_abandoned_approval(&mut pending, &mut answer).await;
            assert!(pending.is_none(), "the dialog releases approval input");
            assert_eq!(
                answer.text(),
                "",
                "partial answers cannot reach a later approval"
            );
            let (approved, ()) = tokio::join!(
                arany::test_support::approval::ask(&approvals, &intent),
                async {
                    inbox.next().await.expect("next approval").decide(true);
                },
            );
            assert!(approved, "the parent can continue with a fresh approval");
        })
        .await
        .expect("approval withdrawal deadline");
    }

    #[test]
    fn numbered_replies_use_choice_identity_and_refuse_unavailable_choices() {
        let prefix = GitPrefix(arany::permissions::TypedGitPrefix::Branch);
        for choices in [
            vec![Yes, prefix, No],
            vec![Yes, No],
            vec![Yes, AutoEdits, No],
            vec![Yes, ExactCommand, No],
            vec![Yes, HostPrefix, No],
        ] {
            for (index, choice) in choices.iter().enumerate() {
                assert_eq!(
                    linear_approval_choice(&(index + 1).to_string(), &choices),
                    Some(*choice)
                );
            }
            for (answer, expected) in [
                ("", Some(No)),
                ("YES", Some(Yes)),
                ("no", Some(No)),
                ("allow", None),
                ("always", None),
                ("0", None),
                ("4", None),
            ] {
                assert_eq!(linear_approval_choice(answer, &choices), expected);
            }
        }
        assert_eq!(AutoEdits.reply(), arany::ApprovalReply::AllowOnce);
        assert_eq!(ExactCommand.reply(), arany::ApprovalReply::AllowForSession);
        assert_eq!(No.reply(), arany::ApprovalReply::Deny);
        assert_eq!(prefix.reply(), arany::ApprovalReply::AllowPrefixInFolder);
        assert_eq!(
            HostPrefix.reply(),
            arany::ApprovalReply::AllowPrefixInFolder
        );
    }
}

#[cfg(test)]
mod driver_tests {
    use super::super::active::session_with_goal;
    use super::approval::interrupt_approval_input;
    use super::*;
    use crate::terminal::ComposerEdit;
    use arany::SteeringRefused;

    #[test]
    fn approval_end_of_input_and_escape_cancel_without_a_decline() {
        for input in [TerminalInput::EndOfInput, TerminalInput::Escape] {
            let cancellation = arany::RunCancellation::new();
            let mut stop = ActiveStop::Running;
            assert!(interrupt_approval_input(&input, &cancellation, &mut stop));
            assert!(stop == ActiveStop::Cancelling);
            assert!(
                !cancellation.cancel(),
                "interruption already cancelled the Run"
            );
        }
        let cancellation = arany::RunCancellation::new();
        let mut stop = ActiveStop::Running;
        assert!(!interrupt_approval_input(
            &TerminalInput::Submit,
            &cancellation,
            &mut stop
        ));
        assert!(stop == ActiveStop::Running);
        assert!(
            cancellation.cancel(),
            "ordinary approval input did not cancel"
        );
    }

    #[test]
    fn end_of_input_waits_for_only_the_current_run_to_settle() {
        let mut view = session_with_goal(false);
        let starting_sequence = view.last_sequence;
        assert!(!run_settled(&view, starting_sequence), "no current Run");
        let mut run = arany::RunView {
            skill: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: arany::RunId::new(),
            objective: "Fix".into(),
            images: Vec::new(),
            config: None,
            provider_fallbacks: Vec::new(),
            provider_retries: Vec::new(),
            agents: Vec::new(),
            assistant_message: None,
            status: arany::RunStatus::Finished,
            accepted_sequence: starting_sequence,
            finished_sequence: Some(starting_sequence),
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
        view.runs.push(run.clone());
        assert!(
            !run_settled(&view, starting_sequence),
            "an earlier Run cannot defer exit"
        );
        run.accepted_sequence = starting_sequence + 1;
        for (status, settled) in [
            (arany::RunStatus::Pending, false),
            (arany::RunStatus::Active, false),
            (arany::RunStatus::Finished, true),
            (arany::RunStatus::Failed, true),
            (arany::RunStatus::Cancelled, true),
        ] {
            run.status = status;
            *view.runs.last_mut().unwrap() = run.clone();
            assert_eq!(run_settled(&view, starting_sequence), settled, "{status:?}");
        }
    }

    #[test]
    fn exit_waits_for_only_the_current_run_to_settle() {
        let mut view = session_with_goal(false);
        let starting_sequence = view.last_sequence;
        let typed = |text: &str| {
            let mut composer = Composer::default();
            for character in text.chars() {
                composer.apply(TerminalInput::Character(character));
            }
            composer
        };
        let exit = typed("/exit");
        assert!(
            !exits_after_settled_run(&exit, &view, starting_sequence),
            "no current Run"
        );
        let mut run = arany::RunView {
            skill: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: arany::RunId::new(),
            objective: "Fix".into(),
            images: Vec::new(),
            config: None,
            provider_fallbacks: Vec::new(),
            provider_retries: Vec::new(),
            agents: Vec::new(),
            assistant_message: None,
            status: arany::RunStatus::Active,
            accepted_sequence: starting_sequence + 1,
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
        };
        view.runs.push(run.clone());
        assert!(
            !exits_after_settled_run(&exit, &view, starting_sequence),
            "an active Run keeps /exit refused"
        );
        run.status = arany::RunStatus::Finished;
        *view.runs.last_mut().unwrap() = run;
        assert!(exits_after_settled_run(&exit, &view, starting_sequence));
        for other in ["/exit now", "/status", "exit"] {
            assert!(
                !exits_after_settled_run(&typed(other), &view, starting_sequence),
                "{other}"
            );
        }
    }

    #[test]
    fn exit_abandons_a_held_linear_draft_and_waiting_note() {
        let mut composer = Composer::default();
        for character in "refused draft".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        let mut held = hold_draft(&mut composer);
        assert!(held.is_some(), "a refused linear draft is held");
        let steering = RunSteering::default();
        steering
            .offer_note("waiting note".into())
            .expect("note waits");
        for character in "/exit".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        abandon_drafts_for_exit(&mut composer, &mut held, &steering);
        assert!(composer.is_empty());
        // Run end hands back nothing, so the deferred Ctrl+D exits instead of retaining a draft.
        assert!(steering.withdraw_note().is_none());
        assert!(held.is_none());
    }

    #[test]
    fn steering_refusals_name_the_retained_draft_without_echoing_text() {
        assert_eq!(
            super::input::steering_refusal(SteeringRefused::TooLarge, false),
            "Steering note exceeds 8.0 KB; draft retained"
        );
        assert_eq!(
            super::input::steering_refusal(SteeringRefused::Unavailable, true),
            "Steering note unavailable; draft retained"
        );
        assert_eq!(
            super::input::steering_refusal(SteeringRefused::RunFull, true),
            "No room for more steering notes this Run. Draft retained; press Enter after this Goal to submit"
        );
    }

    #[test]
    fn recorded_note_clears_only_an_unchanged_composer() {
        let mut seen = 0;
        let mut composer = Composer::default();
        for character in "first note".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        let mut offered = Some(composer.text().to_owned());
        assert!(!settle_recorded_note(
            0,
            &mut seen,
            &mut offered,
            &mut composer
        ));
        assert!(settle_recorded_note(
            1,
            &mut seen,
            &mut offered,
            &mut composer
        ));
        assert!(
            composer.is_empty(),
            "an unchanged offer clears the composer"
        );
        assert!(offered.is_none());
        assert!(!settle_recorded_note(
            1,
            &mut seen,
            &mut offered,
            &mut composer
        ));

        // An edit after the Engine took the note keeps the user's text as a retained draft.
        for character in "second".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        let mut offered = Some("second".to_owned());
        composer.apply(TerminalInput::Character('!'));
        assert!(settle_recorded_note(
            2,
            &mut seen,
            &mut offered,
            &mut composer
        ));
        assert_eq!(composer.text(), "second!");
    }

    #[test]
    fn held_linear_draft_waits_aside_and_returns_before_later_text() {
        let mut composer = Composer::default();
        for character in "kept".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        let held = hold_draft(&mut composer).expect("refused draft is held");
        assert!(
            composer.is_empty(),
            "the invisible draft leaves the composer"
        );
        assert!(hold_draft(&mut composer).is_none());

        for character in "later".chars() {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(release_held_draft(&mut composer, held));
        assert_eq!(composer.text(), "kept\nlater");
    }

    #[test]
    fn overflowing_release_keeps_the_newer_draft() {
        let mut composer = Composer::default();
        composer.insert_paste(&"a".repeat(64 * 1024)).unwrap();
        let held = hold_draft(&mut composer).expect("refused draft is held");
        for character in std::iter::repeat_n('b', 4000) {
            composer.apply(TerminalInput::Character(character));
        }
        assert!(!release_held_draft(&mut composer, held));
        assert_eq!(composer.text(), "b".repeat(4000));
        assert_eq!(
            composer.apply(TerminalInput::Character('c')),
            ComposerEdit::AtCapacity
        );
        composer.clear();
        composer.apply(TerminalInput::Up);
        assert_eq!(composer.text(), "a".repeat(64 * 1024));
        assert!(composer.has_text_pastes());
    }

    #[test]
    fn retained_draft_names_the_autonomous_goal_boundary() {
        assert_eq!(
            draft_retained_notice(true),
            "Draft retained; press Enter after this Goal to submit"
        );
        assert_eq!(
            draft_retained_notice(false),
            "Draft retained; press Enter after this Run to submit"
        );
    }
}

use super::run::{PreparedSubmission, RunProgress, RunSubmission, RunUi, run_attached};
use super::{
    Admission, Feedback, append_notice, confirm_outcome, hidden_answer_guidance, load_view,
    paused_goal_notice, print_run_answer, reload_view_or_keep, retained_draft_notice, run,
    show_command, submission_notice,
};
use crate::accounts::chatgpt;
use crate::cli::provider::select::{Selection, validate_local_selection};
use crate::terminal::{
    AttachedTerminal, Composer, ComposerDraft, ComposerEdit, ShutdownSignal, TerminalInput,
    TerminalNotice,
};
#[cfg(test)]
use arany::WorkspaceIdentity;
use arany::{
    CompactionFailure, CompactionRecord, CompactionStatus, ProviderKind, RunOutcome, RunRequest,
    RunStatus, SessionView,
};
use std::future::Future;

pub(super) enum CompactionOutcome {
    Notice(String),
    Unavailable(String),
    Interrupted(String),
    Shutdown(ShutdownSignal),
}

fn submission_includes(
    admission: &Admission,
    view: &SessionView,
    start: GoalStart,
    text: &str,
) -> Result<Vec<std::path::PathBuf>, String> {
    if start == GoalStart::Continue {
        return continuation_includes(view);
    }
    if start == GoalStart::Resume {
        return view
            .interrupted_goal_id()
            .and_then(|id| view.goal(id))
            .and_then(|goal| goal.include_paths.as_ref())
            .map(|paths| paths.iter().map(std::path::PathBuf::from).collect())
            .ok_or_else(|| arany::EngineError::GoalIncludesUnavailable.to_string());
    }
    objective_includes(admission, text)
}

fn continuation_includes(view: &SessionView) -> Result<Vec<std::path::PathBuf>, String> {
    let source = view
        .continuable_goal()
        .ok_or("No exhausted Goal is available to continue")?;
    source
        .include_paths
        .as_ref()
        .map(|paths| paths.iter().map(std::path::PathBuf::from).collect())
        .ok_or_else(|| arany::EngineError::GoalIncludesUnavailable.to_string())
}

pub(super) fn objective_includes(
    admission: &Admission,
    objective: &str,
) -> Result<Vec<std::path::PathBuf>, String> {
    let mut paths = admission.include_paths.clone();
    for path in arany::mention_paths(objective).map_err(|error| error.to_string())? {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    Ok(paths)
}

pub(super) fn validate_objective_inputs(
    admission: &Admission,
    objective: &str,
) -> Result<(), String> {
    if admission.roster.is_some() {
        return Ok(());
    }
    let paths = objective_includes(admission, objective)?;
    validate_include_paths(admission, &paths)
}

fn validate_include_paths(
    admission: &Admission,
    paths: &[std::path::PathBuf],
) -> Result<(), String> {
    let root = arany::StateRoot::admit(&admission.state_dir)
        .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
    arany::validate_workspace_inputs(
        &root,
        &admission.workspace,
        std::env::current_dir().ok().as_deref(),
        paths,
    )
    .map_err(|error| error.to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GoalStart {
    Plain,
    Autonomous,
    Resume,
    Continue,
}

pub(super) fn goal_request(
    view: &SessionView,
    start: GoalStart,
    settings: super::controls::Preferences,
) -> Result<arany::GoalRequest, String> {
    if start == GoalStart::Resume {
        return view
            .interrupted_goal_id()
            .map(|id| arany::GoalRequest {
                resource_settings: settings.resources,
                ..arany::GoalRequest::resume(id, true)
            })
            .ok_or_else(|| "No interrupted Goal is available to resume".into());
    }
    if let Some(goal) = view.active_goal() {
        return Err(format!(
            "Goal {} is still active and cannot accept another Run",
            goal.id
        ));
    }
    if start == GoalStart::Continue {
        return view
            .continuable_goal()
            .map(|source| {
                arany::GoalRequest::continue_from(
                    source.id,
                    settings.goal_budget,
                    settings.resources,
                )
            })
            .ok_or_else(|| "No exhausted Goal is available to continue".into());
    }
    let mut request = match start {
        GoalStart::Autonomous => {
            arany::GoalRequest::autonomous(settings.goal_budget, arany::GoalPhase::Implementation)
        }
        _ => arany::GoalRequest::default(),
    };
    request.budget = Some(settings.goal_budget);
    request.resource_settings = settings.resources;
    Ok(request)
}

pub(super) fn preflight_idle_run(
    admission: &Admission,
    view: &SessionView,
    start: GoalStart,
    draft: &str,
    has_images: bool,
    linear: bool,
) -> Result<(), String> {
    let Some(profile) = view.defaults.provider.as_deref() else {
        return Err("Use /setup to choose an account".into());
    };
    let Some(model) = view.defaults.model.as_deref() else {
        return Err(
            match view
                .defaults
                .account_id
                .filter(|_| ProviderKind::parse(profile) == Some(ProviderKind::ChatGpt))
            {
                Some(id) => match chatgpt::selected_reauthorization_target(id) {
                    Ok(_) => chatgpt::AuthorizationError::BackendRouteNotAdmitted.to_string(),
                    Err(error) => error.to_string(),
                },
                None => "Choose a model with /model before submitting".into(),
            },
        );
    };
    if let Some(name) = &admission.roster {
        let selected =
            crate::cli::provider::roster::load(&admission.state_dir, &admission.workspace, name)?;
        selected.matches_selection(Selection {
            workspace: &admission.workspace,
            profile,
            model,
            effort: view.defaults.effort,
            account_id: view.defaults.account_id,
        })?;
    }
    goal_request(view, start, Default::default()).map_err(|error| format!("Error: {error}"))?;
    if matches!(start, GoalStart::Resume | GoalStart::Continue) && has_images {
        return Err(
            "Error: /goal resume and /goal continue take no image attachments; clear them first"
                .into(),
        );
    }
    if has_images && ProviderKind::parse(profile) == Some(ProviderKind::Custom) {
        return Err("Error: Image input is not admitted for custom Providers; clear these attachments first".into());
    }
    validate_local_selection(Selection {
        workspace: &admission.workspace,
        profile,
        model,
        effort: view.defaults.effort,
        account_id: view.defaults.account_id,
    })
    .map_err(|error| format!("Error: {error}"))?;
    submission_includes(admission, view, start, draft)
        .and_then(|paths| validate_include_paths(admission, &paths))
        .map_err(|error| {
            format!(
                "Error: {error}; {}",
                if linear {
                    "check the file inputs and enter a new line"
                } else {
                    "check the file inputs; draft unchanged"
                }
            )
        })
}

pub(super) struct PendingSubmission<'a> {
    pub(super) engine: &'a std::sync::Arc<arany::Engine>,
    pub(super) admission: &'a Admission,
    pub(super) goal: GoalStart,
    pub(super) objective: String,
    pub(super) skill: Option<String>,
    pub(super) draft: Option<ComposerDraft>,
}

struct SubmissionInput {
    goal: GoalStart,
    objective: String,
    skill: Option<String>,
}

struct SubmissionFinish<'a> {
    engine: &'a std::sync::Arc<arany::Engine>,
    admission: &'a Admission,
    draft: Option<ComposerDraft>,
}

impl PendingSubmission<'_> {
    pub(super) async fn run(
        self,
        terminal: &mut AttachedTerminal,
        view: &mut SessionView,
        composer: &mut Composer,
    ) -> Result<Option<Feedback>, String> {
        let mut ui = RunUi {
            terminal,
            view,
            composer,
        };
        let starting_sequence = ui.view.last_sequence;
        let PendingSubmission {
            engine,
            admission,
            goal,
            objective,
            skill,
            draft,
        } = self;
        let finish = SubmissionFinish {
            engine,
            admission,
            draft,
        };
        let input = SubmissionInput {
            goal,
            objective,
            skill,
        };
        let result = match admit_submission(input, &finish, &mut ui, starting_sequence).await {
            Ok(prepared) => run_attached(prepared, &mut ui).await,
            Err(error) => Err(error),
        };
        finish.finish_run(&mut ui, result, starting_sequence).await
    }
}

impl SubmissionFinish<'_> {
    async fn finish_run(
        self,
        ui: &mut RunUi<'_>,
        result: Result<RunSubmission, String>,
        starting_sequence: u64,
    ) -> Result<Option<Feedback>, String> {
        match result {
            Ok(RunSubmission::Completed(outcome, user_recorded, stop)) => {
                CompletedRun {
                    engine: self.engine,
                    admission: self.admission,
                    outcome: &outcome,
                    user_recorded,
                    stop: stop.as_deref(),
                }
                .finish(ui)
                .await
            }
            Ok(RunSubmission::InterruptedBeforeRun) => {
                if let Some(submitted) = self.draft {
                    ui.composer.restore_draft(submitted);
                }
                finish_interrupted_run(self.admission, ui, starting_sequence).await
            }
            Ok(RunSubmission::Shutdown(reason)) => Err(reason),
            Err(error) => match self.draft {
                None => Err(error),
                Some(submitted) => {
                    super::reconcile_failed_submission(
                        ui.terminal,
                        &self.admission.state_dir,
                        ui.view,
                        ui.composer,
                        submitted,
                        starting_sequence,
                    )
                    .await?;
                    let text = if ui.composer.is_empty() {
                        format!("Error: {error}")
                    } else {
                        format!(
                            "Error: {error}; draft retained ({} characters)",
                            ui.composer.character_count()
                        )
                    };
                    Ok(Some(Feedback::History(text)))
                }
            },
        }
    }
}

fn preparation_text(objective: &str, draft: Option<&ComposerDraft>) -> String {
    draft.map_or_else(|| objective.to_owned(), ComposerDraft::editable_text)
}

async fn admit_submission<'a>(
    input: SubmissionInput,
    finish: &SubmissionFinish<'a>,
    ui: &mut RunUi<'_>,
    starting_sequence: u64,
) -> Result<PreparedSubmission<'a>, String> {
    let SubmissionInput {
        goal,
        objective,
        skill,
    } = input;
    let engine = finish.engine;
    let admission = finish.admission;
    let draft = finish.draft.as_ref();
    let profile = ui
        .view
        .defaults
        .provider
        .clone()
        .ok_or("select /provider and /model before submitting")?;
    let model = ui
        .view
        .defaults
        .model
        .clone()
        .ok_or("select /model before submitting")?;
    let roster = admission
        .roster
        .as_ref()
        .map(|name| {
            crate::cli::provider::roster::load(&admission.state_dir, &admission.workspace, name)
        })
        .transpose()?;
    let manifest = roster
        .as_ref()
        .map(|selected| selected.manifest())
        .transpose()?;
    let autonomous = match goal {
        GoalStart::Resume => ui
            .view
            .interrupted_goal_id()
            .and_then(|id| ui.view.goal(id))
            .is_some_and(|resumed| resumed.autonomous),
        GoalStart::Continue => ui
            .view
            .continuable_goal()
            .is_some_and(|source| source.autonomous),
        GoalStart::Plain | GoalStart::Autonomous => goal == GoalStart::Autonomous,
    };
    let editable = preparation_text(&objective, draft);
    let include_paths = submission_includes(admission, ui.view, goal, &editable)?;
    let execution_settings = super::controls::settings_or_defaults(admission).await?;
    let goal = goal_request(ui.view, goal, execution_settings)?;
    let (progress, inbox) = RunProgress::new(admission, ui, starting_sequence, autonomous, goal.id);
    let request = RunRequest {
        skill,
        session_id: Some(ui.view.id),
        title: None,
        objective,
        images: draft.map_or_else(
            || ui.composer.take_images(),
            |draft| draft.images().to_vec(),
        ),
        workspace: admission.workspace.clone(),
        launch_directory: std::env::current_dir().ok(),
        include_paths,
        policy: ui.view.defaults.policy,
        goal,
    };
    Ok(PreparedSubmission {
        engine,
        profile,
        model,
        roster,
        manifest,
        request,
        progress,
        inbox,
    })
}

async fn finish_interrupted_run(
    admission: &Admission,
    ui: &mut RunUi<'_>,
    starting_sequence: u64,
) -> Result<Option<Feedback>, String> {
    let reload_error = reload_view_or_keep(
        ui.view,
        load_view(&admission.state_dir, ui.view.id).await,
        None,
        None,
    );
    // Only a Goal this submission started can be paused by it; an older paused Goal is not news.
    let started_goal_paused = ui
        .view
        .interrupted_goal_id()
        .and_then(|id| ui.view.goal(id))
        .filter(|goal| goal.started_sequence > starting_sequence)
        .map(|goal| paused_goal_notice(goal.admits_resume(ui.view)));
    let interrupted = started_goal_paused.unwrap_or(
        "Submission interrupted before Run admission; no task was accepted. Account work already started may still complete.",
    );
    let drawn = match &reload_error {
        Some(error) => format!("{interrupted}; {error}"),
        None => interrupted.to_owned(),
    };
    ui.terminal
        .draw(ui.view, ui.composer, Some(drawn.as_str()))
        .map_err(|error| error.to_string())?;
    let draft = retained_draft_notice(ui.composer);
    Ok(match reload_error {
        Some(error) => Some(Feedback::History(match draft {
            Some(draft) => format!("{draft}; {error}"),
            None => error,
        })),
        None => draft,
    })
}

struct CompletedRun<'a> {
    engine: &'a arany::Engine,
    admission: &'a Admission,
    outcome: &'a RunOutcome,
    user_recorded: bool,
    stop: Option<&'a str>,
}

struct RunNotice<'a> {
    automatic: Option<&'a arany::RunConfig>,
    preview_already_printed: bool,
    hidden_answer: &'static str,
    show: String,
    reload_error: Option<String>,
    notice: Option<Feedback>,
}

impl CompletedRun<'_> {
    async fn finish(&self, ui: &mut RunUi<'_>) -> Result<Option<Feedback>, String> {
        let automatic = self.outcome.run.config.as_ref().filter(|config| {
            self.outcome.run.status == RunStatus::Finished && config.auto_compaction_due()
        });
        let linear = ui.terminal.is_linear();
        let preview_already_printed = self.user_recorded && linear;
        let hidden_answer = hidden_answer_guidance(linear);
        let show = show_command(
            self.outcome.run.id,
            self.admission
                .explicit_state_dir
                .then_some(self.admission.state_dir.as_path()),
        );
        let reloaded = load_view(&self.admission.state_dir, self.outcome.session_id).await;
        if let Ok(confirmed) = &reloaded {
            confirm_outcome(confirmed, self.outcome)?;
        }
        print_run_answer(
            ui.terminal,
            reloaded.as_ref().ok(),
            self.outcome,
            preview_already_printed,
            &show,
        )?;
        if !linear {
            ui.terminal.finish_run_display();
        }
        let reload_error = reload_view_or_keep(ui.view, reloaded, None, Some(hidden_answer));
        if automatic.is_some() {
            ui.terminal
                .draw(
                    ui.view,
                    ui.composer,
                    Some("Context threshold reached; attempting automatic compaction"),
                )
                .map_err(|error| error.to_string())?;
        }
        ui.terminal
            .restore_draft_input(ui.composer.input_capacity())
            .map_err(|error| error.to_string())?;
        let notice = retained_draft_notice(ui.composer);
        self.finish_notice(
            ui,
            RunNotice {
                automatic,
                preview_already_printed,
                hidden_answer,
                show,
                reload_error,
                notice,
            },
        )
        .await
    }

    fn feedback(&self, view: &SessionView) -> Option<String> {
        // A cancelled autonomous loop keeps its Goal open; the reloaded view says how to continue it.
        let run_feedback = crate::presentation::session_run_feedback(view, &self.outcome.run);
        let run_feedback = match self.stop {
            Some(reason) => Some(match run_feedback {
                Some(feedback) => {
                    format!("Error: Goal stopped after this Run: {reason}. {feedback}")
                }
                None => format!("Error: Goal stopped after this Run: {reason}."),
            }),
            None => run_feedback,
        };
        // Only this Run's Goal can have been paused by it; an older paused Goal is not news here.
        let paused_goal = view
            .goal(self.outcome.goal_id)
            .filter(|goal| goal.status == arany::GoalStatus::Interrupted)
            .map(|goal| paused_goal_notice(goal.admits_resume(view)));
        submission_notice(
            paused_goal,
            self.outcome.run.status,
            run_feedback,
            crate::presentation::render_provider_attempts(&self.outcome.run),
        )
    }

    async fn finish_notice(
        &self,
        ui: &mut RunUi<'_>,
        mut display: RunNotice<'_>,
    ) -> Result<Option<Feedback>, String> {
        let feedback = self.feedback(ui.view);
        if let Some(feedback) = feedback.as_deref() {
            if ui.composer.is_empty() {
                display.notice = Some(Feedback::History(feedback.to_owned()));
            } else {
                ui.terminal
                    .draw(ui.view, ui.composer, Some(feedback))
                    .map_err(|error| error.to_string())?;
            }
        }
        if let Some(config) = display.automatic {
            self.compact(ui, &mut display, config, feedback).await?;
        }
        if let Some(error) = display.reload_error {
            display.notice = Some(Feedback::History(match display.notice.as_deref() {
                Some(previous) => format!("{previous}; {error}"),
                None => error,
            }));
        }
        Ok(display.notice)
    }

    async fn compact(
        &self,
        ui: &mut RunUi<'_>,
        display: &mut RunNotice<'_>,
        config: &arany::RunConfig,
        feedback: Option<String>,
    ) -> Result<(), String> {
        let submission_feedback = if ui.composer.is_empty() {
            display.notice.take()
        } else {
            feedback.map(Feedback::History)
        };
        let compaction_notice = match compact_current(
            self.engine,
            ui.terminal,
            self.admission,
            ui.view,
            ui.composer,
            Some((config, self.outcome.run.id)),
        )
        .await?
        {
            CompactionOutcome::Notice(notice) => notice,
            CompactionOutcome::Interrupted(error) => {
                format!("Automatic compaction unavailable: {error}; history retained")
            }
            CompactionOutcome::Unavailable(error) => {
                format!("Error: Automatic compaction unavailable: {error}; history retained")
            }
            CompactionOutcome::Shutdown(signal) => {
                return Err(format!("terminated by {}", signal.name()));
            }
        };
        let text = match retained_draft_notice(ui.composer) {
            Some(draft) => format!("{compaction_notice}; {draft}"),
            None => compaction_notice,
        };
        self.reload_after_compaction(ui, display, text).await?;
        if let Some(feedback) = submission_feedback {
            append_notice(&mut display.notice, &feedback);
        }
        Ok(())
    }

    async fn reload_after_compaction(
        &self,
        ui: &mut RunUi<'_>,
        display: &mut RunNotice<'_>,
        text: String,
    ) -> Result<(), String> {
        // The compaction reload supersedes an earlier failed reload, which is reported at most once here.
        let answer_hidden = display.reload_error.is_some();
        display.reload_error = None;
        let reloaded = load_view(&self.admission.state_dir, self.outcome.session_id).await;
        if let (true, Ok(confirmed)) = (answer_hidden, &reloaded) {
            confirm_outcome(confirmed, self.outcome)?;
            print_run_answer(
                ui.terminal,
                Some(confirmed),
                self.outcome,
                display.preview_already_printed,
                &display.show,
            )?;
        }
        display.notice = reload_view_or_keep(
            ui.view,
            reloaded,
            Some(text),
            answer_hidden.then_some(display.hidden_answer),
        )
        .map(Feedback::History);
        Ok(())
    }
}

pub(super) async fn compact_current(
    engine: &arany::Engine,
    terminal: &mut AttachedTerminal,
    admission: &Admission,
    view: &SessionView,
    composer: &mut Composer,
    pinned: Option<(&arany::RunConfig, arany::RunId)>,
) -> Result<CompactionOutcome, String> {
    let pinned_config = pinned.map(|(config, _)| config).or_else(|| {
        view.runs
            .last()
            .and_then(|run| run.config.as_ref())
            .filter(|config| config.roster.is_some())
    });
    let profile = pinned_config.map(|config| config.provider.profile());
    let selection =
        match compaction_selection(admission, view, pinned_config.zip(profile.as_deref())) {
            Ok(selection) => selection,
            Err(outcome) => return Ok(outcome),
        };
    let progress_text = if pinned.is_some() {
        "Automatically compacting Session... Ctrl+C interrupts"
    } else {
        "Compacting Session... Ctrl+C interrupts"
    };
    terminal
        .draw_busy_notice(
            view,
            composer,
            Some(TerminalNotice::Transient(progress_text)),
        )
        .map_err(|error| error.to_string())?;
    let future = run::compact_selected(
        engine,
        &admission.state_dir,
        selection,
        view.id,
        &admission.workspace,
        pinned.map(|(_, run_id)| run_id),
        view.runs
            .last()
            .and_then(|run| run.config.as_ref())
            .filter(|config| config.roster.is_some()),
    );
    drive_compaction(
        future,
        CompactionUi {
            admission,
            terminal,
            view,
            composer,
            progress_text,
        },
    )
    .await
}

fn compaction_selection<'a>(
    admission: &'a Admission,
    view: &'a SessionView,
    pinned: Option<(&'a arany::RunConfig, &'a str)>,
) -> Result<Selection<'a>, CompactionOutcome> {
    let (profile, model, effort, account_id) = if let Some((config, profile)) = pinned {
        (
            profile,
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
            return Err(CompactionOutcome::Unavailable(
                "select /provider and /model before compacting".into(),
            ));
        };
        let Some(model) = view.defaults.model.as_deref() else {
            return Err(CompactionOutcome::Unavailable(
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
    Ok(Selection {
        workspace: &admission.workspace,
        profile,
        model,
        effort,
        account_id,
    })
}

struct CompactionUi<'a> {
    admission: &'a Admission,
    terminal: &'a mut AttachedTerminal,
    view: &'a SessionView,
    composer: &'a mut Composer,
    progress_text: &'a str,
}

async fn drive_compaction<F>(
    future: F,
    mut ui: CompactionUi<'_>,
) -> Result<CompactionOutcome, String>
where
    F: Future<Output = Result<Option<CompactionRecord>, String>>,
{
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return Ok(match result {
                Ok(Some(record)) => CompactionOutcome::Notice(
                    with_compaction_switches(&ui.admission.state_dir, ui.view, compaction_notice(&record)).await,
                ),
                Ok(None) => CompactionOutcome::Notice(
                    "Automatic compaction skipped; Run boundary changed or already compacted".into(),
                ),
                Err(error) => CompactionOutcome::Unavailable(error),
            }),
            input = ui.terminal.next_chat_input() => {
                let input = input.map_err(|error| error.to_string())?;
                if let Some(outcome) = handle_compaction_input(input, &mut ui).await? {
                    return Ok(outcome);
                }
            }
        }
    }
}

async fn handle_compaction_input(
    input: TerminalInput,
    ui: &mut CompactionUi<'_>,
) -> Result<Option<CompactionOutcome>, String> {
    ui.composer.observe_input(input);
    // The pager and a scrollback dump own their keys before the hidden composer.
    if ui
        .terminal
        .handle_transcript_input(input, ui.view)
        .map_err(|error| error.to_string())?
        || ui
            .terminal
            .handle_history_input(input, ui.view, ui.composer, true)
            .await
    {
        ui.terminal.take_graphics_hover_changed();
        ui.terminal
            .draw_busy(ui.view, ui.composer, None)
            .map_err(|error| error.to_string())?;
        return Ok(None);
    }
    if ui.terminal.take_graphics_hover_changed() || input == TerminalInput::PanelSelectionChanged {
        ui.terminal
            .draw_busy(ui.view, ui.composer, None)
            .map_err(|error| error.to_string())?;
        return Ok(None);
    }
    let input = ui.composer.chat_input(input);
    let notice = match input {
        TerminalInput::Submit if !ui.terminal.is_linear() && ui.composer.continue_line() => {
            ui.terminal
                .draw_busy(ui.view, ui.composer, None)
                .map_err(|error| error.to_string())?;
            None
        }
        TerminalInput::Interrupt => {
            return Ok(Some(CompactionOutcome::Interrupted(
                "Compaction interrupted; check Session history".into(),
            )));
        }
        TerminalInput::Resize => {
            ui.terminal
                .draw_busy(ui.view, ui.composer, None)
                .map_err(|error| error.to_string())?;
            None
        }
        TerminalInput::Suspend => {
            ui.terminal
                .suspend_and_resume(ui.composer.input_capacity())
                .map_err(|error| error.to_string())?;
            ui.terminal
                .draw_busy(ui.view, ui.composer, None)
                .map_err(|error| error.to_string())?;
            None
        }
        TerminalInput::Shutdown(signal) => {
            return Ok(Some(CompactionOutcome::Shutdown(signal)));
        }
        TerminalInput::Submit | TerminalInput::EndOfInput => {
            ui.terminal
                .restore_draft_input(ui.composer.input_capacity())
                .map_err(|error| error.to_string())?;
            Some(Feedback::Transient(
                "Draft retained; press Enter after compaction to submit".into(),
            ))
        }
        input => draft_compaction_notice(input, ui)?,
    };
    if let Some(notice) = notice {
        ui.terminal
            .draw_busy_notice(ui.view, ui.composer, Some(notice.notice()))
            .map_err(|error| error.to_string())?;
    }
    Ok(None)
}

fn draft_compaction_notice(
    input: TerminalInput,
    ui: &mut CompactionUi<'_>,
) -> Result<Option<Feedback>, String> {
    Ok(match input {
        TerminalInput::ClipboardPaste => Some(match ui.terminal.request_clipboard_paste() {
            Ok(()) => Feedback::Transient("Reading clipboard... draft retained".into()),
            Err(error) => Feedback::History(format!("Error: {error}; draft unchanged")),
        }),
        TerminalInput::Paste => match ui.terminal.paste_into_admitted_workspace(
            ui.composer,
            &ui.admission.workspace,
            ui.admission.workspace_identity,
        ) {
            Err(error) => Some(Feedback::History(format!(
                "Error: {error}; draft unchanged"
            ))),
            Ok(ComposerEdit::Changed) => {
                ui.terminal
                    .draw_busy(ui.view, ui.composer, None)
                    .map_err(|error| error.to_string())?;
                None
            }
            Ok(_) => None,
        },
        TerminalInput::LineRejected => Some(Feedback::History(
            "Error: invalid or overlong terminal line; draft unchanged".into(),
        )),
        TerminalInput::LineContinued => Some(Feedback::History(format!(
            "Draft: {} characters; Enter retains until compaction ends",
            ui.composer.character_count(),
        ))),
        TerminalInput::QuickActions => Some(Feedback::History(
            "Quick actions are unavailable during compaction; draft retained".into(),
        )),
        input => match ui.composer.apply(input) {
            ComposerEdit::AtCapacity => Some(Feedback::History(
                "Error: message is too long; shorten it; draft unchanged".into(),
            )),
            ComposerEdit::Changed if !ui.terminal.is_linear() => {
                Some(Feedback::Transient(ui.progress_text.into()))
            }
            ComposerEdit::Changed | ComposerEdit::Unchanged => None,
        },
    })
}

fn compaction_switches(view: &SessionView) -> Vec<&arany::ProviderFallback> {
    view.runs.last().map_or_else(Vec::new, |run| {
        run.provider_fallbacks
            .iter()
            .filter(|(_, fallback)| fallback.role == arany::ProviderRole::Compaction)
            .map(|(_, fallback)| fallback)
            .collect()
    })
}

// `None` marks a view from another Run, so no earlier switch was shown for it.
fn unshown<T>(switches: Vec<T>, shown: Option<usize>) -> Vec<T> {
    switches.into_iter().skip(shown.unwrap_or(0)).collect()
}

async fn with_compaction_switches(
    state_dir: &std::path::Path,
    before: &SessionView,
    notice: String,
) -> String {
    let shown = compaction_switches(before).len();
    match super::load_view(state_dir, before.id).await {
        Ok(after) => {
            let same_run =
                before.runs.last().map(|run| run.id) == after.runs.last().map(|run| run.id);
            let shown = same_run.then_some(shown);
            append_switch_notices(notice, &unshown(compaction_switches(&after), shown))
        }
        Err(error) => {
            format!("{notice}\nError: compaction Provider switches could not be reloaded: {error}")
        }
    }
}

fn append_switch_notices(notice: String, switches: &[&arany::ProviderFallback]) -> String {
    switches.iter().fold(notice, |text, fallback| {
        format!(
            "{text}\n{}",
            crate::presentation::escape_terminal(&fallback.notice())
        )
    })
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
                CompactionFailure::Cancelled => "cancelled",
            };
            format!("Error: Compaction failed: {reason}; {usage}{bound}")
        }
    }
}

#[cfg(test)]
pub(super) fn session_with_goal(autonomous: bool) -> SessionView {
    let session_id = arany::SessionId::new();
    let event = arany::Event::GoalStarted {
        include_paths: Some(Vec::new()),
        goal_id: arany::GoalId::new(),
        objective: "Ship the parser".into(),
        workspace_identity: WorkspaceIdentity {
            device: 1,
            inode: 2,
        },
        budget: arany::GoalBudget::default(),
        phase: arany::GoalPhase::Implementation,
        autonomous,
        resource_settings: arany::ToolResourceSettings::default(),
        continues: None,
    };
    let started = arany::Event::SessionStarted {
        title: "Goals".into(),
        workspace_identity: WorkspaceIdentity {
            device: 1,
            inode: 2,
        },
    };
    let events = [started, event]
        .into_iter()
        .zip(1..)
        .map(|(event, sequence)| arany::EventEnvelope {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_shown_before_a_stale_view_do_not_skip_or_panic() {
        assert_eq!(unshown(vec![1, 2, 3], Some(1)), vec![2, 3]);
        assert_eq!(unshown(vec![1], Some(2)), Vec::<i32>::new());
        assert_eq!(unshown(vec![1, 2], None), vec![1, 2]);
    }

    #[test]
    fn compaction_notice_appends_only_this_compactions_switches() {
        let identity = |model: &str| {
            arany::ProviderIdentity::new("openai", model, arany::Provenance::native())
        };
        let switch = arany::ProviderFallback {
            rate_limits: None,
            role: arany::ProviderRole::Compaction,
            call_index: 0,
            failure: arany::ProviderCallDisposition::Unavailable,
            from: identity("gpt-5.4"),
            to: identity("gpt-5.4-mini"),
        };
        assert_eq!(
            append_switch_notices("Compaction saved".into(), &[]),
            "Compaction saved"
        );
        let text = append_switch_notices("Compaction saved".into(), &[&switch]);
        assert_eq!(text, format!("Compaction saved\n{}", switch.notice()));
        assert!(text.contains("compaction"));
    }

    #[test]
    fn compaction_notice_reports_committed_status_without_exposing_summary() {
        let summary = "\u{1b}[31muntrusted summary";
        let summary_bytes = summary.len() as u32;
        let mut record = CompactionRecord {
            rate_limits: None,
            covered_run_id: arany::RunId::new(),
            covered_sequence: 1,
            source_digest: [0; 32],
            recent_files: Vec::new(),
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            prompt_version: 1,
            schema_version: 1,
            compiler_version: 4,
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
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
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

#[cfg(test)]
mod goal_request_tests {
    use super::*;
    use crate::cli::attached::{Admission, EntryMode};
    use arany::SessionDefaults;

    #[test]
    fn full_bound_restored_prompt_prepares_exact_objective_and_mentions() {
        let temp = tempfile::tempdir().expect("temporary root");
        let marker = " @src/lib.rs";
        let prompt = format!(
            "/{}{}",
            "x".repeat(crate::terminal::MAX_PRIMARY_DRAFT_BYTES - 1 - marker.len()),
            marker
        );
        let mut composer = Composer::default();
        assert_eq!(composer.restore_prompt(&prompt), Ok(ComposerEdit::Changed));
        assert_eq!(
            composer.submission(),
            Ok(crate::terminal::Submission::Objective(&prompt))
        );
        assert_eq!(composer.editable_text(), prompt);
        let draft = composer.take_draft();
        let prepared_text = preparation_text(&prompt, Some(&draft));
        assert_eq!(prepared_text, prompt);

        let admission = Admission {
            roster: None,
            telemetry: arany::Telemetry::disabled(),
            state_dir: temp.path().join("state"),
            explicit_state_dir: false,
            workspace: temp.path().join("workspace"),
            workspace_identity: arany::WorkspaceIdentity {
                device: 0,
                inode: 0,
            },
            include_paths: vec![std::path::PathBuf::from("already-included.rs")],
            tools: false,
            workspace_permissions: None,
            defaults: SessionDefaults::default(),
            screen_reader: false,
            classic: false,
            no_color: false,
            setup_requested: false,
            entry: EntryMode::New,
            prompt: None,
            resume_goal: None,
        };
        assert_eq!(
            submission_includes(
                &admission,
                &session_with_goal(false),
                GoalStart::Plain,
                &prepared_text,
            )
            .unwrap(),
            vec![
                std::path::PathBuf::from("already-included.rs"),
                std::path::PathBuf::from("src/lib.rs"),
            ]
        );
    }

    #[test]
    fn goal_starts_map_to_plain_autonomous_and_resume_requests() {
        let mut view = session_with_goal(false);
        view.goals.clear();
        assert_eq!(
            goal_request(&view, GoalStart::Plain, Default::default()),
            Ok(arany::GoalRequest::default())
        );
        let autonomous = goal_request(&view, GoalStart::Autonomous, Default::default())
            .expect("autonomous start");
        assert!(autonomous.autonomous);
        assert_eq!(autonomous.id, None);
        assert_eq!(
            goal_request(&view, GoalStart::Resume, Default::default()),
            Err("No interrupted Goal is available to resume".into())
        );
    }

    #[test]
    fn continuation_reuses_admitted_includes_without_expanding_stored_text() {
        let mut view = session_with_goal(true);
        view.goals[0].objective = "Fix @src/lib.rs".into();
        view.goals[0].status = arany::GoalStatus::Failed;
        view.goals[0].budget_closed = true;
        let run = arany::RunView {
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
            accepted_sequence: 3,
            finished_sequence: Some(4),
            failure: None,
            tools: Vec::new(),
            tool_compaction_summary: None,
            tool_compaction_recent_files: Vec::new(),
            tool_compaction_covered_count: 0,
            tool_results_cleared_count: 0,
            definitions: Default::default(),
            display: Vec::new(),
            steering_notes: Vec::new(),
            lifecycle_hooks: Vec::new(),
            lifecycle_hook_sequence: None,
            provider_sequences: Default::default(),
        };
        view.goals[0].run_ids.push(run.id);
        view.runs.push(run);
        view.goals[0].include_paths = Some(vec!["admitted.rs".into()]);
        assert_eq!(
            continuation_includes(&view).unwrap(),
            vec![std::path::PathBuf::from("admitted.rs")]
        );
        view.goals[0].include_paths = Some(Vec::new());
        assert!(continuation_includes(&view).unwrap().is_empty());
        view.goals[0].include_paths = None;
        assert_eq!(
            continuation_includes(&view),
            Err(arany::EngineError::GoalIncludesUnavailable.to_string())
        );
    }

    #[test]
    fn goal_continue_refuses_goals_that_were_not_closed_by_exhaustion() {
        let mut view = session_with_goal(true);
        let refusal = Err("No exhausted Goal is available to continue".to_owned());
        view.goals[0].status = arany::GoalStatus::Interrupted;
        assert_eq!(
            goal_request(&view, GoalStart::Continue, Default::default()),
            refusal
        );
        view.goals[0].status = arany::GoalStatus::Failed;
        view.goals[0].budget.max_runs = 0;
        assert_eq!(
            goal_request(&view, GoalStart::Continue, Default::default()),
            refusal
        );
    }

    #[test]
    fn paused_goal_admits_new_goals_and_resumes_only_through_goal_resume() {
        let mut view = session_with_goal(true);
        view.goals[0].status = arany::GoalStatus::Interrupted;
        let id = view.goals[0].id;
        for build_jobs in [None, Some(2)] {
            let mut settings = crate::cli::attached::controls::Preferences::default();
            settings.resources.build_jobs = build_jobs;
            assert_eq!(
                goal_request(&view, GoalStart::Resume, settings),
                Ok(arany::GoalRequest {
                    resource_settings: settings.resources,
                    ..arany::GoalRequest::resume(id, true)
                })
            );
        }
        assert_eq!(
            goal_request(&view, GoalStart::Plain, Default::default()),
            Ok(arany::GoalRequest::default())
        );
        let autonomous = goal_request(&view, GoalStart::Autonomous, Default::default())
            .expect("autonomous start");
        assert!(autonomous.autonomous);
    }

    #[test]
    fn idle_resume_refusal_precedes_draft_images_and_session_state() {
        let temp = tempfile::tempdir().expect("temporary root");
        let mut view = session_with_goal(true);
        view.goals[0].status = arany::GoalStatus::Interrupted;
        view.defaults.provider = Some("openai".into());
        view.defaults.model = Some("gpt-5.4".into());
        let admission = Admission {
            roster: None,
            telemetry: arany::Telemetry::disabled(),
            state_dir: temp.path().join("state"),
            explicit_state_dir: false,
            workspace: temp.path().join("workspace"),
            workspace_identity: arany::WorkspaceIdentity {
                device: 0,
                inode: 0,
            },
            include_paths: Vec::new(),
            tools: false,
            workspace_permissions: None,
            defaults: SessionDefaults::default(),
            screen_reader: false,
            classic: false,
            no_color: false,
            setup_requested: false,
            entry: EntryMode::New,
            prompt: None,
            resume_goal: None,
        };
        view.goals.clear();
        let message = preflight_idle_run(&admission, &view, GoalStart::Resume, "", false, false)
            .expect_err("no interrupted Goal to resume");
        assert_eq!(message, "Error: No interrupted Goal is available to resume");
        assert!(
            !admission.state_dir.exists(),
            "refused submissions create no State"
        );
    }
}

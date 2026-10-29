use super::{
    Admission, AttachedArgs, EntryMode, Feedback, PromptEntry,
    active::{self, GoalStart},
    admit_startup, append_notice, authorize_or_notice, controls, final_startup_notice,
    materialize_session, models, permissions,
    picker::{PickerChoice, pick_session},
    preflight_prompt, refresh_runtime_skills, require_access, restore_failure_notice,
    seed_setup_catalog,
    session::{AttachedSession, SessionParts},
    setup, start_session, startup_progress_notice,
};
use crate::accounts::chatgpt;
use crate::cli::provider::{roster::SelectedRoster, select::ProviderArg};
use crate::terminal::{AttachedTerminal, Composer, Submission, TerminalMode};
use arany::{Engine, ProviderKind, SessionDefaults, SessionId, SessionView, StateRoot, Telemetry};
use std::{io::Write, ops::ControlFlow, str::FromStr};

struct Startup {
    archived_history: Option<arany::ArchivedHistory>,
    setup_catalog: Option<Vec<arany::ModelEntry>>,
    setup_notice: Option<String>,
    setup_session_id: Option<SessionId>,
    composer: Composer,
    terminal: AttachedTerminal,
    admission: Admission,
    saved_view: TerminalMode,
    access: SavedAccess,
}

#[derive(Clone, Copy)]
enum SavedAccess {
    Authorized,
    Unavailable,
}

enum AccessCheck {
    Required,
    Completed,
}

struct StartupObjective {
    prompt: String,
    notice: String,
}

impl StartupObjective {
    fn new(
        prompt: Option<String>,
        goal_resume: Option<arany::GoalId>,
        session_id: Option<SessionId>,
        setup_notice: Option<String>,
        archive_notice: Option<&str>,
    ) -> Option<Self> {
        // `--resume-goal` is the explicit confirmation: without a prompt it resumes at once.
        let prompt = prompt.or_else(|| goal_resume.map(|_| String::new()))?;
        let notice =
            startup_progress_notice(objective_notice(session_id, setup_notice), archive_notice);
        Some(Self { prompt, notice })
    }
}

fn objective_notice(session_id: Option<SessionId>, setup_notice: Option<String>) -> String {
    setup_notice.unwrap_or_else(|| match session_id {
        None => "New conversation · /help for commands".into(),
        Some(id) => format!("Session {id} · /help for commands"),
    })
}

pub(super) async fn start(
    mut args: AttachedArgs,
    telemetry: Telemetry,
) -> Result<ControlFlow<(), AttachedSession>, String> {
    let (_, state_dir) =
        super::super::admit_workspace(args.workspace.clone(), args.state_dir.clone())
            .map_err(str::to_owned)?;
    let archived_records = super::super::record_formats::archive_incompatible(&state_dir)?;
    let selected = select_roster(&mut args)?;
    let admission = args.admit(telemetry)?;
    let goal_resume = admission.resume_goal;
    let restored_settings = controls::restore_settings(&admission).await;
    let mut startup = Startup::acquire(admission, &restored_settings)?;
    for notice in archived_records {
        append_notice(&mut startup.setup_notice, &notice);
    }
    let ControlFlow::Continue(archived) = admit_startup(
        &mut startup.terminal,
        &mut startup.admission,
        &mut startup.composer,
        &mut startup.setup_notice,
        selected,
    )
    .await?
    else {
        return Ok(ControlFlow::Break(()));
    };
    startup.archived_history = archived;
    if let Ok(state) = StateRoot::open_existing(&startup.admission.state_dir)
        && let Ok(environment) = arany::CommandEnvironment::load(&state)
        && environment.process_path_differs()
    {
        append_notice(
            &mut startup.setup_notice,
            "Process PATH differs from the pinned command environment; use /permissions → Command environment to capture explicitly",
        );
    }
    let explicit_provider = startup.admission.defaults.provider.is_some();
    startup.restore_chatgpt_account();
    let reuse_saved_selection =
        matches!(startup.admission.entry, EntryMode::New) && !explicit_provider;
    let access_checked = if reuse_saved_selection {
        match startup.resolve_defaults().await? {
            ControlFlow::Continue(checked) => checked,
            ControlFlow::Break(()) => return Ok(ControlFlow::Break(())),
        }
    } else {
        AccessCheck::Required
    };
    if let ControlFlow::Break(()) = startup.authorize_new(access_checked).await? {
        return Ok(ControlFlow::Break(()));
    }
    if reuse_saved_selection && matches!(startup.access, SavedAccess::Authorized) {
        startup.restore_selection().await;
    }
    if let ControlFlow::Break(()) = startup.pick_session().await? {
        return Ok(ControlFlow::Break(()));
    }
    startup.preflight_new().await?;
    startup.apply_settings(restored_settings);
    startup
        .open_session(goal_resume, reuse_saved_selection)
        .await
}

fn select_roster(args: &mut AttachedArgs) -> Result<Option<SelectedRoster>, String> {
    if let Some(name) = &args.profile {
        if args.setup {
            return Err("--setup cannot be combined with --profile".into());
        }
        let (workspace, state_dir) =
            super::super::admit_workspace(args.workspace.clone(), args.state_dir.clone())
                .map_err(str::to_owned)?;
        let selected = super::super::provider::roster::load(&state_dir, &workspace, name)?;
        selected.matches(args.provider.as_ref(), args.model.as_deref(), args.effort)?;
        let primary = selected.primary();
        args.provider =
            Some(ProviderArg::from_str(&primary.provider.profile()).map_err(str::to_owned)?);
        args.model = Some(primary.model.clone());
        args.effort = primary.effort;
        std::io::stderr()
            .write_all(selected.manifest()?.as_bytes())
            .map_err(|_| "roster manifest output failed")?;
        Ok(Some(selected))
    } else {
        Ok(None)
    }
}

impl Startup {
    fn acquire(
        admission: Admission,
        restored_settings: &Result<controls::Preferences, String>,
    ) -> Result<Self, String> {
        let restored = restored_settings.as_ref().copied().unwrap_or_default();
        let saved_view = restored.view;
        let mut terminal = AttachedTerminal::acquire_with_mode(
            admission.screen_reader,
            if admission.classic {
                TerminalMode::Classic
            } else {
                saved_view
            },
            restored.pointer(),
        )
        .map_err(|error| error.to_string())?;
        if admission.classic {
            terminal.pin_classic();
        }
        if admission.no_color {
            terminal.disable_color();
        }
        Ok(Self {
            admission,
            terminal,
            saved_view,
            composer: Composer::default(),
            setup_notice: None,
            setup_session_id: None,
            setup_catalog: None,
            archived_history: None,
            access: SavedAccess::Authorized,
        })
    }

    fn restore_chatgpt_account(&mut self) {
        if self
            .admission
            .defaults
            .provider
            .as_deref()
            .and_then(ProviderKind::parse)
            == Some(ProviderKind::ChatGpt)
            && self.admission.defaults.account_id.is_none()
        {
            match chatgpt::selected_account_id() {
                Ok(id) => self.admission.defaults.account_id = Some(id),
                Err(error) => {
                    append_notice(&mut self.setup_notice, &restore_failure_notice(error));
                    self.admission.defaults = SessionDefaults::default();
                }
            }
        }
    }

    async fn resolve_setup(&mut self) -> Result<ControlFlow<(), AccessCheck>, String> {
        let mut journal =
            setup::SetupJournal::new(&self.admission.state_dir, &self.admission.workspace, None);
        let result = setup::resolve(
            &mut self.terminal,
            &self.admission.state_dir,
            &self.admission.workspace,
            &mut journal,
            &self.admission.defaults,
        )
        .await;
        self.setup_session_id = journal.session_id();
        match result {
            Ok(Some(setup::SetupSelection::Native {
                defaults,
                notice,
                catalog,
            })) => {
                self.admission.defaults = defaults;
                self.setup_notice = Some(notice);
                self.setup_catalog = Some(catalog);
            }
            Ok(Some(setup::SetupSelection::ChatGpt {
                defaults,
                notice,
                catalog,
            })) => {
                self.admission.defaults = defaults;
                self.setup_notice = notice;
                self.setup_catalog = Some(catalog);
            }
            Ok(Some(setup::SetupSelection::Custom { defaults, notice })) => {
                self.admission.defaults = defaults;
                self.setup_notice = Some(notice);
                self.setup_catalog = Some(Vec::new());
            }
            Ok(None) => return Ok(ControlFlow::Break(())),
            Err(setup::SetupError::Recoverable(error)) => {
                append_notice(
                    &mut self.setup_notice,
                    &format!("Setup could not be completed: {error}; use /setup to try again"),
                );
            }
            Err(error) => return Err(error.into()),
        }

        Ok(ControlFlow::Continue(AccessCheck::Required))
    }

    async fn resolve_defaults(&mut self) -> Result<ControlFlow<(), AccessCheck>, String> {
        if self.admission.setup_requested {
            return self.resolve_setup().await;
        }
        match setup::saved_defaults(&mut self.terminal, &self.admission.workspace).await {
            Ok(setup::SavedDefaults::Selected(defaults)) => self.admission.defaults = defaults,
            Ok(setup::SavedDefaults::AccessChecked(defaults)) => {
                self.admission.defaults = defaults;
                return Ok(ControlFlow::Continue(AccessCheck::Completed));
            }
            Ok(setup::SavedDefaults::Cancelled) => return Ok(ControlFlow::Break(())),
            Ok(setup::SavedDefaults::Unconfigured) => {}
            Err(setup::SetupError::Recoverable(error)) => {
                append_notice(&mut self.setup_notice, &restore_failure_notice(error));
            }
            Err(error) => return Err(error.into()),
        }
        Ok(ControlFlow::Continue(AccessCheck::Required))
    }

    async fn authorize_new(
        &mut self,
        access_checked: AccessCheck,
    ) -> Result<ControlFlow<()>, String> {
        if matches!(self.admission.entry, EntryMode::New)
            && !self.admission.setup_requested
            && matches!(access_checked, AccessCheck::Required)
        {
            match authorize_or_notice(
                &mut self.terminal,
                &self.admission.defaults,
                &mut self.setup_notice,
            )
            .await?
            {
                ControlFlow::Continue(authorized) => {
                    self.access = match authorized {
                        true => SavedAccess::Authorized,
                        false => SavedAccess::Unavailable,
                    }
                }
                ControlFlow::Break(()) => return Ok(ControlFlow::Break(())),
            }
        }

        Ok(ControlFlow::Continue(()))
    }

    async fn restore_selection(&mut self) {
        let mut remembered = self.admission.defaults.clone();
        match models::restore_saved_models(
            &self.admission.workspace,
            &self.admission.state_dir,
            &mut remembered,
            true,
        )
        .await
        {
            Ok(catalog) => {
                if self
                    .setup_catalog
                    .as_ref()
                    .is_none_or(|fresh: &Vec<arany::ModelEntry>| {
                        fresh
                            .iter()
                            .any(|row| Some(row.id.as_str()) == remembered.model.as_deref())
                    })
                {
                    self.admission.defaults = remembered;
                }
                if self.setup_catalog.is_none() {
                    self.setup_catalog = Some(catalog);
                }
            }
            Err(error) => {
                self.setup_notice = Some(format!("Error: {error}; choose /model to select again"))
            }
        }
    }

    async fn pick_session(&mut self) -> Result<ControlFlow<()>, String> {
        if matches!(self.admission.entry, EntryMode::PickSession) {
            let id = match pick_session(&mut self.terminal, &self.admission, None).await? {
                PickerChoice::Selected(id) => id,
                PickerChoice::Unavailable(error) => return Err(error),
                PickerChoice::Empty => return Err("No Sessions for this Workspace".into()),
                PickerChoice::Closed => return Ok(ControlFlow::Break(())),
                PickerChoice::Shutdown(signal) => {
                    return Err(format!("terminated by {}", signal.name()));
                }
            };
            self.admission.entry = EntryMode::Resume(id);
        }

        Ok(ControlFlow::Continue(()))
    }

    async fn preflight_new(&mut self) -> Result<(), String> {
        if self.admission.prompt.is_some()
            && self
                .admission
                .defaults
                .provider
                .as_deref()
                .and_then(ProviderKind::parse)
                == Some(ProviderKind::ChatGpt)
            && self.admission.defaults.model.is_none()
        {
            return Err(
                "ChatGPT account saved; use /model to select a model and effort before a Run"
                    .into(),
            );
        }
        if self.admission.prompt.is_some() && matches!(self.admission.entry, EntryMode::New) {
            require_access(
                matches!(self.access, SavedAccess::Authorized),
                self.setup_notice.as_ref(),
            )?;
            preflight_prompt(
                &self.admission.state_dir,
                &self.admission.workspace,
                &self.admission.defaults,
                PromptEntry::New,
            )
            .await?;
        }

        Ok(())
    }

    fn apply_settings(&mut self, restored_settings: Result<controls::Preferences, String>) {
        match restored_settings {
            Ok(restored) => {
                self.composer.set_completion_layout(restored.completion);
                self.composer.set_newline_shortcut(restored.newline);
                self.terminal.set_newline_shortcut(restored.newline);
            }
            Err(error) => append_notice(
                &mut self.setup_notice,
                &format!("Error: {error}; using default settings"),
            ),
        }
    }

    async fn authorize_existing(
        &mut self,
        view: &SessionView,
        has_objective: bool,
    ) -> Result<ControlFlow<()>, String> {
        if !matches!(self.admission.entry, EntryMode::New) {
            match authorize_or_notice(&mut self.terminal, &view.defaults, &mut self.setup_notice)
                .await?
            {
                ControlFlow::Continue(authorized) => {
                    self.access = match authorized {
                        true => SavedAccess::Authorized,
                        false => SavedAccess::Unavailable,
                    }
                }
                ControlFlow::Break(()) => return Ok(ControlFlow::Break(())),
            }
            if has_objective {
                require_access(
                    matches!(self.access, SavedAccess::Authorized),
                    self.setup_notice.as_ref(),
                )?;
                preflight_prompt(
                    &self.admission.state_dir,
                    &self.admission.workspace,
                    &view.defaults,
                    PromptEntry::Existing,
                )
                .await?;
            }
        }

        Ok(ControlFlow::Continue(()))
    }

    fn open_engine(&self) -> Result<Engine, String> {
        if let Ok(root) = StateRoot::open_existing(&self.admission.state_dir) {
            arany::enable_development_diagnostics(&root);
        }
        // One Engine serves every submission and compaction until exit; each Run takes its own Session locks.
        StateRoot::open_existing(&self.admission.state_dir)
            .map_err(|_| "state directory unavailable or unsafe".to_owned())
            .and_then(|root| {
                Engine::open(
                    root,
                    arany::EngineConfig {
                        worker_concurrency: None,
                        telemetry: self.admission.telemetry.clone(),
                        guard_executable: std::env::current_exe().ok(),
                    },
                )
                .map_err(|_| "state store unavailable".to_owned())
            })
    }

    async fn restore_catalog(&mut self, view: &SessionView, reuse_saved_selection: bool) {
        if self.setup_catalog.is_none() && matches!(self.access, SavedAccess::Authorized) {
            match models::restore_saved_models(
                &self.admission.workspace,
                &self.admission.state_dir,
                &mut view.defaults.clone(),
                false,
            )
            .await
            {
                Ok(catalog) => self.setup_catalog = Some(catalog),
                Err(error) => {
                    self.setup_notice = Some(format!("Error: {error}; cached models unavailable"))
                }
            }
        }
        if (self.admission.setup_requested
            || reuse_saved_selection
            || matches!(
                self.admission.entry,
                EntryMode::Resume(_) | EntryMode::Continue | EntryMode::Fork(_)
            ))
            && let Err(error) = models::remember_models(
                &self.admission.workspace,
                &view.defaults,
                self.setup_catalog.as_deref(),
                true,
            )
            .await
        {
            self.setup_notice = Some(format!(
                "Error: {error}; selection kept for this conversation only"
            ));
        }
    }

    fn take_setup_notice(&mut self) -> Option<String> {
        if self.setup_notice.is_some() {
            self.setup_notice.take()
        } else if self.admission.setup_requested
            && self
                .admission
                .defaults
                .provider
                .as_deref()
                .and_then(ProviderKind::parse)
                == Some(ProviderKind::ChatGpt)
            && self.admission.defaults.model.is_none()
        {
            Some("ChatGPT account saved; use /model to select a model and effort".into())
        } else {
            None
        }
    }

    async fn prepare_composer(&mut self, view: &SessionView, reuse_saved_selection: bool) -> bool {
        permissions::refresh_mode(
            &mut self.composer,
            view.defaults.provider.as_deref(),
            self.admission.tools,
            self.admission.workspace_permissions.as_ref(),
        );
        self.restore_catalog(view, reuse_saved_selection).await;
        let skills_supported = permissions::supports_tools(view.defaults.provider.as_deref());
        refresh_runtime_skills(
            &self.admission,
            &mut self.composer,
            &mut self.setup_notice,
            skills_supported,
        );
        seed_setup_catalog(
            &mut self.composer,
            &view.defaults,
            self.setup_catalog.as_deref(),
        );
        skills_supported
    }

    async fn open_session(
        mut self,
        goal_resume: Option<arany::GoalId>,
        reuse_saved_selection: bool,
    ) -> Result<ControlFlow<(), AttachedSession>, String> {
        let (session_id, view) = match self.setup_session_id {
            Some(id) => {
                let view = super::load_view(&self.admission.state_dir, id).await?;
                super::finish_existing_start(&self.admission, view).await?
            }
            None => start_session(&self.admission).await?,
        };
        if let Some(goal_id) = goal_resume
            && view.interrupted_goal_id() != Some(goal_id)
        {
            return Err("selected Session does not contain that interrupted Goal".into());
        }
        let has_objective = self.admission.prompt.is_some() || goal_resume.is_some();
        let startup_goal = if goal_resume.is_some() {
            GoalStart::Resume
        } else {
            GoalStart::Plain
        };

        if let ControlFlow::Break(()) = self.authorize_existing(&view, has_objective).await? {
            return Ok(ControlFlow::Break(()));
        }
        let engine = self.open_engine()?;
        if let Some(capture) = &view.environment_capture
            && capture.outcome() == arany::EnvironmentCaptureStage::Uncertain
        {
            append_notice(&mut self.setup_notice, capture.outcome().summary());
        }
        let skills_supported = self.prepare_composer(&view, reuse_saved_selection).await;
        let setup_notice = self.take_setup_notice();
        let saved_session_id = (view.created_sequence != 0).then_some(session_id);
        let archive_notice = self
            .archived_history
            .as_ref()
            .map(|archived| super::super::archived_history_notice(archived.path()));
        let startup_objective = StartupObjective::new(
            self.admission.prompt.clone(),
            goal_resume,
            saved_session_id,
            setup_notice.clone(),
            archive_notice.as_deref(),
        );
        let notice = match &startup_objective {
            Some(objective) => Some(Feedback::History(objective.notice.clone())),
            None => final_startup_notice(
                setup_notice.or_else(|| {
                    self.terminal
                        .is_linear()
                        .then(|| objective_notice(saved_session_id, None))
                }),
                archive_notice.as_deref(),
            ),
        };
        let mut parts = SessionParts {
            engine: std::sync::Arc::new(engine),
            view,
            composer: self.composer,
            terminal: self.terminal,
            admission: self.admission,
            session_id,
            saved_view: self.saved_view,
            notice,
            skills_supported,
            archived_history: self.archived_history,
            archive_notice,
        };
        if let Some(objective) = startup_objective {
            Self::submit_startup(&mut parts, objective, startup_goal, self.access).await?;
        }
        Ok(ControlFlow::Continue(AttachedSession::new(parts)))
    }

    async fn submit_startup(
        parts: &mut SessionParts,
        objective: StartupObjective,
        startup_goal: GoalStart,
        access: SavedAccess,
    ) -> Result<(), String> {
        let StartupObjective { prompt, notice } = objective;
        let startup_skill = (startup_goal == GoalStart::Plain)
            .then(|| match parts.composer.parse_submission(&prompt) {
                Ok(Submission::Skill { name, .. }) => Some(name.to_owned()),
                Ok(Submission::Objective(_) | Submission::Command { .. }) | Err(_) => None,
            })
            .flatten();
        require_access(
            matches!(access, SavedAccess::Authorized),
            parts.notice.as_ref(),
        )?;
        active::validate_objective_inputs(&parts.admission, &prompt)?;
        materialize_session(&parts.admission, &mut parts.session_id, &mut parts.view).await?;
        parts
            .terminal
            .draw_progress(&parts.view, &notice)
            .map_err(|error| error.to_string())?;
        parts.notice = active::PendingSubmission {
            engine: &parts.engine,
            admission: &parts.admission,
            goal: startup_goal,
            objective: prompt,
            skill: startup_skill,
            draft: None,
        }
        .run(&mut parts.terminal, &mut parts.view, &mut parts.composer)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_objectives_include_promptless_goal_resume_notices() {
        let session_id = SessionId::from_str("01900000-0000-7000-8000-000000000001").unwrap();
        let goal_id = arany::GoalId::new();
        for (session_id, expected_notice) in [
            (None, "New conversation · /help for commands"),
            (
                Some(session_id),
                "Session 01900000-0000-7000-8000-000000000001 · /help for commands",
            ),
        ] {
            for (prompt, goal_resume, expected_prompt) in [
                (None, Some(goal_id), ""),
                (Some("Continue the task"), None, "Continue the task"),
                (
                    Some("Continue the task"),
                    Some(goal_id),
                    "Continue the task",
                ),
            ] {
                for (setup_notice, expected_notice) in [
                    (None, expected_notice),
                    (Some("Setup ready"), "Setup ready"),
                ] {
                    let objective = StartupObjective::new(
                        prompt.map(str::to_owned),
                        goal_resume,
                        session_id,
                        setup_notice.map(str::to_owned),
                        None,
                    )
                    .expect("startup objective missing");
                    assert_eq!(objective.prompt, expected_prompt);
                    assert_eq!(
                        objective.notice, expected_notice,
                        "prompt={prompt:?}, resume={goal_resume:?}, setup={setup_notice:?}"
                    );
                }
            }
        }
        assert!(StartupObjective::new(None, None, None, None, None).is_none());
    }
}

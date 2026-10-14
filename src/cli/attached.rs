use super::{chatgpt, exec::ProviderArg};
use arany::{
    AttachedTerminal, CollaborationPolicy, Composer, ComposerEdit, Effort, EngineError,
    InteractiveCommand, RunOutcome, RunStatus, SessionDefaults, SessionId, SessionView, StateRoot,
    Store, StoreError, Submission, Telemetry, TerminalInput, continue_session, create_session,
    fork_session, native_protection_supported, parse_submission, render_run_feedback,
    resume_session, set_session_defaults,
};
use clap::Args;
use std::ops::ControlFlow;
use std::{
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    str::FromStr,
    time::Instant,
};

mod active;
mod agents;
mod controls;
mod models;
mod permissions;
mod picker;
mod run;
mod setup;
use active::{CompactionOutcome, GoalStart, RunSubmission, compact_current, submit_objective};
use models::ModelBrowse;
use picker::{PickerChoice, QuickChoice, pick_quick_action, pick_session};

const MAX_OBJECTIVE_BYTES: usize = 8 * 1024;
const EMPTY_INTERRUPT_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
const STATE_UNAVAILABLE: &str = "state directory unavailable or unsafe";

#[derive(Args, Default)]
pub(crate) struct AttachedArgs {
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long, help = "openai, anthropic, chatgpt, or custom:NAME")]
    provider: Option<ProviderArg>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, help = "Model-specific reasoning effort")]
    effort: Option<Effort>,
    #[arg(long)]
    screen_reader: bool,
    #[arg(long, help = "Use the classic chat view instead of the saved one")]
    classic: bool,
    #[arg(long)]
    no_color: bool,
    #[arg(long, help = "Open account setup before starting a new Session")]
    setup: bool,
    #[arg(long = "include")]
    include_paths: Vec<PathBuf>,
    #[arg(long, help = "Enable guarded tools from private tools.json")]
    tools: bool,
    #[arg(long, conflicts_with = "fork")]
    #[arg(num_args = 0..=1)]
    resume: Option<Option<String>>,
    #[arg(long, conflicts_with = "resume")]
    fork: Option<String>,
    #[arg(
        long,
        help = "Explicitly resume an interrupted Goal in --resume SESSION_ID"
    )]
    resume_goal: Option<String>,
    #[arg(long = "continue", conflicts_with_all = ["resume", "fork"])]
    continue_session: bool,
    prompt: Option<String>,
}

#[derive(Clone, Copy)]
enum EntryMode {
    New,
    Resume(SessionId),
    PickSession,
    Fork(SessionId),
    Continue,
}

enum Feedback {
    History(String),
    Transient(String),
}

impl Feedback {
    fn notice(&self) -> arany::TerminalNotice<'_> {
        match self {
            Self::History(text) => arany::TerminalNotice::History(text),
            Self::Transient(text) => arany::TerminalNotice::Transient(text),
        }
    }
}

impl From<String> for Feedback {
    fn from(text: String) -> Self {
        Self::History(text)
    }
}

impl std::ops::Deref for Feedback {
    type Target = str;
    fn deref(&self) -> &str {
        match self {
            Self::History(text) | Self::Transient(text) => text,
        }
    }
}

impl std::fmt::Display for Feedback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self)
    }
}

/// `Notice` keeps the Session; `Fatal` ends it through terminal restoration. Unclassified errors are `Fatal`.
#[derive(Debug)]
enum LoopError {
    Fatal(String),
    Notice(String),
}

impl From<String> for LoopError {
    fn from(error: String) -> Self {
        Self::Fatal(error)
    }
}

impl From<&str> for LoopError {
    fn from(error: &str) -> Self {
        Self::Fatal(error.to_owned())
    }
}

/// The attached loop's only decision point for command failures.
fn feedback_or_end<T: Into<Feedback>>(result: Result<T, LoopError>) -> Result<Feedback, String> {
    match result {
        Ok(feedback) => Ok(feedback.into()),
        Err(LoopError::Notice(text)) => Ok(Feedback::History(text)),
        Err(LoopError::Fatal(reason)) => Err(reason),
    }
}

fn open_state(admission: &Admission) -> Result<StateRoot, LoopError> {
    StateRoot::open_existing(&admission.state_dir)
        .map_err(|_| LoopError::Notice(format!("Error: {STATE_UNAVAILABLE}")))
}

#[derive(Clone, Copy)]
enum Selector {
    Models,
    Agents,
    Resume,
    Setup,
}

struct Admission {
    telemetry: Telemetry,
    state_dir: PathBuf,
    explicit_state_dir: bool,
    workspace: PathBuf,
    include_paths: Vec<PathBuf>,
    tools: bool,
    workspace_permissions: Option<arany::WorkspacePermissions>,
    defaults: SessionDefaults,
    screen_reader: bool,
    classic: bool,
    no_color: bool,
    setup_requested: bool,
    entry: EntryMode,
    prompt: Option<String>,
    resume_goal: Option<arany::GoalId>,
}

impl AttachedArgs {
    pub(crate) fn is_empty(&self) -> bool {
        self.state_dir.is_none()
            && self.workspace.is_none()
            && self.provider.is_none()
            && self.model.is_none()
            && self.effort.is_none()
            && !self.screen_reader
            && !self.classic
            && !self.no_color
            && !self.setup
            && self.include_paths.is_empty()
            && !self.tools
            && self.resume.is_none()
            && self.fork.is_none()
            && self.resume_goal.is_none()
            && !self.continue_session
            && self.prompt.is_none()
    }

    fn admit(self, telemetry: Telemetry) -> Result<Admission, String> {
        if self.model.as_ref().is_some_and(|model| {
            model.is_empty() || model.len() > 128 || model.chars().any(char::is_control)
        }) {
            return Err("invalid model".into());
        }
        if self
            .prompt
            .as_ref()
            .is_some_and(|prompt| prompt.trim().is_empty() || prompt.len() > MAX_OBJECTIVE_BYTES)
        {
            return Err("invalid objective".into());
        }
        let entry = match (self.resume, self.fork, self.continue_session) {
            (Some(Some(id)), None, false) => EntryMode::Resume(
                SessionId::from_str(&id).map_err(|_| "invalid Session ID".to_owned())?,
            ),
            (Some(None), None, false) => EntryMode::PickSession,
            (None, Some(id), false) => EntryMode::Fork(
                SessionId::from_str(&id).map_err(|_| "invalid Session ID".to_owned())?,
            ),
            (None, None, true) => EntryMode::Continue,
            (None, None, false) => EntryMode::New,
            _ => return Err("choose only one Session entry mode".into()),
        };
        let goal_resume = self
            .resume_goal
            .as_deref()
            .map(arany::GoalId::from_str)
            .transpose()
            .map_err(|_| "invalid Goal ID".to_owned())?;
        if goal_resume.is_some() && !matches!(entry, EntryMode::Resume(_)) {
            return Err("--resume-goal requires --resume SESSION_ID".into());
        }
        if self.model.is_some()
            && self.provider.is_none()
            && !matches!(
                entry,
                EntryMode::Resume(_) | EntryMode::PickSession | EntryMode::Continue
            )
        {
            return Err("--model requires --provider".into());
        }
        if self.effort.is_some()
            && self.model.is_none()
            && !matches!(
                entry,
                EntryMode::Resume(_) | EntryMode::PickSession | EntryMode::Continue
            )
        {
            return Err("--effort requires --model".into());
        }
        if matches!(self.provider.as_ref(), Some(ProviderArg::Chatgpt))
            && (self.model.is_none() || self.effort.is_none())
        {
            return Err("ChatGPT requires --model and --effort".into());
        }
        if self.setup && !matches!(entry, EntryMode::New) {
            return Err("--setup requires a new Session".into());
        }
        if self.setup && (self.provider.is_some() || self.model.is_some() || self.effort.is_some())
        {
            return Err("--setup cannot be combined with Provider selection flags".into());
        }
        if self.prompt.is_some()
            && !matches!(
                entry,
                EntryMode::Resume(_) | EntryMode::PickSession | EntryMode::Continue
            )
            && (self.provider.is_some() != self.model.is_some())
        {
            return Err("a prompt requires --provider and --model".into());
        }
        let explicit_state_dir = self.state_dir.is_some();
        let (workspace, state_dir) =
            super::admit_workspace(self.workspace, self.state_dir).map_err(str::to_owned)?;
        let provider = self.provider.map(|value| value.profile_label());
        let defaults = SessionDefaults {
            provider,
            model: self.model,
            effort: self.effort,
            account_id: None,
            policy: CollaborationPolicy::default(),
        };
        Ok(Admission {
            telemetry,
            state_dir,
            explicit_state_dir,
            workspace,
            include_paths: self.include_paths,
            tools: self.tools,
            workspace_permissions: None,
            defaults,
            classic: self.classic,
            screen_reader: self.screen_reader
                || std::env::var("ARANY_SCREEN_READER").ok().as_deref() == Some("1"),
            no_color: self.no_color || std::env::var_os("NO_COLOR").is_some(),
            setup_requested: self.setup,
            entry,
            prompt: self.prompt,
            resume_goal: goal_resume,
        })
    }
}

pub(crate) async fn run(args: AttachedArgs, telemetry: Telemetry) -> Result<(), String> {
    let mut admission = args.admit(telemetry)?;
    let goal_resume = admission.resume_goal;
    let restored_settings = controls::restore_settings(&admission).await;
    let mut saved_view = restored_settings.as_ref().copied().unwrap_or_default().view;
    let mut terminal = AttachedTerminal::acquire_with_mode(
        admission.screen_reader,
        if admission.classic {
            arany::TerminalMode::Classic
        } else {
            saved_view
        },
    )
    .map_err(|error| error.to_string())?;
    if admission.classic {
        terminal.pin_classic();
    }
    if admission.no_color {
        terminal.disable_color();
    }
    let mut composer = Composer::default();
    let mut setup_notice = None;
    if admit_startup(
        &mut terminal,
        &mut admission,
        &mut composer,
        &mut setup_notice,
    )
    .await?
    .is_break()
    {
        return Ok(());
    }
    if !native_protection_supported() {
        append_notice(&mut setup_notice, permissions::UNAVAILABLE_NOTICE);
    }
    let explicit_provider = admission.defaults.provider.is_some();
    if admission.defaults.provider.as_deref() == Some("chatgpt") {
        match chatgpt::selected_account_id() {
            Ok(id) => admission.defaults.account_id = Some(id),
            Err(error) => {
                append_notice(&mut setup_notice, &restore_failure_notice(error));
                admission.defaults = SessionDefaults::default();
            }
        }
    }
    let mut setup_catalog = None;
    let reuse_saved_selection = matches!(admission.entry, EntryMode::New) && !explicit_provider;
    let mut access_checked = false;
    if matches!(admission.entry, EntryMode::New) && !explicit_provider {
        if admission.setup_requested {
            match setup::resolve(&mut terminal, &admission.workspace).await {
                Ok(Some(setup::SetupSelection::Native {
                    defaults,
                    notice,
                    catalog,
                })) => {
                    admission.defaults = defaults;
                    setup_notice = Some(notice);
                    setup_catalog = Some(catalog);
                }
                Ok(Some(setup::SetupSelection::ChatGpt {
                    defaults,
                    notice,
                    catalog,
                })) => {
                    admission.defaults = defaults;
                    setup_notice = notice;
                    setup_catalog = Some(catalog);
                }
                Ok(None) => return Ok(()),
                Err(setup::SetupError::Recoverable(error)) => {
                    append_notice(
                        &mut setup_notice,
                        &format!("Setup could not be completed: {error}; use /setup to try again"),
                    );
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            match setup::saved_defaults(&mut terminal, &admission.workspace).await {
                Ok(setup::SavedDefaults::Selected(defaults)) => admission.defaults = defaults,
                Ok(setup::SavedDefaults::AccessChecked(defaults)) => {
                    admission.defaults = defaults;
                    access_checked = true;
                }
                Ok(setup::SavedDefaults::Cancelled) => return Ok(()),
                Ok(setup::SavedDefaults::Unconfigured) => {}
                Err(setup::SetupError::Recoverable(error)) => {
                    append_notice(&mut setup_notice, &restore_failure_notice(error));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    let mut access_authorized = true;
    if matches!(admission.entry, EntryMode::New) && !admission.setup_requested && !access_checked {
        match authorize_or_notice(&mut terminal, &admission.defaults, &mut setup_notice).await? {
            ControlFlow::Continue(authorized) => access_authorized = authorized,
            ControlFlow::Break(()) => return Ok(()),
        }
    }
    if reuse_saved_selection && access_authorized {
        let mut remembered = admission.defaults.clone();
        match models::restore_saved_models(
            &admission.workspace,
            &admission.state_dir,
            &mut remembered,
            true,
        )
        .await
        {
            Ok(catalog) => {
                if setup_catalog
                    .as_ref()
                    .is_none_or(|fresh: &Vec<arany::ModelEntry>| {
                        fresh
                            .iter()
                            .any(|row| Some(row.id.as_str()) == remembered.model.as_deref())
                    })
                {
                    admission.defaults = remembered;
                }
                if setup_catalog.is_none() {
                    setup_catalog = Some(catalog);
                }
            }
            Err(error) => {
                setup_notice = Some(format!("Error: {error}; choose /model to select again"))
            }
        }
    }
    if matches!(admission.entry, EntryMode::PickSession) {
        let id = match pick_session(&mut terminal, &admission, None).await? {
            PickerChoice::Selected(id) => id,
            PickerChoice::Unavailable(error) => return Err(error),
            PickerChoice::Empty => return Err("No Sessions for this Workspace".into()),
            PickerChoice::Closed => return Ok(()),
            PickerChoice::Shutdown(signal) => {
                return Err(format!("terminated by {}", signal.name()));
            }
        };
        admission.entry = EntryMode::Resume(id);
    }
    if admission.prompt.is_some()
        && admission.defaults.provider.as_deref() == Some("chatgpt")
        && admission.defaults.model.is_none()
    {
        return Err(
            "ChatGPT account saved; use /model to select a model and effort before a Run".into(),
        );
    }
    if admission.prompt.is_some() && matches!(admission.entry, EntryMode::New) {
        require_access(access_authorized, setup_notice.as_ref())?;
        preflight_prompt(
            &admission.state_dir,
            &admission.workspace,
            &admission.defaults,
            PromptEntry::New,
        )
        .await?;
    }
    match restored_settings {
        Ok(restored) => composer.set_completion_layout(restored.completion),
        Err(error) => append_notice(
            &mut setup_notice,
            &format!("Error: {error}; using default settings"),
        ),
    }
    let (mut session_id, mut view) = start_session(&admission).await?;
    if let Some(goal_id) = goal_resume
        && view.interrupted_goal_id() != Some(goal_id)
    {
        return Err("selected Session does not contain that interrupted Goal".into());
    }
    // `--resume-goal` is the explicit confirmation: without a prompt it resumes at once.
    let startup_objective = admission
        .prompt
        .clone()
        .or_else(|| goal_resume.map(|_| String::new()));
    let startup_goal = if goal_resume.is_some() {
        GoalStart::Resume
    } else {
        GoalStart::Plain
    };
    if !matches!(admission.entry, EntryMode::New) {
        match authorize_or_notice(&mut terminal, &view.defaults, &mut setup_notice).await? {
            ControlFlow::Continue(authorized) => access_authorized = authorized,
            ControlFlow::Break(()) => return Ok(()),
        }
        if startup_objective.is_some() {
            require_access(access_authorized, setup_notice.as_ref())?;
            preflight_prompt(
                &admission.state_dir,
                &admission.workspace,
                &view.defaults,
                PromptEntry::Existing,
            )
            .await?;
        }
    }
    if let Ok(root) = StateRoot::open_existing(&admission.state_dir) {
        arany::enable_development_diagnostics(&root);
    }
    permissions::refresh_mode(
        &mut composer,
        view.defaults.provider.as_deref(),
        admission.tools,
        admission.workspace_permissions.as_ref(),
    );
    if setup_catalog.is_none() && access_authorized {
        match models::restore_saved_models(
            &admission.workspace,
            &admission.state_dir,
            &mut view.defaults.clone(),
            false,
        )
        .await
        {
            Ok(catalog) => setup_catalog = Some(catalog),
            Err(error) => setup_notice = Some(format!("Error: {error}; cached models unavailable")),
        }
    }
    if (admission.setup_requested
        || reuse_saved_selection
        || matches!(
            admission.entry,
            EntryMode::Resume(_) | EntryMode::Continue | EntryMode::Fork(_)
        ))
        && let Err(error) = models::remember_models(
            &admission.workspace,
            &view.defaults,
            setup_catalog.as_deref(),
            true,
        )
        .await
    {
        setup_notice = Some(format!(
            "Error: {error}; selection kept for this conversation only"
        ));
    }
    let mut skills_supported = permissions::supports_tools(view.defaults.provider.as_deref());
    refresh_runtime_skills(
        &admission,
        &mut composer,
        &mut setup_notice,
        skills_supported,
    );
    seed_setup_catalog(&mut composer, &view.defaults, setup_catalog.as_deref());
    let mut notice = if setup_notice.is_some() {
        setup_notice
    } else if admission.setup_requested
        && admission.defaults.provider.as_deref() == Some("chatgpt")
        && admission.defaults.model.is_none()
    {
        Some("ChatGPT account saved; use /model to select a model and effort".into())
    } else {
        (terminal.is_linear() || admission.prompt.is_some()).then(|| {
            if view.created_sequence == 0 {
                "New conversation · /help for commands".into()
            } else {
                format!("Session {session_id} · /help for commands")
            }
        })
    }
    .map(Feedback::History);
    let mut empty_interrupt = None::<Instant>;
    if let Some(prompt) = startup_objective {
        require_access(access_authorized, notice.as_ref())?;
        active::validate_objective_inputs(&admission, &prompt)?;
        materialize_session(&admission, &mut session_id, &mut view).await?;
        terminal
            .draw_progress(&view, notice.as_deref().expect("startup notice exists"))
            .map_err(|error| error.to_string())?;
        match submit_objective(
            &mut terminal,
            &admission,
            startup_goal,
            &mut view,
            &mut composer,
            prompt,
        )
        .await?
        {
            RunSubmission::Completed(outcome, user_recorded) => {
                notice = finish_submission(
                    &mut terminal,
                    &admission,
                    &mut composer,
                    &mut view,
                    &outcome,
                    user_recorded,
                )
                .await?;
            }
            RunSubmission::InterruptedBeforeRun => {
                notice = finish_interrupted_submission(
                    &mut terminal,
                    &admission,
                    &composer,
                    &mut view,
                    session_id,
                )
                .await?;
            }
            RunSubmission::Shutdown(reason) => return Err(reason),
        }
    }
    loop {
        let supported = permissions::supports_tools(view.defaults.provider.as_deref());
        if supported != skills_supported {
            refresh_runtime_skills(&admission, &mut composer, &mut notice, supported);
            skills_supported = supported;
        }
        permissions::refresh_mode(
            &mut composer,
            view.defaults.provider.as_deref(),
            admission.tools,
            admission.workspace_permissions.as_ref(),
        );
        if let Some(error) = permissions::refresh_files(&admission.workspace, &mut composer) {
            append_notice(&mut notice, &error);
        }
        composer.set_completion_selection(
            view.defaults.provider.as_deref(),
            view.defaults.model.as_deref(),
            view.defaults.account_id,
        );
        terminal
            .draw_notice(&view, &composer, notice.as_ref().map(Feedback::notice))
            .map_err(|error| error.to_string())?;
        notice = None;
        let input = terminal
            .next_input()
            .await
            .map_err(|error| error.to_string())?;
        if terminal.help_open()
            && !matches!(input, TerminalInput::Suspend | TerminalInput::Shutdown(_))
        {
            terminal.help_input(input);
            continue;
        }
        if terminal
            .handle_transcript_input(input, &view)
            .map_err(|error| error.to_string())?
        {
            continue;
        }
        if terminal.handle_history_input(input, &view) {
            continue;
        }
        match input {
            TerminalInput::CycleApprovalMode => {
                notice = Some(Feedback::History(permissions::cycle(&mut composer)));
                empty_interrupt = None;
            }
            TerminalInput::ClipboardPaste => {
                notice = Some(match terminal.request_clipboard_paste() {
                    Ok(()) => Feedback::Transient("Reading clipboard... draft retained".into()),
                    Err(error) => Feedback::History(format!("Error: {error}; draft unchanged")),
                });
                empty_interrupt = None;
            }
            TerminalInput::Paste => {
                if let Err(error) = terminal.paste_into(&mut composer) {
                    notice = Some(Feedback::History(format!(
                        "Error: {error}; draft unchanged"
                    )));
                }
                empty_interrupt = None;
            }
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
            | TerminalInput::Escape) => {
                if composer.apply(input) == ComposerEdit::AtCapacity {
                    notice = Some(Feedback::History(
                        "Error: message is too long; shorten it; draft unchanged".into(),
                    ));
                }
                empty_interrupt = None;
            }
            TerminalInput::LineRejected => {
                notice = Some(Feedback::History(
                    "Error: invalid or overlong terminal line; draft unchanged".into(),
                ));
                empty_interrupt = None;
            }
            TerminalInput::LineContinued => {
                notice = Some(Feedback::History(format!(
                    "Draft: {} characters; Enter submits",
                    composer.character_count()
                )));
                empty_interrupt = None;
            }
            TerminalInput::QuickActions if !terminal.is_linear() => {
                match pick_quick_action(&mut terminal, &composer, view.defaults.provider.is_none())
                    .await?
                {
                    QuickChoice::Selected(Selector::Resume) if !composer.is_empty() => {
                        notice = Some(Feedback::History(
                            "Submit or clear this draft before switching Sessions".into(),
                        ));
                    }
                    QuickChoice::Selected(selector) => {
                        notice = Some(feedback_or_end(
                            run_selector(
                                selector,
                                &mut terminal,
                                &mut admission,
                                &mut session_id,
                                &mut view,
                                &mut composer,
                            )
                            .await,
                        )?);
                    }
                    QuickChoice::Closed => {}
                    QuickChoice::Shutdown(signal) => {
                        return Err(format!("terminated by {}", signal.name()));
                    }
                }
                empty_interrupt = None;
            }
            TerminalInput::Submit if terminal.clipboard_loading() => {
                notice = Some(Feedback::Transient(
                    "Clipboard loading; press Enter after paste to submit".into(),
                ));
                terminal
                    .restore_draft_input(composer.text().len())
                    .map_err(|error| error.to_string())?;
                empty_interrupt = None;
            }
            TerminalInput::Submit => {
                if !terminal.is_linear()
                    && composer.apply(TerminalInput::Submit) == ComposerEdit::Changed
                {
                    empty_interrupt = None;
                    continue;
                }
                match composer.submission() {
                    Err(error) => {
                        match error {
                            arany::CommandParseError::Empty => composer.clear(),
                            arany::CommandParseError::Unknown { suggestion } => {
                                notice = Some(Feedback::History(match suggestion {
                                    Some(name) => {
                                        format!("Error: Unknown command; did you mean /{name}?")
                                    }
                                    None => "Error: Unknown command".into(),
                                }));
                            }
                            error => notice = Some(Feedback::History(format!("Error: {error}"))),
                        }
                        if terminal.is_linear() {
                            composer.take();
                        }
                        empty_interrupt = None;
                        continue;
                    }
                    Ok(Submission::Command {
                        command: InteractiveCommand::Goal,
                        argument: Some(argument),
                    }) => {
                        let start = if argument == "resume" {
                            GoalStart::Resume
                        } else {
                            GoalStart::Autonomous
                        };
                        if let Err(message) = active::preflight_idle_run(
                            &admission,
                            &view,
                            start,
                            composer.text(),
                            !composer.images().is_empty(),
                            terminal.is_linear(),
                        ) {
                            notice = Some(Feedback::History(message));
                            if terminal.is_linear() {
                                composer.take();
                            }
                            empty_interrupt = None;
                            continue;
                        }
                    }
                    Ok(Submission::Command { command, argument }) => {
                        if !composer.images().is_empty()
                            && matches!(
                                command,
                                InteractiveCommand::New
                                    | InteractiveCommand::Resume
                                    | InteractiveCommand::Fork
                            )
                        {
                            notice = Some(Feedback::History("Error: Submit or clear image attachments before switching Sessions".into()));
                            if terminal.is_linear() {
                                composer.take();
                            }
                            empty_interrupt = None;
                            continue;
                        }
                        if let Err(error) =
                            controls::validate_idle_command(&view, command, argument)
                        {
                            notice = Some(Feedback::History(format!("Error: {error}")));
                            if terminal.is_linear() {
                                composer.take();
                            }
                            empty_interrupt = None;
                            continue;
                        }
                    }
                    Ok(Submission::Objective(_)) => {
                        if let Err(message) = active::preflight_idle_run(
                            &admission,
                            &view,
                            GoalStart::Plain,
                            composer.text(),
                            !composer.images().is_empty(),
                            terminal.is_linear(),
                        ) {
                            notice = Some(Feedback::History(message));
                            if terminal.is_linear() {
                                composer.take();
                            }
                            empty_interrupt = None;
                            continue;
                        }
                    }
                }
                let line = composer.take();
                let submission = match parse_submission(&line) {
                    Err(arany::CommandParseError::Empty) if !composer.images().is_empty() => {
                        Submission::Objective(&line)
                    }
                    result => result.expect("validated submission"),
                };
                match submission {
                    Submission::Objective(objective) => {
                        notice = submit_idle_run(
                            &mut terminal,
                            &admission,
                            &mut composer,
                            &mut session_id,
                            &mut view,
                            GoalStart::Plain,
                            objective.to_owned(),
                        )
                        .await?;
                    }
                    Submission::Command {
                        command: InteractiveCommand::Goal,
                        argument: Some(argument),
                    } => {
                        let (start, objective) = if argument == "resume" {
                            (GoalStart::Resume, String::new())
                        } else {
                            (GoalStart::Autonomous, argument.to_owned())
                        };
                        notice = submit_idle_run(
                            &mut terminal,
                            &admission,
                            &mut composer,
                            &mut session_id,
                            &mut view,
                            start,
                            objective,
                        )
                        .await?;
                    }
                    Submission::Command { command, argument } => {
                        if command == InteractiveCommand::Exit {
                            break;
                        }
                        if command == InteractiveCommand::Paste {
                            notice = Some(match terminal.request_clipboard_paste() {
                                Ok(()) => Feedback::Transient(
                                    "Reading clipboard... draft retained".into(),
                                ),
                                Err(error) => {
                                    Feedback::History(format!("Error: {error}; draft unchanged"))
                                }
                            });
                        } else if command == InteractiveCommand::Transcript {
                            terminal
                                .toggle_transcript(&view)
                                .map_err(|error| error.to_string())?;
                        } else if command == InteractiveCommand::Settings {
                            notice = Some(Feedback::History(
                                controls::settings(
                                    &mut terminal,
                                    &admission,
                                    &mut composer,
                                    &mut saved_view,
                                )
                                .await?,
                            ));
                        } else if command == InteractiveCommand::Permissions {
                            let permissions = match arany::WorkspacePermissions::open(
                                &admission.workspace,
                            ) {
                                Ok(permissions) => permissions.prepare_profile_selection(),
                                Err(error) => {
                                    notice = Some(Feedback::History(format!(
                                        "Error: Could not load folder trust: {error}; existing permissions unchanged"
                                    )));
                                    continue;
                                }
                            };
                            let choice = permissions::choose(
                                &mut terminal,
                                permissions,
                                &mut composer,
                                false,
                            )
                            .await?;
                            let access = match choice {
                                permissions::PermissionChoice::Selected(access) => access,
                                permissions::PermissionChoice::Closed(_) => {
                                    notice =
                                        Some(Feedback::History("Permissions unchanged".into()));
                                    continue;
                                }
                                permissions::PermissionChoice::Rejected(_, error) => {
                                    notice = Some(Feedback::History(format!(
                                        "Error: {error}; existing permissions unchanged"
                                    )));
                                    continue;
                                }
                                permissions::PermissionChoice::Exit => return Ok(()),
                            };
                            notice = Some(Feedback::History(if !native_protection_supported() {
                                format!(
                                    "Folder {} · {}",
                                    if access.is_trusted() {
                                        "trusted"
                                    } else {
                                        "read only"
                                    },
                                    permissions::UNAVAILABLE_NOTICE
                                )
                            } else if access.is_trusted() {
                                format!(
                                    "Folder trusted · {} · Shift+Tab changes mode",
                                    access.mode().label()
                                )
                            } else {
                                "Read only · /permissions can enable guarded Tools".into()
                            }));
                            admission.workspace_permissions = Some(access);
                            admission.tools = false;
                            refresh_runtime_skills(
                                &admission,
                                &mut composer,
                                &mut notice,
                                skills_supported,
                            );
                        } else if command == InteractiveCommand::Setup {
                            notice = Some(
                                configure_account(
                                    &mut terminal,
                                    &mut admission,
                                    session_id,
                                    &mut view,
                                    &mut composer,
                                )
                                .await?,
                            );
                        } else if command == InteractiveCommand::Help {
                            terminal.open_help().map_err(|error| error.to_string())?;
                        } else if command == InteractiveCommand::Resume && argument.is_none() {
                            notice = Some(feedback_or_end(
                                run_selector(
                                    Selector::Resume,
                                    &mut terminal,
                                    &mut admission,
                                    &mut session_id,
                                    &mut view,
                                    &mut composer,
                                )
                                .await,
                            )?);
                        } else if command == InteractiveCommand::Compact {
                            if view.created_sequence == 0 {
                                notice = Some(Feedback::History(
                                    "No messages to compact; send a message first".into(),
                                ));
                                continue;
                            }
                            let maintenance = match compact_current(
                                &mut terminal,
                                &admission,
                                &view,
                                &mut composer,
                                None,
                            )
                            .await?
                            {
                                CompactionOutcome::Notice(notice)
                                | CompactionOutcome::Interrupted(notice) => notice,
                                CompactionOutcome::Unavailable(error) => {
                                    format!("Error: {error}")
                                }
                                CompactionOutcome::Shutdown(signal) => {
                                    return Err(format!("terminated by {}", signal.name()));
                                }
                            };
                            let text = match retained_draft_notice(&composer) {
                                Some(draft) => format!("{maintenance}; {draft}"),
                                None => maintenance,
                            };
                            notice = reload_view_or_keep(
                                &mut view,
                                load_view(&admission.state_dir, session_id).await,
                                Some(text),
                                None,
                            )
                            .map(Feedback::History);
                        } else if argument.is_none()
                            && matches!(
                                command,
                                InteractiveCommand::Model | InteractiveCommand::Agents
                            )
                        {
                            let selector = if command == InteractiveCommand::Agents {
                                Selector::Agents
                            } else {
                                Selector::Models
                            };
                            notice = Some(feedback_or_end(
                                run_selector(
                                    selector,
                                    &mut terminal,
                                    &mut admission,
                                    &mut session_id,
                                    &mut view,
                                    &mut composer,
                                )
                                .await,
                            )?);
                        } else {
                            notice = Some(feedback_or_end(
                                controls::handle_command(
                                    &admission,
                                    &mut session_id,
                                    &mut view,
                                    command,
                                    argument,
                                )
                                .await,
                            )?);
                        }
                    }
                }
                empty_interrupt = None;
            }
            TerminalInput::Interrupt => {
                if terminal.clipboard_loading() {
                    terminal.cancel_clipboard_paste();
                    notice = Some(Feedback::Transient(
                        "Clipboard read cancelled; draft retained".into(),
                    ));
                    empty_interrupt = None;
                } else if !composer.is_empty() {
                    composer.clear();
                    terminal.discard_draft_input();
                    empty_interrupt = None;
                } else if empty_interrupt
                    .is_some_and(|time| time.elapsed() <= EMPTY_INTERRUPT_WINDOW)
                {
                    break;
                } else {
                    empty_interrupt = Some(Instant::now());
                    notice = Some(Feedback::History(
                        "Ctrl+Shift+C copies. Ctrl+C again exits.".into(),
                    ));
                }
            }
            TerminalInput::EndOfInput if composer.is_empty() => break,
            TerminalInput::EndOfInput => {
                notice = Some(Feedback::History(
                    "Draft retained; press Enter to submit or Ctrl+C to clear".into(),
                ));
            }
            TerminalInput::Resize => {}
            TerminalInput::Suspend => {
                terminal
                    .suspend_and_resume(composer.text().len())
                    .map_err(|error| error.to_string())?;
                notice = Some(Feedback::Transient("Terminal resumed".into()));
            }
            TerminalInput::Shutdown(signal) => {
                return Err(format!("terminated by {}", signal.name()));
            }
            _ => {}
        }
    }
    drop(terminal);
    if view.created_sequence != 0 {
        writeln!(std::io::stderr(), "Session: {session_id}")
            .map_err(|_| "output failed".to_owned())?;
    } else {
        writeln!(std::io::stderr(), "No conversation saved.")
            .map_err(|_| "output failed".to_owned())?;
    }
    Ok(())
}

async fn run_selector(
    selector: Selector,
    terminal: &mut AttachedTerminal,
    admission: &mut Admission,
    session_id: &mut SessionId,
    view: &mut SessionView,
    composer: &mut Composer,
) -> Result<Feedback, LoopError> {
    match selector {
        Selector::Setup => {
            Ok(configure_account(terminal, admission, *session_id, view, composer).await?)
        }
        Selector::Models => {
            match models::browse(terminal, admission, *session_id, view, composer).await? {
                ModelBrowse::Notice(notice) => Ok(notice),
                ModelBrowse::Shutdown(signal) => {
                    Err(format!("terminated by {}", signal.name()).into())
                }
            }
        }
        Selector::Agents => match agents::inspect(terminal, view).await? {
            Some(signal) => Err(format!("terminated by {}", signal.name()).into()),
            None => Ok(Feedback::Transient("Agent inspection closed".into())),
        },
        Selector::Resume => {
            let choice = pick_session(terminal, admission, Some(&view.defaults)).await?;
            match choice {
                PickerChoice::Selected(id) => {
                    let root = open_state(admission)?;
                    match resume_session(root, admission.workspace.clone(), id).await {
                        Ok(selected) => {
                            let selected =
                                match prepare_resume(admission, selected, Some(&view.defaults))
                                    .await
                                {
                                    Ok(selected) => selected,
                                    Err(error) => {
                                        return Ok(Feedback::History(format!("Error: {error}")));
                                    }
                                };
                            *session_id = id;
                            *view = selected;
                            Ok(Feedback::History(
                                remembered_session_notice(
                                    admission,
                                    view,
                                    format!(
                                        "Resumed: {}",
                                        arany::escape_terminal(&view.conversation_title())
                                    ),
                                )
                                .await,
                            ))
                        }
                        Err(error) => Ok(Feedback::History(format!("Error: {error}"))),
                    }
                }
                PickerChoice::Unavailable(error) => {
                    Ok(Feedback::History(format!("Error: {error}")))
                }
                PickerChoice::Empty => {
                    Ok(Feedback::History("No Sessions for this Workspace".into()))
                }
                PickerChoice::Closed => Ok(Feedback::Transient("Session selection closed".into())),
                PickerChoice::Shutdown(signal) => {
                    Err(format!("terminated by {}", signal.name()).into())
                }
            }
        }
    }
}

async fn configure_account(
    terminal: &mut AttachedTerminal,
    admission: &mut Admission,
    session_id: SessionId,
    view: &mut SessionView,
    composer: &mut Composer,
) -> Result<Feedback, String> {
    let (defaults, notice, catalog) = match setup::resolve(terminal, &admission.workspace).await {
        Ok(Some(setup::SetupSelection::Native {
            defaults,
            notice,
            catalog,
        })) => (defaults, notice, catalog),
        Ok(Some(setup::SetupSelection::ChatGpt {
            defaults,
            notice,
            catalog,
        })) => {
            let notice = notice.unwrap_or_else(|| {
                if defaults.model.is_some() {
                    "ChatGPT account and model/effort ready".into()
                } else {
                    "ChatGPT account ready; use /model to select a model and effort".into()
                }
            });
            (defaults, notice, catalog)
        }
        Ok(None) => {
            return Ok(Feedback::Transient("Setup closed".into()));
        }
        Err(setup::SetupError::Recoverable(error)) => {
            return Ok(Feedback::History(format!("Error: {error}")));
        }
        Err(error) => return Err(error.into()),
    };
    update_defaults(admission, session_id, view, defaults.clone()).await?;
    admission.defaults = defaults;
    seed_setup_catalog(composer, &view.defaults, Some(&catalog));
    if let Err(error) =
        models::remember_models(&admission.workspace, &view.defaults, Some(&catalog), true).await
    {
        return Ok(Feedback::History(format!(
            "{notice}; Error: {error}; selection kept for this conversation only"
        )));
    }
    Ok(Feedback::History(notice))
}

fn seed_setup_catalog(
    composer: &mut Composer,
    defaults: &SessionDefaults,
    catalog: Option<&[arany::ModelEntry]>,
) {
    composer.set_completion_selection(
        defaults.provider.as_deref(),
        defaults.model.as_deref(),
        defaults.account_id,
    );
    if let (Some(profile), Some(catalog)) = (defaults.provider.as_deref(), catalog) {
        composer.set_model_catalog(profile, catalog, false);
    }
}

fn retained_draft_notice(composer: &Composer) -> Option<Feedback> {
    (!composer.is_empty()).then(|| {
        Feedback::Transient(format!(
            "Draft retained: {} characters; Enter submits",
            composer.character_count()
        ))
    })
}

/// Runs one idle submission, materializing an unsaved conversation first.
async fn submit_idle_run(
    terminal: &mut AttachedTerminal,
    admission: &Admission,
    composer: &mut Composer,
    session_id: &mut SessionId,
    view: &mut SessionView,
    start: GoalStart,
    objective: String,
) -> Result<Option<Feedback>, String> {
    materialize_session(admission, session_id, view).await?;
    match submit_objective(terminal, admission, start, view, composer, objective).await {
        Ok(RunSubmission::Completed(outcome, user_recorded)) => {
            finish_submission(terminal, admission, composer, view, &outcome, user_recorded).await
        }
        Ok(RunSubmission::InterruptedBeforeRun) => {
            finish_interrupted_submission(terminal, admission, composer, view, *session_id).await
        }
        Ok(RunSubmission::Shutdown(reason)) => Err(reason),
        Err(error) => {
            let text = if composer.is_empty() {
                format!("Error: {error}")
            } else {
                format!(
                    "Error: {error}; draft retained ({} characters)",
                    composer.character_count()
                )
            };
            Ok(reload_view_or_keep(
                view,
                load_view(&admission.state_dir, *session_id).await,
                Some(text),
                None,
            )
            .map(Feedback::History))
        }
    }
}

async fn finish_interrupted_submission(
    terminal: &mut AttachedTerminal,
    admission: &Admission,
    composer: &Composer,
    view: &mut SessionView,
    session_id: SessionId,
) -> Result<Option<Feedback>, String> {
    let reload_error = reload_view_or_keep(
        view,
        load_view(&admission.state_dir, session_id).await,
        None,
        None,
    );
    let interrupted = if view.interrupted_goal_id().is_some() {
        "Goal interrupted before the next Run; use /goal resume to continue"
    } else {
        "Submission interrupted before Run admission; no task was accepted. Account work already started may still complete."
    };
    let drawn = match &reload_error {
        Some(error) => format!("{interrupted}; {error}"),
        None => interrupted.to_owned(),
    };
    terminal
        .draw(view, composer, Some(drawn.as_str()))
        .map_err(|error| error.to_string())?;
    let draft = retained_draft_notice(composer);
    Ok(match reload_error {
        Some(error) => Some(Feedback::History(match draft {
            Some(draft) => format!("{draft}; {error}"),
            None => error,
        })),
        None => draft,
    })
}

async fn finish_submission(
    terminal: &mut AttachedTerminal,
    admission: &Admission,
    composer: &mut Composer,
    view: &mut SessionView,
    outcome: &RunOutcome,
    user_recorded: bool,
) -> Result<Option<Feedback>, String> {
    let automatic =
        outcome.run.config.as_ref().filter(|config| {
            outcome.run.status == RunStatus::Finished && config.auto_compaction_due()
        });
    let linear = terminal.is_linear();
    let preview_already_printed = user_recorded && linear;
    let hidden_answer = hidden_answer_guidance(linear);
    let show = show_command(
        outcome.run.id,
        admission
            .explicit_state_dir
            .then_some(admission.state_dir.as_path()),
    );
    let reloaded = load_view(&admission.state_dir, outcome.session_id).await;
    if let Ok(confirmed) = &reloaded {
        confirm_outcome(confirmed, outcome)?;
    }
    print_run_answer(
        terminal,
        reloaded.as_ref().ok(),
        outcome,
        preview_already_printed,
        &show,
    )?;
    if !linear {
        terminal.finish_run_display();
    }
    let mut reload_error = reload_view_or_keep(view, reloaded, None, Some(hidden_answer));
    if automatic.is_some() {
        terminal
            .draw(
                view,
                composer,
                Some("Context threshold reached; attempting automatic compaction"),
            )
            .map_err(|error| error.to_string())?;
    }
    terminal
        .restore_draft_input(composer.text().len())
        .map_err(|error| error.to_string())?;
    let mut notice = retained_draft_notice(composer);
    let feedback = render_run_feedback(&outcome.run);
    if let Some(feedback) = feedback {
        if composer.is_empty() {
            notice = Some(Feedback::History(feedback));
        } else {
            terminal
                .draw(view, composer, Some(&feedback))
                .map_err(|error| error.to_string())?;
        }
    }
    if let Some(config) = automatic {
        let compaction_notice = match compact_current(
            terminal,
            admission,
            view,
            composer,
            Some((config, outcome.run.id)),
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
        let text = match retained_draft_notice(composer) {
            Some(draft) => format!("{compaction_notice}; {draft}"),
            None => compaction_notice,
        };
        // The compaction reload supersedes an earlier failed reload, which is reported at most once here.
        let answer_hidden = reload_error.is_some();
        reload_error = None;
        let reloaded = load_view(&admission.state_dir, outcome.session_id).await;
        if let (true, Ok(confirmed)) = (answer_hidden, &reloaded) {
            confirm_outcome(confirmed, outcome)?;
            print_run_answer(
                terminal,
                Some(confirmed),
                outcome,
                preview_already_printed,
                &show,
            )?;
        }
        notice = reload_view_or_keep(
            view,
            reloaded,
            Some(text),
            answer_hidden.then_some(hidden_answer),
        )
        .map(Feedback::History);
    }
    if let Some(error) = reload_error {
        notice = Some(Feedback::History(match notice.as_deref() {
            Some(previous) => format!("{previous}; {error}"),
            None => error,
        }));
    }
    Ok(notice)
}

async fn start_session(admission: &Admission) -> Result<(SessionId, SessionView), String> {
    match admission.entry {
        EntryMode::New => {
            let root = StateRoot::admit(&admission.state_dir)
                .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
            let store = Store::open(root).map_err(|_| "state store unavailable".to_owned())?;
            store
                .close()
                .await
                .map_err(|_| "store shutdown failed".to_owned())?;
            let view = new_conversation(admission.defaults.clone());
            Ok((view.id, view))
        }
        EntryMode::Resume(id) => {
            let root = open_source_state(&admission.state_dir, Some(id))?;
            let view = resume_session(root, admission.workspace.clone(), id)
                .await
                .map_err(|error| source_error(Some(id), error))?;
            finish_existing_start(admission, view).await
        }
        EntryMode::Fork(source_id) => {
            let root = open_source_state(&admission.state_dir, Some(source_id))?;
            let id = fork_session(root, admission.workspace.clone(), source_id, None)
                .await
                .map_err(|error| source_error(Some(source_id), error))?;
            if admission.defaults != SessionDefaults::default() {
                persist_defaults(admission, id, admission.defaults.clone()).await?;
            }
            finish_existing_start(admission, load_view(&admission.state_dir, id).await?).await
        }
        EntryMode::Continue => {
            let root = open_source_state(&admission.state_dir, None)?;
            let view = continue_session(root, admission.workspace.clone())
                .await
                .map_err(|error| source_error(None, error))?;
            finish_existing_start(admission, view).await
        }
        EntryMode::PickSession => Err("Session selection was not resolved".into()),
    }
}

// A State without history is a missing source, not an unsafe State; ownership and
// permission failures keep the unsafe message.
fn open_source_state(state_dir: &Path, id: Option<SessionId>) -> Result<StateRoot, String> {
    StateRoot::open_existing(state_dir).map_err(|error| match error {
        StoreError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => {
            source_not_found(id)
        }
        _ => "state directory unavailable or unsafe".to_owned(),
    })
}

fn source_error(id: Option<SessionId>, error: EngineError) -> String {
    match error {
        EngineError::MissingSession | EngineError::NoSessionForWorkspace => source_not_found(id),
        EngineError::ForkNotAtRunBoundary => format!(
            "the source conversation has no completed Run to fork; fork a conversation with a completed Run, or {SOURCE_HINT}"
        ),
        other => other.to_string(),
    }
}

const SOURCE_HINT: &str = "list conversations with arany --resume or /resume, or check --state-dir";

fn source_not_found(id: Option<SessionId>) -> String {
    match id {
        Some(id) => {
            format!("conversation {id} was not found in this State/Workspace; {SOURCE_HINT}")
        }
        None => {
            format!("no conversation was found for this Workspace in this State; {SOURCE_HINT}")
        }
    }
}

fn refresh_runtime_skills<N: From<String> + std::fmt::Display>(
    admission: &Admission,
    composer: &mut Composer,
    notice: &mut Option<N>,
    supported: bool,
) {
    let project = arany::project_skills(&admission.workspace);
    if let Ok(project) = &project
        && project.installation_missing
    {
        append_notice(
            notice,
            "Run npx --yes skills@1.7.0 experimental_install in this project: .agents/skills is missing and skills-lock.json exists.",
        );
    }
    let skills = if !supported {
        Ok(Vec::new())
    } else if admission.tools {
        StateRoot::open_existing(&admission.state_dir)
            .map_err(|_| "Tool configuration unavailable".to_owned())
            .and_then(|root| {
                arany::runtime_skill_names(&root, &admission.workspace)
                    .map_err(|error| error.to_string())
            })
    } else if admission
        .workspace_permissions
        .as_ref()
        .is_some_and(|permissions| permissions.is_trusted())
    {
        project
            .map(|project| project.names)
            .map_err(|error| error.to_string())
    } else {
        Ok(Vec::new())
    };
    match skills {
        Ok(names) => composer.set_runtime_skills(&names),
        Err(error) => {
            composer.set_runtime_skills(&[]);
            let recovery = if admission.tools {
                "review tools.json and .agents/skills"
            } else {
                "review .agents/skills"
            };
            append_notice(
                notice,
                &format!("Error: {error}; runtime Skills unavailable; {recovery}"),
            );
        }
    }
}

/// Runs folder consent, then history recovery, before any account or Provider step.
async fn admit_startup(
    terminal: &mut AttachedTerminal,
    admission: &mut Admission,
    composer: &mut Composer,
    setup_notice: &mut Option<String>,
) -> Result<ControlFlow<()>, String> {
    match permissions::admit(terminal, &admission.workspace, composer, admission.tools).await {
        Ok(permissions::PermissionChoice::Selected(access)) => {
            for notice in access.stale_runtime_notices() {
                append_notice(setup_notice, &notice);
            }
            admission.workspace_permissions = Some(access);
        }
        Ok(permissions::PermissionChoice::Closed(access)) => {
            admission.workspace_permissions = Some(access);
        }
        Ok(permissions::PermissionChoice::Rejected(access, error)) => {
            append_notice(
                setup_notice,
                &format!("Error: {error}; continuing read only"),
            );
            admission.workspace_permissions = Some(access);
        }
        Ok(permissions::PermissionChoice::Exit) => return Ok(ControlFlow::Break(())),
        Err(setup::SetupError::Recoverable(error)) => {
            return Err(format!(
                "{error}; restore private folder trust state and retry"
            ));
        }
        Err(error) => return Err(error.into()),
    }
    match setup::admit_history(terminal, &admission.state_dir, &admission.workspace).await {
        Ok(setup::HistoryAdmission::Ready) => {}
        Ok(setup::HistoryAdmission::Reset) => {
            admission.entry = EntryMode::New;
            if admission.prompt.take().is_some() {
                append_notice(
                    setup_notice,
                    "Initial task was not submitted; enter it again.",
                );
            }
            append_notice(
                setup_notice,
                "Starting a new conversation; saved accounts and folder trust kept.",
            );
        }
        Ok(setup::HistoryAdmission::Exit) => return Ok(ControlFlow::Break(())),
        Err(error) => return Err(error.into()),
    }
    Ok(ControlFlow::Continue(()))
}

/// Runs saved-access authorization; `Break` means the user cancelled, `Continue(false)` a
/// recoverable failure.
async fn authorize_or_notice<N: From<String> + std::fmt::Display>(
    terminal: &mut AttachedTerminal,
    defaults: &SessionDefaults,
    notice: &mut Option<N>,
) -> Result<ControlFlow<(), bool>, String> {
    match setup::authorize_saved_access(terminal, defaults).await {
        Ok(true) => Ok(ControlFlow::Continue(true)),
        Ok(false) => Ok(ControlFlow::Break(())),
        Err(setup::SetupError::Recoverable(error)) => {
            append_notice(
                notice,
                &format!("Error: {error}; use /setup to review saved access"),
            );
            Ok(ControlFlow::Continue(false))
        }
        Err(error) => Err(error.into()),
    }
}

fn require_access(authorized: bool, notice: Option<impl std::fmt::Display>) -> Result<(), String> {
    if authorized {
        return Ok(());
    }
    Err(notice.map_or_else(
        || "Saved access was not authorized".into(),
        |notice| notice.to_string(),
    ))
}

/// Replaces the exec-only `arany --setup` hint with the in-chat `/setup` one.
fn restore_failure_notice(error: impl std::fmt::Display) -> String {
    let error = error.to_string();
    let error = error
        .strip_suffix("; use arany --setup to choose or reset access")
        .unwrap_or(&error);
    format!("Saved access could not be restored: {error}; use /setup to choose or reset access")
}

/// Selects the missing-selection hints, which only a new conversation can act on.
#[derive(Clone, Copy)]
enum PromptEntry {
    New,
    Existing,
}

impl PromptEntry {
    fn missing_provider(self) -> &'static str {
        match self {
            Self::New => "a prompt requires a selected Provider; run arany and use /setup",
            Self::Existing => "a prompt requires a selected Provider",
        }
    }

    fn missing_model(self) -> &'static str {
        match self {
            Self::New => "a prompt requires a selected model; run arany and use /model",
            Self::Existing => "a prompt requires a selected model",
        }
    }
}

async fn preflight_prompt(
    state_dir: &Path,
    workspace: &Path,
    defaults: &SessionDefaults,
    entry: PromptEntry,
) -> Result<(), String> {
    run::preflight_selected(
        state_dir,
        run::Selection {
            workspace,
            profile: defaults
                .provider
                .as_deref()
                .ok_or(entry.missing_provider())?,
            model: defaults.model.as_deref().ok_or(entry.missing_model())?,
            effort: defaults.effort,
            account_id: defaults.account_id,
        },
    )
    .await
}

fn append_notice<N: From<String> + std::fmt::Display>(notice: &mut Option<N>, message: &str) {
    *notice = Some(
        match notice.take() {
            Some(previous) => format!("{message}; {previous}"),
            None => message.to_owned(),
        }
        .into(),
    );
}

fn new_conversation(defaults: SessionDefaults) -> SessionView {
    SessionView {
        id: SessionId::new(),
        title: "New Session".into(),
        title_is_explicit: false,
        inherited_title: None,
        workspace_identity: None,
        defaults,
        created_sequence: 0,
        last_sequence: 0,
        lineage: None,
        runs: Vec::new(),
        compactions: Vec::new(),
        goals: Vec::new(),
    }
}

async fn materialize_session(
    admission: &Admission,
    session_id: &mut SessionId,
    view: &mut SessionView,
) -> Result<(), String> {
    if view.created_sequence != 0 {
        return Ok(());
    }
    let root = StateRoot::admit(&admission.state_dir)
        .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
    let title = view.title_is_explicit.then(|| view.title.clone());
    let id = create_session(root, admission.workspace.clone(), title.clone())
        .await
        .map_err(|error| error.to_string())?;
    let defaults = view.defaults.clone();
    *view = load_view(&admission.state_dir, id).await?;
    if let Some(title) = title.filter(|_| !view.title_is_explicit) {
        arany::rename_session(
            StateRoot::open_existing(&admission.state_dir)
                .map_err(|_| "state directory unavailable or unsafe".to_owned())?,
            admission.workspace.clone(),
            id,
            title,
        )
        .await
        .map_err(|error| error.to_string())?;
        *view = load_view(&admission.state_dir, id).await?;
    }
    *session_id = id;
    update_defaults(admission, id, view, defaults)
        .await
        .map_err(String::from)
}

async fn update_defaults(
    admission: &Admission,
    session_id: SessionId,
    view: &mut SessionView,
    defaults: SessionDefaults,
) -> Result<(), DefaultsError> {
    if view.created_sequence == 0 {
        view.defaults = defaults;
        return Ok(());
    }
    persist_defaults(admission, session_id, defaults.clone()).await?;
    match load_view(&admission.state_dir, session_id).await {
        Ok(reloaded) => *view = reloaded,
        Err(error) => {
            view.defaults = defaults;
            return Err(DefaultsError::Reload(error));
        }
    }
    Ok(())
}

async fn remembered_session_notice(
    admission: &Admission,
    view: &SessionView,
    notice: String,
) -> String {
    match models::remember_models(&admission.workspace, &view.defaults, None, true).await {
        Ok(()) => notice,
        Err(error) => {
            format!("{notice}; Error: {error}; selection kept for this conversation only")
        }
    }
}

async fn prepare_resume(
    admission: &Admission,
    mut view: SessionView,
    fallback: Option<&SessionDefaults>,
) -> Result<SessionView, String> {
    let mut defaults = view.defaults.clone();
    if defaults.provider.is_none() {
        let selection =
            if let Some(config) = view.runs.iter().rev().find_map(|run| run.config.as_ref()) {
                Some(SessionDefaults {
                    provider: Some(config.provider.clone()),
                    model: Some(config.model.clone()),
                    effort: config.effort,
                    account_id: config.saved_api_account_id.or_else(|| {
                        config
                            .chatgpt_provenance
                            .as_ref()
                            .map(|source| source.account_id)
                    }),
                    policy: defaults.policy,
                })
            } else if let Some(fallback) = fallback.filter(|defaults| defaults.provider.is_some()) {
                Some(fallback.clone())
            } else {
                models::last_saved_defaults(&admission.workspace)?
            };
        if let Some(mut selection) = selection {
            selection.policy = defaults.policy;
            defaults = selection;
        }
    }
    if defaults.model.is_none() && defaults.account_id.is_some() {
        models::restore_saved_models(
            &admission.workspace,
            &admission.state_dir,
            &mut defaults,
            true,
        )
        .await?;
    }
    if defaults != view.defaults {
        persist_defaults(admission, view.id, defaults).await?;
        view = load_view(&admission.state_dir, view.id).await?;
    }
    Ok(view)
}

async fn finish_existing_start(
    admission: &Admission,
    mut view: SessionView,
) -> Result<(SessionId, SessionView), String> {
    if admission.defaults.provider.is_none() {
        view = prepare_resume(admission, view, None).await?;
    }
    let id = view.id;
    let mut defaults = view.defaults.clone();
    if let Some(provider) = &admission.defaults.provider {
        if defaults.provider.as_ref() != Some(provider) {
            defaults.model = None;
            defaults.effort = None;
        }
        defaults.account_id = admission.defaults.account_id;
        defaults.provider = Some(provider.clone());
    }
    if let Some(model) = &admission.defaults.model {
        if defaults.model.as_ref() != Some(model) {
            defaults.effort = None;
        }
        defaults.model = Some(model.clone());
    }
    if let Some(effort) = admission.defaults.effort {
        defaults.effort = Some(effort);
    }
    if defaults.model.is_some() && defaults.provider.is_none() {
        return Err("--model requires a selected Provider".into());
    }
    if defaults.effort.is_some() && defaults.model.is_none() {
        return Err("--effort requires a selected model".into());
    }
    if admission.prompt.is_some() {
        defaults
            .provider
            .as_deref()
            .ok_or("a prompt requires a selected Provider")?;
        defaults
            .model
            .as_deref()
            .ok_or("a prompt requires a selected model")?;
    }
    if defaults != view.defaults {
        persist_defaults(admission, id, defaults).await?;
        view = load_view(&admission.state_dir, id).await?;
    }
    Ok((id, view))
}

async fn load_confirmed_outcome(
    state_dir: &Path,
    outcome: &RunOutcome,
) -> Result<SessionView, String> {
    let view = load_view(state_dir, outcome.session_id).await?;
    confirm_outcome(&view, outcome)?;
    Ok(view)
}

/// A mismatch means the committed Run is not in the loaded history; it is an integrity failure, not a reload failure.
fn confirm_outcome(view: &SessionView, outcome: &RunOutcome) -> Result<(), String> {
    if !view.runs.iter().any(|run| {
        run.id == outcome.run.id
            && run == &outcome.run
            && matches!(
                run.status,
                RunStatus::Finished | RunStatus::Failed | RunStatus::Cancelled
            )
    }) {
        return Err("Run history unavailable or invalid".into());
    }
    Ok(())
}

fn emit_outcome(
    view: &SessionView,
    outcome: &RunOutcome,
    user_recorded: bool,
    inline: bool,
    visual_mode: Option<arany::TerminalMode>,
) -> Result<(), String> {
    let run = view
        .runs
        .iter()
        .find(|run| run.id == outcome.run.id && *run == &outcome.run)
        .ok_or_else(|| "Run history unavailable or invalid".to_owned())?;
    let status = match run.status {
        RunStatus::Finished => "finished",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
        _ => return Err("Run did not reach a terminal state".into()),
    };
    if !user_recorded && visual_mode != Some(arany::TerminalMode::Fullscreen) {
        AttachedTerminal::print_user_run(run, inline).map_err(|_| "output failed".to_owned())?;
    }
    if visual_mode != Some(arany::TerminalMode::Fullscreen) || !std::io::stdout().is_terminal() {
        AttachedTerminal::print_assistant_run(run).map_err(|_| "output failed".to_owned())?;
    }
    if visual_mode == Some(arany::TerminalMode::Fullscreen) {
        writeln!(
            std::io::stderr(),
            "Session: {} · Status: {status}",
            outcome.session_id
        )
        .map_err(|_| "output failed".to_owned())?;
        return Ok(());
    }
    write!(
        std::io::stderr(),
        "Session: {}\nRun: {}\nStatus: {status}\n{}",
        outcome.session_id,
        run.id,
        match run.config.as_ref() {
            Some(config) if config.chatgpt_provenance.is_some() => {
                "Provider: ChatGPT plan\nOutput bound: local only; remote usage is not capped\n"
            }
            Some(config) if config.custom_profile_provenance.is_some() => {
                "Provider: custom verified\n"
            }
            _ => "",
        }
    )
    .map_err(|_| "output failed".to_owned())?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum DefaultsError {
    #[error("{}", STATE_UNAVAILABLE)]
    StateAdmission,
    #[error("{0}")]
    Operation(arany::EngineError),
    #[error("{0}")]
    Reload(String),
}

impl From<DefaultsError> for String {
    fn from(error: DefaultsError) -> Self {
        error.to_string()
    }
}

fn settings_reload_notice(error: &str) -> String {
    format!("Error: Settings saved, but the conversation could not be reloaded: {error}")
}

fn recover_defaults_change(error: DefaultsError) -> Result<String, LoopError> {
    match error {
        DefaultsError::Operation(arany::EngineError::SessionBusy) => Ok(
            "Error: Session has an active operation; selection unchanged. Try again when it finishes"
                .into(),
        ),
        DefaultsError::StateAdmission => Err(LoopError::Notice(format!("Error: {STATE_UNAVAILABLE}"))),
        DefaultsError::Reload(error) => Err(LoopError::Notice(settings_reload_notice(&error))),
        error => Err(LoopError::Fatal(error.into())),
    }
}

async fn persist_defaults(
    admission: &Admission,
    session_id: SessionId,
    defaults: SessionDefaults,
) -> Result<(), DefaultsError> {
    let root = StateRoot::admit(&admission.state_dir).map_err(|_| DefaultsError::StateAdmission)?;
    set_session_defaults(root, admission.workspace.clone(), session_id, defaults)
        .await
        .map_err(DefaultsError::Operation)
}

async fn load_view(state_dir: &Path, session_id: SessionId) -> Result<SessionView, String> {
    let root = StateRoot::open_existing(state_dir)
        .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
    let store = Store::open_read_only(root).map_err(|_| "state store unavailable".to_owned())?;
    let view = store
        .load_view(session_id)
        .await
        .map_err(|_| "Session history unavailable or invalid".to_owned())?
        .ok_or_else(|| "Session not found".to_owned());
    let closed = store.close().await;
    let view = view?;
    closed.map_err(|_| "store shutdown failed".to_owned())?;
    Ok(view)
}

/// Events are already committed when this runs, so a failed reload keeps the displayed history
/// and returns the notice with the reload failure appended. `hidden` tells the user how to read a
/// committed answer this presentation did not show.
fn reload_view_or_keep(
    view: &mut SessionView,
    reloaded: Result<SessionView, String>,
    notice: Option<String>,
    hidden: Option<&str>,
) -> Option<String> {
    match reloaded {
        Ok(reloaded) => {
            *view = reloaded;
            notice
        }
        Err(error) => {
            let failure = reload_failure(&error, hidden);
            Some(match notice {
                Some(notice) => format!("{notice}; {failure}"),
                None => failure,
            })
        }
    }
}

fn reload_failure(error: &str, hidden: Option<&str>) -> String {
    format!(
        "Error: history could not be reloaded: {error}; {}",
        hidden.unwrap_or("use /resume to reload it")
    )
}

/// Linear output has no history view, so `/resume` cannot show the answer there. Its notice points to
/// the recovery line that `print_run_answer` writes untruncated; other presentations use `/resume`.
fn hidden_answer_guidance(linear: bool) -> &'static str {
    if linear {
        "the committed answer was not shown; see the command above"
    } else {
        "the committed answer was not shown; use /resume to reload it"
    }
}

/// Builds the `show` command that replays a Run. Only an explicit `--state-dir` must be repeated;
/// `show` also reads `ARANY_STATE_DIR` and the default store.
fn show_command(run: arany::RunId, explicit_state_dir: Option<&Path>) -> String {
    match explicit_state_dir {
        Some(dir) => format!(
            "arany show --state-dir {} --output text {run}",
            shell_quote(&dir.to_string_lossy())
        ),
        None => format!("arany show --output text {run}"),
    }
}

fn write_hidden_answer_recovery(writer: &mut impl Write, show: &str) -> std::io::Result<()> {
    writeln!(
        writer,
        "Committed answer not shown; read it with: {}",
        arany::escape_terminal(show)
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Prints a confirmed Run answer for this presentation. Linear output runs on the restored terminal;
/// a missing `confirmed` view (failed reload) writes the untruncated `show` recovery line instead.
fn print_run_answer(
    terminal: &mut AttachedTerminal,
    confirmed: Option<&SessionView>,
    outcome: &RunOutcome,
    preview_already_printed: bool,
    show: &str,
) -> Result<(), String> {
    if terminal.is_linear() {
        terminal.restore().map_err(|error| error.to_string())?;
        let emitted = match confirmed {
            Some(view) => emit_outcome(view, outcome, preview_already_printed, false, None),
            None => write_hidden_answer_recovery(&mut std::io::stderr(), show)
                .map_err(|_| "output failed".to_owned()),
        };
        terminal
            .reacquire_after_output()
            .map_err(|error| error.to_string())?;
        return emitted;
    }
    if confirmed.is_some() && !std::io::stdout().is_terminal() {
        AttachedTerminal::print_assistant_run(&outcome.run)
            .map_err(|_| "output failed".to_owned())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_hidden_answer_guidance_repeats_only_an_explicit_state_dir() {
        let run = arany::RunId::new();
        let mut plain = Vec::new();
        write_hidden_answer_recovery(&mut plain, &show_command(run, None)).unwrap();
        assert_eq!(
            plain,
            format!("Committed answer not shown; read it with: arany show --output text {run}\n")
                .into_bytes()
        );
        let mut explicit = Vec::new();
        write_hidden_answer_recovery(
            &mut explicit,
            &show_command(run, Some(Path::new("/tmp/a b/it's"))),
        )
        .unwrap();
        assert_eq!(
            explicit,
            format!(
                "Committed answer not shown; read it with: arany show --state-dir '/tmp/a b/it'\\''s' --output text {run}\n"
            )
            .into_bytes()
        );
        let long_dir = format!("/tmp/{}", "d".repeat(300));
        let long = show_command(run, Some(Path::new(&long_dir)));
        assert!(long.starts_with(&format!("arany show --state-dir '{long_dir}' ")));
        assert!(long.ends_with(&format!(" --output text {run}")));
    }

    #[test]
    fn linear_hidden_answer_notice_is_short_and_fixed() {
        assert_eq!(
            hidden_answer_guidance(true),
            "the committed answer was not shown; see the command above"
        );
        assert_eq!(
            hidden_answer_guidance(false),
            "the committed answer was not shown; use /resume to reload it"
        );
    }

    #[test]
    fn startup_rejects_invalid_model_before_workspace_or_state_admission() {
        let args = AttachedArgs {
            model: Some("bad\nmodel".into()),
            ..AttachedArgs::default()
        };
        assert!(
            matches!(args.admit(Telemetry::disabled()), Err(error) if error == "invalid model")
        );
    }

    #[tokio::test]
    async fn startup_resumes_exact_id_with_defaults_and_rejects_fork_without_a_run() {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state_dir = temp.path().join("state");
        let defaults = SessionDefaults {
            provider: Some("openai".into()),
            model: Some("gpt-5.4".into()),
            effort: None,
            account_id: None,
            policy: CollaborationPolicy::Single,
        };
        let new = Admission {
            telemetry: Telemetry::disabled(),
            state_dir: state_dir.clone(),
            explicit_state_dir: false,
            workspace: workspace.clone(),
            include_paths: Vec::new(),
            tools: false,
            workspace_permissions: None,
            defaults: defaults.clone(),
            screen_reader: false,
            classic: false,
            no_color: false,
            setup_requested: false,
            entry: EntryMode::New,
            prompt: None,
            resume_goal: None,
        };
        let (mut original_id, mut original) = start_session(&new).await.expect("new Session");
        assert_eq!(original.defaults, defaults);
        assert!(
            arany::list_sessions(
                StateRoot::open_existing(&state_dir).unwrap(),
                workspace.clone()
            )
            .await
            .expect("empty history")
            .is_empty(),
            "opening the composer must not save an empty conversation"
        );
        materialize_session(&new, &mut original_id, &mut original)
            .await
            .expect("first submission saves the conversation");

        let resumed = Admission {
            telemetry: Telemetry::disabled(),
            state_dir: state_dir.clone(),
            explicit_state_dir: false,
            workspace: workspace.clone(),
            include_paths: Vec::new(),
            tools: false,
            workspace_permissions: None,
            defaults: SessionDefaults {
                provider: None,
                model: Some("gpt-5.4-mini".into()),
                effort: None,
                account_id: None,
                policy: CollaborationPolicy::Single,
            },
            screen_reader: false,
            classic: false,
            no_color: false,
            setup_requested: false,
            entry: EntryMode::Resume(original_id),
            prompt: None,
            resume_goal: None,
        };
        let (same_id, resumed_view) = start_session(&resumed).await.expect("resumed Session");
        assert_eq!(same_id, original_id);
        assert_eq!(resumed_view.defaults.provider.as_deref(), Some("openai"));
        assert_eq!(resumed_view.defaults.model.as_deref(), Some("gpt-5.4-mini"));

        let continued = Admission {
            telemetry: Telemetry::disabled(),
            state_dir: state_dir.clone(),
            explicit_state_dir: false,
            workspace: workspace.clone(),
            include_paths: Vec::new(),
            tools: false,
            workspace_permissions: None,
            defaults: SessionDefaults::default(),
            screen_reader: false,
            classic: false,
            no_color: false,
            setup_requested: false,
            entry: EntryMode::Continue,
            prompt: None,
            resume_goal: None,
        };
        let (latest_id, latest_view) = start_session(&continued).await.expect("continued Session");
        assert_eq!(latest_id, original_id);
        assert_eq!(latest_view.defaults.model.as_deref(), Some("gpt-5.4-mini"));

        let empty_id = create_session(
            StateRoot::admit(&state_dir).unwrap(),
            workspace.clone(),
            None,
        )
        .await
        .unwrap();
        let empty = load_view(&state_dir, empty_id).await.unwrap();
        let inherited = prepare_resume(&continued, empty, Some(&resumed_view.defaults))
            .await
            .unwrap();
        let mut expected = resumed_view.defaults.clone();
        expected.policy = SessionDefaults::default().policy;
        assert_eq!(inherited.defaults, expected);
        assert_eq!(
            load_view(&state_dir, empty_id).await.unwrap().defaults,
            expected
        );
        let preserved = prepare_resume(&continued, original, Some(&resumed_view.defaults))
            .await
            .unwrap();
        assert_eq!(preserved.defaults, defaults);

        let forked = Admission {
            telemetry: Telemetry::disabled(),
            state_dir,
            explicit_state_dir: false,
            workspace,
            include_paths: Vec::new(),
            tools: false,
            workspace_permissions: None,
            defaults: SessionDefaults::default(),
            screen_reader: false,
            classic: false,
            no_color: false,
            setup_requested: false,
            entry: EntryMode::Fork(original_id),
            prompt: None,
            resume_goal: None,
        };
        let error = match start_session(&forked).await {
            Ok(_) => panic!("empty Session cannot be forked"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            "the source conversation has no completed Run to fork; fork a conversation with a completed Run, or list conversations with arany --resume or /resume, or check --state-dir"
        );
        let reopened = load_view(&forked.state_dir, original_id)
            .await
            .expect("original Session intact");
        assert_eq!(reopened.id, original_id);
    }
}

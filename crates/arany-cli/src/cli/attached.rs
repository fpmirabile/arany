use super::provider::select::ProviderArg;
use crate::cli::provider::select::Selection;
use crate::terminal::{AttachedTerminal, Composer, ComposerDraft, InteractiveCommand, Submission};
use arany::{
    CollaborationPolicy, Effort, Engine, EngineError, RunOutcome, RunStatus, SessionDefaults,
    SessionId, SessionView, StateRoot, Store, StoreError, Telemetry, continue_session,
    create_session, fork_session, resume_session, set_session_defaults,
};
use clap::Args;
use std::ops::ControlFlow;
use std::{
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    str::FromStr,
};

mod active;
mod agents;
pub(crate) mod controls;
pub(crate) mod models;
mod permissions;
mod picker;
mod rewind;
pub(crate) mod run;
mod session;
mod setup;
mod startup;
use models::ModelBrowse;
use picker::{PickerChoice, pick_session};

const MAX_ATTACHED_DRAFT_BYTES: usize = 8 * 1024;
const EMPTY_INTERRUPT_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
const STATE_UNAVAILABLE: &str = "state directory unavailable or unsafe";

pub(super) async fn execution_limits(
    state_dir: &Path,
) -> Result<(arany::GoalBudget, arany::ToolResourceSettings), String> {
    let settings = controls::restore_settings_at(state_dir).await?;
    Ok((settings.goal_budget, settings.resources))
}

#[derive(Args, Default)]
pub(crate) struct AttachedArgs {
    #[arg(long)]
    state_dir: Option<PathBuf>,
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long, help = "openai, anthropic, chatgpt, or custom:NAME")]
    provider: Option<ProviderArg>,
    #[arg(long)]
    profile: Option<String>,
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
    fn notice(&self) -> crate::terminal::TerminalNotice<'_> {
        match self {
            Self::History(text) => crate::terminal::TerminalNotice::History(text),
            Self::Transient(text) => crate::terminal::TerminalNotice::Transient(text),
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
    Usage,
    Models,
    Agents,
    Resume,
    Setup,
}

struct Admission {
    telemetry: Telemetry,
    state_dir: PathBuf,
    explicit_state_dir: bool,
    roster: Option<String>,
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
            && self.profile.is_none()
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
        if self.prompt.as_ref().is_some_and(|prompt| {
            prompt.trim().is_empty() || prompt.len() > MAX_ATTACHED_DRAFT_BYTES
        }) {
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
            roster: self.profile,
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
    match startup::start(args, telemetry).await? {
        ControlFlow::Continue(session) => session.run().await,
        ControlFlow::Break(()) => Ok(()),
    }
}

// A failed exit drops the Engine instead, which still stops its Store thread.
async fn close_engine(engine: Engine) -> Result<(), String> {
    engine
        .close()
        .await
        .map_err(|_| "store shutdown failed".to_owned())
}

async fn run_selector(
    engine: &Engine,
    selector: Selector,
    terminal: &mut AttachedTerminal,
    admission: &mut Admission,
    session_id: &mut SessionId,
    view: &mut SessionView,
    composer: &mut Composer,
) -> Result<Option<Feedback>, LoopError> {
    Ok(Some(match selector {
        Selector::Usage => {
            terminal
                .open_usage(view, false, None)
                .await
                .map_err(|error| error.to_string())?;
            return Ok(None);
        }
        Selector::Setup => {
            configure_account(terminal, admission, *session_id, view, composer).await?
        }
        Selector::Models => {
            match models::browse(terminal, admission, *session_id, view, composer).await? {
                ModelBrowse::Notice(notice) => notice,
                ModelBrowse::Shutdown(signal) => {
                    return Err(format!("terminated by {}", signal.name()).into());
                }
            }
        }
        Selector::Agents => match agents::inspect(terminal, view).await? {
            Err(signal) => return Err(format!("terminated by {}", signal.name()).into()),
            Ok(Some(text)) => Feedback::Transient(text.into()),
            // The opened agent chat owns the bottom panel.
            Ok(None) if terminal.agent_chat_open() => return Ok(None),
            Ok(None) => Feedback::Transient("Agent inspection closed".into()),
        },
        Selector::Resume => resume_selector(engine, terminal, admission, session_id, view).await?,
    }))
}

async fn resume_selector(
    engine: &Engine,
    terminal: &mut AttachedTerminal,
    admission: &mut Admission,
    session_id: &mut SessionId,
    view: &mut SessionView,
) -> Result<Feedback, LoopError> {
    engine
        .revoke_host_approvals()
        .map_err(|error| error.to_string())?;
    let choice = pick_session(terminal, admission, Some(view)).await?;
    match choice {
        PickerChoice::Selected(id) => {
            let root = open_state(admission)?;
            match resume_session(root, admission.workspace.clone(), id).await {
                Ok(selected) => {
                    let selected =
                        match prepare_resume(admission, selected, Some(&view.defaults)).await {
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
                                crate::presentation::escape_terminal(&view.conversation_title())
                            ),
                        )
                        .await,
                    ))
                }
                Err(error) => Ok(Feedback::History(format!("Error: {error}"))),
            }
        }
        PickerChoice::Unavailable(error) => Ok(Feedback::History(format!("Error: {error}"))),
        PickerChoice::Empty => Ok(Feedback::History("No Sessions for this Workspace".into())),
        PickerChoice::Closed => Ok(Feedback::Transient("Session selection closed".into())),
        PickerChoice::Shutdown(signal) => Err(format!("terminated by {}", signal.name()).into()),
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

fn consume_local_command(composer: &mut Composer, submission: &Submission<'_>) {
    if let Submission::Command { command, argument } = submission
        && !(*command == InteractiveCommand::Goal
            && argument.is_some_and(|argument| argument != "pause"))
    {
        composer.take();
    }
}

async fn reconcile_failed_submission(
    terminal: &mut AttachedTerminal,
    state_dir: &Path,
    view: &mut SessionView,
    composer: &mut Composer,
    submitted: ComposerDraft,
    starting_sequence: u64,
) -> Result<(), String> {
    loop {
        match load_view(state_dir, view.id).await {
            Ok(reloaded) => {
                if !reloaded
                    .runs
                    .iter()
                    .any(|run| run.accepted_sequence > starting_sequence)
                {
                    composer.restore_draft(submitted);
                }
                *view = reloaded;
                return Ok(());
            }
            Err(error) => {
                let notice = format!(
                    "Acceptance is unknown; draft held and cannot be resubmitted. History could not be reloaded: {error}"
                );
                terminal
                    .draw_progress(view, &notice)
                    .map_err(|error| error.to_string())?;
                if setup::choose_focused(
                    terminal,
                    1,
                    "Submission recovery",
                    "Draft held until history confirms acceptance. Check history never submits.",
                    &[
                        (b'1', "Check history"),
                        (b'0', "Exit (discard local draft)"),
                    ],
                    None,
                )
                .await?
                    != Some(b'1')
                {
                    return Err(notice);
                }
            }
        }
    }
}

const INTERRUPTED_GOAL_NOTICE: &str = "Goal paused; use /goal resume to continue.";
const OUT_OF_BUDGET_GOAL_NOTICE: &str =
    "Goal paused and out of budget; /goal resume will close it.";

fn paused_goal_notice(resume_admitted: bool) -> &'static str {
    if resume_admitted {
        INTERRUPTED_GOAL_NOTICE
    } else {
        OUT_OF_BUDGET_GOAL_NOTICE
    }
}

// Cancelled Run feedback repeats the cancellation that the paused notice covers; fallbacks stay.
fn submission_notice(
    paused_goal: Option<&'static str>,
    run_status: RunStatus,
    run_feedback: Option<String>,
    fallbacks: Option<String>,
) -> Option<String> {
    let feedback = match paused_goal {
        Some(notice) if run_status == RunStatus::Cancelled => Some(notice.to_owned()),
        Some(notice) => Some(match run_feedback {
            Some(feedback) if feedback.ends_with(['.', '!', '?']) => {
                format!("{feedback} {notice}")
            }
            Some(feedback) => format!("{feedback}. {notice}"),
            None => notice.to_owned(),
        }),
        None => run_feedback,
    };
    match (fallbacks, feedback) {
        (Some(fallbacks), Some(feedback)) => Some(format!("{fallbacks}\n{feedback}")),
        (fallbacks, feedback) => fallbacks.or(feedback),
    }
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

async fn admit_startup(
    terminal: &mut AttachedTerminal,
    admission: &mut Admission,
    composer: &mut Composer,
    setup_notice: &mut Option<String>,
    selected: Option<super::provider::roster::SelectedRoster>,
) -> Result<ControlFlow<(), Option<arany::ArchivedHistory>>, String> {
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
        Err(setup::SetupError::Recoverable(error)) => return Err(error),
        Err(error) => return Err(error.into()),
    }
    if let Some(selected) = selected {
        admission.defaults.account_id = selected.primary().account_id;
        let mut signals =
            super::exec::StopSignals::install().map_err(|_| "signal handling unavailable")?;
        tokio::select! {
            biased;
            _ = signals.next() => return Err("interrupted before a Run started; no Run was recorded".into()),
            result = selected.admit(&admission.state_dir, &admission.workspace) => { result?; }
        }
    }
    let archived =
        match setup::admit_history(terminal, &admission.state_dir, &admission.workspace).await {
            Ok(setup::HistoryAdmission::Ready) => return Ok(ControlFlow::Continue(None)),
            Ok(setup::HistoryAdmission::Reset) => None,
            Ok(setup::HistoryAdmission::Archived(path)) => Some(path),
            Ok(setup::HistoryAdmission::Exit) => return Ok(ControlFlow::Break(())),
            Err(error) => return Err(error.into()),
        };
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
    Ok(ControlFlow::Continue(archived))
}

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

fn restore_failure_notice(error: impl std::fmt::Display) -> String {
    let error = error.to_string();
    let error = error
        .strip_suffix("; use arany --setup to choose or reset access")
        .unwrap_or(&error);
    format!("Saved access could not be restored: {error}; use /setup to choose or reset access")
}

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
        Selection {
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

// Appended after every setup branch so none can replace the archive path before it is drawn.
fn final_startup_notice(mut notice: Option<String>, archive: Option<&str>) -> Option<Feedback> {
    if let Some(archive) = archive {
        append_notice(&mut notice, archive);
    }
    notice.map(Feedback::History)
}

fn archive_notice_drawn(notice: Option<&Feedback>, archive: Option<&str>) -> bool {
    archive.is_some_and(
        |archive| matches!(notice, Some(Feedback::History(text)) if text.contains(archive)),
    )
}

fn readmit_host_permissions(engine: &Engine, notice: &mut Option<Feedback>) {
    if let Err(error) = engine.readmit_host_permissions() {
        append_notice(
            notice,
            &format!("Error: {error}; host approvals remain denied until /permissions succeeds"),
        );
    }
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
        usage: Default::default(),
        rewinds: Default::default(),
        id: SessionId::new(),
        title: "New Session".into(),
        title_is_explicit: false,
        inherited_title: None,
        workspace_identity: None,
        defaults,
        host_epoch_floor: 0,
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

fn confirm_outcome(view: &SessionView, outcome: &RunOutcome) -> Result<(), String> {
    if !view
        .runs
        .iter()
        .any(|run| run.id == outcome.run.id && run == &outcome.run && run.status.is_outcome())
    {
        return Err("Run history unavailable or invalid".into());
    }
    Ok(())
}

fn emit_outcome(
    view: &SessionView,
    outcome: &RunOutcome,
    user_recorded: bool,
    inline: bool,
    visual_mode: Option<crate::terminal::TerminalMode>,
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
        RunStatus::StoppedByHook => "stopped_by_hook",
        _ => return Err("Run did not reach a terminal state".into()),
    };
    if !user_recorded && visual_mode != Some(crate::terminal::TerminalMode::Fullscreen) {
        AttachedTerminal::print_user_run(run, inline).map_err(|_| "output failed".to_owned())?;
    }
    if visual_mode != Some(crate::terminal::TerminalMode::Fullscreen)
        || !std::io::stdout().is_terminal()
    {
        AttachedTerminal::print_assistant_run(run).map_err(|_| "output failed".to_owned())?;
    }
    if visual_mode == Some(crate::terminal::TerminalMode::Fullscreen) {
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

fn hidden_answer_guidance(linear: bool) -> &'static str {
    if linear {
        "the committed answer was not shown; see the command above"
    } else {
        "the committed answer was not shown; use /resume to reload it"
    }
}

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
        crate::presentation::escape_terminal(show)
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn print_run_answer(
    terminal: &mut AttachedTerminal,
    confirmed: Option<&SessionView>,
    outcome: &RunOutcome,
    preview_already_printed: bool,
    show: &str,
) -> Result<(), String> {
    if terminal.is_linear() {
        if confirmed.is_some() {
            terminal
                .record_tool_rows(&outcome.run)
                .map_err(|error| error.to_string())?;
        }
        terminal
            .release_for_output()
            .map_err(|error| error.to_string())?;
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
    use crate::terminal::TerminalInput;

    #[test]
    fn goal_submission_keeps_editable_mentions_images_and_waiting_drafts() {
        use super::*;
        for image in [false, true] {
            let command = format!("/goal {} @src/lib.rs", "fix ".repeat(300));
            let mut composer = Composer::default();
            composer.insert_literal(&command).unwrap();
            if image {
                composer
                    .attach_image(arany::test_support::test_image())
                    .unwrap();
            }
            let mut held = Composer::default();
            held.insert_paste(&"s".repeat(64 * 1024)).unwrap();
            assert!(!composer.restore_draft(held.take_draft()));
            let submission = composer.parse_submission(&command).unwrap();
            consume_local_command(&mut composer, &submission);
            let submitted = composer.take_draft();
            assert_eq!(submitted.editable_text(), command);
            assert_eq!(submitted.images().len(), usize::from(image));
            assert_eq!(
                arany::mention_paths(&submitted.editable_text()).unwrap(),
                vec![PathBuf::from("src/lib.rs")]
            );
            assert!(composer.restore_draft(submitted));
            assert_eq!(composer.text(), command);
            assert!(!composer.has_text_pastes());
            composer.clear();
            composer.apply(TerminalInput::Up);
            assert_eq!(composer.text(), "s".repeat(64 * 1024));
        }
        let mut composer = Composer::default();
        composer.insert_literal("/help").unwrap();
        consume_local_command(
            &mut composer,
            &crate::terminal::parse_submission("/help").unwrap(),
        );
        assert!(composer.is_empty());
    }

    #[test]
    fn archive_notice_survives_every_setup_outcome_and_gates_acknowledgement() {
        let archive = "History from an incompatible version archived intact to /state/archive/history-20261021T000000.000Z";
        for setup in [
            None,
            Some("Setup could not be completed: synthetic; use /setup to try again"),
            Some("openai ready: gpt-5.4 · low. Use /model to change."),
            Some("Error: synthetic; selection kept for this conversation only"),
        ] {
            let notice = super::final_startup_notice(setup.map(str::to_owned), Some(archive));
            let Some(super::Feedback::History(text)) = &notice else {
                panic!("archive notice missing after {setup:?}");
            };
            assert!(text.starts_with(archive), "{setup:?}: {text}");
            assert!(setup.is_none_or(|setup| text.contains(setup)), "{text}");
            assert!(super::archive_notice_drawn(notice.as_ref(), Some(archive)));
        }
        for drawn in [
            None,
            Some(super::Feedback::History(
                "New conversation · /help for commands".into(),
            )),
            Some(super::Feedback::Transient(archive.into())),
        ] {
            assert!(!super::archive_notice_drawn(drawn.as_ref(), Some(archive)));
        }
        assert!(!super::archive_notice_drawn(
            Some(&super::Feedback::History(archive.into())),
            None
        ));
        assert!(super::final_startup_notice(None, None).is_none());
    }

    use super::*;

    #[test]
    fn hook_stop_feedback_joins_the_pause_notice_as_a_sentence() {
        assert_eq!(
            submission_notice(
                Some(INTERRUPTED_GOAL_NOTICE),
                RunStatus::StoppedByHook,
                Some("Run stopped by hook: halt".into()),
                None
            ),
            Some(format!(
                "Run stopped by hook: halt. {INTERRUPTED_GOAL_NOTICE}"
            ))
        );
    }

    #[test]
    fn interrupted_goal_notice_replaces_only_cancelled_run_feedback() {
        let cancelled =
            Some("Run cancelled. No answer was committed. You can submit a new task.".to_owned());
        assert_eq!(
            submission_notice(
                Some(INTERRUPTED_GOAL_NOTICE),
                RunStatus::Cancelled,
                cancelled.clone(),
                None
            ),
            Some(INTERRUPTED_GOAL_NOTICE.to_owned())
        );
        assert_eq!(
            submission_notice(None, RunStatus::Cancelled, cancelled.clone(), None),
            cancelled
        );
        assert_eq!(
            submission_notice(None, RunStatus::Finished, None, None),
            None
        );
    }

    #[test]
    fn interrupted_goal_notice_keeps_feedback_from_a_finished_run() {
        let cache = Some("Cache updated for the next Run.".to_owned());
        assert_eq!(
            submission_notice(
                Some(INTERRUPTED_GOAL_NOTICE),
                RunStatus::Finished,
                cache,
                None
            ),
            Some(format!(
                "Cache updated for the next Run. {INTERRUPTED_GOAL_NOTICE}"
            ))
        );
        assert_eq!(
            submission_notice(
                Some(INTERRUPTED_GOAL_NOTICE),
                RunStatus::Finished,
                None,
                None
            ),
            Some(INTERRUPTED_GOAL_NOTICE.to_owned())
        );
    }

    #[test]
    fn paused_goal_notice_offers_resume_only_while_a_run_is_admitted() {
        assert_eq!(paused_goal_notice(true), INTERRUPTED_GOAL_NOTICE);
        assert_eq!(paused_goal_notice(false), OUT_OF_BUDGET_GOAL_NOTICE);
        assert!(!OUT_OF_BUDGET_GOAL_NOTICE.contains("use /goal resume to continue"));
        assert_eq!(
            submission_notice(
                Some(paused_goal_notice(false)),
                RunStatus::Cancelled,
                Some("Run cancelled.".to_owned()),
                None
            ),
            Some(OUT_OF_BUDGET_GOAL_NOTICE.to_owned())
        );
    }

    #[test]
    fn paused_goal_notice_keeps_provider_fallbacks_of_a_cancelled_run() {
        let fallback = "Provider fallback (primary): the failed call may have consumed usage.";
        assert_eq!(
            submission_notice(
                Some(INTERRUPTED_GOAL_NOTICE),
                RunStatus::Cancelled,
                Some("Run cancelled.".to_owned()),
                Some(fallback.to_owned())
            ),
            Some(format!("{fallback}\n{INTERRUPTED_GOAL_NOTICE}"))
        );
    }

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
            roster: None,
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
            roster: None,
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
            roster: None,
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
            roster: None,
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

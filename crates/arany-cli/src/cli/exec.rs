use super::attached::models::refresh_at_startup;
use super::provider::select::{ProviderArg, validate_headless_selection};
pub(in crate::cli) use super::run_outcome::SHUTDOWN_GRACE;
use super::run_outcome::{StopSignals, StopStep, stop_after_answer, stop_step, stop_text};
use super::serve::embedded::{self, Connection, Controls, RunAttemptError, RunFailure};
use super::{CommandOutput, OutputArg};
use crate::presentation::render_exec;
use arany::{
    AdmittedProviders, CollaborationPolicy, Effort, Engine, EngineConfig, EngineError, GoalBudget,
    GoalId, GoalPhase, GoalRequest, GoalStatus, MAX_USER_MESSAGE_BYTES, Provider, RunRequest,
    RunResult, RunStatus, SessionId, StateRoot, Store, Telemetry, ToolGrant,
};
use clap::{Args, ValueEnum};
use std::{path::PathBuf, str::FromStr, sync::Arc};

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum CollaborationArg {
    Single,
    Auto,
    Team,
}

impl CollaborationArg {
    pub(crate) fn policy(
        self,
        max_active_children: Option<u8>,
    ) -> Result<CollaborationPolicy, &'static str> {
        match (self, max_active_children) {
            (Self::Single, None) => Ok(CollaborationPolicy::Single),
            (Self::Auto, count) if count.unwrap_or(3) <= 8 => Ok(CollaborationPolicy::Auto {
                max_active_children: count.unwrap_or(3),
            }),
            (Self::Team, count) if (1..=8).contains(&count.unwrap_or(3)) => {
                Ok(CollaborationPolicy::Team {
                    max_active_children: count.unwrap_or(3),
                })
            }
            _ => Err("invalid collaboration policy"),
        }
    }
}

#[derive(Args)]
pub(crate) struct ExecArgs {
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    #[arg(
        long,
        help = "openai, anthropic, chatgpt, or custom:NAME; defaults to the saved selection"
    )]
    pub provider: Option<ProviderArg>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub profile: Option<String>,
    #[arg(long, help = "Model-specific reasoning effort")]
    pub effort: Option<Effort>,
    #[arg(long)]
    pub session_id: Option<String>,
    #[arg(
        long,
        requires = "session_id",
        help = "Explicitly resume an interrupted Goal in --session-id"
    )]
    pub resume_goal: Option<String>,
    #[arg(
        long,
        requires = "session_id",
        conflicts_with_all = ["prompt", "goal", "resume_goal"],
        help = "Start a new Goal with a fresh budget from the exhausted Goal in --session-id"
    )]
    pub continue_goal: Option<String>,
    #[arg(
        long,
        conflicts_with_all = ["prompt", "resume_goal"],
        help = "Run an autonomous Goal on this objective until it is satisfied or stops"
    )]
    pub goal: Option<String>,
    #[arg(long, value_enum, default_value_t = OutputArg::Text)]
    pub output: OutputArg,
    #[arg(long, value_enum, default_value_t = CollaborationArg::Single)]
    pub collaboration: CollaborationArg,
    #[arg(long)]
    pub max_active_children: Option<u8>,
    #[arg(long = "include")]
    pub include_paths: Vec<PathBuf>,
    #[arg(long, help = "Enable guarded tools from private tools.json")]
    pub tools: bool,
    /// Objective of a new Run; omitted only with --resume-goal (continues the pinned objective) or --continue-goal.
    pub prompt: Option<String>,
}

// A continuation keeps the defaults the spec states; saved limits apply to other exec Goals.
fn apply_saved_limits(
    goal: &mut GoalRequest,
    budget: GoalBudget,
    resource_settings: arany::ToolResourceSettings,
    context_window: arany::ContextWindow,
) {
    goal.context_window = context_window;
    if goal.continues.is_some() {
        return;
    }
    if goal.id.is_none() {
        goal.budget = Some(budget);
    }
    goal.resource_settings = resource_settings;
}

struct Admission {
    state_dir: PathBuf,
    workspace: PathBuf,
    session_id: Option<SessionId>,
    objective: String,
    goal: GoalRequest,
    policy: CollaborationPolicy,
}

impl ExecArgs {
    fn admit(&self) -> Result<Admission, &'static str> {
        if let Some(provider) = &self.provider {
            validate_headless_selection(
                provider,
                self.model.as_deref().unwrap_or_default(),
                self.effort,
            )?;
        }
        let policy = self.collaboration.policy(self.max_active_children)?;
        let session_id = self
            .session_id
            .as_deref()
            .map(SessionId::from_str)
            .transpose()?;
        let resume_goal = self
            .resume_goal
            .as_deref()
            .map(arany::GoalId::from_str)
            .transpose()
            .map_err(|_| "invalid Goal ID")?;
        let continue_goal = self
            .continue_goal
            .as_deref()
            .map(arany::GoalId::from_str)
            .transpose()
            .map_err(|_| "invalid Goal ID")?;
        let resumes_without_text = self.goal.is_none()
            && self.prompt.is_none()
            && (resume_goal.is_some() || continue_goal.is_some());
        let (objective, goal) = match (&self.goal, &self.prompt, resume_goal, continue_goal) {
            (Some(objective), None, None, None) => (
                objective.clone(),
                GoalRequest::autonomous(GoalBudget::default(), GoalPhase::Implementation),
            ),
            (None, Some(prompt), None, None) => (prompt.clone(), GoalRequest::default()),
            (None, Some(prompt), Some(id), None) => (prompt.clone(), GoalRequest::resume(id, true)),
            (None, None, Some(id), None) => (String::new(), GoalRequest::resume(id, true)),
            (None, None, None, Some(id)) => (
                String::new(),
                GoalRequest::continue_from(
                    id,
                    GoalBudget::default(),
                    arany::ToolResourceSettings::default(),
                ),
            ),
            _ => {
                return Err(
                    "pass --goal alone, a prompt alone, --resume-goal with an optional prompt, or --continue-goal alone",
                );
            }
        };
        if !resumes_without_text
            && (objective.trim().is_empty() || objective.len() > MAX_USER_MESSAGE_BYTES)
        {
            return Err("invalid objective");
        }
        let (workspace, state_dir) =
            super::admit_workspace(self.workspace.clone(), self.state_dir.clone())?;
        Ok(Admission {
            state_dir,
            workspace,
            session_id,
            objective,
            goal,
            policy,
        })
    }
}

pub(crate) async fn run(mut args: ExecArgs, telemetry: Telemetry) -> Result<CommandOutput, String> {
    let (_, state_dir) = super::admit_workspace(args.workspace.clone(), args.state_dir.clone())
        .map_err(str::to_owned)?;
    super::admit_headless_records(&state_dir)?;
    if let Some(name) = &args.profile {
        let (workspace, state_dir) =
            super::admit_workspace(args.workspace.clone(), args.state_dir.clone())
                .map_err(str::to_owned)?;
        let selected = super::provider::roster::load(&state_dir, &workspace, name)?;
        selected.matches(args.provider.as_ref(), args.model.as_deref(), args.effort)?;
        let primary = selected.primary();
        args.provider =
            Some(ProviderArg::from_str(&primary.provider.profile()).map_err(str::to_owned)?);
        args.model = Some(primary.model.clone());
        args.effort = primary.effort;
        let admission = args.admit().map_err(str::to_owned)?;
        use std::io::Write;
        std::io::stderr()
            .write_all(selected.manifest()?.as_bytes())
            .map_err(|_| "roster manifest output failed")?;
        let mut signals = StopSignals::install().map_err(|_| "signal handling unavailable")?;
        let providers = tokio::select! {
            biased;
            _ = signals.next() => return Err("interrupted before a Run started; no Run was recorded".into()),
            result = selected.admit(&state_dir, &workspace) => result?,
        };
        refresh_at_startup(
            workspace.clone(),
            primary.provider.profile(),
            primary.account_id,
        );
        let state = StateRoot::open_existing(&state_dir)
            .map_err(|_| "state directory unavailable or unsafe")?;
        super::admit_headless_history(&state)?;
        arany::enable_development_diagnostics(&state);
        let engine = Engine::open(
            state,
            EngineConfig {
                worker_concurrency: None,
                telemetry,
                guard_executable: std::env::current_exe().ok(),
            },
        )
        .map_err(|_| "state store unavailable")?;
        return run_on_engine(args, admission, providers, engine, Some(signals)).await;
    }
    apply_saved_selection(&mut args)?;
    let admission = args.admit().map_err(str::to_owned)?;
    let provider = super::provider::select::select_env_provider(
        args.provider.as_ref().ok_or("--provider is required")?,
        args.model.as_deref().ok_or("--model is required")?,
        args.effort,
        admission.workspace.clone(),
        &admission.state_dir,
    )
    .await
    .map_err(|error| error.to_string())?;
    refresh_at_startup(
        admission.workspace.clone(),
        provider.identity().profile.clone(),
        None,
    );
    run_with_provider(args, admission, provider, telemetry).await
}

const MISSING_SELECTION: &str = "no saved model selection; pass --provider and --model, or choose a model with /model in attached mode";

// Like attached mode, exec without --provider uses the saved selection and a missing effort its saved one.
fn apply_saved_selection(args: &mut ExecArgs) -> Result<(), String> {
    let (workspace, _) = super::admit_workspace(args.workspace.clone(), args.state_dir.clone())
        .map_err(str::to_owned)?;
    if args.provider.is_none() {
        if args.model.is_some() {
            return Err("--provider is required with --model".into());
        }
        let saved = super::attached::models::last_saved_defaults(&workspace)?
            .and_then(|defaults| Some((defaults.provider?, defaults.model?, defaults.effort)))
            .ok_or(MISSING_SELECTION)?;
        args.provider = Some(ProviderArg::from_str(&saved.0).map_err(str::to_owned)?);
        args.model = Some(saved.1);
        args.effort = args.effort.or(saved.2);
    }
    if args.effort.is_none()
        && let (Some(provider), Some(model)) = (&args.provider, &args.model)
    {
        args.effort =
            super::attached::models::saved_effort(&workspace, &provider.profile_label(), model)?;
    }
    Ok(())
}

async fn run_with_provider(
    args: ExecArgs,
    admission: Admission,
    provider: Arc<dyn Provider>,
    telemetry: Telemetry,
) -> Result<CommandOutput, String> {
    if provider.identity().profile
        != args
            .provider
            .as_ref()
            .ok_or("--provider is required")?
            .profile_label()
        || Some(provider.identity().model.as_str()) != args.model.as_deref()
    {
        return Err("selected Provider changed before Workspace admission".into());
    }
    let state = StateRoot::admit(&admission.state_dir)
        .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
    super::admit_headless_history(&state)?;
    arany::enable_development_diagnostics(&state);
    let providers = super::provider::routing::configured_providers(
        &admission.state_dir,
        &admission.workspace,
        provider,
    )
    .await
    .map_err(|error| error.to_string())?;
    let engine = Engine::open(
        state,
        EngineConfig {
            worker_concurrency: None,
            telemetry,
            guard_executable: std::env::current_exe().ok(),
        },
    )
    .map_err(|_| "state store unavailable".to_owned())?;
    run_on_engine(args, admission, providers, engine, None).await
}

fn invoked_skill(
    args: &ExecArgs,
    admission: &Admission,
) -> Result<(String, Option<String>), String> {
    let literal = || Ok((admission.objective.clone(), None));
    if !args.tools
        || args.prompt.is_none()
        || args.goal.is_some()
        || !admission.objective.starts_with('/')
    {
        return literal();
    }
    let names = StateRoot::open_existing(&admission.state_dir)
        .map_err(|_| "Tool configuration unavailable".to_owned())
        .and_then(|root| {
            arany::runtime_skill_names(&root, &admission.workspace)
                .map_err(|error| error.to_string())
        })?;
    Ok(
        match crate::terminal::skill_invocation(&admission.objective, &names) {
            Some((name, _)) => (admission.objective.clone(), Some(name.to_owned())),
            None => (admission.objective.clone(), None),
        },
    )
}

async fn run_on_engine(
    args: ExecArgs,
    admission: Admission,
    providers: AdmittedProviders,
    engine: Engine,
    signals: Option<StopSignals>,
) -> Result<CommandOutput, String> {
    let submission = submit_and_wait(args, admission, providers, engine, signals).await?;
    let submission = classify_submission(submission).await?;
    let committed = load_committed_outcome(submission).await?;
    let classification = classify_committed_state(&committed)?;
    render_committed_outcome(committed, classification)
}

struct Submission {
    output: OutputArg,
    state_dir: PathBuf,
    session_id: Option<SessionId>,
    resumed_goal: Option<GoalId>,
    new_session: bool,
    signalled: SignalledRun,
}

async fn submit_and_wait(
    args: ExecArgs,
    admission: Admission,
    providers: AdmittedProviders,
    engine: Engine,
    signals: Option<StopSignals>,
) -> Result<Submission, String> {
    let new_session = admission.session_id.is_none();
    let state_dir = admission.state_dir.clone();
    let session_id = admission.session_id;
    let output = args.output;
    let tools = if args.tools {
        ToolGrant::Configured
    } else {
        ToolGrant::None
    };
    let (goal_budget, resource_settings, context_window) =
        crate::cli::attached::controls::execution_limits(&admission.state_dir).await?;
    let (objective, skill) = invoked_skill(&args, &admission)?;
    let mut goal = admission.goal;
    apply_saved_limits(&mut goal, goal_budget, resource_settings, context_window);
    let resumed_goal = goal.id;
    let request = RunRequest {
        skill,
        session_id: admission.session_id,
        title: None,
        objective,
        images: Vec::new(),
        workspace: admission.workspace,
        launch_directory: std::env::current_dir().ok(),
        include_paths: args.include_paths,
        policy: admission.policy,
        goal,
    };
    let signals = signals
        .map_or_else(StopSignals::install, Ok)
        .map_err(|_| "signal handling unavailable".to_owned())?;
    let engine = Arc::new(engine);
    let (connection, controls) =
        connect(engine.clone(), state_dir.clone(), providers, request, tools);
    let end = run_signalled(connection, controls, signals).await;
    match Arc::try_unwrap(engine) {
        Ok(engine) => engine
            .close()
            .await
            .map_err(|_| "store shutdown failed".to_owned())?,
        Err(_) => {
            return Err(end
                .err()
                .unwrap_or_else(|| "Run did not release the Engine".to_owned()));
        }
    }
    let signalled = end?;
    Ok(Submission {
        output,
        state_dir,
        session_id,
        resumed_goal,
        new_session,
        signalled,
    })
}

fn connect(
    engine: Arc<Engine>,
    state: PathBuf,
    providers: AdmittedProviders,
    request: RunRequest,
    tools: ToolGrant,
) -> (Connection, Controls) {
    let (controls, mut bootstrap) = Controls::new(arany::ApprovalMode::Request, false);
    bootstrap.controls.approvals = None;
    bootstrap.controls.hints = None;
    bootstrap.controls.steering = None;
    bootstrap.controls.tools = tools;
    let connection = Connection::start(
        engine,
        state,
        controls.clone(),
        bootstrap,
        move |controls, inbox| async move {
            Ok(embedded::Submission {
                providers,
                request,
                controls,
                inbox,
            })
        },
    );
    (connection, controls)
}

struct ClassifiedSubmission {
    output: OutputArg,
    state_dir: PathBuf,
    new_session: bool,
    outcome: arany::RunOutcome,
    stopped: Option<arany::RunStop>,
    continuation_error: Option<EngineError>,
    interrupted: bool,
    forced: Option<&'static str>,
}

async fn classify_submission(submission: Submission) -> Result<ClassifiedSubmission, String> {
    let Submission {
        output,
        state_dir,
        session_id,
        resumed_goal,
        new_session,
        signalled:
            SignalledRun {
                result,
                interrupted,
                forced,
            },
    } = submission;
    let (outcome, stopped, continuation_error) = match result {
        Ok(RunResult {
            run,
            stopped,
            continuation_error,
        }) => (run, stopped, continuation_error),
        Err(RunAttemptError::InterruptedBeforeRun) if interrupted => {
            return Err("interrupted before a Run started; no Run was recorded".into());
        }
        Err(RunAttemptError::Failed(RunFailure::Engine(EngineError::GoalBudgetExhausted))) => {
            if let (Some(session), Some(goal_id)) = (session_id, resumed_goal) {
                let root = StateRoot::open_existing(&state_dir)
                    .map_err(|_| "state directory unavailable or unsafe")?;
                let store = Store::open_read_only(root).map_err(|_| "state store unavailable")?;
                let view = store
                    .load_view(session)
                    .await
                    .map_err(|_| "Session history unavailable or invalid")?;
                store.close().await.map_err(|_| "store shutdown failed")?;
                if let Some(view) = view
                    && let Some(feedback) = crate::presentation::budget_refusal_feedback(
                        &view,
                        Some(goal_id),
                    )
                    .or_else(|| {
                        crate::presentation::budget_stop_run(&view, Some(goal_id))
                            .and_then(|run| crate::presentation::session_run_feedback(&view, run))
                    })
                {
                    return Err(feedback);
                }
            }
            return Err(crate::presentation::BUDGET_REFUSAL_UNCOMMITTED.into());
        }
        Err(RunAttemptError::InterruptedBeforeRun) => {
            return Err(EngineError::CancelledBeforeStart.to_string());
        }
        Err(RunAttemptError::Failed(error)) => return Err(error.to_string()),
        Err(RunAttemptError::Stopped { reason, .. }) => return Err(reason),
    };
    Ok(ClassifiedSubmission {
        output,
        state_dir,
        new_session,
        outcome,
        stopped,
        continuation_error,
        interrupted,
        forced,
    })
}

struct CommittedOutcome {
    output: OutputArg,
    new_session: bool,
    outcome: arany::RunOutcome,
    stopped: Option<arany::RunStop>,
    continuation_error: Option<EngineError>,
    interrupted: bool,
    forced: Option<&'static str>,
    events: Vec<arany::EventEnvelope>,
    view: arany::SessionView,
}

impl CommittedOutcome {
    fn budget_stopped(&self) -> bool {
        self.view.budget_stop_goal(&self.outcome.run).is_some()
    }
}

async fn load_committed_outcome(
    submission: ClassifiedSubmission,
) -> Result<CommittedOutcome, String> {
    let ClassifiedSubmission {
        output,
        state_dir,
        new_session,
        outcome,
        stopped,
        continuation_error,
        interrupted,
        forced,
    } = submission;
    let root = StateRoot::open_existing(&state_dir)
        .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
    let store = Store::open_read_only(root).map_err(|_| "state store unavailable".to_owned())?;
    let events = store
        .load_session(outcome.session_id)
        .await
        .map_err(|_| "Session history unavailable or invalid".to_owned())?;
    let view = store
        .load_view(outcome.session_id)
        .await
        .map_err(|_| "Session history unavailable or invalid".to_owned())?
        .ok_or("Session not found")?;
    store
        .close()
        .await
        .map_err(|_| "store shutdown failed".to_owned())?;
    // A later Run without a terminal receipt may follow the returned committed one.
    let run_matches_history = view
        .runs
        .iter()
        .rev()
        .find(|run| run.id == outcome.run.id)
        .is_some_and(|run| *run == outcome.run);
    if !run_matches_history {
        return Err("Run history unavailable or invalid".into());
    }
    Ok(CommittedOutcome {
        output,
        new_session,
        outcome,
        stopped,
        continuation_error,
        interrupted,
        forced,
        events,
        view,
    })
}

struct CommittedClassification {
    paused: bool,
    success: bool,
    stop: Option<String>,
}

fn classify_committed_state(
    committed: &CommittedOutcome,
) -> Result<CommittedClassification, String> {
    let CommittedOutcome {
        outcome,
        interrupted,
        view,
        ..
    } = committed;
    let run = &outcome.run;
    let paused = (*interrupted || run.status == RunStatus::StoppedByHook)
        && view.goal(outcome.goal_id).is_some_and(|goal| {
            matches!(goal.status, GoalStatus::Running | GoalStatus::Interrupted)
        });
    let success = run.status == RunStatus::Finished && !paused;
    // The committed answer is still printed; the stop line makes the exit nonzero.
    let stop = stop_after_answer(
        run.status,
        committed.stopped,
        committed.continuation_error.as_ref(),
        *interrupted,
    )
    .map(|reason| format!("Goal stopped after a committed Run: {reason}"));
    if !run.status.is_outcome() {
        if committed.budget_stopped()
            && let Some(feedback) = crate::presentation::session_run_feedback(view, run)
        {
            return Err(feedback);
        }
        return Err(committed
            .stopped
            .map_or("Run did not reach a terminal state", stop_text)
            .into());
    }
    Ok(CommittedClassification {
        paused,
        success,
        stop,
    })
}

fn render_committed_outcome(
    committed: CommittedOutcome,
    classification: CommittedClassification,
) -> Result<CommandOutput, String> {
    let stdout = render_exec(
        &committed.outcome.run,
        &committed.events,
        committed.output.into(),
        committed.new_session,
    );
    let stderr = match committed.output {
        OutputArg::Text => text_receipt(&committed)?,
        OutputArg::Jsonl if committed.budget_stopped() => jsonl_failure(&committed),
        OutputArg::Jsonl if classification.success => String::new(),
        OutputArg::Jsonl => jsonl_failure(&committed),
    };
    let stderr = append_resume_and_stop(stderr, &committed, classification);
    Ok(CommandOutput {
        stdout,
        success: stderr.success,
        stderr: stderr.text,
    })
}

fn text_receipt(committed: &CommittedOutcome) -> Result<String, String> {
    let run = &committed.outcome.run;
    let view = &committed.view;
    let outcome = &committed.outcome;
    let mut receipt = format!(
        "Session: {}\nRun: {}\nStatus: {}\n{}",
        outcome.session_id,
        run.id,
        match run.status {
            RunStatus::Finished => "finished",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::StoppedByHook => "stopped_by_hook",
            _ => return Err("Run did not reach a terminal state".into()),
        },
        match run.config.as_ref() {
            Some(config) if config.chatgpt_provenance.is_some() => {
                "Provider: ChatGPT plan\nOutput bound: local only; remote usage is not capped\n"
            }
            Some(config) if config.custom_profile_provenance.is_some() => {
                "Provider: custom\n"
            }
            _ => "",
        },
    );
    if run.config.as_ref().is_some_and(|config| {
        config.chatgpt_provenance.is_none()
            && config.providers.as_ref().is_some_and(|providers| {
                providers.values().iter().any(|chain| {
                    chain.iter().any(|identity| {
                        identity.limits.output_token_bound
                            == arany::OutputTokenBound::LocalAcceptanceOnly
                    })
                })
            })
    }) {
        receipt.push_str(
            "Output bound: some role routes are local only; remote usage is not capped\n",
        );
    }
    let goal_runs: Vec<_> = match view.goal(outcome.goal_id) {
        Some(goal) => view
            .runs
            .iter()
            .filter(|candidate| goal.run_ids.contains(&candidate.id))
            .collect(),
        None => vec![run],
    };
    let unguarded: usize = goal_runs.iter().map(|run| run.unguarded_tools()).sum();
    if unguarded > 0 {
        receipt.push_str(&format!("unguarded effects: {unguarded}\n"));
    }
    let host: usize = goal_runs.iter().map(|run| run.host_tools()).sum();
    if host > 0 {
        receipt.push_str(&format!("host effects: {host}\n"));
    }
    if let Some(notice) = crate::presentation::render_goal_provider_attempts(goal_runs) {
        receipt.push_str(&notice);
        receipt.push('\n');
    }
    if let Some(notice) = crate::presentation::render_cache_update_feedback(run) {
        receipt.push_str(notice);
        receipt.push('\n');
    }
    if run.status == RunStatus::StoppedByHook
        && let Some(notice) = crate::presentation::run_feedback(run)
    {
        receipt.push_str("error: ");
        receipt.push_str(&notice);
        receipt.push('\n');
    }
    if committed.budget_stopped()
        && let Some(feedback) = crate::presentation::session_run_feedback(view, run)
    {
        receipt.push_str(&feedback);
        receipt.push('\n');
    }
    Ok(receipt)
}

fn jsonl_failure(committed: &CommittedOutcome) -> String {
    let run = &committed.outcome.run;
    let view = &committed.view;
    if committed.budget_stopped() {
        return format!(
            "{}\n",
            crate::presentation::session_run_feedback(view, run).unwrap_or_default()
        );
    }
    match run.status {
        RunStatus::Cancelled => "error: Run cancelled\n".into(),
        RunStatus::StoppedByHook => format!(
            "error: {}\n",
            crate::presentation::run_feedback(run).unwrap_or_default()
        ),
        RunStatus::Failed => "error: Run failed\n".into(),
        // A finished Run that is not a success keeps its Goal paused.
        _ => "error: Goal paused before completion\n".into(),
    }
}

struct FinalOutput {
    text: String,
    success: bool,
}

fn append_resume_and_stop(
    mut stderr: String,
    committed: &CommittedOutcome,
    classification: CommittedClassification,
) -> FinalOutput {
    if classification.paused {
        stderr.push_str(&resume_hint(
            committed.outcome.session_id,
            committed.outcome.goal_id,
            committed
                .outcome
                .run
                .config
                .as_ref()
                .and_then(|config| config.roster.as_ref())
                .map(|roster| roster.preset.as_str()),
        ));
    }
    let stop = committed.forced.map(str::to_owned).or(classification.stop);
    if let Some(message) = &stop {
        stderr.push_str(&format!("error: {message}\n"));
    }
    FinalOutput {
        text: stderr,
        success: classification.success && stop.is_none(),
    }
}

fn resume_hint(session_id: SessionId, goal_id: GoalId, preset: Option<&str>) -> String {
    let selector = preset.map_or_else(String::new, |name| format!(" --profile {name}"));
    format!(
        "Goal paused; resume with arany exec{selector} --session-id {session_id} --resume-goal {goal_id}\n"
    )
}

struct SignalledRun {
    result: Result<RunResult, RunAttemptError>,
    interrupted: bool,
    forced: Option<&'static str>,
}

async fn run_signalled(
    mut connection: Connection,
    controls: Controls,
    mut signals: StopSignals,
) -> Result<SignalledRun, String> {
    match wait_signalled(&mut connection, &controls, &mut signals).await {
        Ok(run) => connection.close().await.map(|()| run),
        Err(error) => {
            connection.abort().await;
            Err(error)
        }
    }
}

async fn wait_signalled(
    connection: &mut Connection,
    controls: &Controls,
    signals: &mut StopSignals,
) -> Result<SignalledRun, String> {
    let mut interrupted = false;
    let forced = loop {
        tokio::select! {
            biased;
            signal = signals.next() => match stop_step(interrupted, signal) {
                StopStep::Cancel => {
                    interrupted = true;
                    controls.cancel().await?;
                }
                StopStep::Force(reason) => {
                    controls.cancel().await?;
                    break reason;
                }
            },
            event = connection.next() => {
                if let embedded::Event::Finished(result) = event? {
                    return Ok(SignalledRun { result: *result, interrupted, forced: None });
                }
            }
        }
    };
    match tokio::time::timeout(SHUTDOWN_GRACE, async {
        loop {
            if let embedded::Event::Finished(result) = connection.next().await? {
                return Ok(SignalledRun {
                    result: *result,
                    interrupted: true,
                    forced: Some(forced),
                });
            }
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(format!(
            "{forced}; Run may be interrupted; check Session history"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arany::{
        AgentPhase, Answer, Event, ProviderError, ProviderOutcome, ProviderRequest,
        ProviderResponse, RunId, RunStop, SessionView, Tapped,
        test_support::scenario::ScriptedProvider,
    };
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    struct Scripted {
        answers: Arc<Mutex<VecDeque<Result<Answer, ProviderError>>>>,
        observed: Arc<Mutex<Vec<ProviderRequest>>>,
    }

    #[async_trait::async_trait]
    impl Provider for Scripted {
        fn identity(&self) -> &arany::ProviderIdentity {
            static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
                std::sync::LazyLock::new(|| {
                    arany::ProviderIdentity::new("openai", "gpt-5.4", arany::Provenance::native())
                });
            &IDENTITY
        }

        async fn invoke(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            self.observed.lock().expect("observations").push(request);
            let answer = self
                .answers
                .lock()
                .expect("script")
                .pop_front()
                .expect("one scripted call")?;
            Ok(ProviderResponse {
                turn: arany::ProviderTurn::scripted(),
                outcome: ProviderOutcome::Answer(answer),
                response_id: Some("resp_test".into()),
                usage: arany::Usage {
                    input_tokens: Some(20),
                    output_tokens: Some(10),
                    ..Default::default()
                },
                wire_provenance: None,
            })
        }
    }

    fn args(state_dir: PathBuf, workspace: PathBuf, prompt: &str) -> ExecArgs {
        ExecArgs {
            state_dir: Some(state_dir),
            workspace: Some(workspace),
            provider: Some(ProviderArg::Openai),
            model: Some("gpt-5.4".into()),
            profile: None,
            effort: None,
            session_id: None,
            resume_goal: None,
            continue_goal: None,
            goal: None,
            output: OutputArg::Text,
            collaboration: CollaborationArg::Single,
            max_active_children: None,
            include_paths: Vec::new(),
            tools: false,
            prompt: Some(prompt.into()),
        }
    }

    #[test]
    #[cfg_attr(
        not(arany_ci),
        ignore = "native exec host policy; requires the arany executable"
    )]
    fn host_prefix_exec_receipts_follow_headless_policy_and_replay() {
        let _gate = crate::test_process::NATIVE_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let launcher = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("arany");
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                for mode in ["default", "dontAsk", "auto", "acceptEdits", "plan"] {
                    let _deadline = arany::test_support::engine::deadline(
                        "exec host policy",
                        std::time::Duration::from_secs(30),
                    );
                    host_exec_scenario(launcher.clone(), mode).await;
                }
            });
    }

    async fn host_exec_scenario(launcher: PathBuf, mode: &str) {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let state_dir = temp.path().join("state");
        let call = arany::test_support::approval::seed_host_prefix(
            &state_dir,
            &workspace,
            launcher.clone(),
        )
        .await;
        let path = state_dir.join("permissions.json");
        let mut settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        settings["permissions"]["defaultMode"] = serde_json::json!(mode);
        std::fs::write(&path, serde_json::to_vec(&settings).unwrap()).unwrap();
        let provider = Arc::new(
            ScriptedProvider::new(vec![
                arany::test_support::scenario::tool_outcome(call),
                ProviderOutcome::Answer(Answer {
                    text: "Host policy settled".into(),
                }),
            ])
            .with_profile("openai", "gpt-5.4"),
        );
        let mut exec = args(state_dir.clone(), workspace, "Inspect host value");
        exec.tools = true;
        let admission = exec.admit().unwrap();
        let engine = arany::test_support::engine::open(
            StateRoot::open_existing(&state_dir).unwrap(),
            EngineConfig {
                guard_executable: Some(launcher),
                ..Default::default()
            },
        )
        .unwrap();
        let output = run_on_engine(
            exec,
            admission,
            AdmittedProviders::single(provider.clone()),
            engine,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(path).unwrap(),
            serde_json::to_vec(&settings).unwrap()
        );
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
        assert!(provider.responses.lock().unwrap().is_empty());
        assert_host_exec_output(&state_dir, &output, mode != "plan").await;
    }

    async fn assert_host_exec_output(
        state_dir: &std::path::Path,
        output: &CommandOutput,
        allowed: bool,
    ) {
        assert!(output.success);
        assert_eq!(
            output.stdout,
            if allowed {
                "Answer (unguarded effects: 1):\n  Host policy settled\n"
            } else {
                "Answer:\n  Host policy settled\n"
            }
        );
        let session = SessionId::from_str(
            output
                .stderr
                .lines()
                .next()
                .unwrap()
                .strip_prefix("Session: ")
                .unwrap(),
        )
        .unwrap();
        let (events, view) = arany::test_support::goals::replayed(state_dir, session).await;
        assert_eq!(view.runs[0].unguarded_tools(), usize::from(allowed));
        let tool = &view.runs[0].tools[0];
        assert_eq!(tool.permission_verdict.as_ref().unwrap().approved, allowed);
        if allowed {
            assert_eq!(
                tool.permission_verdict.as_ref().unwrap().source,
                arany::permissions::PermissionSource::HostPrefixRule
            );
        }
        assert_eq!(
            output.stderr,
            format!(
                "Session: {session}\nRun: {}\nStatus: finished\n{}",
                view.runs[0].id,
                if allowed {
                    "unguarded effects: 1\n"
                } else {
                    ""
                }
            )
        );
        for prefix in 1..=events.len() {
            SessionView::replay(session, &events[..prefix]).unwrap();
        }
    }

    #[test]
    fn slash_skill_prompts_invoke_admitted_skills_and_everything_else_stays_literal() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("temp");
        let workspace = temp.path().join("workspace");
        for name in ["ask-matt", "help"] {
            let dir = workspace.join(".agents/skills").join(name);
            std::fs::create_dir_all(&dir).expect("skill directory");
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: Guide\n---\nBody\n"),
            )
            .expect("skill");
        }
        let state_dir = temp.path().join("state");
        let state = StateRoot::admit(&state_dir).expect("state");
        let config = state_dir.join("tools.json");
        std::fs::write(
            &config,
            r#"{"workspace_paths":["."],"write":false,"commands":[],"mcp":[],"skills":[]}"#,
        )
        .expect("config");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).expect("mode");
        drop(state);
        let invoked = |prompt: &str, tools: bool| {
            let mut exec = args(state_dir.clone(), workspace.clone(), prompt);
            exec.tools = tools;
            let admission = exec.admit().expect("admitted");
            invoked_skill(&exec, &admission).expect("skill resolution")
        };
        for text in ["/ask-matt do X", "/ask-matt   do X  \nnext"] {
            assert_eq!(
                invoked(text, true),
                (text.to_owned(), Some("ask-matt".into()))
            );
        }
        assert_eq!(
            invoked("/ask-matt", true),
            ("/ask-matt".to_owned(), Some("ask-matt".into()))
        );
        for literal in ["/help", "//ask-matt", "/unknown x", "ask-matt"] {
            assert_eq!(invoked(literal, true), (literal.to_owned(), None));
        }
        assert_eq!(
            invoked("/ask-matt do X", false),
            ("/ask-matt do X".to_owned(), None)
        );
    }

    #[test]
    fn only_a_stop_after_a_committed_answer_writes_the_stop_line() {
        let exhausted = RunStop::GoalExhausted(arany::GoalLimit::Tokens);
        // (Run status, projected stop, pause requested, stop line)
        let rows = [
            (RunStatus::Finished, None, false, None),
            (
                RunStatus::Finished,
                Some(exhausted),
                false,
                Some("Goal budget is exhausted"),
            ),
            (
                RunStatus::Finished,
                Some(exhausted),
                true,
                Some("Goal budget is exhausted"),
            ),
            (
                RunStatus::Finished,
                Some(RunStop::Paused),
                false,
                Some("Goal paused before completion"),
            ),
            (RunStatus::Finished, Some(RunStop::Paused), true, None),
            (
                RunStatus::Finished,
                Some(RunStop::Uncertain),
                true,
                Some("Tool effect completion is uncertain"),
            ),
            (
                RunStatus::Finished,
                Some(RunStop::Interrupted),
                false,
                Some("Run interrupted without a terminal receipt; check Session history"),
            ),
            (
                RunStatus::Interrupted,
                Some(RunStop::Interrupted),
                false,
                None,
            ),
            (RunStatus::Failed, Some(exhausted), false, None),
            (
                RunStatus::Failed,
                Some(RunStop::Failed(arany::RunFailure::ToolStopped)),
                false,
                None,
            ),
            (RunStatus::Cancelled, Some(RunStop::Paused), true, None),
            (
                RunStatus::StoppedByHook,
                Some(RunStop::StoppedByHook),
                false,
                None,
            ),
        ];
        for (status, stop, pause_requested, line) in rows {
            assert_eq!(
                stop_after_answer(status, stop, None, pause_requested).as_deref(),
                line,
                "{status:?} {stop:?} {pause_requested}"
            );
        }
        // A refused next Run keeps its actionable error, never a pause.
        let refused = EngineError::WorkspaceUnavailable;
        assert_eq!(
            stop_after_answer(
                RunStatus::Finished,
                Some(RunStop::Paused),
                Some(&refused),
                false
            )
            .as_deref(),
            Some("Workspace input is unavailable")
        );
        assert_eq!(
            stop_after_answer(
                RunStatus::Failed,
                Some(RunStop::Paused),
                Some(&refused),
                false
            ),
            None
        );
        assert_eq!(
            stop_text(RunStop::Interrupted),
            "Run interrupted without a terminal receipt; check Session history"
        );
    }

    #[test]
    fn resume_hint_names_the_session_and_goal() {
        let session = SessionId::new();
        let goal = GoalId::new();
        assert_eq!(
            resume_hint(session, goal, None),
            format!(
                "Goal paused; resume with arany exec --session-id {session} --resume-goal {goal}\n"
            )
        );
        assert_eq!(
            resume_hint(session, goal, Some("coding")),
            format!(
                "Goal paused; resume with arany exec --profile coding --session-id {session} --resume-goal {goal}\n"
            )
        );
    }

    #[test]
    fn interrupted_goal_resume_requires_an_explicit_session() {
        #[derive(clap::Parser)]
        struct Probe {
            #[command(flatten)]
            exec: ExecArgs,
        }
        let goal = arany::GoalId::new().to_string();
        let error = <Probe as clap::Parser>::try_parse_from([
            "probe",
            "--provider",
            "openai",
            "--model",
            "gpt-5.4",
            "--resume-goal",
            goal.as_str(),
            "resume",
        ])
        .err()
        .expect("--resume-goal without --session-id is a usage error");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn exec_goal_and_prompt_less_resume_admit_their_objective_shapes() {
        #[derive(clap::Parser)]
        struct Probe {
            #[command(flatten)]
            exec: ExecArgs,
        }
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state = temp.path().join("state");
        let (state_arg, workspace_arg) = (
            state.to_str().expect("UTF-8 state path"),
            workspace.to_str().expect("UTF-8 workspace path"),
        );
        let session = SessionId::default().to_string();
        let goal_id = arany::GoalId::new().to_string();
        let parse = |tail: &[&str]| {
            let mut argv = vec![
                "probe",
                "--provider",
                "openai",
                "--model",
                "gpt-5.4",
                "--state-dir",
                state_arg,
                "--workspace",
                workspace_arg,
            ];
            argv.extend_from_slice(tail);
            <Probe as clap::Parser>::try_parse_from(argv).map(|probe| probe.exec)
        };

        let admission = parse(&["--goal", "Ship the change"])
            .expect("--goal needs no positional prompt")
            .admit()
            .expect("autonomous objective");
        assert_eq!(admission.objective, "Ship the change");
        assert!(admission.goal.autonomous);

        let admission = parse(&["--session-id", &session, "--continue-goal", &goal_id])
            .expect("continue parses without a prompt")
            .admit()
            .expect("prompt-less continuation");
        assert!(admission.objective.is_empty());
        assert_eq!(
            admission.goal.continues,
            Some(goal_id.parse().expect("Goal ID"))
        );
        assert_eq!(admission.goal.id, None);
        let mut continued = admission.goal;
        let saved = GoalBudget {
            max_runs: 1,
            ..GoalBudget::default()
        };
        let window = arany::ContextWindow::Tokens(500_000);
        apply_saved_limits(
            &mut continued,
            saved,
            arany::ToolResourceSettings::default(),
            window,
        );
        assert_eq!(continued.budget, Some(GoalBudget::default()));
        assert_eq!(
            continued.context_window, window,
            "every Run uses the saved window"
        );
        let mut fresh = GoalRequest::default();
        apply_saved_limits(
            &mut fresh,
            saved,
            arany::ToolResourceSettings::default(),
            window,
        );
        assert_eq!(fresh.budget, Some(saved));
        assert_eq!(fresh.context_window, window);
        assert!(parse(&["--continue-goal", &goal_id]).is_err());
        assert!(
            parse(&[
                "--session-id",
                &session,
                "--continue-goal",
                &goal_id,
                "--resume-goal",
                &goal_id
            ])
            .is_err()
        );

        let admission = parse(&["--session-id", &session, "--resume-goal", &goal_id])
            .expect("resume parses without a prompt")
            .admit()
            .expect("prompt-less resume");
        assert!(admission.objective.is_empty());
        assert_eq!(
            admission.goal.id,
            Some(arany::GoalId::from_str(&goal_id).expect("Goal ID"))
        );

        let admission = parse(&[
            "--session-id",
            &session,
            "--resume-goal",
            &goal_id,
            "Keep going",
        ])
        .expect("resume accepts a prompt as its text")
        .admit()
        .expect("prompt with resume");
        assert_eq!(admission.objective, "Keep going");
        assert_eq!(
            parse(&[]).expect("parses").admit().err(),
            Some(
                "pass --goal alone, a prompt alone, --resume-goal with an optional prompt, or --continue-goal alone"
            )
        );
        assert_eq!(
            parse(&["--goal", " "]).expect("parses").admit().err(),
            Some("invalid objective")
        );
        let largest = "x".repeat(64 * 1024);
        assert_eq!(
            parse(&["--goal", &largest])
                .expect("parses")
                .admit()
                .expect("64 KiB objective")
                .objective
                .len(),
            64 * 1024
        );
        assert_eq!(
            parse(&["--goal", &format!("{largest}x")])
                .expect("parses")
                .admit()
                .err(),
            Some("invalid objective")
        );
        for tail in [
            vec!["--goal", "Ship the change", "Also a prompt"],
            vec![
                "--goal",
                "Ship the change",
                "--session-id",
                &session,
                "--resume-goal",
                &goal_id,
            ],
        ] {
            assert_eq!(
                parse(&tail).err().expect("conflicting shapes").kind(),
                clap::error::ErrorKind::ArgumentConflict
            );
        }
    }

    fn scripted(
        answer: Result<Answer, ProviderError>,
        observed: &Arc<Mutex<Vec<ProviderRequest>>>,
    ) -> Scripted {
        Scripted {
            answers: Arc::new(Mutex::new(VecDeque::from([answer]))),
            observed: Arc::clone(observed),
        }
    }

    #[tokio::test]
    async fn exec_defaults_complete_past_64_calls_and_30_active_minutes() {
        use arany::test_support::goals::{
            assert_long_turn, facts_workspace, long_turn_provider, replayed,
        };
        use std::os::unix::fs::PermissionsExt;
        for finite in [false, true] {
            let temp = tempfile::tempdir().expect("private root");
            let workspace = facts_workspace(temp.path(), "Fact");
            let state_dir = temp.path().join("state");
            let root = StateRoot::admit(&state_dir).expect("state");
            let config = state_dir.join("tools.json");
            std::fs::write(
                &config,
                br#"{"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}"#,
            )
            .expect("config");
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))
                .expect("private config");
            if finite {
                let preferences = serde_json::json!({"preferences":{
                    "goal_budget":GoalBudget { max_provider_calls:Some(1), ..Default::default() },
                }});
                root.record(arany::RecordKind::UiPreferences)
                    .replace(&serde_json::to_vec(&preferences).expect("settings"))
                    .expect("finite settings");
            }
            drop(root);
            let provider = Arc::new(long_turn_provider());
            let mut exec = args(state_dir.clone(), workspace, "Inspect the folder");
            exec.tools = true;
            let admission = exec.admit().expect("exec admission");
            let report =
                run_with_provider(exec, admission, provider.clone(), Telemetry::disabled())
                    .await
                    .expect("long exec");
            assert_eq!(report.success, !finite);
            assert_eq!(
                report.stdout,
                if finite {
                    ""
                } else {
                    "Answer:\n  Long turn completed\n"
                }
            );
            let receipt: Vec<_> = report.stderr.lines().collect();
            assert_eq!(receipt.len(), if finite { 6 } else { 3 });
            let id = SessionId::from_str(
                receipt[0]
                    .strip_prefix("Session: ")
                    .expect("Session receipt"),
            )
            .expect("Session ID");
            let (events, view) = replayed(&state_dir, id).await;
            assert_eq!(
                report.stderr,
                format!(
                    "Session: {id}\nRun: {}\nStatus: {}\n",
                    view.runs[0].id,
                    if finite { "failed" } else { "finished" }
                ) + &if finite {
                    format!(
                        "{}\n",
                        crate::presentation::session_run_feedback(&view, &view.runs[0]).unwrap()
                    )
                } else {
                    String::new()
                }
            );
            if finite {
                assert_eq!(view.goals[0].budget.max_provider_calls, Some(1));
                assert_eq!(view.goals[0].provider_calls, 1);
                assert_eq!(view.goals[0].status, arany::GoalStatus::Failed);
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

    #[tokio::test]
    async fn exec_rejects_unsafe_settings_before_dispatch() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("private root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state = temp.path().join("state");
        let root = StateRoot::admit(&state).expect("State");
        root.record(arany::RecordKind::UiPreferences)
            .replace(b"malformed")
            .expect("record");
        std::fs::set_permissions(
            state.join("ui-preferences.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .expect("unsafe fixture");
        drop(root);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let exec = args(state.clone(), workspace.clone(), "Never dispatched");
        let admission = exec.admit().expect("arguments");
        let error = run_with_provider(
            exec,
            admission,
            Arc::new(scripted(Err(ProviderError::Unavailable), &observed)),
            Telemetry::disabled(),
        )
        .await
        .err()
        .expect("settings refusal");
        assert_eq!(error, "display settings unavailable or unsafe");
        assert!(observed.lock().expect("observations").is_empty());
        assert!(
            arany::list_sessions(StateRoot::open_existing(&state).expect("State"), workspace)
                .await
                .expect("history")
                .is_empty()
        );
        assert_eq!(
            std::fs::read(state.join("ui-preferences.json")).expect("unchanged record"),
            b"malformed"
        );
    }

    #[tokio::test]
    async fn exec_resume_of_an_exhausted_goal_closes_it_and_names_the_reason() {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state_dir = temp.path().join("state");
        let observed = Arc::new(Mutex::new(Vec::new()));
        let engine = Arc::new(
            arany::test_support::engine::open(
                StateRoot::admit(&state_dir).expect("state"),
                EngineConfig::default(),
            )
            .expect("engine"),
        );
        let provider = Arc::new(scripted(
            Ok(Answer {
                text: "first".into(),
            }),
            &observed,
        ));
        let mut goal = GoalRequest::new(
            GoalBudget {
                max_provider_calls: Some(1),
                ..GoalBudget::default()
            },
            GoalPhase::Implementation,
        );
        goal.finish_after_run = false;
        let paused = embedded::run_with_providers(
            &engine,
            AdmittedProviders::single(provider),
            RunRequest {
                skill: None,
                session_id: None,
                title: Some("Exhausted".into()),
                objective: "Use the only Provider call".into(),
                images: Vec::new(),
                workspace: workspace.clone(),
                launch_directory: None,
                include_paths: Vec::new(),
                policy: CollaborationPolicy::Single,
                goal,
            },
            &state_dir,
            false,
            None,
            arany::ApprovalMode::Request,
        )
        .await
        .expect("paused Goal Run")
        .run;
        Arc::try_unwrap(engine)
            .ok()
            .expect("Engine released")
            .close()
            .await
            .expect("engine shutdown");

        let mut resume = args(state_dir.clone(), workspace.clone(), "unused");
        resume.prompt = None;
        resume.session_id = Some(paused.session_id.to_string());
        resume.resume_goal = Some(paused.goal_id.to_string().to_uppercase());
        let admission = resume.admit().expect("valid resume");
        let error = run_with_provider(
            resume,
            admission,
            Arc::new(scripted(Err(ProviderError::Unavailable), &observed)),
            Telemetry::disabled(),
        )
        .await
        .err()
        .expect("an exhausted resume exits unsuccessfully with its reason");
        assert!(error.starts_with("Arany status: Goal budget refused resume: Provider calls; exhausted; used at refusal 1, pinned limit 1; requested 1; required minimum 1"));
        assert!(error.ends_with("The committed answer is preserved."));
        assert_eq!(observed.lock().expect("observations").len(), 1);

        let mut again = args(state_dir.clone(), workspace.clone(), "unused");
        again.prompt = None;
        again.session_id = Some(paused.session_id.to_string());
        again.resume_goal = Some(paused.goal_id.to_string());
        let admission = again.admit().expect("valid resume");
        let error = run_with_provider(
            again,
            admission,
            Arc::new(scripted(Err(ProviderError::Unavailable), &observed)),
            Telemetry::disabled(),
        )
        .await
        .err()
        .expect("a closed Goal is not resumable");
        assert_eq!(error, "Goal is not available for explicit resume");
        assert_finished_run_jsonl_budget_feedback(state_dir, paused).await;
    }

    async fn assert_finished_run_jsonl_budget_feedback(
        state_dir: PathBuf,
        outcome: arany::RunOutcome,
    ) {
        let session_id = outcome.session_id;
        let mut committed = load_committed_outcome(ClassifiedSubmission {
            output: OutputArg::Jsonl,
            state_dir,
            new_session: true,
            outcome,
            stopped: None,
            continuation_error: None,
            interrupted: false,
            forced: None,
        })
        .await
        .expect("reopened exhausted Goal with its committed answer");
        let run = &committed.outcome.run;
        assert_eq!(run.status, RunStatus::Finished);
        assert_eq!(run.assistant_message.as_deref(), Some("first"));
        assert_eq!(
            committed.view.goals.last().expect("Failed Goal").status,
            GoalStatus::Failed
        );
        let feedback = crate::presentation::session_run_feedback(&committed.view, run)
            .expect("budget refusal feedback");
        committed.stopped = committed.view.run_stop(run);
        assert_eq!(
            committed.stopped,
            Some(RunStop::GoalExhausted(arany::GoalLimit::Calls))
        );
        let classification = classify_committed_state(&committed).expect("committed state");
        let report = render_committed_outcome(committed, classification).expect("JSONL report");
        assert_jsonl_committed_answer(&report.stdout, "first");
        let first_event: serde_json::Value =
            serde_json::from_str(report.stdout.lines().next().expect("SessionStarted line"))
                .expect("JSONL Event");
        assert_eq!(first_event["session_id"], session_id.to_string());
        assert_eq!(
            report.stderr,
            format!(
                "{feedback}\nerror: Goal stopped after a committed Run: Goal budget is exhausted\n"
            )
        );
        assert!(!report.success);
    }

    const ACTIVE_LIMIT_MS: u64 = 60_000;
    const ANSWER_GATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

    async fn run_with_budget_spent_after_answer(
        exec: ExecArgs,
        admission: Admission,
        provider: ScriptedProvider,
        active_time: std::time::Duration,
    ) -> CommandOutput {
        let requests = Arc::clone(&provider.requests);
        let (gate_tx, mut gate_rx) = tokio::sync::mpsc::channel(1);
        let provider = provider.with_before_call(1, move || {
            let session = requests
                .lock()
                .expect("requests")
                .last()
                .expect("call")
                .session_id;
            let gate = arany::FaultPlan::gate_goal_append_acknowledgment(session, |event| {
                matches!(event, Event::MessageCommitted { .. })
            });
            gate_tx.try_send(gate).expect("one answer gate");
        });
        let run = run_with_provider(exec, admission, Arc::new(provider), Telemetry::disabled());
        tokio::pin!(run);
        let answer_deadline = tokio::time::Instant::now() + ANSWER_GATE_TIMEOUT;
        let gate = tokio::select! {
            gate = tokio::time::timeout_at(answer_deadline, gate_rx.recv()) => gate,
            result = &mut run => panic!("Run ended before the answer gate: {:?}", result.map(|output| output.success)),
        };
        let (entered, release) = match gate {
            Ok(Some(gate)) => gate,
            _ => {
                drop(gate_rx);
                panic!("answer gate was not installed before the deadline");
            }
        };
        let entered = tokio::select! {
            entered = tokio::time::timeout_at(answer_deadline, entered) => entered,
            result = &mut run => {
                drop(release);
                panic!("Run ended before answer acknowledgment: {:?}", result.map(|output| output.success));
            }
        };
        if !matches!(entered, Ok(Ok(()))) {
            drop(release);
            panic!("committed answer gate failed: {entered:?}");
        }
        tokio::time::pause();
        tokio::time::advance(active_time).await;
        tokio::time::resume();
        release.send(()).expect("release accepted answer");
        tokio::time::timeout(ANSWER_GATE_TIMEOUT, run)
            .await
            .expect("Goal settlement deadline")
            .expect("completed Run with an exhausted Goal")
    }

    fn exec_budget_fixture(
        state_dir: PathBuf,
        workspace: PathBuf,
        budget: GoalBudget,
    ) -> (ExecArgs, ScriptedProvider) {
        use std::os::unix::fs::PermissionsExt;
        let root = StateRoot::admit(&state_dir).expect("state");
        let tools = state_dir.join("tools.json");
        std::fs::write(
            &tools,
            br#"{"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#,
        )
        .expect("Tool configuration");
        std::fs::set_permissions(&tools, std::fs::Permissions::from_mode(0o600))
            .expect("private Tool configuration");
        let preferences = serde_json::json!({"preferences":{"goal_budget":budget}});
        root.record(arany::RecordKind::UiPreferences)
            .replace(&serde_json::to_vec(&preferences).expect("settings"))
            .expect("record settings");
        drop(root);

        let contract = arany::GoalPlanContract {
            mission: "Inspect the notes and report the result".into(),
            paths: vec!["notes".into()],
            commands: Vec::new(),
            resource_class: arany::ToolResourceClass::Default,
            command_timeout_ms: None,
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            budget,
            success_criterion: arany::GoalSuccessCriterion::WorkspaceFileContent {
                path: "notes/facts.txt".into(),
                content: "v2".into(),
            },
        };
        let provider = arany::test_support::scenario::ScriptedProvider::new(vec![
            arany::test_support::scenario::plan_outcome(contract),
            arany::test_support::goals::finish_with("The notes remain unchanged"),
        ])
        .with_profile("openai", "gpt-5.4");
        let mut exec = args(state_dir.clone(), workspace.clone(), "Inspect the notes");
        exec.prompt = None;
        exec.goal = Some("Inspect the notes".into());
        exec.output = OutputArg::Jsonl;
        exec.tools = true;
        (exec, provider)
    }

    async fn exec_budget_report(
        budget: GoalBudget,
    ) -> (CommandOutput, Vec<arany::EventEnvelope>, SessionView) {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = arany::test_support::goals::facts_workspace(temp.path(), "v1");
        let state_dir = temp.path().join("state");
        let (exec, provider) = exec_budget_fixture(state_dir.clone(), workspace, budget);
        let requests = Arc::clone(&provider.requests);
        let admission = exec.admit().expect("valid autonomous command");
        let report = if let Some(active_ms) = budget.max_active_ms {
            run_with_budget_spent_after_answer(
                exec,
                admission,
                provider,
                std::time::Duration::from_millis(active_ms),
            )
            .await
        } else {
            run_with_provider(exec, admission, Arc::new(provider), Telemetry::disabled())
                .await
                .expect("completed Run with an exhausted Goal")
        };
        let first_event: serde_json::Value =
            serde_json::from_str(report.stdout.lines().next().expect("first Event line"))
                .expect("JSONL Event");
        let session_id =
            SessionId::from_str(first_event["session_id"].as_str().expect("Session ID"))
                .expect("typed Session ID");
        assert_eq!(
            requests
                .lock()
                .expect("requests")
                .first()
                .expect("first call")
                .session_id,
            session_id
        );
        let (events, view) = arany::test_support::goals::replayed(&state_dir, session_id).await;
        assert_eq!(requests.lock().expect("Provider requests").len(), 2);
        (report, events, view)
    }

    #[tokio::test]
    async fn exec_jsonl_keeps_budget_feedback_for_settled_runs() {
        let (report, events, view) = exec_budget_report(GoalBudget {
            max_provider_calls: Some(2),
            ..GoalBudget::default()
        })
        .await;
        let run = view.runs.last().expect("Failed Run");
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.failure, Some(arany::RunFailure::CriterionUnmet));
        assert!(run.assistant_message.is_none());
        assert_eq!(
            view.goals.last().expect("Failed Goal").status,
            GoalStatus::Failed
        );
        let feedback =
            crate::presentation::session_run_feedback(&view, run).expect("budget refusal feedback");
        assert_eq!(report.stderr, format!("{feedback}\n"));
        assert!(!report.success);
        assert_finished_jsonl_budget_feedback(events);
    }

    #[tokio::test]
    async fn exec_jsonl_keeps_budget_feedback_for_a_finished_run() {
        const WATCHDOG_GRACE: std::time::Duration = std::time::Duration::from_secs(60);
        let _deadline = arany::test_support::engine::deadline(
            "JSONL budget feedback",
            ANSWER_GATE_TIMEOUT + WATCHDOG_GRACE,
        );
        let budget = GoalBudget {
            max_active_ms: Some(ACTIVE_LIMIT_MS),
            ..GoalBudget::default()
        };
        let (report, _, view) = exec_budget_report(budget).await;
        let run = view.runs.last().expect("Finished Run");
        assert_eq!(run.status, RunStatus::Finished);
        assert_eq!(
            run.assistant_message.as_deref(),
            Some("The notes remain unchanged")
        );
        let goal = view.goals.last().expect("Failed Goal");
        assert_eq!(goal.status, GoalStatus::Failed);
        assert!(goal.active_ms >= ACTIVE_LIMIT_MS);
        assert_eq!(
            goal.budget_refusal.expect("budget refusal").dimension,
            arany::GoalLimit::ActiveTime
        );
        let feedback =
            crate::presentation::session_run_feedback(&view, run).expect("budget refusal feedback");
        assert_eq!(
            report.stderr,
            format!(
                "{feedback}\nerror: Goal stopped after a committed Run: Goal budget is exhausted\n"
            )
        );
        assert!(!report.success);
        assert_jsonl_committed_answer(&report.stdout, "The notes remain unchanged");
    }

    #[tokio::test]
    async fn exec_jsonl_keeps_budget_feedback_for_a_finished_run_in_an_existing_session() {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = arany::test_support::goals::facts_workspace(temp.path(), "v1");
        let state_dir = temp.path().join("state");
        let budget = GoalBudget {
            max_active_ms: Some(ACTIVE_LIMIT_MS),
            ..GoalBudget::default()
        };
        let (mut exec, provider) =
            exec_budget_fixture(state_dir.clone(), workspace.clone(), budget);
        let session_id = arany::create_session(
            StateRoot::open_existing(&state_dir).expect("state"),
            workspace,
            None,
        )
        .await
        .expect("existing Session");
        exec.session_id = Some(session_id.to_string());
        let admission = exec.admit().expect("valid autonomous command");
        let report = run_with_budget_spent_after_answer(
            exec,
            admission,
            provider,
            std::time::Duration::from_millis(budget.max_active_ms.expect("finite active time")),
        )
        .await;
        let (_, view) = arany::test_support::goals::replayed(&state_dir, session_id).await;
        let run = view.runs.last().expect("Finished Run");
        assert_eq!(run.status, RunStatus::Finished);
        assert_eq!(
            view.goals.last().expect("Failed Goal").status,
            GoalStatus::Failed
        );
        let feedback =
            crate::presentation::session_run_feedback(&view, run).expect("budget refusal feedback");
        assert_eq!(
            report.stderr,
            format!(
                "{feedback}\nerror: Goal stopped after a committed Run: Goal budget is exhausted\n"
            )
        );
        assert!(!report.success);
        assert_jsonl_committed_answer(&report.stdout, "The notes remain unchanged");
    }

    fn assert_finished_jsonl_budget_feedback(events: Vec<arany::EventEnvelope>) {
        let committed = finished_run_at_active_limit(events);
        let feedback =
            crate::presentation::session_run_feedback(&committed.view, &committed.outcome.run)
                .expect("budget refusal feedback for a committed answer");
        assert!(feedback.ends_with("The committed answer is preserved."));
        let answer = committed
            .outcome
            .run
            .assistant_message
            .clone()
            .expect("committed answer");
        let classification = classify_committed_state(&committed).expect("committed outcome");
        let report = render_committed_outcome(committed, classification).expect("JSONL rendering");
        assert_eq!(
            report.stderr,
            format!(
                "{feedback}\nerror: Goal stopped after a committed Run: Goal budget is exhausted\n"
            )
        );
        assert_jsonl_committed_answer(&report.stdout, &answer);
        assert!(!report.success);
    }

    fn assert_jsonl_committed_answer(stdout: &str, answer: &str) {
        let answer_line = stdout
            .lines()
            .map(|line| {
                serde_json::from_str::<arany_cli::protocol::EventLine>(line).expect("JSONL Event")
            })
            .find(|line| line.kind == "MessageCommitted")
            .expect("committed answer Event");
        assert_eq!(answer_line.payload["text"], answer);
    }

    fn finished_run_at_active_limit(events: Vec<arany::EventEnvelope>) -> CommittedOutcome {
        const ANSWER: &str = "The notes remain unchanged";
        let budget = GoalBudget {
            max_active_ms: Some(ACTIVE_LIMIT_MS),
            ..GoalBudget::default()
        };
        let mut fixture = Vec::new();
        for mut envelope in events {
            if let Some(message) = finish_budget_event(&mut envelope, budget, ANSWER) {
                fixture.push(message);
            }
            fixture.push(envelope);
        }
        for (envelope, sequence) in fixture.iter_mut().zip(1..) {
            envelope.sequence = sequence;
        }
        let session_id = fixture[0].session_id;
        let view = SessionView::replay(session_id, &fixture)
            .expect("valid late-settlement fixture")
            .expect("Session");
        let run = view.runs.last().expect("Finished Run").clone();
        assert_eq!(run.status, RunStatus::Finished);
        assert_eq!(run.assistant_message.as_deref(), Some(ANSWER));
        assert_eq!(
            view.goals.last().expect("Failed Goal").status,
            GoalStatus::Failed
        );
        CommittedOutcome {
            output: OutputArg::Jsonl,
            new_session: true,
            outcome: arany::RunOutcome {
                session_id,
                goal_id: view.goals.last().expect("Failed Goal").id,
                run,
            },
            stopped: Some(RunStop::GoalExhausted(arany::GoalLimit::ActiveTime)),
            continuation_error: None,
            interrupted: false,
            forced: None,
            events: fixture,
            view,
        }
    }

    fn finish_budget_event(
        envelope: &mut arany::EventEnvelope,
        budget: GoalBudget,
        answer: &str,
    ) -> Option<arany::EventEnvelope> {
        let active_limit = budget.max_active_ms.expect("finite fixture budget");
        match &mut envelope.event {
            arany::Event::GoalStarted { budget: pinned, .. } => *pinned = budget,
            arany::Event::RunStarted {
                active_deadline_ms, ..
            } => {
                *active_deadline_ms = Some(active_limit);
            }
            arany::Event::GoalProviderCallReserved {
                active_ceiling_ms, ..
            } => {
                *active_ceiling_ms = (*active_ceiling_ms).min(active_limit);
            }
            arany::Event::GoalPlanContractRecorded { contract, .. } => contract.budget = budget,
            arany::Event::AgentFinished {
                disposition,
                summary,
                result,
                ..
            } => {
                *disposition = arany::AgentDisposition::Finished;
                *summary = Some(answer.into());
                *result = Some(answer.into());
            }
            arany::Event::RunFinished {
                run_id,
                disposition,
                failure,
                active_ms,
            } => {
                *disposition = arany::RunDisposition::Finished;
                *failure = None;
                *active_ms = active_limit;
                return Some(arany::EventEnvelope {
                    event: arany::Event::MessageCommitted {
                        run_id: *run_id,
                        text: answer.into(),
                    },
                    ..envelope.clone()
                });
            }
            arany::Event::GoalFinished {
                active_ms,
                budget_refusal,
                ..
            } => {
                *active_ms = active_limit;
                let refusal = budget_refusal.as_mut().expect("budget refusal");
                refusal.dimension = arany::GoalLimit::ActiveTime;
                refusal.used = active_limit;
                refusal.limit = active_limit;
                refusal.requested = None;
                refusal.required_minimum = None;
            }
            _ => {}
        }
        None
    }

    #[tokio::test]
    async fn exec_jsonl_equals_the_event_stream() {
        let _deadline = crate::cli::serve::tests::deadline();
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state_dir = temp.path().join("state");
        let engine = arany::test_support::engine::open(
            StateRoot::admit(&state_dir).expect("state"),
            EngineConfig::default(),
        )
        .expect("engine");
        let mut provider = ScriptedProvider::new(vec![ProviderOutcome::Answer(Answer {
            text: "stream answer".into(),
        })])
        .with_profile("openai", "gpt-5.4");
        provider.preview = Some(arany::PreviewText {
            answer: "provisional answer".into(),
            ..Default::default()
        });
        provider.preview_gate = Some(Arc::new(tokio::sync::Notify::new()));
        let provider = Arc::new(provider);
        let mut stream = engine.subscribe();
        let mut command = args(state_dir.clone(), workspace, "Stream the Run");
        command.output = OutputArg::Jsonl;
        let admission = command.admit().expect("valid JSONL command");
        // The stream ends when exec closes its Engine.
        let collect = async {
            let mut streamed = Vec::new();
            while let Some(tapped) = stream.next().await {
                match tapped {
                    Tapped::Committed(envelope) => streamed.push((*envelope).clone()),
                    Tapped::Resync { .. } => panic!("one Run stays within the tap ring"),
                }
            }
            streamed
        };
        let (report, streamed) = tokio::join!(
            run_on_engine(
                command,
                admission,
                AdmittedProviders::single(provider.clone()),
                engine,
                None
            ),
            collect
        );
        let report = report.expect("JSONL exec");
        assert!(report.success);
        assert!(report.stderr.is_empty());
        assert_eq!(provider.requests.lock().expect("requests").len(), 1);
        assert!(provider.responses.lock().expect("responses").is_empty());
        assert!(
            !streamed
                .iter()
                .any(|envelope| matches!(envelope.event, Event::AssistantDisplayRecorded { .. }))
        );
        let lines: Vec<serde_json::Value> = report
            .stdout
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSONL Event"))
            .collect();
        assert_eq!(
            lines.len(),
            streamed.len(),
            "each streamed Event is one line"
        );
        for (line, envelope) in lines.iter().zip(&streamed) {
            assert_eq!(line["sequence"], envelope.sequence);
            assert_eq!(line["created_at_ms"], envelope.created_at_ms);
        }
        let session_id = SessionId::from_str(lines[0]["session_id"].as_str().expect("Session ID"))
            .expect("typed Session ID");
        let store = Store::open_read_only(StateRoot::open_existing(&state_dir).expect("state"))
            .expect("read-only reopen");
        let view = store
            .load_view(session_id)
            .await
            .expect("resolved Session")
            .expect("Session exists");
        store.close().await.expect("closed store");
        assert_eq!(
            report.stdout,
            render_exec(
                view.runs.last().expect("streamed Run"),
                &streamed,
                crate::presentation::Output::Jsonl,
                true
            ),
            "exec JSONL renders exactly the committed stream"
        );
        let (expected_input, expected_usage, expected_events) = context_headroom_case(false).await;
        let (actual_input, actual_usage, actual_events) = context_headroom_case(true).await;
        assert!(
            actual_input == expected_input,
            "headless Provider inputs differ from default controls"
        );
        assert_eq!(actual_usage, expected_usage, "headless context reservation");
        assert_eq!(
            actual_events, expected_events,
            "committed headless outcomes"
        );
    }

    const CONTEXT_HEADROOM_BYTES: u32 = 1024;
    const CONTEXT_WINDOW_TOKENS: u32 = 32 * 1024;

    async fn context_headroom_case(
        server: bool,
    ) -> (arany::RunInput, arany::ContextUsage, Vec<String>) {
        use arany::test_support::engine::run_to_end;
        let temp = tempfile::tempdir().expect("context fixture");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let state = temp.path().join("state");
        let engine = arany::test_support::engine::open(
            StateRoot::admit(&state).expect("state"),
            EngineConfig::default(),
        )
        .expect("Engine");
        let prior_answer = "Prior context. ".repeat(1024);
        let provider = Arc::new(
            ScriptedProvider::new(vec![
                ProviderOutcome::Answer(Answer {
                    text: prior_answer.clone(),
                }),
                ProviderOutcome::Answer(Answer {
                    text: "Bounded answer".into(),
                }),
            ])
            .with_chatgpt_account(uuid::Uuid::now_v7(), [7; 32])
            .with_context_window(CONTEXT_WINDOW_TOKENS),
        );
        let mut input = arany::test_support::scenario::request(&workspace, "Prior question");
        input.include_paths.clear();
        input.title = None;
        let seed = run_to_end(&engine, &provider, input)
            .await
            .expect("prior Run");
        assert_eq!(
            seed.run.config.as_ref().unwrap().context_window_tokens,
            Some(CONTEXT_WINDOW_TOKENS),
            "the headroom fixture must pin an admitted context window"
        );
        let budget = seed
            .run
            .config
            .as_ref()
            .unwrap()
            .context_usage
            .as_ref()
            .unwrap()
            .budget_bytes as usize;
        const HISTORY_TURN_FRAMING_BYTES: usize = 64;
        let history_bytes =
            seed.run.objective.len() + prior_answer.len() + HISTORY_TURN_FRAMING_BYTES;
        let objective = "Q".repeat(budget - history_bytes - CONTEXT_HEADROOM_BYTES as usize);
        let mut command = args(state.clone(), workspace.clone(), &objective);
        command.provider = Some(ProviderArg::Anthropic);
        command.model = Some(provider.identity().model.clone());
        command.effort = provider.identity().effort;
        command.session_id = Some(seed.session_id.to_string());
        command.output = OutputArg::Jsonl;
        drive_context_case(server, command, engine, &provider).await;
        context_headroom_result(&state, &provider, seed).await
    }

    async fn drive_context_case(
        server: bool,
        command: ExecArgs,
        engine: Engine,
        provider: &Arc<ScriptedProvider>,
    ) {
        use arany::test_support::engine::run_to_end;
        let admission = command.admit().expect("context arguments");
        if server {
            let output = run_on_engine(
                command,
                admission,
                AdmittedProviders::single(provider.clone()),
                engine,
                None,
            )
            .await
            .expect("headless context Run");
            assert!(output.success);
            assert!(output.stderr.is_empty());
        } else {
            let mut input =
                arany::test_support::scenario::request(&admission.workspace, &admission.objective);
            input.session_id = admission.session_id;
            input.title = None;
            input.launch_directory = std::env::current_dir().ok();
            input.include_paths.clear();
            input.goal.budget = Some(GoalBudget::default());
            run_to_end(&engine, provider, input)
                .await
                .expect("default-controls context Run");
            engine.close().await.expect("closed Engine");
        }
    }

    async fn context_headroom_result(
        state: &std::path::Path,
        provider: &ScriptedProvider,
        seed: arany::RunOutcome,
    ) -> (arany::RunInput, arany::ContextUsage, Vec<String>) {
        let (events, view) = arany::test_support::goals::replayed(state, seed.session_id).await;
        let run = view.runs.last().expect("context Run");
        assert_eq!(run.status, RunStatus::Finished);
        assert_eq!(run.assistant_message.as_deref(), Some("Bounded answer"));
        assert_eq!(view.goals.last().unwrap().status, GoalStatus::Completed);
        let config = run.config.as_ref().unwrap();
        assert_eq!(config.history_run_ids, vec![seed.run.id]);
        assert_eq!(config.excluded_history_runs, 0);
        let usage = config.context_usage.clone().unwrap();
        assert_eq!(
            usage.budget_bytes - usage.used_bytes,
            CONTEXT_HEADROOM_BYTES
        );
        let requests = provider.requests.lock().expect("Provider requests");
        assert_eq!(requests.len(), 2);
        assert!(provider.responses.lock().expect("responses").is_empty());
        assert_eq!(
            requests[1].input.history,
            vec![arany::HistoryTurn {
                user: seed.run.objective,
                assistant: seed.run.assistant_message.unwrap(),
            }]
        );
        let kinds = events
            .iter()
            .filter(|event| event.sequence > seed.run.finished_sequence.unwrap())
            .map(|event| event.event.kind().to_owned())
            .collect();
        (requests[1].input.clone(), usage, kinds)
    }

    #[tokio::test]
    async fn exec_new_append_and_failed_run_reopen_exact_committed_facts() {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state_dir = temp.path().join("state");
        let observed = Arc::new(Mutex::new(Vec::new()));
        let first = args(state_dir.clone(), workspace.clone(), "/literal objective");
        let first_admission = first.admit().expect("valid first command");
        let first_report = run_with_provider(
            first,
            first_admission,
            Arc::new(scripted(
                Ok(Answer {
                    text: "first\u{1b}[31m\nSession: forged".into(),
                }),
                &observed,
            )),
            Telemetry::disabled(),
        )
        .await
        .expect("first exec");
        assert!(first_report.success);
        assert_eq!(
            first_report.stdout,
            "Answer:\n  first\\u{001b}[31m\n  Session: forged\n"
        );
        assert!(!first_report.stdout.contains('\u{1b}'));
        let receipt: Vec<_> = first_report.stderr.lines().collect();
        assert_eq!(receipt.len(), 3);
        assert_eq!(receipt[2], "Status: finished");
        let session_id = SessionId::from_str(receipt[0].strip_prefix("Session: ").unwrap())
            .expect("Session ID in receipt");
        let first_run_id =
            RunId::from_str(receipt[1].strip_prefix("Run: ").unwrap()).expect("Run ID in receipt");

        let store = Store::open_read_only(StateRoot::open_existing(&state_dir).expect("state"))
            .expect("read-only reopen");
        let first_events = store
            .load_session(session_id)
            .await
            .expect("committed Events");
        let first_view = SessionView::replay(session_id, &first_events)
            .expect("replayed Session")
            .expect("Session exists");
        assert_eq!(first_view.runs.len(), 1);
        assert_eq!(first_view.runs[0].id, first_run_id);
        assert_eq!(first_view.runs[0].status, RunStatus::Finished);
        assert_eq!(
            first_view.runs[0].assistant_message.as_deref(),
            Some("first\u{1b}[31m\nSession: forged")
        );
        store.close().await.expect("closed store");

        let mut second = args(state_dir.clone(), workspace.clone(), "@literal follow-up");
        second.session_id = Some(session_id.to_string());
        second.output = OutputArg::Jsonl;
        let second_admission = second.admit().expect("valid append");
        let second_report = run_with_provider(
            second,
            second_admission,
            Arc::new(scripted(
                Ok(Answer {
                    text: "second answer".into(),
                }),
                &observed,
            )),
            Telemetry::disabled(),
        )
        .await
        .expect("second exec");
        assert!(second_report.success);
        assert_eq!(second_report.stderr, "");
        assert!(second_report.stdout.ends_with('\n'));
        let lines: Vec<serde_json::Value> = second_report
            .stdout
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
                "GoalRunLinked",
                "MessageAccepted",
                "RunStarted",
                "AgentSpawned",
                "GoalProviderCallReserved",
                "ProviderCallRecorded",
                "AgentFinished",
                "MessageCommitted",
                "RunFinished",
                "GoalFinished",
            ]
        );
        assert_eq!(lines[2]["payload"]["text"], "@literal follow-up");
        assert_eq!(lines[6]["payload"]["phase"], "root_plan");
        assert_eq!(lines[6]["payload"]["disposition"], "finished");
        assert_eq!(lines[6]["payload"]["response_id"], "resp_test");
        assert_eq!(lines[6]["payload"]["input_tokens"], 20);
        assert_eq!(lines[6]["payload"]["output_tokens"], 10);
        assert_eq!(lines[9]["kind"], "RunFinished");
        assert_eq!(lines[10]["kind"], "GoalFinished");
        assert_eq!(
            lines[0]["payload"]["goal_id"],
            lines[1]["payload"]["goal_id"]
        );
        assert_eq!(
            lines[10]["payload"]["goal_id"],
            lines[1]["payload"]["goal_id"]
        );
        assert!(
            lines
                .iter()
                .all(|line| line["session_id"] == session_id.to_string())
        );
        assert!(
            lines
                .iter()
                .filter(|line| !line["run_id"].is_null())
                .all(|line| line["run_id"] == lines[1]["run_id"])
        );
        assert!(lines[0]["run_id"].is_null());
        assert!(lines[10]["run_id"].is_null());
        assert!(!second_report.stdout.contains("Session: forged"));

        let store = Store::open_read_only(StateRoot::open_existing(&state_dir).expect("state"))
            .expect("second read-only reopen");
        let events = store.load_session(session_id).await.expect("all Events");
        let view = store
            .load_view(session_id)
            .await
            .expect("resolved Session")
            .expect("Session exists");
        assert_eq!(view.runs.len(), 2);
        assert_eq!(view.runs[0], first_view.runs[0]);
        let run_lines: Vec<_> = lines
            .iter()
            .filter(|line| !line["run_id"].is_null())
            .collect();
        let run_events: Vec<_> = events
            .iter()
            .filter(|event| event.run_id == Some(view.runs[1].id))
            .collect();
        assert_eq!(run_lines.len(), run_events.len());
        for (line, event) in run_lines.into_iter().zip(run_events) {
            assert_eq!(line["sequence"], event.sequence);
            assert_eq!(line["created_at_ms"], event.created_at_ms);
            assert_eq!(line["kind"], event.event.kind());
        }
        store.close().await.expect("closed store");

        let mut third = args(state_dir.clone(), workspace, "!literal failed objective");
        third.session_id = Some(session_id.to_string());
        third.output = OutputArg::Jsonl;
        let third_admission = third.admit().expect("valid failed append");
        let failure = run_with_provider(
            third,
            third_admission,
            Arc::new(scripted(Err(ProviderError::Unavailable), &observed)),
            Telemetry::disabled(),
        )
        .await
        .expect("durable failed Run");
        assert!(!failure.success);
        assert_eq!(failure.stderr, "error: Run failed\n");
        let failed_lines: Vec<serde_json::Value> = failure
            .stdout
            .lines()
            .map(|line| serde_json::from_str(line).expect("failure JSONL"))
            .collect();
        assert!(
            failed_lines
                .iter()
                .any(|line| line["kind"] == "RunFinished")
        );
        assert_eq!(failed_lines[0]["kind"], "GoalStarted");
        assert_eq!(failed_lines.last().unwrap()["kind"], "GoalFinished");
        assert_eq!(failed_lines.last().unwrap()["payload"]["status"], "failed");
        assert_eq!(
            failed_lines[0]["payload"]["goal_id"],
            failed_lines.last().unwrap()["payload"]["goal_id"]
        );
        assert!(
            failed_lines
                .iter()
                .filter(|line| !line["run_id"].is_null())
                .all(|line| line["run_id"] == failed_lines[1]["run_id"])
        );
        assert!(
            !failed_lines
                .iter()
                .any(|line| line["kind"] == "MessageCommitted")
        );
        let store = Store::open_read_only(StateRoot::open_existing(&state_dir).expect("state"))
            .expect("third read-only reopen");
        let final_view = store
            .load_view(session_id)
            .await
            .expect("resolved Session")
            .expect("Session exists");
        assert_eq!(final_view.runs.len(), 3);
        assert_eq!(final_view.runs[2].status, RunStatus::Failed);
        assert_eq!(final_view.runs[2].assistant_message, None);
        assert_eq!(final_view.runs[2].agents[0].provider_calls.len(), 1);
        assert_eq!(
            final_view.runs[2].agents[0].provider_calls[0].disposition,
            arany::ProviderCallDisposition::Unavailable
        );
        assert_eq!(
            final_view.runs[2].agents[0].provider_calls[0].input_tokens,
            None
        );
        store.close().await.expect("closed store");

        let jsonl_state = temp.path().join("jsonl-state");
        let mut new_jsonl = args(
            jsonl_state.clone(),
            temp.path().join("workspace"),
            "//literal new",
        );
        new_jsonl.output = OutputArg::Jsonl;
        let jsonl_admission = new_jsonl.admit().expect("valid new JSONL command");
        let separate_observed = Arc::new(Mutex::new(Vec::new()));
        let jsonl_report = run_with_provider(
            new_jsonl,
            jsonl_admission,
            Arc::new(scripted(
                Ok(Answer {
                    text: "JSONL answer".into(),
                }),
                &separate_observed,
            )),
            Telemetry::disabled(),
        )
        .await
        .expect("new JSONL exec");
        assert!(jsonl_report.success);
        assert_eq!(jsonl_report.stderr, "");
        let new_lines: Vec<serde_json::Value> = jsonl_report
            .stdout
            .lines()
            .map(|line| serde_json::from_str(line).expect("new JSONL Event"))
            .collect();
        assert_eq!(new_lines.len(), 12);
        assert_eq!(new_lines[0]["kind"], "SessionStarted");
        assert_eq!(
            new_lines
                .iter()
                .map(|line| line["kind"].as_str().expect("Event kind"))
                .collect::<Vec<_>>(),
            vec![
                "SessionStarted",
                "GoalStarted",
                "GoalRunLinked",
                "MessageAccepted",
                "RunStarted",
                "AgentSpawned",
                "GoalProviderCallReserved",
                "ProviderCallRecorded",
                "AgentFinished",
                "MessageCommitted",
                "RunFinished",
                "GoalFinished",
            ]
        );
        assert_eq!(new_lines[3]["payload"]["text"], "//literal new");
        assert_eq!(new_lines[10]["kind"], "RunFinished");
        assert_eq!(new_lines[11]["kind"], "GoalFinished");
        assert_eq!(
            new_lines[1]["payload"]["goal_id"],
            new_lines[2]["payload"]["goal_id"]
        );
        assert_eq!(
            new_lines[11]["payload"]["goal_id"],
            new_lines[2]["payload"]["goal_id"]
        );

        let calls = observed.lock().expect("observations");
        assert_eq!(calls.len(), 3);
        let separate_calls = separate_observed.lock().expect("new observations");
        assert_eq!(separate_calls.len(), 1);
        for (call, objective, expected_history) in [
            (&calls[0], "/literal objective", Vec::new()),
            (
                &calls[1],
                "@literal follow-up",
                vec![arany::HistoryTurn {
                    user: "/literal objective".into(),
                    assistant: "first\u{1b}[31m\nSession: forged".into(),
                }],
            ),
            (
                &calls[2],
                "!literal failed objective",
                vec![
                    arany::HistoryTurn {
                        user: "/literal objective".into(),
                        assistant: "first\u{1b}[31m\nSession: forged".into(),
                    },
                    arany::HistoryTurn {
                        user: "@literal follow-up".into(),
                        assistant: "second answer".into(),
                    },
                ],
            ),
            (&separate_calls[0], "//literal new", Vec::new()),
        ] {
            assert_eq!(call.phase, AgentPhase::RootPlan, "exec call phase");
            assert!(call.model == "gpt-5.4", "exec model");
            assert!(call.input.objective == objective, "exec objective");
            assert!(call.input.images.is_empty(), "exec images");
            assert!(call.input.history == expected_history, "exec history");
            assert!(
                call.input.workspace_guidance.is_empty(),
                "exec instructions"
            );
            assert!(call.input.includes.is_empty(), "exec includes");
            assert!(call.input.context_summary.is_none(), "exec summary");
            assert!(call.input.child_results.is_empty(), "exec children");
            assert_eq!(call.max_output_tokens, 4096, "exec output cap");
        }
        for (call, run) in calls.iter().zip(&final_view.runs) {
            assert_eq!(call.run_id, run.id);
            assert_eq!(call.agent_run_id, run.agents[0].id);
        }
    }
}

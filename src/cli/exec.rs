//! Tokio registers each signal once per process and never restores the default disposition: a
//! SIGINT after the Run is absorbed, so only bounded output and shutdown remain. Signal handling
//! never writes stdout.
use super::{CommandOutput, OutputArg, chatgpt, native_run_key_from_env};
use arany::{
    AnthropicProvider, CollaborationPolicy, CustomProvider, Effort, Engine, EngineError,
    GoalBudget, GoalId, GoalPhase, GoalRequest, GoalStatus, OpenAiProvider, Provider,
    RunCancellation, RunOutcome, RunRequest, RunStatus, SessionId, StateRoot, Store, Telemetry,
    native_protection_supported, render_exec, resolve_native_effort_for_run,
    validate_native_model_id,
};
use clap::{Args, ValueEnum};
use std::{path::PathBuf, str::FromStr, time::Duration};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProviderArg {
    Openai,
    Anthropic,
    Chatgpt,
    Custom(String),
}

impl ProviderArg {
    pub(crate) fn profile_label(&self) -> String {
        match self {
            Self::Openai => "openai".into(),
            Self::Anthropic => "anthropic".into(),
            Self::Chatgpt => "chatgpt".into(),
            Self::Custom(name) => format!("custom:{name}"),
        }
    }
}

impl FromStr for ProviderArg {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "openai" => Ok(Self::Openai),
            "anthropic" => Ok(Self::Anthropic),
            "chatgpt" => Ok(Self::Chatgpt),
            _ => {
                let name = value.strip_prefix("custom:").ok_or("invalid Provider")?;
                if name.is_empty()
                    || name.len() > 64
                    || !name.starts_with(|character: char| character.is_ascii_lowercase())
                    || !name.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
                {
                    return Err("invalid custom Provider name");
                }
                Ok(Self::Custom(name.to_owned()))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum CollaborationArg {
    Single,
    Auto,
    Team,
}

#[derive(Args)]
pub(crate) struct ExecArgs {
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    #[arg(long, help = "openai, anthropic, chatgpt, or custom:NAME")]
    pub provider: ProviderArg,
    #[arg(long)]
    pub model: String,
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
        conflicts_with_all = ["prompt", "resume_goal"],
        help = "Run an autonomous Goal on this objective until it is satisfied or stops"
    )]
    pub goal: Option<String>,
    #[arg(long, value_enum, default_value_t = OutputArg::Text)]
    pub output: OutputArg,
    #[arg(long, value_enum, default_value_t = CollaborationArg::Auto)]
    pub collaboration: CollaborationArg,
    #[arg(long)]
    pub max_active_children: Option<u8>,
    #[arg(long = "include")]
    pub include_paths: Vec<PathBuf>,
    #[arg(long, help = "Enable guarded tools from private tools.json")]
    pub tools: bool,
    /// Objective of a new Run; omitted only with --resume-goal, which continues the pinned objective.
    pub prompt: Option<String>,
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
        if matches!(
            self.provider,
            ProviderArg::Openai | ProviderArg::Anthropic | ProviderArg::Chatgpt
        ) && validate_native_model_id(&self.model).is_err()
        {
            return Err("invalid native model ID");
        }
        if matches!(self.provider, ProviderArg::Chatgpt) && self.effort.is_none() {
            return Err("ChatGPT requires --effort LEVEL");
        }
        if matches!(self.provider, ProviderArg::Openai | ProviderArg::Anthropic)
            && resolve_native_effort_for_run(
                &self.provider.profile_label(),
                &self.model,
                self.effort,
            )
            .is_err()
        {
            return Err("invalid native model/effort; unknown models require --effort LEVEL");
        }
        let policy = match (self.collaboration, self.max_active_children) {
            (CollaborationArg::Single, None) => CollaborationPolicy::Single,
            (CollaborationArg::Auto, count) if count.unwrap_or(3) <= 8 => {
                CollaborationPolicy::Auto {
                    max_active_children: count.unwrap_or(3),
                }
            }
            (CollaborationArg::Team, count) if (1..=8).contains(&count.unwrap_or(3)) => {
                CollaborationPolicy::Team {
                    max_active_children: count.unwrap_or(3),
                }
            }
            _ => return Err("invalid collaboration policy"),
        };
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
        let resumes_without_text =
            self.goal.is_none() && self.prompt.is_none() && resume_goal.is_some();
        let (objective, goal) = match (&self.goal, &self.prompt, resume_goal) {
            (Some(objective), None, None) => (
                objective.clone(),
                GoalRequest::autonomous(GoalBudget::default(), GoalPhase::Implementation),
            ),
            (None, Some(prompt), None) => (prompt.clone(), GoalRequest::default()),
            (None, Some(prompt), Some(id)) => (prompt.clone(), GoalRequest::resume(id, true)),
            (None, None, Some(id)) => (String::new(), GoalRequest::resume(id, true)),
            _ => {
                return Err(
                    "pass --goal alone, a prompt alone, or --resume-goal with an optional prompt",
                );
            }
        };
        if !resumes_without_text && (objective.trim().is_empty() || objective.len() > 64 * 1024) {
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

pub(crate) async fn run(args: ExecArgs, telemetry: Telemetry) -> Result<CommandOutput, String> {
    let admission = args.admit().map_err(str::to_owned)?;
    match &args.provider {
        ProviderArg::Openai => {
            let provider = native_run_key_from_env("openai", &args.model, args.effort)
                .and_then(|key| {
                    OpenAiProvider::from_api_key_with_effort(args.model.clone(), args.effort, key)
                })
                .map_err(|_| "OPENAI_API_KEY unavailable".to_owned())?;
            run_with_provider(args, admission, provider, telemetry).await
        }
        ProviderArg::Anthropic => {
            let provider = AnthropicProvider::from_env_with_effort(args.model.clone(), args.effort)
                .map_err(|error| match error {
                    arany::ProviderError::Rejected => "invalid ANTHROPIC_WORKSPACE_ID".to_owned(),
                    _ => "ANTHROPIC_API_KEY unavailable".to_owned(),
                })?;
            run_with_provider(args, admission, provider, telemetry).await
        }
        ProviderArg::Chatgpt => {
            let provider = chatgpt::provider_for_run(
                admission.workspace.clone(),
                None,
                args.model.clone(),
                args.effort.expect("validated ChatGPT effort"),
            )
            .await
            .map_err(|error| error.to_string())?;
            run_with_provider(args, admission, provider, telemetry).await
        }
        ProviderArg::Custom(name) => {
            let root = StateRoot::open_existing(&admission.state_dir)
                .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
            let provider = CustomProvider::admit(&root, name, &args.model, args.effort)
                .await
                .map_err(|error| error.to_string())?;
            run_with_provider(args, admission, provider, telemetry).await
        }
    }
}

async fn run_with_provider<P: Provider + 'static>(
    args: ExecArgs,
    admission: Admission,
    provider: P,
    telemetry: Telemetry,
) -> Result<CommandOutput, String> {
    if provider.profile_name() != args.provider.profile_label()
        || provider.model_name() != args.model
    {
        return Err("selected Provider changed before Workspace admission".into());
    }
    let new_session = admission.session_id.is_none();
    let state = StateRoot::admit(&admission.state_dir)
        .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
    arany::enable_development_diagnostics(&state);
    let mut engine = Engine::open_with_telemetry(state, provider, telemetry)
        .map_err(|_| "state store unavailable".to_owned())?;
    if args.tools {
        if !native_protection_supported() {
            return Err(
                "Tools require native OS protection, unavailable on this host; rerun without --tools"
                    .to_owned(),
            );
        }
        engine
            .enable_tools(
                std::env::current_exe().map_err(|_| "Guard executable unavailable".to_owned())?,
            )
            .map_err(|error| error.to_string())?;
    }
    let (goal_budget, resource_settings) =
        crate::cli::attached::execution_limits(&admission.state_dir).await?;
    let mut goal = admission.goal;
    if goal.id.is_none() {
        goal.budget = Some(goal_budget);
    }
    goal.resource_settings = resource_settings;
    let end = run_signalled(
        &mut engine,
        RunRequest {
            session_id: admission.session_id,
            title: None,
            objective: admission.objective,
            images: Vec::new(),
            workspace: admission.workspace,
            include_paths: args.include_paths,
            policy: admission.policy,
            goal,
        },
    )
    .await;
    engine
        .close()
        .await
        .map_err(|_| "store shutdown failed".to_owned())?;
    let SignalledRun {
        result,
        interrupted,
        forced,
    } = end?;
    let stop = result
        .as_ref()
        .err()
        .filter(|error| matches!(error, EngineError::StoppedAfterRun { .. }))
        .map(ToString::to_string);
    let outcome = match result {
        Ok(outcome) => outcome,
        // The committed answer is still printed; the stop error makes the exit nonzero.
        Err(EngineError::StoppedAfterRun { run, .. }) => *run,
        Err(EngineError::CancelledBeforeStart) if interrupted => {
            return Err("interrupted before a Run started; no Run was recorded".into());
        }
        Err(error) => return Err(error.to_string()),
    };
    let root = StateRoot::open_existing(&admission.state_dir)
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
    let run = view
        .runs
        .last()
        .filter(|run| **run == outcome.run)
        .ok_or("Run history unavailable or invalid")?;
    let paused = interrupted
        && view.goal(outcome.goal_id).is_some_and(|goal| {
            matches!(goal.status, GoalStatus::Running | GoalStatus::Interrupted)
        });
    let success = run.status == RunStatus::Finished && !paused;
    if !matches!(
        run.status,
        RunStatus::Finished | RunStatus::Failed | RunStatus::Cancelled
    ) {
        return Err("Run did not reach a terminal state".into());
    }
    let stdout = render_exec(run, &events, args.output.into(), new_session);
    let stderr = match args.output {
        OutputArg::Text => {
            let mut receipt = format!(
                "Session: {}\nRun: {}\nStatus: {}\n{}",
                outcome.session_id,
                run.id,
                match run.status {
                    RunStatus::Finished => "finished",
                    RunStatus::Failed => "failed",
                    RunStatus::Cancelled => "cancelled",
                    _ => return Err("Run did not reach a terminal state".into()),
                },
                match run.config.as_ref() {
                    Some(config) if config.chatgpt_provenance.is_some() => {
                        "Provider: ChatGPT plan\nOutput bound: local only; remote usage is not capped\n"
                    }
                    Some(config) if config.custom_profile_provenance.is_some() => {
                        "Provider: custom verified\n"
                    }
                    _ => "",
                },
            );
            if let Some(notice) = arany::render_cache_update_feedback(run) {
                receipt.push_str(notice);
                receipt.push('\n');
            }
            receipt
        }
        OutputArg::Jsonl if success => String::new(),
        OutputArg::Jsonl => match run.status {
            RunStatus::Cancelled => "error: Run cancelled\n".into(),
            RunStatus::Failed => "error: Run failed\n".into(),
            // A finished Run that is not a success keeps its Goal paused.
            _ => "error: Goal paused before completion\n".into(),
        },
    };
    let stderr = if paused {
        format!(
            "{stderr}{}",
            resume_hint(outcome.session_id, outcome.goal_id)
        )
    } else {
        stderr
    };
    let stop = forced.map(str::to_owned).or(stop);
    let stderr = match &stop {
        Some(message) => format!("{stderr}error: {message}\n"),
        None => stderr,
    };
    Ok(CommandOutput {
        stdout,
        stderr,
        success: success && stop.is_none(),
    })
}

/// Grace for the Run's Cancelled receipt after a forced stop; matches the attached terminal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopSignal {
    Interrupt,
    Hangup,
    Terminate,
}

#[derive(Debug, PartialEq, Eq)]
enum StopStep {
    /// The first Ctrl+C requests cancellation and keeps waiting for the receipt.
    Cancel,
    /// A second Ctrl+C or a termination signal stops waiting after the grace period.
    Force(&'static str),
}

fn stop_step(cancelling: bool, signal: StopSignal) -> StopStep {
    match (signal, cancelling) {
        (StopSignal::Interrupt, false) => StopStep::Cancel,
        (StopSignal::Interrupt, true) => StopStep::Force("forced shutdown after second Ctrl+C"),
        (StopSignal::Hangup, _) => StopStep::Force("terminated by SIGHUP"),
        (StopSignal::Terminate, _) => StopStep::Force("terminated by SIGTERM"),
    }
}

/// Names the command that resumes a Goal left paused by interruption.
fn resume_hint(session_id: SessionId, goal_id: GoalId) -> String {
    format!(
        "Goal paused; resume with arany exec --session-id {session_id} --resume-goal {goal_id}\n"
    )
}

#[cfg(unix)]
struct StopSignals {
    interrupt: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl StopSignals {
    fn install() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            hangup: signal(SignalKind::hangup())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    async fn next(&mut self) -> StopSignal {
        tokio::select! {
            _ = self.interrupt.recv() => StopSignal::Interrupt,
            _ = self.hangup.recv() => StopSignal::Hangup,
            _ = self.terminate.recv() => StopSignal::Terminate,
        }
    }
}

#[cfg(not(unix))]
struct StopSignals;

#[cfg(not(unix))]
impl StopSignals {
    fn install() -> std::io::Result<Self> {
        Ok(Self)
    }

    async fn next(&mut self) -> StopSignal {
        std::future::pending().await
    }
}

struct SignalledRun {
    result: Result<RunOutcome, EngineError>,
    /// A stop signal requested cancellation before the Run's receipt.
    interrupted: bool,
    /// Set when a forced stop produced the receipt within the grace period.
    forced: Option<&'static str>,
}

/// Drives one Run with stop signals installed only while it is in flight.
async fn run_signalled(engine: &mut Engine, request: RunRequest) -> Result<SignalledRun, String> {
    let cancellation = RunCancellation::new();
    let mut signals =
        StopSignals::install().map_err(|_| "signal handling unavailable".to_owned())?;
    let run = engine.run_with_cancel(request, cancellation.clone());
    tokio::pin!(run);
    let mut interrupted = false;
    let forced = loop {
        tokio::select! {
            result = &mut run => {
                return Ok(SignalledRun { result, interrupted, forced: None });
            }
            signal = signals.next() => match stop_step(interrupted, signal) {
                StopStep::Cancel => {
                    interrupted = true;
                    cancellation.cancel();
                }
                StopStep::Force(reason) => {
                    cancellation.cancel();
                    break reason;
                }
            },
        }
    };
    match tokio::time::timeout(SHUTDOWN_GRACE, &mut run).await {
        Ok(result) => Ok(SignalledRun {
            result,
            interrupted: true,
            forced: Some(forced),
        }),
        Err(_) => Err(format!(
            "{forced}; Run may be interrupted; check Session history"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arany::{
        AgentPhase, Finish, ProviderError, ProviderOutcome, ProviderRequest, ProviderResponse,
        RunId, SessionView,
    };
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    struct Scripted {
        answers: Arc<Mutex<VecDeque<Result<Finish, ProviderError>>>>,
        observed: Arc<Mutex<Vec<ProviderRequest>>>,
    }

    #[async_trait::async_trait]
    impl Provider for Scripted {
        fn profile_name(&self) -> &str {
            "openai"
        }

        fn model_name(&self) -> &str {
            "gpt-5.4"
        }

        fn max_concurrent_calls(&self) -> u8 {
            1
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
                outcome: ProviderOutcome::Finish(answer),
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
            provider: ProviderArg::Openai,
            model: "gpt-5.4".into(),
            effort: None,
            session_id: None,
            resume_goal: None,
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
    fn stop_signals_cancel_once_then_force() {
        assert_eq!(stop_step(false, StopSignal::Interrupt), StopStep::Cancel);
        assert_eq!(
            stop_step(true, StopSignal::Interrupt),
            StopStep::Force("forced shutdown after second Ctrl+C")
        );
        assert_eq!(
            stop_step(false, StopSignal::Terminate),
            StopStep::Force("terminated by SIGTERM")
        );
        assert_eq!(
            stop_step(true, StopSignal::Hangup),
            StopStep::Force("terminated by SIGHUP")
        );
    }

    #[test]
    fn resume_hint_names_the_session_and_goal() {
        let session = SessionId::new();
        let goal = GoalId::new();
        assert_eq!(
            resume_hint(session, goal),
            format!(
                "Goal paused; resume with arany exec --session-id {session} --resume-goal {goal}\n"
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
            Some("pass --goal alone, a prompt alone, or --resume-goal with an optional prompt")
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
        answer: Result<Finish, ProviderError>,
        observed: &Arc<Mutex<Vec<ProviderRequest>>>,
    ) -> Scripted {
        Scripted {
            answers: Arc::new(Mutex::new(VecDeque::from([answer]))),
            observed: Arc::clone(observed),
        }
    }

    #[tokio::test]
    async fn exec_resume_of_an_exhausted_goal_closes_it_and_names_the_reason() {
        let temp = tempfile::tempdir().expect("private test root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state_dir = temp.path().join("state");
        let observed = Arc::new(Mutex::new(Vec::new()));
        let mut engine = Engine::open(
            StateRoot::admit(&state_dir).expect("state"),
            scripted(
                Ok(Finish {
                    summary: "first summary".into(),
                    result: "first".into(),
                }),
                &observed,
            ),
        )
        .expect("engine");
        let mut goal = GoalRequest::new(
            GoalBudget {
                max_provider_calls: Some(1),
                ..GoalBudget::default()
            },
            GoalPhase::Implementation,
        );
        goal.finish_after_run = false;
        let paused = engine
            .run(RunRequest {
                session_id: None,
                title: Some("Exhausted".into()),
                objective: "Use the only Provider call".into(),
                images: Vec::new(),
                workspace: workspace.clone(),
                include_paths: Vec::new(),
                policy: CollaborationPolicy::Single,
                goal,
            })
            .await
            .expect("paused Goal Run");
        engine.close().await.expect("engine shutdown");

        let mut resume = args(state_dir.clone(), workspace.clone(), "unused");
        resume.prompt = None;
        resume.session_id = Some(paused.session_id.to_string());
        resume.resume_goal = Some(paused.goal_id.to_string());
        let admission = resume.admit().expect("valid resume");
        let error = run_with_provider(
            resume,
            admission,
            scripted(Err(ProviderError::Unavailable), &observed),
            Telemetry::disabled(),
        )
        .await
        .err()
        .expect("an exhausted resume exits unsuccessfully with its reason");
        assert_eq!(error, "Goal budget is exhausted");
        assert_eq!(observed.lock().expect("observations").len(), 1);

        let mut again = args(state_dir.clone(), workspace.clone(), "unused");
        again.prompt = None;
        again.session_id = Some(paused.session_id.to_string());
        again.resume_goal = Some(paused.goal_id.to_string());
        let admission = again.admit().expect("valid resume");
        let error = run_with_provider(
            again,
            admission,
            scripted(Err(ProviderError::Unavailable), &observed),
            Telemetry::disabled(),
        )
        .await
        .err()
        .expect("a closed Goal is not resumable");
        assert_eq!(error, "Goal is not available for explicit resume");
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
            scripted(
                Ok(Finish {
                    summary: "first summary".into(),
                    result: "first\u{1b}[31m\nSession: forged".into(),
                }),
                &observed,
            ),
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
            scripted(
                Ok(Finish {
                    summary: "second summary".into(),
                    result: "second answer".into(),
                }),
                &observed,
            ),
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
                "GoalActiveTimeReserved",
                "GoalProviderCallReserved",
                "ProviderCallRecorded",
                "GoalActiveTimeSettled",
                "AgentFinished",
                "MessageCommitted",
                "RunFinished",
                "GoalFinished",
            ]
        );
        assert_eq!(lines[2]["payload"]["text"], "@literal follow-up");
        assert_eq!(lines[7]["payload"]["phase"], "root_plan");
        assert_eq!(lines[7]["payload"]["disposition"], "finished");
        assert_eq!(lines[7]["payload"]["response_id"], "resp_test");
        assert_eq!(lines[7]["payload"]["input_tokens"], 20);
        assert_eq!(lines[7]["payload"]["output_tokens"], 10);
        assert_eq!(lines[11]["kind"], "RunFinished");
        assert_eq!(lines[12]["kind"], "GoalFinished");
        assert_eq!(
            lines[0]["payload"]["goal_id"],
            lines[1]["payload"]["goal_id"]
        );
        assert_eq!(
            lines[12]["payload"]["goal_id"],
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
        assert!(lines[12]["run_id"].is_null());
        assert!(!second_report.stdout.contains("first summary"));

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
            assert_eq!(line["event_version"], 6);
        }
        store.close().await.expect("closed store");

        let mut third = args(state_dir.clone(), workspace, "!literal failed objective");
        third.session_id = Some(session_id.to_string());
        third.output = OutputArg::Jsonl;
        let third_admission = third.admit().expect("valid failed append");
        let failure = run_with_provider(
            third,
            third_admission,
            scripted(Err(ProviderError::Unavailable), &observed),
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
            scripted(
                Ok(Finish {
                    summary: "JSONL summary".into(),
                    result: "JSONL answer".into(),
                }),
                &separate_observed,
            ),
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
        assert_eq!(new_lines.len(), 14);
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
                "GoalActiveTimeReserved",
                "GoalProviderCallReserved",
                "ProviderCallRecorded",
                "GoalActiveTimeSettled",
                "AgentFinished",
                "MessageCommitted",
                "RunFinished",
                "GoalFinished",
            ]
        );
        assert_eq!(new_lines[3]["payload"]["text"], "//literal new");
        assert_eq!(new_lines[12]["kind"], "RunFinished");
        assert_eq!(new_lines[13]["kind"], "GoalFinished");
        assert_eq!(
            new_lines[1]["payload"]["goal_id"],
            new_lines[2]["payload"]["goal_id"]
        );
        assert_eq!(
            new_lines[13]["payload"]["goal_id"],
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
            assert!(call.input.workspace_guidance.is_none(), "exec instructions");
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

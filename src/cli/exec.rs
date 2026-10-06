use super::{CommandOutput, OutputArg, chatgpt, native_run_key_from_env};
use arany::{
    AnthropicProvider, CollaborationPolicy, CustomProvider, Effort, Engine, OpenAiProvider,
    Provider, RunRequest, RunStatus, SessionId, StateRoot, Store, Telemetry, render_exec,
    resolve_native_effort_for_run, validate_native_model_id,
};
use clap::{Args, ValueEnum};
use std::{path::PathBuf, str::FromStr};

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
    pub prompt: String,
}

struct Admission {
    state_dir: PathBuf,
    workspace: PathBuf,
    session_id: Option<SessionId>,
    policy: CollaborationPolicy,
}

impl ExecArgs {
    fn admit(&self) -> Result<Admission, &'static str> {
        if self.prompt.trim().is_empty() || self.prompt.len() > 8 * 1024 {
            return Err("invalid objective");
        }
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
        let (workspace, state_dir) =
            super::admit_workspace(self.workspace.clone(), self.state_dir.clone())?;
        Ok(Admission {
            state_dir,
            workspace,
            session_id,
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
        engine
            .enable_tools(
                std::env::current_exe().map_err(|_| "Guard executable unavailable".to_owned())?,
            )
            .map_err(|error| error.to_string())?;
    }
    let result = engine
        .run(RunRequest {
            session_id: admission.session_id,
            title: None,
            objective: args.prompt,
            images: Vec::new(),
            workspace: admission.workspace,
            include_paths: args.include_paths,
            policy: admission.policy,
        })
        .await;
    engine
        .close()
        .await
        .map_err(|_| "store shutdown failed".to_owned())?;
    let outcome = result.map_err(|error| error.to_string())?;
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
    let success = run.status == RunStatus::Finished;
    if !matches!(
        run.status,
        RunStatus::Finished | RunStatus::Failed | RunStatus::Cancelled
    ) {
        return Err("Run did not reach a terminal state".into());
    }
    let stdout = render_exec(run, &events, args.output.into(), new_session);
    let stderr = match args.output {
        OutputArg::Text => format!(
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
        ),
        OutputArg::Jsonl if success => String::new(),
        OutputArg::Jsonl => "error: Run failed\n".into(),
    };
    Ok(CommandOutput {
        stdout,
        stderr,
        success,
    })
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
                outcome: ProviderOutcome::Finish(answer),
                response_id: Some("resp_test".into()),
                input_tokens: Some(20),
                output_tokens: Some(10),
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
            output: OutputArg::Text,
            collaboration: CollaborationArg::Single,
            max_active_children: None,
            include_paths: Vec::new(),
            tools: false,
            prompt: prompt.into(),
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
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[0]["kind"], "MessageAccepted");
        assert_eq!(lines[0]["payload"]["text"], "@literal follow-up");
        assert_eq!(lines[3]["kind"], "ProviderCallRecorded");
        assert_eq!(lines[3]["payload"]["phase"], "root_plan");
        assert_eq!(lines[3]["payload"]["disposition"], "finished");
        assert_eq!(lines[3]["payload"]["response_id"], "resp_test");
        assert_eq!(lines[3]["payload"]["input_tokens"], 20);
        assert_eq!(lines[3]["payload"]["output_tokens"], 10);
        assert_eq!(lines[6]["kind"], "RunFinished");
        assert!(
            lines
                .iter()
                .all(|line| line["session_id"] == session_id.to_string())
        );
        assert!(
            lines
                .iter()
                .all(|line| line["run_id"] == lines[0]["run_id"])
        );
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
        assert_eq!(
            lines.len(),
            events
                .iter()
                .filter(|event| event.run_id == Some(view.runs[1].id))
                .count()
        );
        for (line, event) in lines.iter().zip(
            events
                .iter()
                .filter(|event| event.run_id == Some(view.runs[1].id)),
        ) {
            assert_eq!(line["sequence"], event.sequence);
            assert_eq!(line["created_at_ms"], event.created_at_ms);
            assert_eq!(line["event_version"], 1);
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
        assert_eq!(new_lines.len(), 8);
        assert_eq!(new_lines[0]["kind"], "SessionStarted");
        assert_eq!(new_lines[1]["kind"], "MessageAccepted");
        assert_eq!(new_lines[1]["payload"]["text"], "//literal new");
        assert_eq!(new_lines[7]["kind"], "RunFinished");

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
            assert!(call.objective == objective, "exec objective");
            assert!(call.images.is_empty(), "exec images");
            assert!(call.history == expected_history, "exec history");
            assert!(call.instructions.is_none(), "exec instructions");
            assert!(call.includes.is_empty(), "exec includes");
            assert!(call.context_summary.is_none(), "exec summary");
            assert!(call.child_results.is_empty(), "exec children");
            assert_eq!(call.max_output_tokens, 4096, "exec output cap");
        }
        for (call, run) in calls.iter().zip(&final_view.runs) {
            assert_eq!(call.run_id, run.id);
            assert_eq!(call.agent_run_id, run.agents[0].id);
        }
    }
}

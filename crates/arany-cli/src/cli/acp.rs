use super::{exec::CollaborationArg, provider::select::ProviderArg, serve};
use arany::{Effort, Engine, EngineConfig, StateRoot, Telemetry};
use arany_cli::protocol::{methods::*, rpc::*};
use clap::Args;
use serde_json::Value;
use std::{
    io::IsTerminal,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncRead, BufReader},
    sync::{mpsc, watch},
};

mod translate;
mod wire;

#[derive(Args)]
pub(crate) struct AcpArgs {
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
    #[arg(long, hide = true)]
    pub tools: bool,
    #[arg(long, required_unless_present = "profile")]
    pub provider: Option<ProviderArg>,
    #[arg(long, required_unless_present = "profile")]
    pub model: Option<String>,
    #[arg(long)]
    pub effort: Option<Effort>,
    #[arg(long)]
    pub profile: Option<String>,
    #[arg(long, value_enum, default_value_t = CollaborationArg::Auto)]
    pub collaboration: CollaborationArg,
    #[arg(long)]
    pub max_active_children: Option<u8>,
}

pub(crate) fn admit(args: &AcpArgs) -> Result<(), &'static str> {
    if std::io::stdin().is_terminal() || std::io::stdout().is_terminal() {
        return Err("acp requires piped stdin and stdout");
    }
    if args.tools {
        return Err("tools are unavailable in the ACP core text adapter");
    }
    if !matches!(
        (args.collaboration, args.max_active_children),
        (CollaborationArg::Single, None)
            | (CollaborationArg::Auto, None | Some(0..=8))
            | (CollaborationArg::Team, None | Some(1..=8))
    ) {
        return Err("invalid collaboration policy");
    }
    if args.profile.is_none()
        && let Some(provider) = &args.provider
    {
        let model = args.model.as_deref().unwrap_or_default();
        validate_tuple(provider, model, args.effort)?;
    }
    Ok(())
}

fn validate_tuple(
    provider: &ProviderArg,
    model: &str,
    effort: Option<Effort>,
) -> Result<(), &'static str> {
    if model.is_empty() || model.len() > 128 || model.chars().any(char::is_control) {
        return Err("invalid model");
    }
    match provider {
        ProviderArg::Openai | ProviderArg::Anthropic => {
            arany::resolve_native_effort_for_run(&provider.profile_label(), model, effort)
                .map_err(
                    |_| "invalid native model/effort; unknown models require --effort LEVEL",
                )?;
        }
        ProviderArg::Chatgpt => {
            arany::validate_native_model_id(model).map_err(|_| "invalid native model ID")?;
            if effort.is_none() {
                return Err("ChatGPT requires --effort LEVEL");
            }
        }
        ProviderArg::Custom(_) => (),
    }
    Ok(())
}

struct Config {
    state: PathBuf,
    workspace: PathBuf,
    provider: ProviderArg,
    model: String,
    effort: Option<Effort>,
    collaboration: Collaboration,
    max_active_children: Option<u8>,
    roster: Option<super::provider::roster::SelectedRoster>,
    #[cfg(test)]
    authenticated: Option<bool>,
}

impl Config {
    fn thread(&self, cwd: String) -> ThreadStartParams {
        ThreadStartParams {
            cwd,
            provider: self.provider.profile_label(),
            model: self.model.clone(),
            effort: self.effort,
            title: None,
            collaboration: Some(self.collaboration.clone()),
            max_active_children: self.max_active_children,
            tools: Some(false),
            permission_mode: None,
            include_paths: None,
        }
    }

    async fn authenticate(&self) -> bool {
        #[cfg(test)]
        if let Some(authenticated) = self.authenticated {
            return authenticated;
        }
        if let Some(roster) = &self.roster {
            roster.admit(&self.state, &self.workspace).await.is_ok()
        } else {
            super::provider::select::select_env_provider(
                &self.provider,
                &self.model,
                self.effort,
                self.workspace.clone(),
                &self.state,
            )
            .await
            .is_ok()
        }
    }
}

pub(crate) async fn run(
    args: AcpArgs,
    telemetry: Telemetry,
) -> Result<super::CommandOutput, String> {
    let state_dir = super::state_dir(args.state_dir).map_err(str::to_owned)?;
    let state =
        StateRoot::admit(&state_dir).map_err(|_| "state directory unavailable or unsafe")?;
    let workspace = std::env::current_dir().map_err(|_| "Workspace unavailable")?;
    let roster = args
        .profile
        .as_deref()
        .map(|name| super::provider::roster::load(&state_dir, &workspace, name))
        .transpose()?;
    let (provider, model, effort) = if let Some(roster) = &roster {
        roster.matches(args.provider.as_ref(), args.model.as_deref(), args.effort)?;
        let primary = roster.primary();
        use std::str::FromStr;
        (
            ProviderArg::from_str(&primary.provider).map_err(str::to_owned)?,
            primary.model.clone(),
            primary.effort,
        )
    } else {
        (
            args.provider.ok_or("--provider is required")?,
            args.model.ok_or("--model is required")?,
            args.effort,
        )
    };
    validate_tuple(&provider, &model, effort)?;
    if let Some(roster) = &roster {
        use std::io::Write;
        std::io::stderr()
            .write_all(roster.manifest()?.as_bytes())
            .map_err(|_| "roster manifest output failed")?;
    }
    let config = Config {
        state: state_dir.clone(),
        workspace,
        provider,
        model,
        effort,
        collaboration: match args.collaboration {
            CollaborationArg::Single => Collaboration::Single,
            CollaborationArg::Auto => Collaboration::Auto,
            CollaborationArg::Team => Collaboration::Team,
        },
        max_active_children: args.max_active_children,
        roster: roster.clone(),
        #[cfg(test)]
        authenticated: None,
    };
    arany::enable_development_diagnostics(&state);
    let mut signals =
        super::exec::StopSignals::install().map_err(|_| "signal handling unavailable")?;
    let engine = Engine::open(
        state,
        EngineConfig {
            telemetry,
            guard_executable: std::env::current_exe().ok(),
        },
    )
    .map_err(|_| "state store unavailable")?;
    let (inbound, receiver) = mpsc::channel(serve::INBOUND_CAPACITY);
    let (events, event_rx) = mpsc::channel(serve::OUTBOUND_CAPACITY);
    let (outbound, writer) = mpsc::channel(serve::OUTBOUND_CAPACITY);
    let shared = Arc::new(Mutex::new(translate::State::default()));
    let input = std::fs::File::from(
        std::os::fd::AsFd::as_fd(&std::io::stdin())
            .try_clone_to_owned()
            .map_err(|_| "protocol input unavailable")?,
    );
    let output = std::fs::File::from(
        std::os::fd::AsFd::as_fd(&std::io::stdout())
            .try_clone_to_owned()
            .map_err(|_| "protocol output unavailable")?,
    );
    let (shutdown, _) = watch::channel(false);
    let reader_task = tokio::spawn(read_messages(
        tokio::fs::File::from_std(input),
        config,
        shared.clone(),
        inbound.clone(),
        outbound.clone(),
        shutdown.clone(),
    ));
    let writer_task = tokio::spawn(serve::write_messages(
        tokio::fs::File::from_std(output),
        writer,
    ));
    let stop_state = shared.clone();
    let translation_input = inbound.clone();
    let translation_output = outbound.clone();
    let stopping = shutdown.clone();
    let serving = async move {
        let server = serve::server::run_configured(
            engine,
            state_dir,
            receiver,
            events,
            stopping.clone(),
            roster,
        );
        let translator = translate::output(
            event_rx,
            translation_input,
            translation_output,
            shared,
            stopping.subscribe(),
        );
        let (server, translator) = tokio::join!(server, translator);
        server.and(translator)
    };
    serve::serve_transport(
        serving,
        async {
            signals.next().await;
            translate::cancel_active(&stop_state);
        },
        inbound,
        outbound,
        reader_task,
        writer_task,
        shutdown,
    )
    .await?;
    Ok(super::CommandOutput {
        stdout: String::new(),
        stderr: String::new(),
        success: true,
    })
}

async fn read_messages(
    input: impl AsyncRead + Unpin,
    config: Config,
    shared: translate::Shared,
    inbound: mpsc::Sender<serve::server::ClientMessage>,
    outbound: mpsc::Sender<Value>,
    shutdown: watch::Sender<bool>,
) -> Result<(), String> {
    let mut input = BufReader::with_capacity(64 * 1024, input);
    let result = async {
        while let Some(line) = serve::read_bounded_line(&mut input)
            .await
            .map_err(|_| "protocol input failed")?
        {
            match line.and_then(|line| wire::decode(&line)) {
                Ok(message) => {
                    translate::input(message, &config, &shared, &inbound, &outbound).await?
                }
                Err(error) => {
                    let large = error == CodecError::TooLarge;
                    translate::immediate(
                        &outbound,
                        wire::error(
                            None,
                            if large { -32600 } else { -32700 },
                            &error.to_string(),
                            if large {
                                "messageTooLarge"
                            } else {
                                "parseError"
                            },
                        ),
                    )?;
                }
            }
        }
        Ok(())
    }
    .await;
    translate::cancel_active(&shared);
    shutdown.send_replace(true);
    let _ = inbound.send(serve::server::ClientMessage::Eof).await;
    result
}

#[cfg(test)]
#[path = "../../tests/acp/in_process.rs"]
mod tests;

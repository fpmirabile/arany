use super::{
    exec::CollaborationArg,
    provider::select::{ProviderArg, validate_headless_selection},
    serve,
};
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

mod tools;
mod translate;
mod wire;

#[derive(Args)]
pub(crate) struct AcpArgs {
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
    #[arg(long, required_unless_present = "profile")]
    pub provider: Option<ProviderArg>,
    #[arg(long, required_unless_present = "profile")]
    pub model: Option<String>,
    #[arg(long)]
    pub effort: Option<Effort>,
    #[arg(long)]
    pub profile: Option<String>,
    #[arg(long, value_enum, default_value_t = CollaborationArg::Single)]
    pub collaboration: CollaborationArg,
    #[arg(long)]
    pub max_active_children: Option<u8>,
    #[arg(long)]
    pub tools: bool,
    #[arg(long, default_value = "default", value_parser = parse_permission_mode)]
    pub permission_mode: PermissionMode,
}

fn parse_permission_mode(value: &str) -> Result<PermissionMode, &'static str> {
    serde_json::from_value(Value::String(value.into())).map_err(|_| "unsupported permission mode")
}

pub(crate) fn admit(args: &AcpArgs) -> Result<(), &'static str> {
    if std::io::stdin().is_terminal() || std::io::stdout().is_terminal() {
        return Err("acp requires piped stdin and stdout");
    }
    args.collaboration.policy(args.max_active_children)?;
    if args.profile.is_none()
        && let Some(provider) = &args.provider
    {
        let model = args.model.as_deref().unwrap_or_default();
        validate_headless_selection(provider, model, args.effort)?;
    }
    Ok(())
}

struct Config {
    state: PathBuf,
    provider: ProviderArg,
    model: String,
    effort: Option<Effort>,
    collaboration: Collaboration,
    max_active_children: Option<u8>,
    roster: Option<super::provider::roster::SelectedRoster>,
    tools: bool,
    permission_mode: PermissionMode,
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
            tools: Some(self.tools),
            permission_mode: Some(self.permission_mode.clone()),
            include_paths: None,
        }
    }

    async fn authenticate(&self) -> bool {
        #[cfg(test)]
        if let Some(authenticated) = self.authenticated {
            return authenticated;
        }
        if let Some(roster) = &self.roster {
            roster.authenticate(&self.state).await
        } else {
            super::provider::select::authenticate(
                &self.state,
                &self.provider.profile_label(),
                &self.model,
                self.effort,
                None,
            )
            .await
        }
    }
}

pub(crate) async fn run(
    args: AcpArgs,
    telemetry: Telemetry,
) -> Result<super::CommandOutput, String> {
    let state_dir = super::state_dir(args.state_dir.clone()).map_err(str::to_owned)?;
    super::admit_headless_records(&state_dir)?;
    let state =
        StateRoot::admit(&state_dir).map_err(|_| "state directory unavailable or unsafe")?;
    let config = acp_config(args, &state_dir)?;
    super::admit_headless_history(&state)?;
    arany::enable_development_diagnostics(&state);
    let signals =
        super::run_outcome::StopSignals::install().map_err(|_| "signal handling unavailable")?;
    let engine = Engine::open(
        state,
        EngineConfig {
            worker_concurrency: None,
            telemetry,
            guard_executable: std::env::current_exe().ok(),
        },
    )
    .map_err(|_| "state store unavailable")?;
    serve_acp(engine, state_dir, config, signals).await?;
    Ok(super::CommandOutput {
        stdout: String::new(),
        stderr: String::new(),
        success: true,
    })
}

fn acp_config(args: AcpArgs, state_dir: &std::path::Path) -> Result<Config, String> {
    let roster = args
        .profile
        .as_deref()
        .map(|name| super::provider::roster::load_without_workspace(state_dir, name))
        .transpose()?;
    let (provider, model, effort) =
        acp_selection(roster.as_ref(), args.provider, args.model, args.effort)?;
    validate_headless_selection(&provider, &model, effort)?;
    if let Some(roster) = &roster {
        use std::io::Write;
        std::io::stderr()
            .write_all(roster.manifest()?.as_bytes())
            .map_err(|_| "roster manifest output failed")?;
    }
    if let Ok(workspace) = std::env::current_dir() {
        let (profile, account_id) = match &roster {
            Some(roster) => (
                roster.primary().provider.profile(),
                roster.primary().account_id,
            ),
            None => (provider.profile_label(), None),
        };
        super::attached::models::refresh_at_startup(workspace, profile, account_id);
    }
    Ok(Config {
        state: state_dir.to_path_buf(),
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
        tools: args.tools,
        permission_mode: args.permission_mode,
        #[cfg(test)]
        authenticated: None,
    })
}

async fn serve_acp(
    engine: Engine,
    state_dir: PathBuf,
    config: Config,
    mut signals: super::run_outcome::StopSignals,
) -> Result<(), String> {
    let roster = config.roster.clone();
    let (inbound, receiver) = mpsc::channel(serve::INBOUND_CAPACITY);
    let (events, event_rx) = mpsc::channel(serve::OUTBOUND_CAPACITY);
    let (outbound, writer) = mpsc::channel(serve::OUTBOUND_CAPACITY);
    let shared = Arc::new(Mutex::new(translate::State::default()));
    let (input, output) = serve::protocol_stdio()?;
    let (shutdown, _) = watch::channel(false);
    let reader_task = tokio::spawn(read_messages(
        tokio::fs::File::from_std(input),
        AcpReader {
            config,
            shared: shared.clone(),
            inbound: inbound.clone(),
            outbound: outbound.clone(),
            shutdown: shutdown.clone(),
        },
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
        let server = run_server(
            engine,
            state_dir,
            serve::server::ServerIo {
                inbound: receiver,
                outbound: events,
                stopping: stopping.clone(),
            },
            serve::threads::Threads::with_roster(roster),
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
        serve::Transport {
            inbound,
            outbound,
            reader: reader_task,
            writer: writer_task,
            shutdown,
        },
    )
    .await
}

// A roster's primary route wins over flags, which then must match it; otherwise the flags are required.
fn acp_selection(
    roster: Option<&super::provider::roster::SelectedRoster>,
    provider: Option<ProviderArg>,
    model: Option<String>,
    effort: Option<arany::Effort>,
) -> Result<(ProviderArg, String, Option<arany::Effort>), String> {
    if let Some(roster) = roster {
        roster.matches(provider.as_ref(), model.as_deref(), effort)?;
        let primary = roster.primary();
        use std::str::FromStr;
        return Ok((
            ProviderArg::from_str(&primary.provider.profile()).map_err(str::to_owned)?,
            primary.model.clone(),
            primary.effort,
        ));
    }
    Ok((
        provider.ok_or("--provider is required")?,
        model.ok_or("--model is required")?,
        effort,
    ))
}

async fn run_server(
    engine: Engine,
    state: PathBuf,
    io: serve::server::ServerIo,
    mut threads: serve::threads::Threads,
) -> Result<(), String> {
    threads.approval_channel = serve::server::ApprovalChannel::Restricted;
    serve::server::run_configured(engine, state, io, threads).await
}

struct AcpReader {
    config: Config,
    shared: translate::Shared,
    inbound: mpsc::Sender<serve::server::ClientMessage>,
    outbound: mpsc::Sender<Value>,
    shutdown: watch::Sender<bool>,
}

async fn read_messages(input: impl AsyncRead + Unpin, reader: AcpReader) -> Result<(), String> {
    let AcpReader {
        config,
        shared,
        inbound,
        outbound,
        shutdown,
    } = reader;
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

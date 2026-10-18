#![forbid(unsafe_code)]

mod accounts;
mod cli;
use arany_cli::{presentation, terminal};

#[cfg(all(test, unix))]
use arany::test_support::process as test_process;

use crate::presentation::render_session;
use arany::{RunId, SessionId, StateRoot, Store, Telemetry};
use clap::{ColorChoice, Parser, Subcommand, error::ErrorKind};
use cli::{
    CommandOutput, OutputArg, attached::AttachedArgs, exec::ExecArgs, provider::ProviderArgs,
};
use std::{io::Write, path::PathBuf, process::ExitCode, str::FromStr};

#[derive(Parser)]
#[command(name = "arany", version, about = "A durable CLI agent harness", color = ColorChoice::Never)]
struct Cli {
    #[arg(long, global = true)]
    otlp_endpoint: Option<String>,
    #[command(flatten)]
    attached: AttachedArgs,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Exec(ExecArgs),
    Serve(cli::serve::ServeArgs),
    Acp(cli::acp::AcpArgs),
    Provider(ProviderArgs),
    GenerateSchema {
        #[arg(long)]
        out: Option<PathBuf>,
    },
    Show {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        output: OutputArg,
        #[arg(value_name = "SESSION_OR_RUN_ID")]
        session_id: String,
    },
}

fn main() -> ExitCode {
    #[cfg(target_os = "macos")]
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new("--internal-native-tool-unit"))
    {
        return arany::tool_guard_unit_main();
    }
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--internal-tool-guard"))
    {
        return arany::tool_guard_main();
    }
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new("--internal-credential-helper"))
    {
        return accounts::credentials::keyring_helper_main();
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            return if std::io::stdout()
                .write_all(error.to_string().as_bytes())
                .is_ok()
            {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        Err(_) => {
            report_error("invalid arguments; run arany --help");
            return ExitCode::FAILURE;
        }
    };
    if let Some(Command::Serve(args)) = &cli.command
        && let Err(error) = cli::serve::admit(args)
    {
        report_error(error);
        return ExitCode::from(2);
    }
    if let Some(Command::Acp(args)) = &cli.command
        && let Err(error) = cli::acp::admit(args)
    {
        report_error(error);
        return ExitCode::from(2);
    }
    let telemetry = match Telemetry::from_process(cli.otlp_endpoint.as_deref()) {
        Ok(telemetry) => telemetry,
        Err(error) => {
            report_error(error);
            return ExitCode::from(2);
        }
    };
    let result = run_cli(cli, telemetry.clone());
    telemetry.shutdown();
    result
}

fn run_cli(cli: Cli, telemetry: Telemetry) -> ExitCode {
    let runtime = match build_runtime() {
        Ok(runtime) => runtime,
        Err(_) => {
            report_error("runtime initialization failed");
            return ExitCode::FAILURE;
        }
    };
    if cli.command.is_none() {
        return match runtime.block_on(cli::attached::run(cli.attached, telemetry)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                arany::record_development_failure(arany::DevelopmentFailure::AttachedExit);
                report_error(error);
                ExitCode::FAILURE
            }
        };
    }
    if !cli.attached.is_empty() {
        report_error("attached options cannot be used with a subcommand");
        return ExitCode::FAILURE;
    }
    let serving = matches!(cli.command, Some(Command::Serve(_) | Command::Acp(_)));
    let result = runtime.block_on(run(cli.command.expect("checked subcommand"), telemetry));
    if serving {
        // A cancelled stdio read may still occupy Tokio's blocking pool.
        runtime.shutdown_timeout(std::time::Duration::ZERO);
    }
    match result {
        Ok(output) => {
            if std::io::stdout()
                .write_all(output.stdout.as_bytes())
                .is_err()
                || std::io::stderr()
                    .write_all(output.stderr.as_bytes())
                    .is_err()
            {
                arany::record_development_failure(arany::DevelopmentFailure::Output);
                report_error("output failed");
                ExitCode::FAILURE
            } else if output.success {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            arany::record_development_failure(arany::DevelopmentFailure::ExecExit);
            report_error(error);
            ExitCode::FAILURE
        }
    }
}

fn report_error(message: impl std::fmt::Display) {
    let _ = writeln!(std::io::stderr().lock(), "error: {message}");
}

fn build_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

async fn run(command: Command, telemetry: Telemetry) -> Result<CommandOutput, String> {
    match command {
        Command::Acp(args) => cli::acp::run(args, telemetry).await,
        Command::Serve(args) => cli::serve::run(args, telemetry).await,
        Command::Exec(args) => cli::exec::run(args, telemetry).await,
        Command::Provider(args) => cli::provider::run(args).await,
        Command::GenerateSchema { out } => cli::generate_schema::run(out.as_deref()),
        Command::Show {
            state_dir,
            output,
            session_id,
        } => {
            let stdout = show(state_dir, output, &session_id)
                .await
                .map_err(str::to_owned)?;
            Ok(CommandOutput {
                stdout,
                stderr: String::new(),
                success: true,
            })
        }
    }
}

async fn show(
    state_dir: Option<PathBuf>,
    output: OutputArg,
    session_id: &str,
) -> Result<String, &'static str> {
    let mut id = SessionId::from_str(session_id).map_err(|_| "invalid Session or Run ID")?;
    let run_id = RunId::from_str(session_id).map_err(|_| "invalid Session or Run ID")?;
    let state_dir = cli::state_dir(state_dir)?;
    let root = StateRoot::open_existing(&state_dir)
        .map_err(|_| "state directory unavailable or unsafe")?;
    let store = Store::open_read_only(root).map_err(|_| "state store unavailable")?;
    let mut events = store
        .load_session(id)
        .await
        .map_err(|_| "Session history unavailable or invalid")?;
    let run_selected = events.is_empty();
    if run_selected {
        id = store
            .session_for_run(run_id)
            .await
            .map_err(|_| "Session history unavailable or invalid")?
            .ok_or("Session or Run not found")?;
        events = store
            .load_session(id)
            .await
            .map_err(|_| "Session history unavailable or invalid")?;
    }
    let view = store
        .load_view(id)
        .await
        .map_err(|_| "Session history unavailable or invalid")?
        .ok_or("Session or Run not found")?;
    store.close().await.map_err(|_| "store shutdown failed")?;
    if run_selected {
        if !view.runs.iter().any(|run| run.id == run_id) {
            return Err("Session history unavailable or invalid");
        }
        events.retain(|event| event.run_id == Some(run_id));
    }
    Ok(render_session(&view, &events, output.into()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn product_runtime_supports_deadlines_and_io() {
        let runtime = super::build_runtime().expect("CLI runtime");
        let result = runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::ZERO, std::future::pending::<()>()).await
        });
        assert!(result.is_err());
        #[cfg(unix)]
        runtime.block_on(async {
            let (socket, _peer) = tokio::net::UnixStream::pair().expect("local socket pair");
            socket.writable().await.expect("runtime I/O driver");
        });
    }
}

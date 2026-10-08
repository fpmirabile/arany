#![forbid(unsafe_code)]

mod cli;

use arany::{RunId, SessionId, SessionView, StateRoot, Store, Telemetry, render_session};
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
    Provider(ProviderArgs),
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
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--internal-tool-guard"))
    {
        return arany::tool_guard_main();
    }
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new("--internal-credential-helper"))
    {
        return cli::credentials::keyring_helper_main();
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
    match runtime.block_on(run(cli.command.expect("checked subcommand"), telemetry)) {
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
        Command::Exec(args) => cli::exec::run(args, telemetry).await,
        Command::Provider(args) => cli::provider::run(args).await,
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
    store.close().await.map_err(|_| "store shutdown failed")?;
    let view = SessionView::replay(id, &events)
        .map_err(|_| "Session history unavailable or invalid")?
        .ok_or("Session or Run not found")?;
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

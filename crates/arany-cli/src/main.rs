#![forbid(unsafe_code)]

mod accounts;
mod cli;
mod files;
use arany_cli::{presentation, terminal, terminal::report_error};

#[cfg(all(test, unix))]
use arany::test_support::process as test_process;

use crate::presentation::render_session;
use arany::{RunId, SessionId, StateRoot, Store, Telemetry};
use clap::{ColorChoice, Parser, Subcommand, error::ErrorKind};
use cli::{CommandOutput, attached::AttachedArgs, exec::ExecArgs, provider::ProviderArgs};
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

#[derive(Clone, Copy, clap::ValueEnum)]
enum ShowOutput {
    Text,
    Jsonl,
    SupervisorCorpus,
}

#[derive(Subcommand)]
enum Command {
    Exec(ExecArgs),
    #[command(about = "Report Arany-owned storage metadata")]
    Storage(cli::storage::StorageArgs),
    Serve(cli::serve::ServeArgs),
    Acp(cli::acp::AcpArgs),
    Provider(ProviderArgs),
    GenerateSchema {
        #[arg(long)]
        out: Option<PathBuf>,
    },
    #[command(about = "Generate the Rust-owned Tool limits reference")]
    GenerateLimits,
    Show {
        #[arg(long)]
        state_dir: Option<PathBuf>,
        #[arg(long)]
        output: ShowOutput,
        #[arg(value_name = "SESSION_OR_RUN_ID")]
        session_id: String,
    },
}

fn main() -> ExitCode {
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new("--internal-host-command"))
    {
        return arany::tool_host_main();
    }
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
    if matches!(cli.command, Some(Command::Storage(_))) {
        return run_cli(cli, Telemetry::disabled());
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
    let storage = matches!(cli.command, Some(Command::Storage(_)));
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
                if !storage {
                    arany::record_development_failure(arany::DevelopmentFailure::Output);
                }
                report_error("output failed");
                ExitCode::FAILURE
            } else if output.success {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            if !storage {
                arany::record_development_failure(arany::DevelopmentFailure::ExecExit);
            }
            report_error(error);
            ExitCode::FAILURE
        }
    }
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
        Command::Storage(args) => cli::storage::run(args).await,
        Command::Exec(args) => cli::exec::run(args, telemetry).await,
        Command::Provider(args) => cli::provider::run(args).await,
        Command::GenerateSchema { out } => cli::generate_schema::run(out.as_deref()),
        Command::GenerateLimits => Ok(cli::generate_schema::run_limits()),
        Command::Show {
            state_dir,
            output,
            session_id,
        } => show(state_dir, output, &session_id).await,
    }
}

async fn show(
    state_dir: Option<PathBuf>,
    output: ShowOutput,
    session_id: &str,
) -> Result<CommandOutput, String> {
    let mut id = SessionId::from_str(session_id).map_err(|_| "invalid Session or Run ID")?;
    let run_id = RunId::from_str(session_id).map_err(|_| "invalid Session or Run ID")?;
    let state_dir = cli::state_dir(state_dir)?;
    let root = StateRoot::open_existing(&state_dir)
        .map_err(|_| "state directory unavailable or unsafe")?;
    if matches!(output, ShowOutput::SupervisorCorpus) {
        return arany::supervisor::export_corpus(root, id)
            .await
            .map(|stdout| CommandOutput {
                stdout,
                stderr: String::new(),
                success: true,
            })
            .map_err(|error| error.to_string());
    }
    let store = Store::open_read_only(root).map_err(|_| "state store unavailable")?;
    let mut events = match store.load_session(id).await {
        Ok(events) => events,
        Err(arany::StoreError::InvalidHistory | arany::StoreError::InvalidSnapshot) => {
            let inspected = store
                .inspect_session(id)
                .await
                .map_err(|_| "Session history unavailable or invalid")?;
            store.close().await.map_err(|_| "store shutdown failed")?;
            if inspected.events.is_empty() {
                return Err("Session history unavailable or invalid".into());
            }
            return render_invalid_history(inspected, output);
        }
        Err(_) => return Err("Session history unavailable or invalid".into()),
    };
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
            return Err("Session history unavailable or invalid".into());
        }
        events.retain(|event| event.run_id == Some(run_id));
    }
    let output = match output {
        ShowOutput::Text => presentation::Output::Text,
        ShowOutput::Jsonl => presentation::Output::Jsonl,
        ShowOutput::SupervisorCorpus => unreachable!("corpus returned before ordinary show"),
    };
    Ok(CommandOutput {
        stdout: render_session(&view, &events, output),
        stderr: String::new(),
        success: true,
    })
}

fn render_invalid_history(
    inspection: arany::SessionInspection,
    output: ShowOutput,
) -> Result<CommandOutput, String> {
    let failure = inspection
        .failure
        .as_ref()
        .ok_or("Session or Run not found")?;
    let prefix: Vec<_> = inspection
        .events
        .iter()
        .take_while(|item| item.sequence < failure.sequence)
        .filter_map(|item| item.event.as_ref().ok().cloned())
        .collect();
    let mut rendered = render_projection_prefix(inspection.prefix_view.as_ref(), &prefix, output);
    let tail = inspection
        .events
        .iter()
        .filter(|item| item.sequence >= failure.sequence)
        .map(|item| {
            (
                item.sequence,
                item.kind.clone(),
                item.event.as_ref().ok().map(|event| event.event.clone()),
            )
        })
        .collect();
    let notice = format!(
        "History invalid at sequence {}: {}; shown read-only, not resumable",
        failure.sequence, failure.cause
    );
    rendered.push_str(&render_lenient_tail(output, &notice, tail));
    Ok(CommandOutput {
        stdout: rendered,
        stderr: String::new(),
        success: false,
    })
}

fn render_projection_prefix(
    view: Option<&arany::SessionView>,
    prefix: &[arany::EventEnvelope],
    output: ShowOutput,
) -> String {
    view.map(|view| {
        let output = match output {
            ShowOutput::Text => presentation::Output::Text,
            ShowOutput::Jsonl => presentation::Output::Jsonl,
            ShowOutput::SupervisorCorpus => {
                unreachable!("corpus returned before ordinary show")
            }
        };
        presentation::render_session(view, prefix, output)
    })
    .unwrap_or_default()
}

fn render_lenient_tail(
    output: ShowOutput,
    notice: &str,
    tail: Vec<(u64, String, Option<arany::Event>)>,
) -> String {
    let mut rendered = String::new();
    if matches!(output, ShowOutput::Text) {
        rendered.push_str(notice);
        rendered.push('\n');
        for (sequence, kind, event) in tail {
            let payload = event
                .and_then(|event| event.payload().ok())
                .unwrap_or_else(|| "undecodable".into());
            rendered.push_str(&format!(
                "{sequence} {} {}\n",
                arany::escape_status_text(&kind),
                arany::escape_status_text(&payload)
            ));
        }
    } else {
        rendered.push_str(&presentation::escape_json_controls(
            &serde_json::json!({"notice": notice}).to_string(),
        ));
        rendered.push('\n');
        for (sequence, kind, event) in tail {
            let payload = event.and_then(|event| event.payload().ok());
            let payload =
                payload.and_then(|value| serde_json::from_str::<serde_json::Value>(&value).ok());
            let record = match payload {
                Some(payload) => {
                    serde_json::json!({"sequence": sequence, "kind": kind, "payload": payload})
                }
                None => serde_json::json!({
                    "sequence": sequence,
                    "kind": kind,
                    "payload": "undecodable"
                }),
            };
            rendered.push_str(&presentation::escape_json_controls(&record.to_string()));
            rendered.push('\n');
        }
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::{ShowOutput, render_lenient_tail};

    #[test]
    fn lenient_jsonl_escapes_hostile_unicode_in_tail_fields() {
        let output = render_lenient_tail(
            ShowOutput::Jsonl,
            "Notice\u{0085}",
            vec![(
                1,
                "Bad\u{202e}Kind".into(),
                Some(arany::Event::SessionRenamed {
                    title: "Name\u{0085}\u{202e}".into(),
                }),
            )],
        );
        assert!(output.contains("\\u0085"));
        assert!(output.contains("\\u202e"));
        assert!(!output.contains('\u{0085}') && !output.contains('\u{202e}'));
    }

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

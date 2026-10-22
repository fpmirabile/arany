use super::CommandOutput;
use crate::presentation::storage::render_storage;
use crate::terminal::{AttachedTerminal, Composer, ComposerEdit, TerminalInput};
use arany::{SessionView, StateRoot, StorageError, StorageReport};
use clap::Args;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Args)]
pub(crate) struct StorageArgs {
    #[arg(long)]
    workspace: Option<PathBuf>,
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

struct Inventory {
    cancelled: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<Result<StorageReport, String>>,
}

impl Inventory {
    fn start(workspace: PathBuf, selected: PathBuf) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = cancelled.clone();
        let task = tokio::task::spawn_blocking(move || {
            if signal.load(Ordering::Acquire) {
                return Err("storage inventory cancelled; no complete report".into());
            }
            let workspace =
                std::fs::canonicalize(workspace).map_err(|_| "Workspace unavailable")?;
            let default =
                StateRoot::default_path().map_err(|_| "default state location unavailable")?;
            let account = StateRoot::account_path().map_err(|_| "account location unavailable")?;
            arany::collect_storage(&workspace, &selected, &default, &account, &signal).map_err(
                |error| {
                    match error {
                        StorageError::Cancelled => {
                            "storage inventory cancelled; no complete report"
                        }
                        StorageError::Busy => "storage inventory already running; no report",
                        StorageError::WorkspaceUnavailable => "Workspace unavailable; no report",
                    }
                    .to_owned()
                },
            )
        });
        Self { cancelled, task }
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

impl Drop for Inventory {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn result(
    value: Result<Result<StorageReport, String>, tokio::task::JoinError>,
) -> Result<CommandOutput, String> {
    let report = value.map_err(|_| "storage inventory unavailable")??;
    let (stdout, success) = render_storage(&report);
    Ok(CommandOutput {
        stdout,
        stderr: if success {
            String::new()
        } else {
            "error: storage inventory partial; see known subtotal and coverage\n".into()
        },
        success,
    })
}

pub(crate) async fn run(args: StorageArgs) -> Result<CommandOutput, String> {
    let workspace = args.workspace.map_or_else(
        || std::env::current_dir().map_err(|_| "Workspace unavailable".to_owned()),
        Ok,
    )?;
    let selected = super::state_dir(args.state_dir)?;
    let mut signals =
        super::exec::StopSignals::install().map_err(|_| "signal handling unavailable")?;
    let mut inventory = Inventory::start(workspace, selected);
    tokio::select! {
        biased;
        _ = signals.next() => { inventory.cancel(); let _ = (&mut inventory.task).await; Err("storage inventory cancelled; no complete report".into()) }
        value = &mut inventory.task => result(value),
    }
}

pub(super) async fn attached(
    terminal: &mut AttachedTerminal,
    workspace: PathBuf,
    selected: PathBuf,
    view: &SessionView,
    composer: &mut Composer,
) -> Result<(), String> {
    let mut inventory = Inventory::start(workspace, selected);
    terminal
        .draw_progress_with_draft(
            view,
            composer,
            "Inspecting storage metadata; Esc or Ctrl+C cancels",
        )
        .map_err(|error| error.to_string())?;
    loop {
        tokio::select! {
            biased;
            input = terminal.next_input() => {
                let input = match input {
                    Ok(input) => input,
                    Err(error) => { inventory.cancel(); let _ = (&mut inventory.task).await; return Err(error.to_string()); }
                };
                match input {
                    TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput | TerminalInput::Shutdown(_) => {
                        inventory.cancel();
                        let _ = (&mut inventory.task).await;
                        if matches!(input, TerminalInput::Shutdown(_) | TerminalInput::EndOfInput) { terminal.defer_input(input); return Ok(()); }
                        return terminal.show_local_report(view, "Storage inventory cancelled; no complete report.\n").await.map_err(|error| error.to_string());
                    }
                    TerminalInput::Suspend => { terminal.suspend_and_resume(composer.text().len()).map_err(|error| error.to_string())?; }
                    TerminalInput::Resize => { terminal.draw_progress_with_draft(view, composer, "Inspecting storage metadata; Esc or Ctrl+C cancels").map_err(|error| error.to_string())?; }
                    TerminalInput::Submit => {
                        terminal.restore_draft_input(composer.text().len()).map_err(|error| error.to_string())?;
                    }
                    TerminalInput::Paste => {
                        let _ = terminal.paste_into(composer);
                    }
                    input => {
                        if composer.apply(input) == ComposerEdit::Changed && !terminal.is_linear() {
                            terminal.draw_progress_with_draft(view, composer, "Inspecting storage metadata; Esc or Ctrl+C cancels").map_err(|error| error.to_string())?;
                        }
                    }
                }
            }
            value = &mut inventory.task => {
                let text = match result(value) { Ok(output) => output.stdout, Err(error) => format!("Error: {error}\n") };
                return terminal.show_local_report(view, &text).await.map_err(|error| error.to_string());
            }
        }
    }
}

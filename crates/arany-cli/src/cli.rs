use crate::presentation::Output;
use arany::{Effort, ProviderError, ProviderKind, StateRoot, resolve_native_effort_for_run};
use clap::ValueEnum;
use std::path::PathBuf;

pub(crate) fn admit_workspace_identity(
    workspace: &std::path::Path,
) -> Result<arany::WorkspaceIdentity, &'static str> {
    use rustix::fs::{Mode, OFlags, fstat, open};

    let workspace = workspace
        .canonicalize()
        .map_err(|_| "Workspace unavailable")?;
    let directory = open(
        workspace,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| "Workspace unavailable")?;
    let metadata = fstat(&directory).map_err(|_| "Workspace unavailable")?;
    Ok(arany::WorkspaceIdentity {
        device: metadata.st_dev as u64,
        inode: metadata.st_ino as u64,
    })
}

pub(crate) mod acp;
pub(crate) mod attached;
pub(crate) mod exec;
pub(crate) mod generate_schema;
pub(crate) mod provider;
#[cfg(test)]
mod record_formats;
pub(crate) mod run_outcome;
pub(crate) mod serve;
pub(crate) mod storage;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum OutputArg {
    Text,
    Jsonl,
}

impl From<OutputArg> for Output {
    fn from(value: OutputArg) -> Self {
        match value {
            OutputArg::Text => Self::Text,
            OutputArg::Jsonl => Self::Jsonl,
        }
    }
}

pub(crate) struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

pub(crate) fn state_dir(explicit: Option<PathBuf>) -> Result<PathBuf, &'static str> {
    match explicit {
        Some(path) => Ok(path),
        None => match std::env::var_os("ARANY_STATE_DIR") {
            Some(path) if path.is_empty() => Err("empty ARANY_STATE_DIR"),
            Some(path) => Ok(PathBuf::from(path)),
            None => StateRoot::default_path().map_err(|_| "invalid state directory"),
        },
    }
}

fn admit_workspace(
    workspace: Option<PathBuf>,
    state: Option<PathBuf>,
) -> Result<(PathBuf, PathBuf), &'static str> {
    let workspace = workspace.map_or_else(
        || std::env::current_dir().map_err(|_| "Workspace unavailable"),
        Ok,
    )?;
    let state = state_dir(state)?;
    let identity = std::fs::canonicalize(&workspace).map_err(|_| "Workspace unavailable")?;
    if state.starts_with(&identity) {
        return Err("state directory overlaps the Workspace");
    }
    Ok((workspace, state))
}

pub(crate) fn archived_history_notice(path: &std::path::Path) -> String {
    crate::presentation::escape_terminal(&format!(
        "History from an incompatible version archived intact to {}",
        path.display()
    ))
}

pub(crate) fn admit_headless_history(state: &StateRoot) -> Result<(), String> {
    use arany::HistoryRecovery;
    let error = match state.archive_incompatible_history() {
        Ok(None) => return Ok(()),
        Ok(Some(archived)) => {
            use std::io::Write;
            std::io::stderr()
                .write_all(
                    format!(
                        "warning: {}; started a new history\n",
                        archived_history_notice(archived.path())
                    )
                    .as_bytes(),
                )
                .map_err(|_| "history notice output failed".to_owned())?;
            return archived.acknowledge().map_err(|error| error.to_string());
        }
        Err(error) => error,
    };
    Err(match error.history_recovery() {
        HistoryRecovery::InvalidHistory | HistoryRecovery::Damaged => {
            "saved history is incompatible or damaged; run `arany` to reset history"
        }
        HistoryRecovery::ArchiveConflict => {
            "an interrupted history archive no longer matches its files; inspect the archive folder in the state directory"
        }
        HistoryRecovery::MigrationConflict => {
            "an interrupted history migration no longer matches its files; inspect the migrations folder in the state directory"
        }
        HistoryRecovery::Busy => "history is in use; close the other Arany and retry",
        HistoryRecovery::Changed => "history changed during recovery; retry",
        HistoryRecovery::StorageFull => "state storage is full; free space and retry",
        HistoryRecovery::Inaccessible | HistoryRecovery::InvalidStateDirectory => {
            "history files are unsafe or inaccessible; check permissions and retry"
        }
        HistoryRecovery::Unavailable => "history cannot be opened; check storage access and retry",
    }
    .to_owned())
}

pub(crate) fn native_api_key_from_env(profile: &str) -> Result<String, ProviderError> {
    let name = match ProviderKind::parse(profile) {
        Some(ProviderKind::OpenAi) => "OPENAI_API_KEY",
        Some(ProviderKind::Anthropic) => "ANTHROPIC_API_KEY",
        Some(ProviderKind::ChatGpt | ProviderKind::Custom) | None => {
            return Err(ProviderError::Rejected);
        }
    };
    std::env::var(name).map_err(|_| ProviderError::Unavailable)
}

pub(crate) fn native_run_key_from_env(
    profile: &str,
    model: &str,
    effort: Option<Effort>,
) -> Result<String, ProviderError> {
    resolve_native_effort_for_run(profile, model, effort)?;
    native_api_key_from_env(profile)
}

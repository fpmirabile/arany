use arany::{Effort, Output, ProviderError, StateRoot, resolve_native_effort_for_run};
use clap::ValueEnum;
use std::path::PathBuf;

pub(crate) mod attached;
#[expect(dead_code, reason = "OAuth token and listener gates are not wired yet")]
pub(crate) mod chatgpt;
pub(crate) mod credentials;
pub(crate) mod exec;
pub(crate) mod provider;

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

pub(crate) fn native_api_key_from_env(profile: &str) -> Result<String, ProviderError> {
    let name = match profile {
        "openai" => "OPENAI_API_KEY",
        "anthropic" => "ANTHROPIC_API_KEY",
        _ => return Err(ProviderError::Rejected),
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

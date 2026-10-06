use arany::{Effort, Output, ProviderError, StateRoot, resolve_native_effort_for_run};
use clap::ValueEnum;
use serde::Deserialize;
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

fn open_optional_state(path: &std::path::Path) -> Result<Option<StateRoot>, arany::StoreError> {
    match StateRoot::open_existing(path) {
        Ok(root) => Ok(Some(root)),
        Err(arany::StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::symlink_metadata(path) {
                Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => Ok(None),
                _ => Err(arany::StoreError::Io(error)),
            }
        }
        Err(error) => Err(error),
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

pub(crate) fn bounded_list<'de, D, T, const LIMIT: usize>(
    deserializer: D,
    limit_error: &'static str,
) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Visitor<T, const LIMIT: usize> {
        limit_error: &'static str,
        marker: std::marker::PhantomData<T>,
    }
    impl<'de, T: Deserialize<'de>, const LIMIT: usize> serde::de::Visitor<'de> for Visitor<T, LIMIT> {
        type Value = Vec<T>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "at most {LIMIT} entries")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Vec<T>, A::Error> {
            let mut values = Vec::new();
            while values.len() < LIMIT {
                let Some(value) = sequence.next_element()? else {
                    return Ok(values);
                };
                values.push(value);
            }
            if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom(self.limit_error));
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Visitor::<T, LIMIT> {
        limit_error,
        marker: std::marker::PhantomData,
    })
}

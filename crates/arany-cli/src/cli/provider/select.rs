use crate::accounts::{chatgpt, credentials};
use crate::cli::native_run_key_from_env;
use arany::{
    AnthropicProvider, CustomProvider, Effort, NativeApiCredentials, OpenAiProvider, Provider,
    ProviderKind, StateRoot, resolve_native_effort_for_run, validate_native_model_id,
};
use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProviderArg {
    Openai,
    Anthropic,
    Chatgpt,
    Custom(String),
}

impl ProviderArg {
    pub(crate) fn profile_label(&self) -> String {
        let provider = match self {
            Self::Openai => ProviderKind::OpenAi,
            Self::Anthropic => ProviderKind::Anthropic,
            Self::Chatgpt => ProviderKind::ChatGpt,
            Self::Custom(name) => return format!("custom:{name}"),
        };
        provider.native_name().expect("native Provider name").into()
    }
}

impl FromStr for ProviderArg {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match ProviderKind::parse(value) {
            Some(ProviderKind::OpenAi) => Ok(Self::Openai),
            Some(ProviderKind::Anthropic) => Ok(Self::Anthropic),
            Some(ProviderKind::ChatGpt) => Ok(Self::Chatgpt),
            Some(ProviderKind::Custom) | None => {
                let name = value.strip_prefix("custom:").ok_or("invalid Provider")?;
                if !arany::valid_profile_name(name) {
                    return Err("invalid custom Provider name");
                }
                Ok(Self::Custom(name.to_owned()))
            }
        }
    }
}

pub(crate) fn validate_headless_selection(
    provider: &ProviderArg,
    model: &str,
    effort: Option<Effort>,
) -> Result<(), &'static str> {
    if !matches!(provider, ProviderArg::Custom(_)) {
        validate_native_model_id(model).map_err(|_| "invalid native model ID")?;
    }
    match provider {
        ProviderArg::Openai | ProviderArg::Anthropic => {
            resolve_native_effort_for_run(&provider.profile_label(), model, effort).map_err(
                |_| "invalid native model/effort; unknown models require --effort LEVEL",
            )?;
        }
        ProviderArg::Chatgpt if effort.is_none() => {
            return Err("ChatGPT requires --effort LEVEL");
        }
        _ => (),
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SelectionError {
    #[error("{0}")]
    InvalidLocalSelection(&'static str),
    #[error("select a ChatGPT account with /setup first")]
    ChatGptAccountRequired,
    #[error("selected OpenAI API key unavailable")]
    OpenAiKeyUnavailable,
    #[error("selected Anthropic API key unavailable")]
    AnthropicKeyUnavailable,
    #[error("saved account requires a native Provider")]
    SavedAccountRequiresNative,
    #[error("unsupported Provider profile")]
    UnsupportedProfile,
    #[error("state directory unavailable or unsafe")]
    StateUnavailable,
    #[error("selected native API credentials or workspace configuration unavailable")]
    NativeCredentialsUnavailable,
    #[error("OPENAI_API_KEY unavailable")]
    OpenAiEnvKeyUnavailable,
    #[error("invalid ANTHROPIC_WORKSPACE_ID")]
    InvalidAnthropicWorkspace,
    #[error("ANTHROPIC_API_KEY unavailable")]
    AnthropicEnvKeyUnavailable,
    #[error(transparent)]
    ChatGpt(chatgpt::AuthorizationError),
    #[error(transparent)]
    Custom(arany::CustomProfileError),
    #[error(transparent)]
    Credentials(credentials::CredentialError),
}

#[derive(Clone, Copy)]
pub(crate) struct Selection<'a> {
    pub workspace: &'a Path,
    pub profile: &'a str,
    pub model: &'a str,
    pub effort: Option<Effort>,
    pub account_id: Option<Uuid>,
}

pub(crate) fn validate_local_selection(selection: Selection<'_>) -> Result<(), &'static str> {
    match ProviderKind::parse(selection.profile) {
        Some(ProviderKind::OpenAi | ProviderKind::Anthropic) => {
            resolve_native_effort_for_run(selection.profile, selection.model, selection.effort)
                .map_err(
                    |_| "invalid native model/effort; choose a model and effort with /model",
                )?;
        }
        Some(ProviderKind::ChatGpt) => {
            if selection.account_id.is_none() {
                return Err("select a ChatGPT account with /setup first");
            }
            if selection.effort.is_none() {
                return Err("ChatGPT requires a model effort; use /model");
            }
        }
        Some(ProviderKind::Custom) | None => {}
    }
    Ok(())
}

pub(crate) async fn select_provider(
    state_dir: &Path,
    selection: Selection<'_>,
) -> Result<Arc<dyn Provider>, SelectionError> {
    select_provider_inner(state_dir, selection)
        .await
        .inspect_err(|_| {
            arany::record_development_failure(arany::DevelopmentFailure::ProviderAdmission);
        })
}

async fn select_provider_inner(
    state_dir: &Path,
    selection: Selection<'_>,
) -> Result<Arc<dyn Provider>, SelectionError> {
    validate_local_selection(selection).map_err(SelectionError::InvalidLocalSelection)?;
    match ProviderKind::parse(selection.profile) {
        Some(ProviderKind::OpenAi) => {
            let key = native_key(selection.profile, selection.account_id)
                .await?
                .into_api_key();
            let provider = match selection.account_id {
                Some(id) => OpenAiProvider::from_saved_api_key_with_effort(
                    selection.model.to_owned(),
                    selection.effort,
                    key,
                    id,
                ),
                None => OpenAiProvider::from_api_key_with_effort(
                    selection.model.to_owned(),
                    selection.effort,
                    key,
                ),
            }
            .map_err(|_| SelectionError::OpenAiKeyUnavailable)?;
            Ok(Arc::new(provider))
        }
        Some(ProviderKind::Anthropic) => {
            let credentials = native_key(selection.profile, selection.account_id).await?;
            let root = admit_evidence_root(state_dir)?;
            let provider = AnthropicProvider::from_credentials_with_evidence(
                root,
                selection.model.to_owned(),
                selection.effort,
                credentials,
                selection.account_id,
            )
            .map_err(|_| SelectionError::AnthropicKeyUnavailable)?;
            Ok(Arc::new(provider))
        }
        Some(ProviderKind::ChatGpt) => {
            let account_id = selection
                .account_id
                .ok_or(SelectionError::ChatGptAccountRequired)?;
            let effort = selection
                .effort
                .expect("local selection requires explicit ChatGPT effort");
            let provider = chatgpt::provider_for_run(
                selection.workspace.to_path_buf(),
                Some(account_id),
                selection.model.to_owned(),
                effort,
            )
            .await
            .map_err(SelectionError::ChatGpt)?;
            Ok(Arc::new(provider))
        }
        Some(ProviderKind::Custom) | None => {
            if selection.account_id.is_some() {
                return Err(SelectionError::SavedAccountRequiresNative);
            }
            let name = selection
                .profile
                .strip_prefix("custom:")
                .ok_or(SelectionError::UnsupportedProfile)?;
            let root = StateRoot::open_existing(state_dir)
                .map_err(|_| SelectionError::StateUnavailable)?;
            let provider = CustomProvider::admit(&root, name, selection.model, selection.effort)
                .await
                .map_err(SelectionError::Custom)?;
            Ok(Arc::new(provider))
        }
    }
}

fn admit_evidence_root(state_dir: &Path) -> Result<StateRoot, SelectionError> {
    StateRoot::admit(state_dir).map_err(|_| SelectionError::StateUnavailable)
}

async fn native_key(
    profile: &str,
    account_id: Option<Uuid>,
) -> Result<NativeApiCredentials, SelectionError> {
    match account_id {
        Some(id) => credentials::load_selected(id, profile)
            .await
            .map_err(SelectionError::Credentials),
        None => NativeApiCredentials::from_env(profile)
            .map_err(|_| SelectionError::NativeCredentialsUnavailable),
    }
}

pub(crate) async fn authenticate(
    state_dir: &Path,
    profile: &str,
    model: &str,
    effort: Option<Effort>,
    account_id: Option<Uuid>,
) -> bool {
    match ProviderKind::parse(profile) {
        Some(ProviderKind::OpenAi | ProviderKind::Anthropic) => {
            native_key(profile, account_id).await.is_ok()
        }
        Some(ProviderKind::ChatGpt) => chatgpt::authenticate(account_id).await.is_ok(),
        Some(ProviderKind::Custom) | None => {
            let Some(name) = profile.strip_prefix("custom:") else {
                return false;
            };
            let Ok(root) = StateRoot::open_existing(state_dir) else {
                return false;
            };
            CustomProvider::admit(&root, name, model, effort)
                .await
                .is_ok()
        }
    }
}

pub(crate) async fn select_env_provider(
    provider: &ProviderArg,
    model: &str,
    effort: Option<Effort>,
    workspace: PathBuf,
    state_dir: &Path,
) -> Result<Arc<dyn Provider>, SelectionError> {
    let provider: Arc<dyn Provider> = match provider {
        ProviderArg::Openai => Arc::new(
            native_run_key_from_env(
                ProviderKind::OpenAi
                    .native_name()
                    .expect("native Provider name"),
                model,
                effort,
            )
            .and_then(|key| OpenAiProvider::from_api_key_with_effort(model.to_owned(), effort, key))
            .map_err(|_| SelectionError::OpenAiEnvKeyUnavailable)?,
        ),
        ProviderArg::Anthropic => {
            let credentials =
                NativeApiCredentials::from_env("anthropic").map_err(|error| match error {
                    arany::ProviderError::Rejected => SelectionError::InvalidAnthropicWorkspace,
                    _ => SelectionError::AnthropicEnvKeyUnavailable,
                })?;
            let root = admit_evidence_root(state_dir)?;
            Arc::new(
                AnthropicProvider::from_credentials_with_evidence(
                    root,
                    model.to_owned(),
                    effort,
                    credentials,
                    None,
                )
                .map_err(|_| SelectionError::AnthropicEnvKeyUnavailable)?,
            )
        }
        ProviderArg::Chatgpt => Arc::new(
            chatgpt::provider_for_run(
                workspace,
                None,
                model.to_owned(),
                effort.expect("validated ChatGPT effort"),
            )
            .await
            .map_err(SelectionError::ChatGpt)?,
        ),
        ProviderArg::Custom(name) => {
            let root = StateRoot::open_existing(state_dir)
                .map_err(|_| SelectionError::StateUnavailable)?;
            Arc::new(
                CustomProvider::admit(&root, name, model, effort)
                    .await
                    .map_err(SelectionError::Custom)?,
            )
        }
    };
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn anthropic_selection_starts_without_state_and_rejects_unsafe_state() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("state");
        let root = admit_evidence_root(&missing).unwrap();
        let credentials =
            NativeApiCredentials::new("anthropic", "fixture-key".into(), None).unwrap();
        let provider = AnthropicProvider::from_credentials_with_evidence(
            root,
            "claude-sonnet-5".into(),
            Some(Effort::Low),
            credentials,
            None,
        )
        .unwrap();
        assert_eq!(provider.context_window_tokens_for_run().await, None);

        let empty = temp.path().join("empty-state");
        std::fs::create_dir(&empty).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&empty, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let root = admit_evidence_root(&empty).unwrap();
        assert!(root.path().join("events.sqlite3").exists());
        let credentials =
            NativeApiCredentials::new("anthropic", "fixture-key".into(), None).unwrap();
        let provider = AnthropicProvider::from_credentials_with_evidence(
            root,
            "claude-sonnet-5".into(),
            Some(Effort::Low),
            credentials,
            None,
        )
        .unwrap();
        assert_eq!(provider.context_window_tokens_for_run().await, None);

        #[cfg(unix)]
        {
            let unsafe_state = temp.path().join("unsafe-state");
            std::os::unix::fs::symlink(temp.path(), &unsafe_state).unwrap();
            assert!(admit_evidence_root(&unsafe_state).is_err());
        }
    }

    #[test]
    fn selection_error_display_preserves_cli_text() {
        for (error, expected) in [
            (
                SelectionError::ChatGptAccountRequired,
                "select a ChatGPT account with /setup first",
            ),
            (
                SelectionError::OpenAiKeyUnavailable,
                "selected OpenAI API key unavailable",
            ),
            (
                SelectionError::AnthropicKeyUnavailable,
                "selected Anthropic API key unavailable",
            ),
            (
                SelectionError::SavedAccountRequiresNative,
                "saved account requires a native Provider",
            ),
            (
                SelectionError::UnsupportedProfile,
                "unsupported Provider profile",
            ),
            (
                SelectionError::StateUnavailable,
                "state directory unavailable or unsafe",
            ),
            (
                SelectionError::NativeCredentialsUnavailable,
                "selected native API credentials or workspace configuration unavailable",
            ),
            (
                SelectionError::OpenAiEnvKeyUnavailable,
                "OPENAI_API_KEY unavailable",
            ),
            (
                SelectionError::InvalidAnthropicWorkspace,
                "invalid ANTHROPIC_WORKSPACE_ID",
            ),
            (
                SelectionError::AnthropicEnvKeyUnavailable,
                "ANTHROPIC_API_KEY unavailable",
            ),
            (
                SelectionError::InvalidLocalSelection(
                    "invalid native model/effort; choose a model and effort with /model",
                ),
                "invalid native model/effort; choose a model and effort with /model",
            ),
            (
                SelectionError::InvalidLocalSelection("select a ChatGPT account with /setup first"),
                "select a ChatGPT account with /setup first",
            ),
            (
                SelectionError::InvalidLocalSelection(
                    "ChatGPT requires a model effort; use /model",
                ),
                "ChatGPT requires a model effort; use /model",
            ),
        ] {
            assert_eq!(error.to_string(), expected);
        }
    }
}

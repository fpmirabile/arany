use crate::cli::{chatgpt, credentials};
use arany::{
    AnthropicProvider, ChatGptProvider, CompactionRecord, CustomProvider, Effort, Engine,
    NativeApiCredentials, OpenAiProvider, Provider, RunCancellation, RunId, RunOutcome,
    RunProgress, RunRequest, SessionId, StateRoot, Telemetry, resolve_native_effort_for_run,
};
use std::path::Path;
use uuid::Uuid;

#[derive(Clone, Copy)]
pub(super) struct ToolAccess<'a> {
    pub configured: bool,
    pub permissions: Option<&'a arany::WorkspacePermissions>,
    pub approvals: &'a arany::ToolApprovals,
}

enum SelectedProvider {
    OpenAi(OpenAiProvider),
    Anthropic(AnthropicProvider),
    ChatGpt(ChatGptProvider),
    Custom(CustomProvider),
}

pub(super) enum RunAttemptError {
    InterruptedBeforeRun,
    Failed(String),
}

#[derive(Clone, Copy)]
pub(super) struct Selection<'a> {
    pub workspace: &'a Path,
    pub profile: &'a str,
    pub model: &'a str,
    pub effort: Option<Effort>,
    pub account_id: Option<Uuid>,
}

pub(super) fn validate_local_selection(selection: Selection<'_>) -> Result<(), &'static str> {
    match selection.profile {
        "openai" | "anthropic" => {
            resolve_native_effort_for_run(selection.profile, selection.model, selection.effort)
                .map_err(
                    |_| "invalid native model/effort; choose a model and effort with /model",
                )?;
        }
        "chatgpt" => {
            if selection.account_id.is_none() {
                return Err("select a ChatGPT account with /setup first");
            }
            if selection.effort.is_none() {
                return Err("ChatGPT requires a model effort; use /model");
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) async fn preflight_selected(
    state_dir: &Path,
    selection: Selection<'_>,
) -> Result<(), String> {
    select_provider(state_dir, selection).await.map(|_| ())
}

pub(super) async fn run_selected(
    state_dir: &Path,
    telemetry: &Telemetry,
    selection: Selection<'_>,
    request: RunRequest,
    cancellation: RunCancellation,
    progress: RunProgress,
    tools: ToolAccess<'_>,
) -> Result<RunOutcome, RunAttemptError> {
    let mut admission_cancellation = cancellation.clone();
    let provider = tokio::select! {
        biased;
        () = admission_cancellation.cancelled() => return Err(RunAttemptError::InterruptedBeforeRun),
        result = select_provider(state_dir, selection) => {
            result.map_err(RunAttemptError::Failed)?
        }
    };
    match provider {
        SelectedProvider::OpenAi(provider) => {
            run_with_provider(
                state_dir,
                telemetry,
                provider,
                request,
                cancellation,
                progress,
                tools,
            )
            .await
        }
        SelectedProvider::Anthropic(provider) => {
            run_with_provider(
                state_dir,
                telemetry,
                provider,
                request,
                cancellation,
                progress,
                tools,
            )
            .await
        }
        SelectedProvider::ChatGpt(provider) => {
            run_with_provider(
                state_dir,
                telemetry,
                provider,
                request,
                cancellation,
                progress,
                tools,
            )
            .await
        }
        SelectedProvider::Custom(provider) => {
            run_with_provider(
                state_dir,
                telemetry,
                provider,
                request,
                cancellation,
                progress,
                tools,
            )
            .await
        }
    }
    .map_err(RunAttemptError::Failed)
}

pub(super) async fn compact_selected(
    state_dir: &Path,
    telemetry: &Telemetry,
    selection: Selection<'_>,
    session_id: SessionId,
    workspace: &Path,
    expected_run_id: Option<RunId>,
) -> Result<Option<CompactionRecord>, String> {
    match select_provider(state_dir, selection).await? {
        SelectedProvider::OpenAi(provider) => {
            compact_with_provider(
                state_dir,
                telemetry,
                provider,
                session_id,
                workspace,
                expected_run_id,
            )
            .await
        }
        SelectedProvider::Anthropic(provider) => {
            compact_with_provider(
                state_dir,
                telemetry,
                provider,
                session_id,
                workspace,
                expected_run_id,
            )
            .await
        }
        SelectedProvider::ChatGpt(provider) => {
            compact_with_provider(
                state_dir,
                telemetry,
                provider,
                session_id,
                workspace,
                expected_run_id,
            )
            .await
        }
        SelectedProvider::Custom(provider) => {
            compact_with_provider(
                state_dir,
                telemetry,
                provider,
                session_id,
                workspace,
                expected_run_id,
            )
            .await
        }
    }
}

async fn select_provider(
    state_dir: &Path,
    selection: Selection<'_>,
) -> Result<SelectedProvider, String> {
    select_provider_inner(state_dir, selection)
        .await
        .inspect_err(|_| {
            arany::record_development_failure(arany::DevelopmentFailure::ProviderAdmission);
        })
}

async fn select_provider_inner(
    state_dir: &Path,
    selection: Selection<'_>,
) -> Result<SelectedProvider, String> {
    validate_local_selection(selection)?;
    match selection.profile {
        "openai" => {
            let key = native_key(selection).await?.into_api_key();
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
            .map_err(|_| "selected OpenAI API key unavailable".to_owned())?;
            Ok(SelectedProvider::OpenAi(provider))
        }
        "anthropic" => {
            let credentials = native_key(selection).await?;
            let provider = AnthropicProvider::from_credentials_with_effort(
                selection.model.to_owned(),
                selection.effort,
                credentials,
                selection.account_id,
            )
            .map_err(|_| "selected Anthropic API key unavailable".to_owned())?;
            Ok(SelectedProvider::Anthropic(provider))
        }
        "chatgpt" => {
            let account_id = selection
                .account_id
                .ok_or("select a ChatGPT account with /setup first")?;
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
            .map_err(|error| error.to_string())?;
            Ok(SelectedProvider::ChatGpt(provider))
        }
        _ => {
            if selection.account_id.is_some() {
                return Err("saved account requires a native Provider".into());
            }
            let name = selection
                .profile
                .strip_prefix("custom:")
                .ok_or_else(|| "unsupported Provider profile".to_owned())?;
            let root = StateRoot::open_existing(state_dir)
                .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
            let provider = CustomProvider::admit(&root, name, selection.model, selection.effort)
                .await
                .map_err(|error| error.to_string())?;
            Ok(SelectedProvider::Custom(provider))
        }
    }
}

async fn native_key(selection: Selection<'_>) -> Result<NativeApiCredentials, String> {
    match selection.account_id {
        Some(id) => credentials::load_selected(selection.workspace, id, selection.profile)
            .await
            .map_err(|error| error.to_string()),
        None => NativeApiCredentials::from_env(selection.profile).map_err(|_| {
            "selected native API credentials or workspace configuration unavailable".to_owned()
        }),
    }
}

async fn run_with_provider<P: Provider + 'static>(
    state_dir: &Path,
    telemetry: &Telemetry,
    provider: P,
    request: RunRequest,
    cancellation: RunCancellation,
    progress: RunProgress,
    tools: ToolAccess<'_>,
) -> Result<RunOutcome, String> {
    let root = StateRoot::admit(state_dir)
        .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
    let mut engine = Engine::open_with_telemetry(root, provider, telemetry.clone())
        .map_err(|_| "state store unavailable".to_owned())?;
    if tools.configured {
        engine
            .enable_tools(
                std::env::current_exe().map_err(|_| "Guard executable unavailable".to_owned())?,
            )
            .map_err(|error| {
                arany::record_development_failure(arany::DevelopmentFailure::ToolAdmission);
                error.to_string()
            })?;
    }
    if let Some(permissions) = tools
        .permissions
        .filter(|permissions| permissions.is_trusted())
    {
        engine
            .enable_trusted_workspace(
                permissions.clone(),
                &request.workspace,
                std::env::current_exe().map_err(|_| "Guard executable unavailable".to_owned())?,
                tools.approvals.clone(),
            )
            .map_err(|error| {
                arany::record_development_failure(arany::DevelopmentFailure::ToolAdmission);
                error.to_string()
            })?;
    }
    engine.set_tool_approvals(tools.approvals.clone());
    let result = engine
        .run_with_progress(request, cancellation, progress)
        .await;
    let closed = engine.close().await;
    let outcome = result.map_err(|error| error.to_string())?;
    closed.map_err(|_| "store shutdown failed".to_owned())?;
    Ok(outcome)
}

async fn compact_with_provider<P: Provider + 'static>(
    state_dir: &Path,
    telemetry: &Telemetry,
    provider: P,
    session_id: SessionId,
    workspace: &Path,
    expected_run_id: Option<RunId>,
) -> Result<Option<CompactionRecord>, String> {
    let root = StateRoot::open_existing(state_dir)
        .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
    let mut engine = Engine::open_with_telemetry(root, provider, telemetry.clone())
        .map_err(|_| "state store unavailable".to_owned())?;
    let result = if let Some(expected_run_id) = expected_run_id {
        engine
            .auto_compact_session(session_id, workspace.to_path_buf(), expected_run_id)
            .await
    } else {
        engine
            .compact_session(session_id, workspace.to_path_buf())
            .await
            .map(Some)
    };
    let closed = engine.close().await;
    let record = result.map_err(|error| error.to_string())?;
    closed.map_err(|_| "store shutdown failed".to_owned())?;
    Ok(record)
}

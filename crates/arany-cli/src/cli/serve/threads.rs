use super::{items, server::Fault};
use crate::cli::{
    attached::execution_limits,
    provider::{
        routing::configured_providers,
        select::{ProviderArg, select_env_provider},
    },
};
use arany::{
    AdmittedProviders, ApprovalMode, CollaborationPolicy, Engine, GoalBudget, SessionId,
    SessionView, StateRoot, Store, ToolGrant, ToolResourceSettings,
};
use arany_cli::protocol::{methods::*, rpc::Reason};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    str::FromStr,
};

pub(super) const MAX_THREADS: usize = 64;

#[derive(Default)]
pub(super) struct Threads {
    pub map: HashMap<SessionId, LoadedThread>,
    #[cfg(test)]
    pub provider: Option<std::sync::Arc<dyn arany::Provider>>,
}

pub(super) struct LoadedThread {
    pub workspace: PathBuf,
    pub providers: AdmittedProviders,
    pub grant: ToolGrant,
    pub approval_mode: ApprovalMode,
    pub include_paths: Vec<PathBuf>,
    pub policy: CollaborationPolicy,
    pub goal_budget: GoalBudget,
    pub resource_settings: ToolResourceSettings,
}

impl Threads {
    pub fn check_capacity(&self, id: Option<SessionId>) -> Result<(), Fault> {
        if self.map.len() >= MAX_THREADS && !id.is_some_and(|id| self.map.contains_key(&id)) {
            return Err(Fault::state(Reason::ThreadLimit));
        }
        Ok(())
    }

    pub async fn admit(
        &self,
        state_dir: &Path,
        params: &ThreadStartParams,
    ) -> Result<LoadedThread, Fault> {
        let provider = ProviderArg::from_str(&params.provider)
            .map_err(|_| Fault::selection("invalid Provider", Reason::InvalidSelection))?;
        if params.model.is_empty()
            || params.model.len() > 128
            || params.model.chars().any(char::is_control)
        {
            return Err(Fault::params());
        }
        match provider {
            ProviderArg::Openai | ProviderArg::Anthropic => {
                arany::resolve_native_effort_for_run(
                    &params.provider,
                    &params.model,
                    params.effort,
                )
                .map_err(|_| {
                    Fault::selection("invalid native model/effort", Reason::InvalidSelection)
                })?;
            }
            ProviderArg::Chatgpt => {
                arany::validate_native_model_id(&params.model).map_err(|_| Fault::params())?;
                if params.effort.is_none() {
                    return Err(Fault::params());
                }
            }
            ProviderArg::Custom(_) => {}
        }
        let policy = match (
            params
                .collaboration
                .as_ref()
                .unwrap_or(&Collaboration::Auto),
            params.max_active_children,
        ) {
            (Collaboration::Single, None) => CollaborationPolicy::Single,
            (Collaboration::Auto, count) if count.unwrap_or(3) <= 8 => CollaborationPolicy::Auto {
                max_active_children: count.unwrap_or(3),
            },
            (Collaboration::Team, count) if (1..=8).contains(&count.unwrap_or(3)) => {
                CollaborationPolicy::Team {
                    max_active_children: count.unwrap_or(3),
                }
            }
            _ => return Err(Fault::params()),
        };
        let (workspace, _) =
            crate::cli::admit_workspace(Some(PathBuf::from(&params.cwd)), Some(state_dir.into()))
                .map_err(|_| Fault::params())?;
        let grant = if params.tools.unwrap_or(false) {
            if !arany::native_protection_supported() {
                return Err(Fault::engine(arany::EngineError::GuardUnavailable));
            }
            ToolGrant::Configured
        } else {
            ToolGrant::None
        };
        let include_paths = params
            .include_paths
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|path| {
                Path::new(path)
                    .strip_prefix(&workspace)
                    .map(Path::to_path_buf)
                    .map_err(|_| Fault::params())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let primary = self
            .select(&provider, params, workspace.clone(), state_dir)
            .await?;
        if primary.identity().profile != params.provider || primary.identity().model != params.model
        {
            return Err(Fault::selection(
                "selected Provider changed before Workspace admission",
                Reason::ProviderRejected,
            ));
        }
        let providers = configured_providers(state_dir, &workspace, primary)
            .await
            .map_err(|_| {
                Fault::selection("Provider routing unavailable", Reason::ProviderUnavailable)
            })?;
        let (goal_budget, resource_settings) = execution_limits(state_dir)
            .await
            .map_err(|_| Fault::engine(arany::EngineError::InvalidRequest))?;
        Ok(LoadedThread {
            workspace,
            providers,
            grant,
            approval_mode: match params
                .permission_mode
                .as_ref()
                .unwrap_or(&PermissionMode::Default)
            {
                PermissionMode::Default => ApprovalMode::Request,
                PermissionMode::AcceptEdits => ApprovalMode::AutoEdits,
                PermissionMode::Auto => ApprovalMode::Auto,
            },
            include_paths,
            policy,
            goal_budget,
            resource_settings,
        })
    }

    async fn select(
        &self,
        provider: &ProviderArg,
        params: &ThreadStartParams,
        workspace: PathBuf,
        state: &Path,
    ) -> Result<std::sync::Arc<dyn arany::Provider>, Fault> {
        #[cfg(test)]
        if let Some(provider) = &self.provider {
            return Ok(provider.clone());
        }
        select_env_provider(provider, &params.model, params.effort, workspace, state)
            .await
            .map_err(|error| {
                use crate::cli::provider::select::SelectionError;
                let reason = match error {
                    SelectionError::InvalidLocalSelection(_)
                    | SelectionError::InvalidAnthropicWorkspace
                    | SelectionError::UnsupportedProfile => Reason::InvalidSelection,
                    _ => Reason::ProviderUnavailable,
                };
                Fault::selection(&error.to_string(), reason)
            })
    }
}

pub(super) async fn load_view(state: &Path, id: SessionId) -> Result<(SessionView, i64), Fault> {
    let root = StateRoot::open_existing(state)
        .map_err(arany::EngineError::from)
        .map_err(Fault::engine)?;
    let store = Store::open_read_only(root)
        .map_err(arany::EngineError::from)
        .map_err(Fault::engine)?;
    let result = async {
        let view = store
            .load_view(id)
            .await?
            .ok_or(arany::EngineError::MissingSession)?;
        let events = store.load_session(id).await?;
        let created = events
            .first()
            .ok_or(arany::EngineError::InvalidHistory)?
            .created_at_ms;
        Ok::<_, arany::EngineError>((view, created))
    }
    .await;
    let closed = store.close().await;
    let value = result.map_err(Fault::engine)?;
    closed
        .map_err(arany::EngineError::from)
        .map_err(Fault::engine)?;
    Ok(value)
}

pub(super) fn thread(view: &SessionView, created_at_ms: i64, workspace: &Path) -> Thread {
    Thread {
        id: view.id,
        title: view.conversation_title(),
        workspace: workspace.to_string_lossy().into_owned(),
        created_at_ms,
        forked_from_id: view
            .lineage
            .as_ref()
            .map(|lineage| lineage.source_session_id),
        goal: view.goals.last().map(items::goal_summary),
    }
}

pub(super) async fn start(
    engine: &Engine,
    state: &Path,
    threads: &mut Threads,
    params: ThreadStartParams,
) -> Result<ThreadResult, Fault> {
    threads.check_capacity(None)?;
    let loaded = threads.admit(state, &params).await?;
    let id = engine
        .create_session(loaded.workspace.clone(), params.title)
        .await
        .map_err(Fault::engine)?;
    let (view, created) = load_view(state, id).await?;
    let thread = thread(&view, created, &loaded.workspace);
    threads.map.insert(id, loaded);
    Ok(ThreadResult { thread })
}

pub(super) async fn resume(
    state: &Path,
    threads: &mut Threads,
    params: ThreadResumeParams,
) -> Result<ThreadResumeResult, Fault> {
    threads.check_capacity(Some(params.thread_id))?;
    let selection = ThreadStartParams {
        cwd: params.cwd,
        provider: params.provider,
        model: params.model,
        effort: params.effort,
        title: None,
        collaboration: None,
        max_active_children: None,
        tools: params.tools,
        permission_mode: params.permission_mode,
        include_paths: params.include_paths,
    };
    let loaded = threads.admit(state, &selection).await?;
    arany::resume_session(
        StateRoot::open_existing(state)
            .map_err(arany::EngineError::from)
            .map_err(Fault::engine)?,
        loaded.workspace.clone(),
        params.thread_id,
    )
    .await
    .map_err(Fault::engine)?;
    let (view, created) = load_view(state, params.thread_id).await?;
    let turns = view
        .runs
        .iter()
        .map(|run| {
            let goal = view
                .goals
                .iter()
                .find(|goal| goal.run_ids.contains(&run.id))
                .ok_or_else(|| Fault::engine(arany::EngineError::InvalidHistory))?;
            Ok(items::turn_summary(run, goal, true))
        })
        .collect::<Result<_, Fault>>()?;
    let thread = thread(&view, created, &loaded.workspace);
    threads.map.insert(params.thread_id, loaded);
    Ok(ThreadResumeResult { thread, turns })
}

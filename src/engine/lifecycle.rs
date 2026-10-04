use super::EngineError;
use crate::session::input::WorkspaceInputs;
use crate::session::{Event, RunStatus, SessionDefaults, SessionId, SessionListItem, SessionView};
use crate::store::{SessionRunLock, StateRoot, Store, StoreError};
use std::path::{Path, PathBuf};

pub async fn create_session(
    state: StateRoot,
    workspace: PathBuf,
    title: Option<String>,
) -> Result<SessionId, EngineError> {
    if title
        .as_ref()
        .is_some_and(|value| value.is_empty() || value.len() > 128)
    {
        return Err(EngineError::InvalidRequest);
    }
    let session_id = SessionId::new();
    let mut session_lock = lock_session(&state, session_id)?;
    let workspace_identity = WorkspaceInputs::admit_identity(&workspace, &state)?;
    let store = Store::open(state)?;
    let appended = store
        .append(
            session_id,
            Event::SessionStarted {
                title: title.unwrap_or_else(|| "New Session".into()),
                workspace_identity: Some(workspace_identity),
            },
        )
        .await;
    if appended.is_ok() {
        session_lock.keep();
    }
    let closed = store.close().await;
    appended?;
    closed?;
    drop(session_lock);
    Ok(session_id)
}

pub async fn set_session_defaults(
    state: StateRoot,
    workspace: PathBuf,
    session_id: SessionId,
    defaults: SessionDefaults,
) -> Result<(), EngineError> {
    defaults
        .validate()
        .map_err(|_| EngineError::InvalidRequest)?;
    let mut session_lock = lock_session(&state, session_id)?;
    let store = Store::open(state.try_clone()?)?;
    let result = async {
        let view = store
            .load_view(session_id)
            .await?
            .ok_or(EngineError::MissingSession)?;
        session_lock.keep();
        if view
            .runs
            .last()
            .is_some_and(|run| matches!(run.status, RunStatus::Pending | RunStatus::Active))
        {
            return Err(EngineError::SessionNotIdle);
        }
        let (device, inode) = WorkspaceInputs::admit_identity(&workspace, &state)?;
        ensure_session_workspace(&view, device, inode)?;
        store
            .append(session_id, Event::SessionDefaultChanged { defaults })
            .await?;
        Ok(())
    }
    .await;
    let closed = store.close().await;
    result?;
    closed?;
    drop(session_lock);
    Ok(())
}

pub async fn resume_session(
    state: StateRoot,
    workspace: PathBuf,
    session_id: SessionId,
) -> Result<SessionView, EngineError> {
    let mut session_lock = lock_session(&state, session_id)?;
    let store = Store::open_read_only(state.try_clone()?)?;
    let result: Result<SessionView, EngineError> = async {
        let view = store
            .load_view(session_id)
            .await?
            .ok_or(EngineError::MissingSession)?;
        session_lock.keep();
        let (device, inode) = WorkspaceInputs::admit_identity(&workspace, &state)?;
        ensure_session_workspace(&view, device, inode)?;
        Ok(view)
    }
    .await;
    let closed = store.close().await;
    let view = result?;
    closed?;
    drop(session_lock);
    Ok(view)
}

pub async fn continue_session(
    state: StateRoot,
    workspace: PathBuf,
) -> Result<SessionView, EngineError> {
    let workspace_identity = WorkspaceInputs::admit_identity(&workspace, &state)?;
    let store = Store::open_read_only(state.try_clone()?)?;
    let selected = store.latest_session_id(workspace_identity).await;
    let closed = store.close().await;
    let session_id = selected
        .map_err(|error| match error {
            StoreError::TooManySessions => EngineError::TooManySessions,
            other => EngineError::Store(other),
        })?
        .ok_or(EngineError::NoSessionForWorkspace)?;
    closed?;
    resume_session(state, workspace, session_id).await
}

pub async fn list_sessions(
    state: StateRoot,
    workspace: PathBuf,
) -> Result<Vec<SessionListItem>, EngineError> {
    let workspace_identity = WorkspaceInputs::admit_identity(&workspace, &state)?;
    let store = Store::open_read_only(state)?;
    let listed = store.list_sessions(workspace_identity).await;
    let closed = store.close().await;
    let items = listed.map_err(|error| match error {
        StoreError::TooManySessions => EngineError::TooManySessions,
        other => EngineError::Store(other),
    })?;
    closed?;
    Ok(items)
}

pub async fn rename_session(
    state: StateRoot,
    workspace: PathBuf,
    session_id: SessionId,
    title: String,
) -> Result<(), EngineError> {
    if title.is_empty() || title.len() > 128 {
        return Err(EngineError::InvalidRequest);
    }
    let mut session_lock = lock_session(&state, session_id)?;
    let store = Store::open(state.try_clone()?)?;
    let result: Result<(), EngineError> = async {
        let view = store
            .load_view(session_id)
            .await?
            .ok_or(EngineError::MissingSession)?;
        session_lock.keep();
        if view
            .runs
            .last()
            .is_some_and(|run| matches!(run.status, RunStatus::Pending | RunStatus::Active))
        {
            return Err(EngineError::SessionNotIdle);
        }
        let (device, inode) = WorkspaceInputs::admit_identity(&workspace, &state)?;
        ensure_session_workspace(&view, device, inode)?;
        store
            .append(session_id, Event::SessionRenamed { title })
            .await?;
        Ok(())
    }
    .await;
    let closed = store.close().await;
    result?;
    closed?;
    drop(session_lock);
    Ok(())
}

pub async fn fork_session(
    state: StateRoot,
    workspace: PathBuf,
    source_session_id: SessionId,
    title: Option<String>,
) -> Result<SessionId, EngineError> {
    validate_session_title(title.as_deref())?;
    let store = Store::open(state.try_clone()?)?;
    let result = fork_with_store(&state, &store, source_session_id, &workspace, title).await;
    let closed = store.close().await;
    let session_id = result?;
    closed?;
    Ok(session_id)
}

pub(super) async fn fork_with_store(
    state: &StateRoot,
    store: &Store,
    source_session_id: SessionId,
    workspace: &Path,
    title: Option<String>,
) -> Result<SessionId, EngineError> {
    validate_session_title(title.as_deref())?;
    let mut source_lock = lock_session(state, source_session_id)?;
    let source = store
        .load_view(source_session_id)
        .await?
        .ok_or(EngineError::MissingSession)?;
    source_lock.keep();
    let (device, inode) = WorkspaceInputs::admit_identity(workspace, state)?;
    ensure_session_workspace(&source, device, inode)?;
    if !source.runs.last().is_some_and(|run| {
        matches!(
            run.status,
            RunStatus::Finished | RunStatus::Failed | RunStatus::Cancelled
        )
    }) {
        return Err(EngineError::ForkNotAtRunBoundary);
    }
    let forked = store
        .fork_session(
            source_session_id,
            title.unwrap_or_else(|| "Forked Session".into()),
        )
        .await
        .map_err(EngineError::from);
    drop(source_lock);
    forked
}

fn validate_session_title(title: Option<&str>) -> Result<(), EngineError> {
    if title.is_some_and(|value| value.is_empty() || value.len() > 128) {
        return Err(EngineError::InvalidRequest);
    }
    Ok(())
}

pub(super) fn ensure_session_workspace(
    view: &SessionView,
    device: u64,
    inode: u64,
) -> Result<(), EngineError> {
    if view
        .workspace_identity
        .is_some_and(|identity| identity != (device, inode))
    {
        return Err(EngineError::WorkspaceMismatch);
    }
    Ok(())
}

pub(super) fn lock_session(
    state: &StateRoot,
    session_id: SessionId,
) -> Result<SessionRunLock, EngineError> {
    state.lock_session(session_id).map_err(|error| match error {
        StoreError::SessionBusy => EngineError::SessionBusy,
        other => EngineError::Store(other),
    })
}

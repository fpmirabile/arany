use super::*;

pub(super) fn selected(intent: &EffectIntent) -> bool {
    selection_phase::primary_command(intent)
        && intent.limits.resource_class == ToolResourceClass::Default
}

pub(super) async fn session(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    deadline: tokio::time::Instant,
) -> Result<Option<BuildCacheSession>, ToolError> {
    if !matches!(intent.call, ToolCall::Command { .. }) || intent.limits.cache_bytes == 0 {
        return Ok(None);
    }
    if runtime.worker.is_none()
        && let Some(state) = &runtime.cache_state
    {
        super::recover_build_cache(state, Some(&runtime.workspace))
            .await
            .map_err(super::map_cache_error)?;
    }
    let mut cache = super::build_cache_session(runtime, intent)?;
    if selected(intent)
        && let Some(cache) = &mut cache
    {
        tokio::time::timeout_at(deadline, prepare(cache))
            .await
            .map_err(|_| ToolError::Limit)??;
    }
    Ok(cache)
}

async fn prepare(cache: &mut BuildCacheSession) -> Result<(), ToolError> {
    let lease = Arc::clone(&cache.lease);
    let lower = tokio::task::spawn_blocking(move || lease.prepare_directory())
        .await
        .map_err(|_| ToolError::Operation)?
        .map_err(map_cache_error)?;
    cache.directory = Some(lower);
    Ok(())
}

pub(super) fn started(cache: Option<&BuildCacheSession>, id: uuid::Uuid) -> Result<(), ToolError> {
    if let Some(cache) = cache.filter(|cache| cache.directory.is_some()) {
        cache.lease.guard_started(id).map_err(map_cache_error)?;
    }
    Ok(())
}

pub(super) fn finished(cache: Option<&BuildCacheSession>) -> Result<(), ToolError> {
    if let Some(cache) = cache.filter(|cache| cache.directory.is_some()) {
        cache.lease.guard_finished().map_err(map_cache_error)?;
    }
    Ok(())
}

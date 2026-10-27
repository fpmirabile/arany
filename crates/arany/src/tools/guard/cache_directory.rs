use super::*;
use cap_std::fs::Dir;

pub(in crate::tools) struct Session {
    pub(super) lower: Dir,
    pub(super) upper: Option<Dir>,
}

pub(super) fn selected(intent: &EffectIntent) -> bool {
    selection_phase::primary_command(intent)
}

pub(super) async fn session(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
    deadline: tokio::time::Instant,
) -> Result<Option<BuildCacheSession>, ToolError> {
    if !matches!(intent.call, ToolCall::Command { .. }) {
        return Ok(None);
    }
    if intent.limits.cache_bytes == 0 {
        return if selected(intent)
            && intent.limits.cache_requested_bytes > 0
            && runtime.cache_state.is_some()
        {
            Err(ToolError::Limit)
        } else {
            Ok(None)
        };
    }
    if let Some(state) = &runtime.cache_state {
        super::recover_build_cache(state, Some(&runtime.workspace))
            .await
            .map_err(super::map_cache_error)?;
    }
    let runtime = runtime.clone();
    let intent = intent.clone();
    tokio::time::timeout_at(
        deadline,
        tokio::task::spawn_blocking(move || prepare(&runtime, &intent)),
    )
    .await
    .map_err(|_| ToolError::Limit)?
    .map_err(|_| ToolError::Operation)?
}

fn prepare(
    runtime: &ToolRuntime,
    intent: &EffectIntent,
) -> Result<Option<BuildCacheSession>, ToolError> {
    let mut cache = super::build_cache_session(runtime, intent)?;
    if selected(intent)
        && let Some(cache) = &mut cache
    {
        let lower = Arc::get_mut(&mut cache.lease)
            .ok_or(ToolError::Configuration)?
            .prepare_directory()
            .map_err(map_cache_error)?;
        cache.directory = Some(Session { lower, upper: None });
    }
    Ok(cache)
}

pub(super) fn publish(cache: BuildCacheSession) -> CacheUpdate {
    let Some(session) = cache.directory else {
        return CacheUpdate::Failed;
    };
    let mut lease = match Arc::try_unwrap(cache.lease) {
        Ok(lease) => lease,
        Err(lease) => {
            drop(session);
            drop(lease);
            return CacheUpdate::Uncertain;
        }
    };
    let Some(upper) = session.upper else {
        return CacheUpdate::Failed;
    };
    let changes = match super::super::fs::command::overlay::cache::capture(
        &session.lower,
        &upper,
        &cache.transfer.exclusions,
        std::time::Instant::now() + super::super::selection::PHASE_TIME,
        cache.transfer.candidate_bytes,
    ) {
        Ok(changes) => changes,
        Err(ToolError::Limit | ToolError::Timeout(_)) => return CacheUpdate::Rejected,
        Err(_) => return CacheUpdate::Failed,
    };
    let update =
        match lease.publish_directory(&session.lower, &upper, &changes, &cache.transfer.exclusions)
        {
            Ok(()) => CacheUpdate::Published,
            Err(_) if lease.publication_uncertain() => CacheUpdate::Uncertain,
            Err(StoreError::StorageFull) => CacheUpdate::Rejected,
            Err(_) => CacheUpdate::Failed,
        };
    drop(upper);
    drop(session.lower);
    drop(lease);
    update
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

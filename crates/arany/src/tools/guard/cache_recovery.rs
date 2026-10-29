use super::*;

pub async fn recover_build_cache(
    state: &crate::store::StateRoot,
    workspace: Option<&Path>,
) -> Result<(), StoreError> {
    {
        let state = state.try_clone()?;
        let workspace = workspace.map(Path::to_path_buf);
        let recoveries: Vec<crate::store::CacheGuardRecovery> =
            tokio::task::spawn_blocking(move || state.cache_guard_recoveries(workspace.as_deref()))
                .await
                .map_err(|_| StoreError::InvalidBuildCache)??;
        if !recoveries.is_empty() {
            let ids = recoveries
                .iter()
                .map(|recovery| recovery.id)
                .collect::<Vec<_>>();
            recover_private(&ids)
                .await
                .map_err(|_| StoreError::BuildCacheBusy)?;
            tokio::task::spawn_blocking(move || {
                recoveries
                    .into_iter()
                    .try_for_each(crate::store::CacheGuardRecovery::complete)
            })
            .await
            .map_err(|_| StoreError::InvalidBuildCache)??;
        }
    }
    Ok(())
}

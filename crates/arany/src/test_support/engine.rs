use crate::{
    AdmittedProviders, Engine, EngineError, Provider, RunControls, RunOutcome, RunRequest,
};
use std::sync::Arc;

/// Drives one default submission to its result.
pub async fn run_to_end<P: Provider + 'static>(
    engine: &Engine,
    provider: &Arc<P>,
    request: RunRequest,
) -> Result<RunOutcome, EngineError> {
    run_with(engine, provider, request, RunControls::default()).await
}

/// Drives one submission to its result; an autonomous stop after a committed Run fails the test.
pub async fn run_with<P: Provider + 'static>(
    engine: &Engine,
    provider: &Arc<P>,
    request: RunRequest,
    controls: RunControls,
) -> Result<RunOutcome, EngineError> {
    run_routed(
        engine,
        AdmittedProviders::single(provider.clone()),
        request,
        controls,
    )
    .await
}

/// Drives one submission with per-role Provider chains to its result.
pub async fn run_routed(
    engine: &Engine,
    providers: AdmittedProviders,
    request: RunRequest,
    controls: RunControls,
) -> Result<RunOutcome, EngineError> {
    let result = engine.submit(providers, request, controls)?.await?;
    if let Some(reason) = result.stopped {
        panic!("the Goal stopped after a committed Run: {reason}");
    }
    Ok(result.run)
}

use crate::{
    AdmittedProviders, Engine, EngineConfig, EngineError, Provider, RunControls, RunOutcome,
    RunRequest, RunStop, StateRoot,
};
use std::sync::Arc;

pub fn deadline(name: &str, duration: std::time::Duration) -> impl Drop {
    struct Deadline {
        stop: std::sync::mpsc::SyncSender<()>,
        worker: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for Deadline {
        fn drop(&mut self) {
            let _ = self.stop.send(());
            self.worker
                .take()
                .expect("deadline worker")
                .join()
                .expect("deadline worker joined");
        }
    }
    let (stop, stopped) = std::sync::mpsc::sync_channel(1);
    let name = name.to_owned();
    // Like the serve watchdog, this also bounds a paused or blocked runtime during native teardown.
    let worker = std::thread::spawn(move || {
        if stopped.recv_timeout(duration) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
            eprintln!(
                "{name} exceeded its {}-second wall-clock deadline",
                duration.as_secs()
            );
            std::process::abort();
        }
    });
    Deadline {
        stop,
        worker: Some(worker),
    }
}

pub fn open(state: StateRoot, config: EngineConfig) -> Result<Engine, EngineError> {
    super::isolate_account_root();
    Engine::open(state, config)
}

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
    // A Goal left open after a Finished Run is an ordinary outcome; exhaustion or an unknown later Run is not.
    if let Some(error) = result.continuation_error {
        panic!("the Goal stopped after a committed Run: {error}");
    }
    if result.run.run.status == crate::RunStatus::Finished
        && let Some(
            stop @ (RunStop::GoalExhausted(
                crate::GoalLimit::Calls
                | crate::GoalLimit::Runs
                | crate::GoalLimit::ActiveTime
                | crate::GoalLimit::Tokens,
            )
            | RunStop::Uncertain
            | RunStop::Interrupted),
        ) = result.stopped
    {
        panic!("the Goal stopped after a committed Run: {stop:?}");
    }
    Ok(result.run)
}

pub fn record_tool_latency(engine: &Engine) -> Arc<super::latency::Recorder> {
    engine.record_tool_latency()
}

use super::EngineError;
use crate::session::{Event, EventEnvelope, RunId, RunView, SessionId, SessionView};
use crate::store::Store;
use crate::telemetry::{RunTrace, TraceEventKind};
use std::sync::Mutex;
use tokio::sync::watch;

/// Latest committed Run snapshot; older display snapshots may be coalesced.
#[derive(Clone)]
pub struct RunProgress {
    sender: watch::Sender<Option<RunUpdate>>,
    receiver: watch::Receiver<Option<RunUpdate>>,
}

/// An acknowledged Event sequence and the Run state reduced through it.
#[derive(Clone)]
pub struct RunUpdate {
    pub sequence: u64,
    pub run: RunView,
}

impl RunProgress {
    #[must_use]
    pub fn new() -> Self {
        let (sender, receiver) = watch::channel(None);
        Self { sender, receiver }
    }

    /// Waits for a newer committed snapshot and returns the latest one.
    pub async fn changed(&mut self) -> RunUpdate {
        loop {
            self.receiver
                .changed()
                .await
                .expect("RunProgress retains a sender");
            if let Some(run) = self.receiver.borrow_and_update().clone() {
                return run;
            }
        }
    }

    fn publish(&self, update: RunUpdate) {
        self.sender.send_replace(Some(update));
    }
}

impl Default for RunProgress {
    fn default() -> Self {
        Self::new()
    }
}

pub(super) struct ProgressPublisher {
    view: Mutex<SessionView>,
    progress: RunProgress,
    run_id: RunId,
}

impl ProgressPublisher {
    pub(super) fn new(view: SessionView, progress: RunProgress, run_id: RunId) -> Self {
        Self {
            view: Mutex::new(view),
            progress,
            run_id,
        }
    }

    fn committed(&self, envelope: &EventEnvelope) -> Result<(), EngineError> {
        let update = {
            let mut view = self
                .view
                .lock()
                .map_err(|_| EngineError::CoordinatorFailed)?;
            view.apply_committed(envelope)
                .map_err(|_| EngineError::InvalidHistory)?;
            view.runs
                .last()
                .filter(|run| run.id == self.run_id)
                .cloned()
                .map(|run| RunUpdate {
                    sequence: envelope.sequence,
                    run,
                })
        };
        if let Some(update) = update {
            self.progress.publish(update);
        }
        Ok(())
    }
}

pub(super) async fn append_observed(
    store: &Store,
    progress: Option<&ProgressPublisher>,
    trace: Option<&RunTrace>,
    session_id: SessionId,
    event: Event,
) -> Result<u64, EngineError> {
    let kind = match &event {
        Event::MessageAccepted { .. } => TraceEventKind::MessageAccepted,
        Event::RunStarted { .. } => TraceEventKind::RunStarted,
        Event::AgentSpawned { .. } => TraceEventKind::AgentSpawned,
        Event::ProviderCallRecorded { .. } => TraceEventKind::ProviderCallRecorded,
        Event::ToolStarted { .. } => TraceEventKind::ToolStarted,
        Event::ToolFinished { .. } => TraceEventKind::ToolFinished,
        Event::AgentFinished { .. } => TraceEventKind::AgentFinished,
        Event::MessageCommitted { .. } => TraceEventKind::MessageCommitted,
        Event::RunFinished { .. } => TraceEventKind::RunFinished,
        _ => return Err(EngineError::CoordinatorFailed),
    };
    let committed = store.append(session_id, event).await?;
    crate::diagnostics::event_failure(&committed.event);
    if let Some(trace) = trace {
        trace.event_committed(committed.sequence, kind);
    }
    if let Some(progress) = progress {
        progress.committed(&committed)?;
    }
    Ok(committed.sequence)
}

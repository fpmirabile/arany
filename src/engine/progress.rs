use super::EngineError;
use crate::provider::PreviewText;
use crate::session::{
    AgentRunId, Event, EventEnvelope, GoalView, RunId, RunView, SessionId, SessionView,
};
use crate::store::Store;
use crate::telemetry::{RunTrace, TraceEventKind};
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::watch;

/// Committed Run snapshots and separate temporary display and Tool capacity hints.
#[derive(Clone)]
pub struct RunProgress {
    sender: watch::Sender<Option<RunUpdate>>,
    receiver: watch::Receiver<Option<RunUpdate>>,
    waiting_sender: watch::Sender<Option<uuid::Uuid>>,
    waiting_receiver: watch::Receiver<Option<uuid::Uuid>>,
    preview_sender: watch::Sender<Option<DisplayUpdate>>,
    preview_receiver: watch::Receiver<Option<DisplayUpdate>>,
    previews_enabled: bool,
}

#[derive(Clone)]
pub struct DisplayUpdate {
    pub run_id: RunId,
    pub agent_run_id: AgentRunId,
    pub call_index: u8,
    pub text: Arc<PreviewText>,
}

pub struct RunPreview {
    receiver: watch::Receiver<Option<DisplayUpdate>>,
    _sender: watch::Sender<Option<DisplayUpdate>>,
}

impl RunPreview {
    pub async fn changed(&mut self) -> Option<DisplayUpdate> {
        self.receiver
            .changed()
            .await
            .expect("preview owner retains sender");
        self.receiver.borrow_and_update().clone()
    }
}

/// An acknowledged Event sequence, the Run state and the Session's Goals reduced through it.
#[derive(Clone)]
pub struct RunUpdate {
    pub sequence: u64,
    pub run: RunView,
    pub goals: Vec<GoalView>,
}

impl RunProgress {
    #[must_use]
    pub fn new() -> Self {
        let (sender, receiver) = watch::channel(None);
        let (waiting_sender, waiting_receiver) = watch::channel(None);
        let (preview_sender, preview_receiver) = watch::channel(None);
        Self {
            sender,
            receiver,
            waiting_sender,
            waiting_receiver,
            preview_sender,
            preview_receiver,
            previews_enabled: false,
        }
    }

    pub fn with_previews() -> Self {
        Self {
            previews_enabled: true,
            ..Self::new()
        }
    }

    pub fn previews(&self) -> RunPreview {
        RunPreview {
            receiver: self.preview_receiver.clone(),
            _sender: self.preview_sender.clone(),
        }
    }

    pub(super) fn previews_enabled(&self) -> bool {
        self.previews_enabled
    }

    /// Latest temporary capacity wait, keyed to a committed pending Tool; never replayed.
    pub async fn waiting_changed(&mut self) -> Option<uuid::Uuid> {
        self.waiting_receiver
            .changed()
            .await
            .expect("RunProgress retains a sender");
        *self.waiting_receiver.borrow_and_update()
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

pub(super) struct PreviewContext {
    pub(super) call_index: u8,
    pub(super) chunks: usize,
    pub(super) commentary_bytes: usize,
    pub(super) commentary_items: usize,
    pub(super) limit_recorded: bool,
}

pub(super) struct ProgressPublisher {
    view: Mutex<SessionView>,
    progress: RunProgress,
    run_id: RunId,
}

impl ProgressPublisher {
    pub(super) fn tool_queue(&self) -> watch::Sender<Option<uuid::Uuid>> {
        self.progress.waiting_sender.clone()
    }

    pub(super) fn preview_context(
        &self,
        primary: AgentRunId,
    ) -> Result<Option<PreviewContext>, EngineError> {
        if !self.progress.previews_enabled {
            return Ok(None);
        }
        let view = self
            .view
            .lock()
            .map_err(|_| EngineError::CoordinatorFailed)?;
        let run = view
            .runs
            .last()
            .filter(|run| run.id == self.run_id)
            .ok_or(EngineError::CoordinatorFailed)?;
        let agent = run
            .agents
            .first()
            .filter(|agent| agent.id == primary)
            .ok_or(EngineError::CoordinatorFailed)?;
        let mut items = std::collections::HashSet::new();
        let commentary_bytes = run
            .display
            .iter()
            .filter(|chunk| chunk.category == crate::session::DisplayCategory::Commentary)
            .map(|chunk| {
                items.insert((chunk.call_index, chunk.item));
                chunk.text.len()
            })
            .sum();
        Ok(Some(PreviewContext {
            call_index: u8::try_from(agent.provider_calls.len())
                .map_err(|_| EngineError::CoordinatorFailed)?,
            chunks: run.display.len(),
            commentary_bytes,
            commentary_items: items.len(),
            limit_recorded: run
                .display
                .iter()
                .any(|chunk| chunk.category == crate::session::DisplayCategory::Limit),
        }))
    }

    pub(super) fn publish_preview(
        &self,
        agent_run_id: AgentRunId,
        call_index: u8,
        text: Option<Arc<PreviewText>>,
    ) {
        self.progress
            .preview_sender
            .send_replace(text.map(|text| DisplayUpdate {
                run_id: self.run_id,
                agent_run_id,
                call_index,
                text,
            }));
    }
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
            let goals = view.goals.clone();
            view.runs
                .last()
                .filter(|run| run.id == self.run_id)
                .cloned()
                .map(|run| RunUpdate {
                    sequence: envelope.sequence,
                    run,
                    goals,
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
        Event::GoalStarted { .. } => Some(TraceEventKind::GoalStarted),
        Event::GoalRunLinked { .. } => Some(TraceEventKind::GoalRunLinked),
        Event::GoalResumed { .. } => Some(TraceEventKind::GoalResumed),
        Event::GoalActiveTimeReserved { .. } => Some(TraceEventKind::GoalActiveTimeReserved),
        Event::GoalActiveTimeSettled { .. } => Some(TraceEventKind::GoalActiveTimeSettled),
        Event::GoalPlanContractRecorded { .. } => Some(TraceEventKind::GoalPlanContractRecorded),
        Event::GoalFinished { .. } => Some(TraceEventKind::GoalFinished),
        Event::GoalProviderCallReserved { .. } => Some(TraceEventKind::GoalProviderCallReserved),
        Event::MessageAccepted { .. } => Some(TraceEventKind::MessageAccepted),
        Event::RunStarted { .. } => Some(TraceEventKind::RunStarted),
        Event::AgentSpawned { .. } => Some(TraceEventKind::AgentSpawned),
        Event::ProviderCallRecorded { .. } => Some(TraceEventKind::ProviderCallRecorded),
        Event::ToolStarted { .. } => Some(TraceEventKind::ToolStarted),
        Event::ToolFinished { .. } => Some(TraceEventKind::ToolFinished),
        Event::AgentFinished { .. } => Some(TraceEventKind::AgentFinished),
        Event::MessageCommitted { .. } => Some(TraceEventKind::MessageCommitted),
        Event::RunFinished { .. } => Some(TraceEventKind::RunFinished),
        Event::AssistantDisplayRecorded { .. }
        | Event::SessionStarted { .. }
        | Event::SessionDefaultChanged { .. }
        | Event::SessionForked { .. }
        | Event::ContextCompacted { .. }
        | Event::SessionRenamed { .. } => None,
    };
    let committed = store.append(session_id, event).await?;
    crate::diagnostics::event_failure(&committed.event);
    if let (Some(trace), Some(kind)) = (trace, kind) {
        trace.event_committed(committed.sequence, kind);
    }
    if let Some(progress) = progress {
        progress.committed(&committed)?;
    }
    Ok(committed.sequence)
}

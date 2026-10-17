use super::EngineError;
use super::ledger::GoalLedger;
use crate::journal::Subscription;
use crate::provider::PreviewText;
use crate::session::{AgentRunId, GoalView, RunId, RunView, SessionView};
use std::future::Future;
use std::sync::Arc;
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
    pub call_index: u32,
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

/// An acknowledged Event sequence, the Run state and the Goal that owns the Run, reduced through it.
#[derive(Clone)]
pub struct RunUpdate {
    pub sequence: u64,
    pub run: RunView,
    pub goal: Option<GoalView>,
}

impl RunProgress {
    pub(super) fn previews_enabled(&self) -> bool {
        self.previews_enabled
    }

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
    pub(super) call_index: u32,
    pub(super) chunks: usize,
    pub(super) commentary_bytes: usize,
    pub(super) commentary_items: usize,
    pub(super) limit_recorded: bool,
}

pub(super) struct ProgressPublisher {
    progress: RunProgress,
    run_id: RunId,
}

impl ProgressPublisher {
    pub(super) fn new(progress: RunProgress, run_id: RunId) -> Self {
        Self { progress, run_id }
    }

    pub(super) fn previews_enabled(&self) -> bool {
        self.progress.previews_enabled()
    }

    pub(super) fn tool_queue(&self) -> watch::Sender<Option<uuid::Uuid>> {
        self.progress.waiting_sender.clone()
    }

    pub(super) fn preview_context(
        &self,
        ledger: &GoalLedger,
        primary: AgentRunId,
    ) -> Result<Option<PreviewContext>, EngineError> {
        if !self.previews_enabled() {
            return Ok(None);
        }
        ledger.view(|view| {
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
                call_index: u32::try_from(agent.provider_calls.len())
                    .map_err(|_| EngineError::CoordinatorFailed)?,
                chunks: run.display.len(),
                commentary_bytes,
                commentary_items: items.len(),
                limit_recorded: run
                    .display
                    .iter()
                    .any(|chunk| chunk.category == crate::session::DisplayCategory::Limit),
            }))
        })?
    }

    pub(super) fn publish_preview(
        &self,
        agent_run_id: AgentRunId,
        call_index: u32,
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

    /// Publishes the ledger's view after each tapped Event. The run-scoped append feeds the ledger
    /// before the tap, so the view already holds every Event received, and a Resync recovers from it.
    pub(super) async fn follow(
        &self,
        subscription: Subscription,
        end: impl Future<Output = ()>,
        ledger: &GoalLedger,
    ) {
        let mut published = None;
        subscription
            .follow(end, |_| {
                let Ok(Some(update)) = ledger.view(|view| self.project(view)) else {
                    return;
                };
                if published.is_none_or(|sequence| update.sequence > sequence) {
                    published = Some(update.sequence);
                    self.progress.publish(update);
                }
            })
            .await;
    }

    fn project(&self, view: &SessionView) -> Option<RunUpdate> {
        view.runs
            .last()
            .filter(|run| run.id == self.run_id)
            .cloned()
            .map(|run| RunUpdate {
                sequence: view.last_sequence,
                goal: view
                    .goals
                    .iter()
                    .find(|goal| goal.run_ids.contains(&self.run_id))
                    .cloned(),
                run,
            })
    }
}

use super::EngineError;
use super::ledger::GoalLedger;
use crate::provider::PreviewText;
use crate::session::{AgentRunId, RunId};
use std::sync::Arc;
use tokio::sync::watch;

/// Temporary display and Tool capacity hints, separate from committed Events.
#[derive(Clone)]
pub struct RunHints {
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

impl RunHints {
    pub(super) fn previews_enabled(&self) -> bool {
        self.previews_enabled
    }

    #[must_use]
    pub fn new() -> Self {
        let (waiting_sender, waiting_receiver) = watch::channel(None);
        let (preview_sender, preview_receiver) = watch::channel(None);
        Self {
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
            .expect("RunHints retains a sender");
        *self.waiting_receiver.borrow_and_update()
    }
}

impl Default for RunHints {
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

pub(super) struct HintPublisher {
    hints: RunHints,
    run_id: RunId,
}

impl HintPublisher {
    pub(super) fn new(hints: RunHints, run_id: RunId) -> Self {
        Self { hints, run_id }
    }

    pub(super) fn previews_enabled(&self) -> bool {
        self.hints.previews_enabled()
    }

    pub(super) fn tool_queue(&self) -> watch::Sender<Option<uuid::Uuid>> {
        self.hints.waiting_sender.clone()
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
        self.hints
            .preview_sender
            .send_replace(text.map(|text| DisplayUpdate {
                run_id: self.run_id,
                agent_run_id,
                call_index,
                text,
            }));
    }
}

use super::BoxedStep;
use crate::engine::EngineError;
use crate::engine::run_loop::RunLoop;
use crate::tools::{EffectIntent, ToolError, ToolRuntime};
use std::time::Duration;
use tokio::time::Instant;

pub(super) const CHECKPOINT_WAIT: Duration = Duration::from_secs(3);
pub(super) const CHECKPOINT_GO_MARGIN: Duration = Duration::from_secs(1);

impl RunLoop<'_> {
    pub(super) fn capture_checkpoint<'a>(
        &'a self,
        runtime: &'a ToolRuntime,
        intent: &'a EffectIntent,
        deadline: Instant,
    ) -> BoxedStep<'a, crate::session::Checkpoint> {
        Box::pin(async move {
            use crate::session::{Checkpoint, CheckpointReason};
            let paths = self.ledger().view(|view| {
                let run = view
                    .runs
                    .iter()
                    .find(|run| run.id == self.run_id)
                    .ok_or(EngineError::InvalidHistory)?;
                let paths = crate::session::checkpoint::before_versions(run, &intent.call)
                    .into_iter()
                    .filter_map(|(path, before)| {
                        before
                            .filter(|digest| {
                                crate::session::checkpoint::capture_needed(
                                    view,
                                    self.run_id,
                                    &path,
                                    digest,
                                )
                            })
                            .map(|digest| (path, digest))
                    })
                    .collect::<Vec<_>>();
                Ok::<_, EngineError>(paths)
            })??;
            if paths.is_empty() {
                return Ok(Checkpoint::not_needed());
            }
            let intent_id = intent.id;
            let (runtime, intent) = (runtime.clone(), intent.clone());
            let capture =
                tokio::task::spawn_blocking(move || runtime.checkpoint_bytes(&intent, &paths));
            let files = match tokio::time::timeout_at(deadline, capture).await {
                Ok(Ok(Ok(files))) => files,
                Ok(Ok(Err(ToolError::ChangedInput | ToolError::Path))) => {
                    return Ok(Checkpoint::skipped(CheckpointReason::DigestMismatch));
                }
                Ok(Ok(Err(ToolError::Limit))) => {
                    return Ok(Checkpoint::skipped(CheckpointReason::TooLarge));
                }
                _ => return Ok(Checkpoint::skipped(CheckpointReason::StoreUnavailable)),
            };
            let capture = crate::store::CheckpointCapture {
                session: self.session_id,
                owner: crate::store::CheckpointOwner::Tool {
                    goal: self.goal()?,
                    run: self.run_id,
                    intent_id,
                },
                files,
            };
            let stalled = crate::fault::checkpoint_stall(self.session_id);
            let store = self.journal.store();
            let stored = async {
                stalled.wait().await;
                store.capture_checkpoint(capture).await
            };
            let checkpoint = match tokio::time::timeout_at(deadline, stored).await {
                Ok(Ok(checkpoint)) => checkpoint,
                _ => Checkpoint::skipped(CheckpointReason::StoreUnavailable),
            };
            crate::fault::checkpoint_captured(self.session_id, &checkpoint, deadline).await;
            Ok(checkpoint)
        })
    }
}

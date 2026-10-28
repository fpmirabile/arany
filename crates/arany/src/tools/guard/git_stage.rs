use super::super::ToolError;
use super::super::spill::report_cap;
use super::super::types::{CacheUpdate, EffectIntent, ToolObservation};
use super::{Task, parse_observation, read_line_capped, transfer};
use crate::tools::git::WRITE_FRAME_BYTES;
use serde::{Deserialize, Serialize};
use std::io::Read;
use tokio::io::AsyncBufReadExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::tools) enum GitStage {
    Plan,
    Publish,
}

pub(in crate::tools) struct GitWork {
    pub(in crate::tools) stage: Option<GitStage>,
    pub(in crate::tools) input: Option<(Vec<u8>, Vec<u8>)>,
}

// An executor's report may be followed by its plan frame; the flag says whether bytes remain.
pub(super) async fn read_plan_report<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    intent: &EffectIntent,
) -> Result<(ToolObservation, bool), ToolError> {
    let bytes = read_line_capped(reader, report_cap(intent)).await?;
    let observation = parse_observation(&bytes)?;
    if observation.cache_update != CacheUpdate::NotApplicable || !observation.attested_for(intent) {
        return Err(ToolError::Operation);
    }
    let framed = !AsyncBufReadExt::fill_buf(reader)
        .await
        .map_err(|_| ToolError::Operation)?
        .is_empty();
    Ok((observation, framed))
}

// A publisher receives the executor's result text, then its plan frame, before GO.
pub(super) fn receive_git_input(
    task: &Task,
    stdin: &mut std::io::BufReader<impl Read>,
) -> Result<GitWork, ToolError> {
    let stage = task.git_stage;
    #[cfg(any(test, feature = "test-support"))]
    if task.git_fault == Some(crate::fault::git_write::GitWriteStage::Published) {
        crate::fault::git_write::arm_death(task.intent.id);
    }
    if stage != Some(GitStage::Publish) {
        return Ok(GitWork { stage, input: None });
    }
    let result = transfer::read_frame(stdin, task.intent.limits.result_bytes as usize)?;
    let frame = transfer::read_frame(stdin, WRITE_FRAME_BYTES)?;
    Ok(GitWork {
        stage,
        input: Some((result, frame)),
    })
}

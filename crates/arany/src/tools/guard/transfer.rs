use crate::session::worker::*;
use crate::tools::{Config, EffectIntent, ToolCall, ToolError};
use crate::validate::digest;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

pub(in crate::tools) const WORKER_PREPARATION_MS: u32 = 120_000;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tools) struct PublicationTransfer {
    bytes: u32,
    sha256: [u8; 32],
}

pub(in crate::tools) fn handover_ref(intent: &EffectIntent) -> Option<PublicationTransfer> {
    match &intent.call {
        ToolCall::ApplyChangeset {
            source: ChangesetSource::Rewind { bytes, sha256, .. },
        } if intent.target == EffectTarget::Workspace => Some(PublicationTransfer {
            bytes: *bytes,
            sha256: *sha256,
        }),
        _ => None,
    }
}

pub(in crate::tools) fn decode_handover(
    intent: &EffectIntent,
    bytes: &[u8],
    config: &Config,
) -> Result<WorkerChangeset, ToolError> {
    let data = handover_ref(intent).ok_or(ToolError::Configuration)?;
    if !intent.valid()
        || !config.write
        || bytes.len() != data.bytes as usize
        || digest(bytes) != data.sha256
    {
        return Err(ToolError::ChangedInput);
    }
    crate::session::worker::decode_rewind_data(bytes).map_err(|_| ToolError::Limit)?;
    let changes: WorkerChangeset =
        serde_json::from_slice(bytes).map_err(|_| ToolError::Configuration)?;
    if changes.files.is_empty()
        || changes
            .files
            .iter()
            .any(|f| !config.in_workspace_scope(&f.path))
    {
        return Err(ToolError::Path);
    }
    Ok(changes)
}

pub(crate) fn transfer_time(intent: &EffectIntent) -> std::time::Duration {
    if handover_ref(intent).is_some() {
        std::time::Duration::from_millis(u64::from(
            intent.limits.runtime_ms.min(WORKER_PREPARATION_MS),
        ))
    } else {
        std::time::Duration::ZERO
    }
}

pub(in crate::tools) fn encode_frame(bytes: &[u8], max_bytes: usize) -> Result<Vec<u8>, ToolError> {
    if bytes.is_empty() || bytes.len() > max_bytes {
        return Err(ToolError::Limit);
    }
    let mut frame =
        Vec::with_capacity(bytes.len() + 40 + 4 * bytes.len().div_ceil(WORKER_CHUNK_BYTES));
    frame.extend_from_slice(
        &u32::try_from(bytes.len())
            .map_err(|_| ToolError::Limit)?
            .to_le_bytes(),
    );
    frame.extend_from_slice(&digest(bytes));
    for chunk in bytes.chunks(WORKER_CHUNK_BYTES) {
        frame.extend_from_slice(
            &u32::try_from(chunk.len())
                .map_err(|_| ToolError::Limit)?
                .to_le_bytes(),
        );
        frame.extend_from_slice(chunk);
    }
    frame.extend_from_slice(&0u32.to_le_bytes());
    Ok(frame)
}

pub(in crate::tools) fn read_frame(
    reader: &mut impl std::io::Read,
    max_bytes: usize,
) -> Result<Vec<u8>, ToolError> {
    let mut size = [0; 4];
    reader
        .read_exact(&mut size)
        .map_err(|_| ToolError::Operation)?;
    let size = u32::from_le_bytes(size) as usize;
    if size == 0 || size > max_bytes {
        return Err(ToolError::Limit);
    }
    let mut hash = [0; 32];
    reader
        .read_exact(&mut hash)
        .map_err(|_| ToolError::Operation)?;
    let mut bytes = Vec::with_capacity(size);
    while bytes.len() < size {
        let mut count = [0; 4];
        reader
            .read_exact(&mut count)
            .map_err(|_| ToolError::Operation)?;
        let count = u32::from_le_bytes(count) as usize;
        if count != (size - bytes.len()).min(WORKER_CHUNK_BYTES) {
            return Err(ToolError::Configuration);
        }
        let start = bytes.len();
        bytes.resize(start + count, 0);
        reader
            .read_exact(&mut bytes[start..])
            .map_err(|_| ToolError::Operation)?;
    }
    let mut end = [0; 4];
    reader
        .read_exact(&mut end)
        .map_err(|_| ToolError::Operation)?;
    if end != [0; 4] || digest(&bytes) != hash {
        return Err(ToolError::ChangedInput);
    }
    Ok(bytes)
}

pub(in crate::tools) async fn send_frame(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    bytes: &[u8],
) -> Result<(), ToolError> {
    writer
        .write_all(&encode_frame(bytes, WORKER_DATA_BYTES)?)
        .await
        .map_err(|_| ToolError::Operation)
}

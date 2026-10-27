use crate::session::worker::*;
use crate::tools::{Config, EffectIntent, ToolCall, ToolError};
use crate::validate::digest;
use serde::{Deserialize, Serialize};
#[cfg(any(target_os = "macos", test))]
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

pub(in crate::tools) const WORKER_PREPARATION_MS: u32 = 120_000;

pub(crate) async fn recover_private(ids: &[uuid::Uuid]) -> Result<(), ToolError> {
    super::recover_private(ids).await
}

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

pub(in crate::tools) fn write_frame(
    writer: &mut impl std::io::Write,
    bytes: &[u8],
    max_bytes: usize,
) -> Result<(), ToolError> {
    writer
        .write_all(&encode_frame(bytes, max_bytes)?)
        .map_err(|_| ToolError::Operation)
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
    send_frame_capped(writer, bytes, WORKER_DATA_BYTES).await
}

pub(in crate::tools) async fn send_frame_capped(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    bytes: &[u8],
    max_bytes: usize,
) -> Result<(), ToolError> {
    writer
        .write_all(&encode_frame(bytes, max_bytes)?)
        .await
        .map_err(|_| ToolError::Operation)
}

#[cfg(target_os = "macos")]
pub(in crate::tools) async fn receive_frame_capped(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    max_bytes: usize,
) -> Result<Vec<u8>, ToolError> {
    let size = reader
        .read_u32_le()
        .await
        .map_err(|_| ToolError::Operation)? as usize;
    if size == 0 || size > max_bytes {
        return Err(ToolError::Limit);
    }
    let mut hash = [0; 32];
    reader
        .read_exact(&mut hash)
        .await
        .map_err(|_| ToolError::Operation)?;
    let mut bytes = Vec::with_capacity(size);
    while bytes.len() < size {
        let count = reader
            .read_u32_le()
            .await
            .map_err(|_| ToolError::Operation)? as usize;
        if count != (size - bytes.len()).min(WORKER_CHUNK_BYTES) {
            return Err(ToolError::Configuration);
        }
        let start = bytes.len();
        bytes.resize(start + count, 0);
        reader
            .read_exact(&mut bytes[start..])
            .await
            .map_err(|_| ToolError::Operation)?;
    }
    if reader
        .read_u32_le()
        .await
        .map_err(|_| ToolError::Operation)?
        != 0
        || digest(&bytes) != hash
    {
        return Err(ToolError::ChangedInput);
    }
    Ok(bytes)
}

// Forwards one frame segment by segment (header, each chunk, end marker) with the receiver holding at most one segment, and verifies its hash and length before returning.
#[cfg(any(target_os = "macos", test))]
pub(in crate::tools) async fn relay_frame(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    max_bytes: usize,
    segments: &tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Result<(), ToolError> {
    use sha2::{Digest, Sha256};

    let forward = |segment: Vec<u8>| async {
        segments
            .send(segment)
            .await
            .map_err(|_| ToolError::Operation)
    };
    let size = reader
        .read_u32_le()
        .await
        .map_err(|_| ToolError::Operation)? as usize;
    if size == 0 || size > max_bytes {
        return Err(ToolError::Limit);
    }
    let mut hash = [0; 32];
    reader
        .read_exact(&mut hash)
        .await
        .map_err(|_| ToolError::Operation)?;
    let mut header = (size as u32).to_le_bytes().to_vec();
    header.extend_from_slice(&hash);
    forward(header).await?;
    let mut hasher = Sha256::new();
    let mut seen = 0;
    while seen < size {
        let count = reader
            .read_u32_le()
            .await
            .map_err(|_| ToolError::Operation)? as usize;
        if count != (size - seen).min(WORKER_CHUNK_BYTES) {
            return Err(ToolError::Configuration);
        }
        let mut segment = (count as u32).to_le_bytes().to_vec();
        segment.resize(4 + count, 0);
        reader
            .read_exact(&mut segment[4..])
            .await
            .map_err(|_| ToolError::Operation)?;
        hasher.update(&segment[4..]);
        seen += count;
        forward(segment).await?;
    }
    if reader
        .read_u32_le()
        .await
        .map_err(|_| ToolError::Operation)?
        != 0
        || <[u8; 32]>::from(hasher.finalize()) != hash
    {
        return Err(ToolError::ChangedInput);
    }
    forward(0u32.to_le_bytes().to_vec()).await
}

#[cfg(test)]
mod relay_tests;

use crate::session::worker::*;
use crate::tools::{
    Attestation, Config, EffectIntent, FileChange, FileVersion, ToolCall, ToolDisposition,
    ToolError, ToolObservation, ToolResourceClass, ToolRuntime, fs, types::*,
};
use crate::validate::{digest, hex_digest, valid_relative};
use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(in crate::tools) const WORKER_PREPARATION_MS: u32 = 120_000;

pub(crate) async fn recover_private(ids: &[uuid::Uuid]) -> Result<(), ToolError> {
    super::recover_private(ids).await
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::tools) enum Entry {
    Directory,
    File {
        content: Arc<[u8]>,
        executable: bool,
    },
    Absent,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum EncodedEntry {
    Directory,
    File { version: FileVersion },
    Absent,
}

impl Entry {
    pub(in crate::tools) fn file(content: &[u8], executable: bool) -> Self {
        Self::File {
            content: content.into(),
            executable,
        }
    }
    fn version(&self) -> Option<FileVersion> {
        match self {
            Self::File {
                content,
                executable,
            } => Some(FileVersion::from_bytes(content, *executable)),
            _ => None,
        }
    }
    pub(in crate::tools) fn bytes(&self) -> usize {
        match self {
            Self::File { content, .. } => content.len(),
            _ => 0,
        }
    }
}

impl Serialize for Entry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Directory => EncodedEntry::Directory,
            Self::Absent => EncodedEntry::Absent,
            Self::File { .. } => EncodedEntry::File {
                version: self.version().expect("file entry"),
            },
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Entry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match EncodedEntry::deserialize(deserializer)? {
            EncodedEntry::Directory => Self::Directory,
            EncodedEntry::Absent => Self::Absent,
            EncodedEntry::File { version } => Self::file(
                &version
                    .bytes_with_limit(MAX_FILE_BYTES)
                    .ok_or_else(|| serde::de::Error::custom("invalid private file version"))?,
                version.executable,
            ),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tools) struct Image {
    #[serde(deserialize_with = "bounded_entries")]
    pub(in crate::tools) entries: BTreeMap<String, Entry>,
}

fn bounded_entries<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, Entry>, D::Error> {
    struct Entries;
    impl<'de> serde::de::Visitor<'de> for Entries {
        type Value = BTreeMap<String, Entry>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("at most 2048 unique snapshot entries")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut entries = BTreeMap::new();
            let mut bytes = 0usize;
            while let Some(path) = map.next_key::<String>()? {
                if entries.len() >= WORKER_FILES
                    || entries.contains_key(&path)
                    || !valid_relative(&path, false)
                {
                    return Err(serde::de::Error::custom("invalid snapshot entry"));
                }
                let entry: Entry = map.next_value()?;
                bytes = bytes
                    .checked_add(entry.bytes())
                    .ok_or_else(|| serde::de::Error::custom("private byte overflow"))?;
                if bytes > WORKER_CONTENT_BYTES {
                    return Err(serde::de::Error::custom("private byte bound exceeded"));
                }
                entries.insert(path, entry);
            }
            Ok(entries)
        }
    }
    deserializer.deserialize_map(Entries)
}

impl Image {
    pub(in crate::tools) fn validate(&self, grant: &WorkerGrant) -> Result<(), ToolError> {
        if self.entries.len() > WORKER_FILES {
            return Err(ToolError::Limit);
        }
        let mut bytes = 0usize;
        let mut names = BTreeMap::new();
        let mut prefixes = BTreeMap::new();
        for (path, entry) in &self.entries {
            if !valid_relative(path, false)
                || path.split('/').count() > 16
                || !within(path, &grant.read_paths)
            {
                return Err(ToolError::Path);
            }
            for (at, _) in path
                .match_indices('/')
                .chain(std::iter::once((path.len(), "")))
            {
                let prefix = &path[..at];
                if prefixes
                    .insert(prefix.to_ascii_lowercase(), prefix)
                    .is_some_and(|old| old != prefix)
                {
                    return Err(ToolError::Path);
                }
            }
            let folded = path.to_ascii_lowercase();
            if names.insert(folded.clone(), entry).is_some() {
                return Err(ToolError::Path);
            }
            let mut parent = std::path::Path::new(&folded).parent();
            while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
                if names
                    .get(p.to_str().ok_or(ToolError::Path)?)
                    .is_some_and(|e| !matches!(e, Entry::Directory))
                {
                    return Err(ToolError::Path);
                }
                parent = p.parent();
            }
            if let Entry::File { content, .. } = entry {
                if content.len() > MAX_FILE_BYTES {
                    return Err(ToolError::Limit);
                }
                bytes = bytes.checked_add(content.len()).ok_or(ToolError::Limit)?;
                if bytes > WORKER_CONTENT_BYTES {
                    return Err(ToolError::Limit);
                }
            }
        }
        Ok(())
    }

    pub(in crate::tools) fn encode(&self) -> Result<Vec<u8>, ToolError> {
        let bytes = serde_json::to_vec(self).map_err(|_| ToolError::Operation)?;
        if bytes.len() > WORKER_DATA_BYTES {
            return Err(ToolError::Limit);
        }
        Ok(bytes)
    }

    pub(in crate::tools) fn decode(bytes: &[u8], grant: &WorkerGrant) -> Result<Self, ToolError> {
        if bytes.len() > WORKER_DATA_BYTES {
            return Err(ToolError::Limit);
        }
        let image: Self = serde_json::from_slice(bytes).map_err(|_| ToolError::Configuration)?;
        image.validate(grant)?;
        if image.encode()? != bytes {
            return Err(ToolError::Configuration);
        }
        Ok(image)
    }
}

pub(in crate::tools) fn changes(base: &Image, after: &Image) -> Result<Vec<u8>, ToolError> {
    let paths: std::collections::BTreeSet<_> =
        base.entries.keys().chain(after.entries.keys()).collect();
    let mut files = Vec::new();
    let mut total = 0usize;
    for path in paths {
        if base.entries.get(path) == after.entries.get(path) {
            continue;
        }
        if !matches!(base.entries.get(path), Some(Entry::File { .. }))
            && !matches!(after.entries.get(path), Some(Entry::File { .. }))
        {
            continue;
        }
        for image in [base, after] {
            total = total
                .checked_add(image.entries.get(path).map_or(0, Entry::bytes))
                .ok_or(ToolError::Limit)?;
            if total > WORKER_CONTENT_BYTES {
                return Err(ToolError::Limit);
            }
        }
        if files.len() == WORKER_FILES {
            return Err(ToolError::Limit);
        }
        let before = base.entries.get(path).and_then(Entry::version);
        let new = after.entries.get(path).and_then(Entry::version);
        files.push(FileChange {
            path: path.clone(),
            before,
            after: new,
        });
    }
    let bytes = serde_json::to_vec(&WorkerChangeset { files }).map_err(|_| ToolError::Operation)?;
    decode_data(&bytes, WorkerDataPurpose::Mutation).map_err(|_| ToolError::Limit)?;
    Ok(bytes)
}

pub(in crate::tools) fn validate_command_image(
    before: &Image,
    after: &Image,
    grant: &WorkerGrant,
) -> Result<Vec<u8>, ToolError> {
    after.validate(grant)?;
    for (path, old) in &before.entries {
        if matches!(
            (old, after.entries.get(path)),
            (Entry::Directory, Some(Entry::File { .. }))
                | (Entry::File { .. }, Some(Entry::Directory))
        ) {
            return Err(ToolError::Path);
        }
    }
    let data = changes(before, after)?;
    let WorkerDataFacts::Changes(files) =
        decode_data(&data, WorkerDataPurpose::Mutation).map_err(|_| ToolError::Limit)?
    else {
        return Err(ToolError::Operation);
    };
    if files
        .iter()
        .any(|file| !within(&file.path, &grant.write_paths))
    {
        return Err(ToolError::FilesystemDenied);
    }
    Ok(data)
}

pub(in crate::tools) fn command_capture(
    before: &Image,
    root: &Dir,
    grant: &WorkerGrant,
) -> (Image, PrivateCapture) {
    let captured = (|| {
        let mut after = fs::worker_command_image(root).map_err(|error| {
            if matches!(error, ToolError::Limit) {
                ToolError::Limit
            } else {
                ToolError::Path
            }
        })?;
        // Check lease membership before read-scope validation so an ordinary outside write is explicit.
        let data = changes(before, &after)?;
        let WorkerDataFacts::Changes(files) =
            decode_data(&data, WorkerDataPurpose::Mutation).map_err(|_| ToolError::Limit)?
        else {
            return Err(ToolError::Operation);
        };
        if files
            .iter()
            .any(|file| !within(&file.path, &grant.write_paths))
        {
            return Err(ToolError::FilesystemDenied);
        }
        after.entries.retain(|path, entry| {
            !matches!(entry, Entry::Directory) || within(path, &grant.read_paths)
        });
        for (path, entry) in &before.entries {
            if *entry == Entry::Absent && !after.entries.contains_key(path) {
                after.entries.insert(path.clone(), Entry::Absent);
            }
        }
        validate_command_image(before, &after, grant)?;
        let mut image = before.clone();
        for file in files {
            if let Some(entry @ Entry::File { .. }) = after.entries.get(&file.path) {
                image.entries.insert(file.path.clone(), entry.clone());
                let mut path = file.path.as_str();
                while let Some((parent, _)) = path.rsplit_once('/') {
                    if within(parent, &grant.read_paths) {
                        image.entries.insert(parent.into(), Entry::Directory);
                    }
                    path = parent;
                }
            } else {
                image.entries.remove(&file.path);
            }
        }
        image.validate(grant)?;
        Ok(image)
    })();
    match captured {
        Ok(image) => (image, PrivateCapture::None {}),
        Err(error) => (
            before.clone(),
            PrivateCapture::Rejected {
                cause: match error {
                    ToolError::Limit => PrivateCaptureRejection::Overflow,
                    ToolError::FilesystemDenied => PrivateCaptureRejection::OutOfLease,
                    _ => PrivateCaptureRejection::UnsupportedEntry,
                },
            },
        ),
    }
}

pub(in crate::tools) fn validate_edit(
    before: &Image,
    after: &Image,
    call: &ToolCall,
    grant: &WorkerGrant,
) -> Result<(), ToolError> {
    if edited_image(before, call, grant)?.entries != after.entries {
        return Err(ToolError::ChangedInput);
    }
    Ok(())
}

fn removed_image(before: &Image, call: &ToolCall, grant: &WorkerGrant) -> Result<Image, ToolError> {
    let (source, destination, expected) = match call {
        ToolCall::Delete {
            path,
            expected_digest,
        } => (path, None, expected_digest),
        ToolCall::Move {
            source,
            destination,
            expected_digest,
        } => (source, Some(destination), expected_digest),
        _ => return Err(ToolError::Configuration),
    };
    let Some(entry @ Entry::File { content, .. }) = before.entries.get(source) else {
        return Err(ToolError::NotFound);
    };
    if hex_digest(content) != *expected {
        return Err(ToolError::Conflict);
    }
    let mut image = before.clone();
    if let Some(destination) = destination {
        if image
            .entries
            .get(destination)
            .is_some_and(|e| *e != Entry::Absent)
        {
            return Err(ToolError::Conflict);
        }
        for (end, _) in destination.match_indices('/') {
            let parent = &destination[..end];
            match image.entries.get(parent) {
                Some(Entry::Directory) => {}
                None | Some(Entry::Absent) => {
                    if within(parent, &grant.read_paths) {
                        image.entries.insert(parent.into(), Entry::Directory);
                    }
                }
                _ => return Err(ToolError::Path),
            }
        }
        image.entries.insert(destination.clone(), entry.clone());
    }
    if grant.read_paths.contains(source) || grant.write_paths.contains(source) {
        image.entries.insert(source.clone(), Entry::Absent);
    } else {
        image.entries.remove(source);
    }
    if image.entries.len() > WORKER_FILES {
        return Err(ToolError::Limit);
    }
    Ok(image)
}

pub(in crate::tools) fn edited_image(
    before: &Image,
    call: &ToolCall,
    grant: &WorkerGrant,
) -> Result<Image, ToolError> {
    if matches!(call, ToolCall::Delete { .. } | ToolCall::Move { .. }) {
        return removed_image(before, call, grant);
    }
    let path = match call {
        ToolCall::Mkdir { path }
        | ToolCall::Write { path, .. }
        | ToolCall::Edit { path, .. }
        | ToolCall::MultiEdit {
            file_path: path, ..
        } => path,
        _ => return Err(ToolError::Configuration),
    };
    if !before.entries.contains_key(path) && before.entries.len() >= WORKER_FILES {
        return Err(ToolError::Limit);
    }
    if matches!(call, ToolCall::Mkdir { .. } | ToolCall::Write { .. })
        && let Some((parent, _)) = path.rsplit_once('/')
    {
        let nested = format!("{parent}/");
        if !(before.entries.get(parent) == Some(&Entry::Directory)
            || before.entries.keys().any(|key| key.starts_with(&nested)))
        {
            return Err(ToolError::Path);
        }
    }
    let old = before.entries.get(path);
    let expected = match call {
        ToolCall::Mkdir { .. } if old.is_none_or(|e| *e == Entry::Absent) => Entry::Directory,
        ToolCall::Write {
            expected_digest,
            content,
            ..
        } => {
            let executable = match old {
                Some(Entry::File {
                    content,
                    executable,
                }) if expected_digest.as_ref() == Some(&hex_digest(content)) => *executable,
                None | Some(Entry::Absent) if expected_digest.is_none() => false,
                _ => return Err(ToolError::Conflict),
            };
            Entry::file(content.as_bytes(), executable)
        }
        ToolCall::Edit { .. } | ToolCall::MultiEdit { .. } => {
            let Some(Entry::File {
                content,
                executable,
            }) = old
            else {
                return Err(ToolError::Conflict);
            };
            Entry::file(fs::edited_content(content, call)?.as_bytes(), *executable)
        }
        _ => return Err(ToolError::Conflict),
    };
    let mut image = before.clone();
    image.entries.insert(path.clone(), expected);
    Ok(image)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tools) struct Transfer {
    pub(in crate::tools) target: EffectTarget,
    pub(in crate::tools) grant: WorkerGrant,
    pub(in crate::tools) bytes: u32,
    pub(in crate::tools) sha256: [u8; 32],
}
impl Transfer {
    pub(in crate::tools) fn validate(
        &self,
        intent: &EffectIntent,
        config: &Config,
    ) -> Result<(), ToolError> {
        let command = matches!(intent.call, ToolCall::Command { .. });
        if !command && intent.limits.resource_class != ToolResourceClass::Default
            || intent.limits.project_bytes != SCRATCH_BYTES
            || intent.limits.scratch_bytes != SCRATCH_BYTES
            || !command && intent.limits.cache_bytes != 0
            || intent.limits.cache_candidate_bytes != 0
            || !command && intent.limits.runtime_ms > WORKER_PREPARATION_MS
            || !config.write
            || !self.grant.valid()
            || self.target != intent.target
            || !matches!(self.target, EffectTarget::WorkerView { assignment_id, .. } if assignment_id == intent.agent_run_id)
            || !(self.grant.allows(&intent.call) && config.allows(&intent.call))
            || !(matches!(
                intent.call,
                ToolCall::Write { .. }
                    | ToolCall::Edit { .. }
                    | ToolCall::MultiEdit { .. }
                    | ToolCall::Delete { .. }
                    | ToolCall::Move { .. }
                    | ToolCall::Mkdir { .. }
            ) || crate::tools::discovery::possible(&intent.call)
                || command)
            || self.bytes == 0
            || self.bytes as usize > WORKER_DATA_BYTES
            || self.sha256 == [0; 32]
            || self
                .grant
                .read_paths
                .iter()
                .chain(&self.grant.write_paths)
                .any(|p| !config.in_workspace_scope(p))
        {
            return Err(ToolError::Configuration);
        }
        Ok(())
    }
    pub(in crate::tools) fn decode(&self, bytes: &[u8]) -> Result<Image, ToolError> {
        if bytes.len() != self.bytes as usize || digest(bytes) != self.sha256 {
            return Err(ToolError::ChangedInput);
        }
        Image::decode(bytes, &self.grant)
    }
}

pub(in crate::tools) struct Action {
    pub(in crate::tools) transfer: Transfer,
    pub(in crate::tools) input: Arc<[u8]>,
    pub(in crate::tools) output: Mutex<Option<Vec<u8>>>,
    pub(in crate::tools) contained: std::sync::atomic::AtomicBool,
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
    if matches!(intent.target, EffectTarget::WorkerView { .. }) || handover_ref(intent).is_some() {
        std::time::Duration::from_millis(u64::from(
            intent.limits.runtime_ms.min(WORKER_PREPARATION_MS),
        ))
    } else {
        std::time::Duration::ZERO
    }
}

pub(in crate::tools) fn transfer_overhead(intent: &EffectIntent) -> std::time::Duration {
    transfer_time(intent).saturating_mul(
        if matches!(intent.target, EffectTarget::WorkerView { .. }) {
            2
        } else {
            1
        },
    )
}

pub(in crate::tools) fn bulk_input(runtime: &ToolRuntime) -> Option<&[u8]> {
    runtime
        .worker
        .as_ref()
        .map(|a| a.input.as_ref())
        .or(runtime.handover.as_deref())
}

pub(in crate::tools) fn capture_paths(grant: &WorkerGrant) -> Vec<String> {
    let mut paths = grant.read_paths.clone();
    for path in &grant.write_paths {
        if !paths.contains(path) {
            paths.push(path.clone());
        }
    }
    paths
}

pub(in crate::tools) fn wire_valid(observation: &ToolObservation, intent: &EffectIntent) -> bool {
    observation.intent == *intent
        && (observation.limit_cause.is_some()
            == (observation.disposition == ToolDisposition::Limit))
        && observation.output.len() <= intent.limits.result_bytes as usize
        && (!matches!(intent.call, ToolCall::Command { .. })
            || serde_json::from_str::<WorkerCommandResult>(&observation.output).is_ok_and(
                |result| {
                    result.valid()
                        && !matches!(result.private_capture, PrivateCapture::Sealed { .. })
                        && (observation.disposition == ToolDisposition::Succeeded
                            || matches!(result.private_capture, PrivateCapture::None {}))
                },
            ))
        && observation.cache_update
            == if matches!(intent.call, ToolCall::Command { .. }) {
                crate::tools::CacheUpdate::Discarded
            } else {
                crate::tools::CacheUpdate::NotApplicable
            }
        && matches!(&observation.attestation, Some(Attestation::Guard(receipt)) if receipt.valid_for(intent))
}

pub(in crate::tools) fn materialize(image: &Image, root: &Dir) -> Result<(), ToolError> {
    if root
        .entries()
        .map_err(|_| ToolError::Path)?
        .next()
        .is_some()
    {
        return Err(ToolError::ChangedInput);
    }
    for (path, entry) in &image.entries {
        match entry {
            Entry::File {
                content,
                executable,
            } => fs::put(root, path, content, *executable)?,
            Entry::Directory => fs::put_directory(root, path)?,
            Entry::Absent => fs::put_parent_directories(root, path)?,
        }
    }
    Ok(())
}

fn encode_frame(bytes: &[u8]) -> Result<Vec<u8>, ToolError> {
    if bytes.is_empty() || bytes.len() > WORKER_DATA_BYTES {
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
) -> Result<(), ToolError> {
    writer
        .write_all(&encode_frame(bytes)?)
        .map_err(|_| ToolError::Operation)
}

pub(in crate::tools) fn read_frame(reader: &mut impl std::io::Read) -> Result<Vec<u8>, ToolError> {
    let mut size = [0; 4];
    reader
        .read_exact(&mut size)
        .map_err(|_| ToolError::Operation)?;
    let size = u32::from_le_bytes(size) as usize;
    if size == 0 || size > WORKER_DATA_BYTES {
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
        .write_all(&encode_frame(bytes)?)
        .await
        .map_err(|_| ToolError::Operation)
}

pub(in crate::tools) async fn receive_frame(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
) -> Result<Vec<u8>, ToolError> {
    let size = reader
        .read_u32_le()
        .await
        .map_err(|_| ToolError::Operation)? as usize;
    if size == 0 || size > WORKER_DATA_BYTES {
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

use super::{
    Attestation, Cancellation, EffectIntent, FileChange, FileVersion, ToolCall, ToolDisposition,
    ToolError, ToolObservation, ToolRuntime, fs, types::*,
};
use crate::session::{AgentRunId, worker::*};
use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(crate) async fn recover_private(ids: &[uuid::Uuid]) -> Result<(), ToolError> {
    super::guard::recover_private(ids).await
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Entry {
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
    pub(super) fn file(content: &[u8], executable: bool) -> Self {
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
    fn bytes(&self) -> usize {
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
pub(super) struct Image {
    #[serde(deserialize_with = "bounded_entries")]
    pub(super) entries: BTreeMap<String, Entry>,
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
    fn retained_bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|(path, entry)| path.len() + 128 + entry.bytes())
            .sum()
    }

    pub(super) fn validate(&self, grant: &WorkerGrant) -> Result<(), ToolError> {
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

    fn manifest(&self) -> WorkerManifest {
        WorkerManifest {
            entries: self
                .entries
                .iter()
                .map(|(path, entry)| WorkerManifestEntry {
                    path: path.clone(),
                    version: match entry {
                        Entry::Absent => WorkerBaseVersion::Absent,
                        Entry::Directory => WorkerBaseVersion::Directory,
                        Entry::File {
                            content,
                            executable,
                        } => WorkerBaseVersion::File {
                            sha256: hex_digest(content),
                            bytes: u32::try_from(content.len()).expect("bounded private file"),
                            executable: *executable,
                        },
                    },
                })
                .collect(),
        }
    }

    pub(super) fn encode(&self) -> Result<Vec<u8>, ToolError> {
        let bytes = serde_json::to_vec(self).map_err(|_| ToolError::Operation)?;
        if bytes.len() > WORKER_DATA_BYTES {
            return Err(ToolError::Limit);
        }
        Ok(bytes)
    }

    pub(super) fn decode(bytes: &[u8], grant: &WorkerGrant) -> Result<Self, ToolError> {
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

fn denied(intent: EffectIntent) -> ToolObservation {
    let mut observation = super::unstarted_observation(intent, ToolError::Configuration);
    observation.disposition = ToolDisposition::Denied;
    observation.limit_cause = None;
    observation
}

pub(crate) struct View {
    run_id: crate::session::RunId,
    policy_digest: [u8; 32],
    workspace_identity: crate::session::WorkspaceIdentity,
    assignment: AgentRunId,
    grant: WorkerGrant,
    base: Arc<Image>,
    current: Image,
    base_digest: [u8; 32],
    revision: u32,
    stopped: bool,
    pending: Option<uuid::Uuid>,
    contained: bool,
    remaining_data: usize,
    retained_limit: usize,
    enforcement_digest: [u8; 32],
    process_enforcement_digest: [u8; 32],
    last_intent_digest: Option<[u8; 32]>,
    last_enforcement_digest: Option<[u8; 32]>,
    sealed: Option<WorkerDataRef>,
    cache_lease: Option<crate::store::BuildCacheLease>,
    cache_generation: Option<[u8; 32]>,
}

impl View {
    pub(crate) fn capture(
        runtime: &ToolRuntime,
        run_id: crate::session::RunId,
        assignment: AgentRunId,
        grant: WorkerGrant,
        data_bytes: u32,
    ) -> Result<Self, ToolError> {
        if data_bytes == 0
            || data_bytes as usize > WORKER_DATA_BYTES
            || !assignment.valid()
            || !grant.valid()
            || grant
                .read_paths
                .iter()
                .any(|p| !runtime.config.in_workspace_scope(p))
            || grant
                .write_paths
                .iter()
                .any(|p| !runtime.config.in_workspace_scope(p))
            || !runtime.config.write
        {
            return Err(ToolError::Configuration);
        }
        fs::validate_workspace(runtime)?;
        let reads = runtime
            .reads
            .as_ref()
            .ok_or(ToolError::ProtectionUnavailable)?;
        let _permit = reads.slots.try_acquire().map_err(|_| ToolError::Limit)?;
        let root = reads.root.as_ref();
        let image = fs::worker_image(root, &capture_paths(&grant))?;
        image.validate(&grant)?;
        if grant.tools.contains(&WorkerTool::Command)
            && image
                .entries
                .keys()
                .any(|path| path.split('/').any(fs::ignored_directory))
        {
            return Err(ToolError::Path);
        }
        let manifest = serde_json::to_vec(&image.manifest()).map_err(|_| ToolError::Operation)?;
        decode_data(&manifest, WorkerDataPurpose::Base).map_err(|_| ToolError::Limit)?;
        // A second bounded scan pins subtree membership as well as bytes and types.
        if fs::worker_image(root, &capture_paths(&grant))? != image {
            return Err(ToolError::ChangedInput);
        }
        fs::validate_workspace(runtime)?;
        Ok(Self {
            run_id,
            policy_digest: runtime.config.digest(),
            workspace_identity: runtime.workspace_identity,
            assignment,
            grant,
            base_digest: digest(&manifest),
            base: Arc::new(image.clone()),
            current: image,
            revision: 0,
            stopped: false,
            pending: None,
            contained: true,
            enforcement_digest: runtime.enforcement_digest,
            process_enforcement_digest: runtime.process_enforcement_digest,
            last_intent_digest: None,
            last_enforcement_digest: None,
            sealed: None,
            cache_lease: None,
            cache_generation: None,
            retained_limit: 64 * 1024 * 1024,
            remaining_data: (data_bytes as usize)
                .checked_sub(manifest.len())
                .ok_or(ToolError::Limit)?,
        })
    }

    pub(crate) fn restrict_retained(&mut self, limit: usize) -> Result<(), ToolError> {
        if self.base.retained_bytes() + self.current.retained_bytes() > limit {
            return Err(ToolError::Limit);
        }
        self.retained_limit = self.retained_limit.min(limit);
        Ok(())
    }

    pub(crate) fn note_intent(&mut self, intent: &EffectIntent) {
        self.last_intent_digest = Some(intent.digest());
        self.last_enforcement_digest = Some(intent.enforcement_digest);
    }

    pub(crate) fn reusable_data(&self, bytes: &[u8]) -> Option<WorkerDataRef> {
        self.sealed
            .as_ref()
            .filter(|data| data.sha256 == digest(bytes) && data.bytes as usize == bytes.len())
            .cloned()
    }

    pub(crate) fn manifest(&self) -> Result<Vec<u8>, ToolError> {
        serde_json::to_vec(&self.base.manifest()).map_err(|_| ToolError::Operation)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn run_id(&self) -> crate::session::RunId {
        self.run_id
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn assignment(&self) -> AgentRunId {
        self.assignment
    }

    pub(crate) fn target(&self) -> EffectTarget {
        EffectTarget::WorkerView {
            cache_generation: self.cache_generation,
            assignment_id: self.assignment,
            base_digest: self.base_digest,
            revision: self.revision,
        }
    }

    pub(crate) fn pin_command_cache(
        &mut self,
        runtime: &ToolRuntime,
        bytes: u64,
    ) -> Result<(), ToolError> {
        if self.revision != 0 || self.last_intent_digest.is_some() || self.cache_lease.is_some() {
            return Err(ToolError::Configuration);
        }
        if bytes == 0 {
            return Ok(());
        }
        let state = runtime
            .cache_state
            .as_ref()
            .ok_or(ToolError::Configuration)?;
        let identity = crate::store::BuildCacheIdentity::new(
            &runtime.workspace,
            self.workspace_identity.device,
            self.workspace_identity.inode,
        )
        .map_err(super::guard::map_cache_error)?;
        let lease = state
            .read_build_cache(identity, bytes)
            .map_err(super::guard::map_cache_error)?;
        self.cache_generation = lease.generation().map_err(super::guard::map_cache_error)?;
        self.cache_lease = Some(lease);
        Ok(())
    }

    pub(crate) fn command<'a>(
        &'a mut self,
        runtime: &'a ToolRuntime,
        intent: EffectIntent,
        cancellation: Cancellation,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Candidate, Box<ToolObservation>>> + Send + 'a>,
    > {
        Box::pin(async move {
            if !matches!(intent.call, ToolCall::Command { .. }) {
                return Err(Box::new(denied(intent)));
            }
            self.dispatch(runtime, intent, cancellation).await
        })
    }

    pub(crate) fn read(&self, intent: EffectIntent) -> ToolObservation {
        if self.check(&intent).is_err() {
            return denied(intent);
        }
        let result = inspect(
            &self.current,
            &intent.call,
            intent.limits.result_bytes as usize,
            std::time::Instant::now()
                + std::time::Duration::from_millis(u64::from(intent.limits.runtime_ms)),
        );
        let (disposition, output, enforcement_cause, limit_cause) =
            super::dispatch::outcome(&intent, result);
        let attestation =
            (disposition == ToolDisposition::Succeeded).then(|| Attestation::WorkerRead {
                intent_digest: intent.digest(),
                guard: None,
            });
        ToolObservation {
            intent,
            disposition,
            output,
            enforcement_cause,
            limit_cause,
            attestation,
            cache_update: super::CacheUpdate::NotApplicable,
        }
    }

    fn check(&self, intent: &EffectIntent) -> Result<(), ToolError> {
        if self.stopped
            || self.pending.is_some()
            || intent.expires_at_ms <= super::now_ms()
            || intent.run_id != self.run_id
            || intent.policy_digest != self.policy_digest
            || intent.enforcement_digest
                != if intent.call.is_process() {
                    self.process_enforcement_digest
                } else {
                    self.enforcement_digest
                }
            || intent.workspace_device != self.workspace_identity.device
            || intent.workspace_inode != self.workspace_identity.inode
            || !intent.valid()
            || intent.target != self.target()
            || intent.agent_run_id != self.assignment
            || !(self.grant.allows(&intent.call) || handover_call(&intent.call, self.assignment))
        {
            return Err(ToolError::Configuration);
        }
        Ok(())
    }

    pub(crate) async fn edit(
        &mut self,
        runtime: &ToolRuntime,
        intent: EffectIntent,
        cancellation: Cancellation,
    ) -> Result<Candidate, Box<ToolObservation>> {
        if matches!(intent.call, ToolCall::Command { .. }) {
            return Err(Box::new(denied(intent)));
        }
        self.dispatch(runtime, intent, cancellation).await
    }

    async fn dispatch(
        &mut self,
        runtime: &ToolRuntime,
        intent: EffectIntent,
        cancellation: Cancellation,
    ) -> Result<Candidate, Box<ToolObservation>> {
        if self.check(&intent).is_err()
            || matches!(intent.call, ToolCall::Command { .. })
                && intent.limits.cache_bytes > 0
                && self.cache_lease.is_none()
            || !(handover_call(&intent.call, self.assignment)
                || matches!(
                    intent.call,
                    ToolCall::Mkdir { .. }
                        | ToolCall::Write { .. }
                        | ToolCall::Edit { .. }
                        | ToolCall::Command { .. }
                ))
            || intent.call.mutates() && self.revision >= 16
            || !(runtime.config.allows(&intent.call)
                || handover_call(&intent.call, self.assignment))
        {
            return Err(Box::new(denied(intent)));
        }
        let proposed = (|| {
            if matches!(intent.call, ToolCall::Command { .. }) {
                return Ok(());
            }
            let image = edited_image(&self.current, &intent.call)?;
            image.validate(&self.grant)?;
            if self.base.retained_bytes() + image.retained_bytes() > self.retained_limit {
                return Err(ToolError::Limit);
            }
            let data = self.data_for(&image, &intent.call)?;
            if data.len() > self.remaining_data
                && !(handover_call(&intent.call, self.assignment)
                    && self
                        .sealed
                        .as_ref()
                        .is_some_and(|r| r.sha256 == digest(&data)))
            {
                return Err(ToolError::Limit);
            }
            if let ToolCall::PrepareWorker { preparation } = &intent.call
                && (data.len() > preparation.max_bytes as usize
                    || data.len().div_ceil(WORKER_CHUNK_BYTES)
                        > usize::from(preparation.max_chunks))
            {
                return Err(ToolError::Limit);
            }
            Ok(())
        })();
        if let Err(error) = proposed {
            return Err(Box::new(super::unstarted_observation(intent, error)));
        }
        let bytes = match self.current.encode() {
            Ok(bytes) => bytes,
            Err(error) => return Err(Box::new(super::unstarted_observation(intent, error))),
        };
        let action = Arc::new(Action {
            transfer: Transfer {
                target: self.target(),
                grant: self.grant.clone(),
                bytes: u32::try_from(bytes.len()).expect("bounded worker transfer bytes"),
                sha256: digest(&bytes),
            },
            input: bytes.into(),
            output: Mutex::new(None),
            contained: std::sync::atomic::AtomicBool::new(true),
        });
        let mut private = runtime.clone();
        private.worker = Some(action.clone());
        self.note_intent(&intent);
        self.pending = Some(intent.id);
        let result = super::guard::execute(&private, &intent, cancellation).await;
        self.receive(intent, &action, result)
    }

    fn receive(
        &mut self,
        intent: EffectIntent,
        action: &Action,
        result: Result<ToolObservation, ToolError>,
    ) -> Result<Candidate, Box<ToolObservation>> {
        let mut observation = match result {
            Ok(observation) => observation,
            Err(error) => {
                self.contained = action.contained.load(std::sync::atomic::Ordering::Acquire);
                super::uncertain_observation(intent, error, super::CacheUpdate::NotApplicable)
            }
        };
        if matches!(observation.intent.call, ToolCall::Command { .. }) {
            return self.receive_command(observation, action);
        }
        if observation.disposition != ToolDisposition::Succeeded {
            self.contained = action.contained.load(std::sync::atomic::Ordering::Acquire);
            observation.attestation = None;
            observation.enforcement_cause = None;
            self.stopped = true;
            self.pending = None;
            return Err(Box::new(observation));
        }
        let checked: Result<Candidate, ToolError> = (|| {
            let bytes = action
                .output
                .lock()
                .map_err(|_| ToolError::Operation)?
                .take()
                .ok_or(ToolError::Operation)?;
            let image = Image::decode(&bytes, &self.grant)?;
            validate_edit(&self.current, &image, &observation.intent.call)?;
            let data = self.data_for(&image, &observation.intent.call)?;
            Ok(Candidate {
                observation: observation.clone(),
                image,
                data,
                target: self.target(),
            })
        })();
        match checked {
            Ok(candidate) => Ok(candidate),
            Err(_) => {
                self.stopped = true;
                self.pending = None;
                Err(Box::new(super::uncertain_observation(
                    observation.intent,
                    ToolError::Uncertain,
                    super::CacheUpdate::NotApplicable,
                )))
            }
        }
    }

    fn receive_command(
        &mut self,
        mut observation: ToolObservation,
        action: &Action,
    ) -> Result<Candidate, Box<ToolObservation>> {
        self.contained = action.contained.load(std::sync::atomic::Ordering::Acquire);
        if observation.disposition == ToolDisposition::Cancelled {
            if let Some(Attestation::Guard(receipt)) = observation.attestation.take() {
                observation.attestation = Some(Attestation::WorkerCommand { receipt });
            }
            self.stopped = true;
            self.pending = None;
            return Err(Box::new(observation));
        }
        let checked = (|| {
            if observation.disposition == ToolDisposition::Uncertain || !self.contained {
                return Err(ToolError::Uncertain);
            }
            let Some(Attestation::Guard(receipt)) = observation.attestation.take() else {
                return Ok(None);
            };
            let mut result: WorkerCommandResult =
                serde_json::from_str(&observation.output).map_err(|_| ToolError::Operation)?;
            observation.attestation = Some(Attestation::WorkerCommand { receipt });
            let bytes = action
                .output
                .lock()
                .map_err(|_| ToolError::Operation)?
                .take()
                .ok_or(ToolError::Operation)?;
            let image = Image::decode(&bytes, &self.grant)?;
            if observation.disposition != ToolDisposition::Succeeded
                || matches!(result.private_capture, PrivateCapture::Rejected { .. })
            {
                if image != self.current {
                    return Err(ToolError::ChangedInput);
                }
                return Ok(None);
            }
            let data = validate_command_image(&self.current, &image, &self.grant)?;
            let WorkerDataFacts::Changes(files) = decode_data(&data, WorkerDataPurpose::Mutation)
                .map_err(|_| ToolError::Operation)?
            else {
                return Err(ToolError::Operation);
            };
            if files.is_empty() {
                return Ok(None);
            }
            if self.revision >= 16
                || data.len() > self.remaining_data
                || self.base.retained_bytes() + image.retained_bytes() > self.retained_limit
                || changes(&self.base, &image).is_err()
            {
                result.private_capture = PrivateCapture::Rejected {
                    cause: PrivateCaptureRejection::Overflow,
                };
                observation.output =
                    serde_json::to_string(&result).map_err(|_| ToolError::Operation)?;
                return Ok(None);
            }
            Ok(Some((image, data)))
        })();
        match checked {
            Ok(Some((image, data))) => Ok(Candidate {
                observation,
                image,
                data,
                target: self.target(),
            }),
            Ok(None) => {
                self.pending = None;
                Err(Box::new(observation))
            }
            Err(_) => {
                self.stopped = true;
                self.pending = None;
                Err(Box::new(super::uncertain_observation(
                    observation.intent,
                    ToolError::Uncertain,
                    super::CacheUpdate::NotApplicable,
                )))
            }
        }
    }

    fn data_for(&self, image: &Image, call: &ToolCall) -> Result<Vec<u8>, ToolError> {
        let cumulative = changes(&self.base, image)?;
        if handover_call(call, self.assignment) {
            Ok(cumulative)
        } else {
            changes(&self.current, image)
        }
    }

    pub(crate) fn acknowledge(
        &mut self,
        candidate: Candidate,
        data: WorkerDataRef,
    ) -> Result<ToolObservation, ToolError> {
        if self.stopped
            || self.pending != Some(candidate.observation.intent.id)
            || candidate.target != self.target()
            || data.assignment_id != self.assignment
            || (if handover_call(&candidate.observation.intent.call, self.assignment) {
                data.purpose != WorkerDataPurpose::Handover && self.sealed.as_ref() != Some(&data)
            } else {
                data.purpose != WorkerDataPurpose::Mutation
            })
            || data.bytes as usize != candidate.data.len()
            || data.sha256 != digest(&candidate.data)
            || (!handover_call(&candidate.observation.intent.call, self.assignment)
                && self
                    .sealed
                    .as_ref()
                    .is_some_and(|old| data.first_sequence <= old.seal_sequence))
            || !data.valid()
        {
            return Err(ToolError::ChangedInput);
        }
        let observation = candidate.observation(&data)?;
        if self.sealed.as_ref() != Some(&data) {
            self.remaining_data = self
                .remaining_data
                .checked_sub(candidate.data.len())
                .ok_or(ToolError::Limit)?;
        }
        self.current = candidate.image;
        if observation.intent.call.mutates()
            || matches!(observation.intent.call, ToolCall::Command { .. })
        {
            self.revision += 1;
        }
        self.sealed = Some(data);
        self.pending = None;
        Ok(observation)
    }

    pub(crate) fn handover(&self) -> Result<Vec<u8>, ToolError> {
        if self.stopped || self.pending.is_some() {
            return Err(ToolError::Uncertain);
        }
        changes(&self.base, &self.current)
    }

    pub(crate) fn dispose(self) -> Result<WorkerCleanupReceipt, ToolError> {
        if !self.contained || self.pending.is_some() {
            return Err(ToolError::Uncertain);
        }
        Ok(WorkerCleanupReceipt {
            assignment_id: self.assignment,
            base_digest: self.base_digest,
            revision: self.revision,
            enforcement_digest: self
                .last_enforcement_digest
                .unwrap_or(self.enforcement_digest),
            last_intent_digest: self.last_intent_digest,
        })
    }
}

pub(crate) struct Candidate {
    observation: ToolObservation,
    image: Image,
    pub(crate) data: Vec<u8>,
    target: EffectTarget,
}

impl Candidate {
    pub(crate) fn observation(&self, data: &WorkerDataRef) -> Result<ToolObservation, ToolError> {
        if data.assignment_id != self.observation.intent.agent_run_id
            || !data.valid()
            || data.bytes as usize != self.data.len()
            || data.sha256 != digest(&self.data)
        {
            return Err(ToolError::ChangedInput);
        }
        let mut observation = self.observation.clone();
        let Some(Attestation::Guard(receipt)) = observation.attestation.take() else {
            return self.command_observation(observation, data);
        };
        observation.attestation = Some(
            if handover_call(&observation.intent.call, data.assignment_id) {
                Attestation::WorkerData {
                    guard: Some(Box::new(receipt)),
                    intent_digest: observation.intent.digest(),
                    data: data.clone(),
                }
            } else {
                let EffectTarget::WorkerView { revision, .. } = observation.intent.target else {
                    return Err(ToolError::Configuration);
                };
                Attestation::WorkerMutation {
                    receipt,
                    mutation: Box::new(WorkerMutation {
                        previous_revision: revision,
                        next_revision: revision + 1,
                        data: data.clone(),
                    }),
                }
            },
        );
        if !observation.valid() {
            return Err(ToolError::Configuration);
        }
        Ok(observation)
    }

    fn command_observation(
        &self,
        mut observation: ToolObservation,
        data: &WorkerDataRef,
    ) -> Result<ToolObservation, ToolError> {
        let Some(Attestation::WorkerCommand { receipt }) = self.observation.attestation.clone()
        else {
            return Err(ToolError::Configuration);
        };
        let mut result: WorkerCommandResult =
            serde_json::from_str(&observation.output).map_err(|_| ToolError::Configuration)?;
        result.private_capture = PrivateCapture::Sealed { data: data.clone() };
        observation.output = serde_json::to_string(&result).map_err(|_| ToolError::Operation)?;
        observation.attestation = Some(Attestation::WorkerCommand { receipt });
        if !observation.valid() {
            return Err(ToolError::Limit);
        }
        Ok(observation)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn chunks(&self) -> impl Iterator<Item = WorkerDataChunk> + '_ {
        use base64::Engine as _;
        self.data
            .chunks(WORKER_CHUNK_BYTES)
            .enumerate()
            .map(|(ordinal, bytes)| WorkerDataChunk {
                assignment_id: self.observation.intent.agent_run_id,
                purpose: if handover_call(
                    &self.observation.intent.call,
                    self.observation.intent.agent_run_id,
                ) {
                    WorkerDataPurpose::Handover
                } else {
                    WorkerDataPurpose::Mutation
                },
                tool_id: self.observation.intent.id,
                ordinal: u16::try_from(ordinal).expect("bounded worker chunks"),
                bytes: u32::try_from(bytes.len()).expect("bounded chunk bytes"),
                sha256: digest(bytes),
                content: base64::engine::general_purpose::STANDARD.encode(bytes),
            })
    }
}

fn changes(base: &Image, after: &Image) -> Result<Vec<u8>, ToolError> {
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

fn validate_command_image(
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

pub(super) fn command_capture(
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

pub(super) fn validate_edit(
    before: &Image,
    after: &Image,
    call: &ToolCall,
) -> Result<(), ToolError> {
    if edited_image(before, call)?.entries != after.entries {
        return Err(ToolError::ChangedInput);
    }
    Ok(())
}

fn edited_image(before: &Image, call: &ToolCall) -> Result<Image, ToolError> {
    if matches!(call, ToolCall::PrepareWorker { .. }) {
        return Ok(before.clone());
    }
    let path = match call {
        ToolCall::Mkdir { path } | ToolCall::Write { path, .. } | ToolCall::Edit { path, .. } => {
            path
        }
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
        ToolCall::Edit {
            expected_digest,
            old: text,
            new,
            ..
        } => {
            let Some(Entry::File {
                content,
                executable,
            }) = old
            else {
                return Err(ToolError::Conflict);
            };
            let bytes = content;
            let content = std::str::from_utf8(bytes).map_err(|_| ToolError::Path)?;
            if hex_digest(bytes) != *expected_digest || content.match_indices(text).count() != 1 {
                return Err(ToolError::Conflict);
            }
            if content
                .len()
                .checked_sub(text.len())
                .and_then(|n| n.checked_add(new.len()))
                .is_none_or(|n| n > MAX_FILE_BYTES)
            {
                return Err(ToolError::Limit);
            }
            Entry::file(content.replacen(text, new, 1).as_bytes(), *executable)
        }
        _ => return Err(ToolError::Conflict),
    };
    let mut image = before.clone();
    image.entries.insert(path.clone(), expected);
    Ok(image)
}

fn inspect(
    image: &Image,
    call: &ToolCall,
    result_bytes: usize,
    deadline: std::time::Instant,
) -> Result<String, ToolError> {
    use serde_json::json;
    match call {
        ToolCall::Read {
            path,
            offset,
            limit,
        } => {
            let Some(Entry::File { content, .. }) = image.entries.get(path) else {
                return Err(ToolError::Path);
            };
            fs::read_page(content.as_ref(), *offset, *limit, result_bytes)
        }
        ToolCall::List { path } => {
            let mut rows = Vec::new();
            for (name, entry) in image.entries.iter().filter(|(p, _)| contains(path, p)) {
                match entry {
                    Entry::Directory => rows.push(json!({"path":name,"kind":"directory"})),
                    Entry::File { content, .. } => {
                        rows.push(json!({"path":name,"bytes":content.len()}))
                    }
                    Entry::Absent => {}
                }
                if rows.len() == 64 {
                    break;
                }
            }
            let truncated = rows.len() == 64;
            fs::fit_rows(rows, truncated, result_bytes)
        }
        ToolCall::Search { path, .. } => {
            let mut search = super::search::Search::new(call, deadline)?;
            // BTreeMap order is the Workspace's UTF-8 byte order, as in the Workspace search.
            for (name, entry) in image.entries.iter().filter(|(p, _)| contains(path, p)) {
                if let Entry::File { content, .. } = entry
                    && search.includes(name)
                    && search.scan(name, content)?
                {
                    break;
                }
            }
            search.finish(result_bytes)
        }
        _ => Err(ToolError::Configuration),
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Transfer {
    pub(super) target: EffectTarget,
    pub(super) grant: WorkerGrant,
    bytes: u32,
    sha256: [u8; 32],
}
impl Transfer {
    pub(super) fn validate(
        &self,
        intent: &EffectIntent,
        config: &super::Config,
    ) -> Result<(), ToolError> {
        let command = matches!(intent.call, ToolCall::Command { .. });
        if !command && intent.limits.resource_class != super::ToolResourceClass::Default
            || intent.limits.project_bytes != SCRATCH_BYTES
            || intent.limits.scratch_bytes != SCRATCH_BYTES
            || !command && intent.limits.cache_bytes != 0
            || intent.limits.cache_candidate_bytes != 0
            || !command && intent.limits.runtime_ms > 120_000
            || !config.write
            || !self.grant.valid()
            || self.target != intent.target
            || !matches!(self.target, EffectTarget::WorkerView { assignment_id, .. } if assignment_id == intent.agent_run_id)
            || !(self.grant.allows(&intent.call) && config.allows(&intent.call)
                || handover_call(&intent.call, intent.agent_run_id))
            || !(matches!(
                intent.call,
                ToolCall::Write { .. } | ToolCall::Edit { .. } | ToolCall::Mkdir { .. }
            ) || command
                || handover_call(&intent.call, intent.agent_run_id))
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
    pub(super) fn decode(&self, bytes: &[u8]) -> Result<Image, ToolError> {
        if bytes.len() != self.bytes as usize || digest(bytes) != self.sha256 {
            return Err(ToolError::ChangedInput);
        }
        Image::decode(bytes, &self.grant)
    }
}

pub(super) struct Action {
    pub(super) transfer: Transfer,
    pub(super) input: Arc<[u8]>,
    pub(super) output: Mutex<Option<Vec<u8>>>,
    pub(super) contained: std::sync::atomic::AtomicBool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PublicationTransfer {
    bytes: u32,
    sha256: [u8; 32],
}

pub(super) fn handover_ref(intent: &EffectIntent) -> Option<PublicationTransfer> {
    match &intent.call {
        ToolCall::ApplyChangeset { source } if intent.target == EffectTarget::Workspace => {
            match source {
                ChangesetSource::WorkerHandover { data, .. } => Some(PublicationTransfer {
                    bytes: data.bytes,
                    sha256: data.sha256,
                }),
                ChangesetSource::Rewind { bytes, sha256, .. } => Some(PublicationTransfer {
                    bytes: *bytes,
                    sha256: *sha256,
                }),
                _ => None,
            }
        }
        _ => None,
    }
}

pub(super) fn decode_handover(
    intent: &EffectIntent,
    bytes: &[u8],
    config: &super::Config,
) -> Result<WorkerChangeset, ToolError> {
    let data = handover_ref(intent).ok_or(ToolError::Configuration)?;
    if !intent.valid()
        || !config.write
        || bytes.len() != data.bytes as usize
        || digest(bytes) != data.sha256
    {
        return Err(ToolError::ChangedInput);
    }
    if matches!(
        intent.call,
        ToolCall::ApplyChangeset {
            source: ChangesetSource::Rewind { .. }
        }
    ) {
        crate::session::worker::decode_rewind_data(bytes).map_err(|_| ToolError::Limit)?;
    } else if let ToolCall::ApplyChangeset {
        source: ChangesetSource::WorkerHandover { data, .. },
    } = &intent.call
    {
        decode_data(bytes, data.purpose).map_err(|_| ToolError::Limit)?;
    } else {
        return Err(ToolError::Configuration);
    }
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
        std::time::Duration::from_millis(u64::from(intent.limits.runtime_ms.min(120_000)))
    } else {
        std::time::Duration::ZERO
    }
}

pub(super) fn transfer_overhead(intent: &EffectIntent) -> std::time::Duration {
    transfer_time(intent).saturating_mul(
        if matches!(intent.target, EffectTarget::WorkerView { .. }) {
            2
        } else {
            1
        },
    )
}

pub(super) fn bulk_input(runtime: &ToolRuntime) -> Option<&[u8]> {
    runtime
        .worker
        .as_ref()
        .map(|a| a.input.as_ref())
        .or(runtime.handover.as_deref())
}

pub(super) fn handover_call(call: &ToolCall, assignment: AgentRunId) -> bool {
    matches!(call, ToolCall::PrepareWorker { preparation } if preparation.valid() && preparation.assignment_id == assignment && preparation.purpose == WorkerDataPurpose::Handover)
}

pub(super) fn capture_paths(grant: &WorkerGrant) -> Vec<String> {
    let mut paths = grant.read_paths.clone();
    for path in &grant.write_paths {
        if !paths.contains(path) {
            paths.push(path.clone());
        }
    }
    paths
}

pub(super) fn wire_valid(observation: &ToolObservation, intent: &EffectIntent) -> bool {
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
                super::CacheUpdate::Discarded
            } else {
                super::CacheUpdate::NotApplicable
            }
        && matches!(&observation.attestation, Some(Attestation::Guard(receipt)) if receipt.valid_for(intent))
}

pub(super) fn materialize(image: &Image, root: &Dir) -> Result<(), ToolError> {
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

pub(super) fn write_frame(writer: &mut impl std::io::Write, bytes: &[u8]) -> Result<(), ToolError> {
    writer
        .write_all(&encode_frame(bytes)?)
        .map_err(|_| ToolError::Operation)
}

pub(super) fn read_frame(reader: &mut impl std::io::Read) -> Result<Vec<u8>, ToolError> {
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

pub(super) async fn send_frame(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    bytes: &[u8],
) -> Result<(), ToolError> {
    writer
        .write_all(&encode_frame(bytes)?)
        .await
        .map_err(|_| ToolError::Operation)
}

pub(super) async fn receive_frame(
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn grant() -> WorkerGrant {
        WorkerGrant {
            commands: Vec::new(),
            read_paths: vec![".".into()],
            write_paths: vec!["owned".into()],
            tools: vec![
                WorkerTool::Read,
                WorkerTool::List,
                WorkerTool::Search,
                WorkerTool::Write,
                WorkerTool::Edit,
                WorkerTool::Mkdir,
            ],
            skills: vec![],
            mcp_servers: vec![],
            permission_mode: None,
        }
    }

    fn runtime(workspace: &std::path::Path) -> ToolRuntime {
        super::super::tests::in_process_runtime(
            workspace,
            json!({"version":1,"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]}),
        )
    }

    fn fixture() -> (tempfile::TempDir, ToolRuntime, View) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("owned")).unwrap();
        std::fs::write(dir.path().join("owned/a"), b"original").unwrap();
        let runtime = runtime(dir.path());
        let view = View::capture(
            &runtime,
            crate::session::RunId::new(),
            AgentRunId::new(),
            grant(),
            WORKER_DATA_BYTES as u32,
        )
        .unwrap();
        (dir, runtime, view)
    }

    fn intent(runtime: &ToolRuntime, view: &View, call: ToolCall) -> EffectIntent {
        let mut intent = runtime.intent(view.run_id, view.assignment, call);
        intent.target = view.target();
        intent
    }

    fn captured(view: &mut View, intent: EffectIntent) -> Candidate {
        let root = tempfile::tempdir().unwrap();
        let directory = fs::open_directory(root.path()).unwrap();
        materialize(&view.current, &directory).unwrap();
        let config = serde_json::from_value(json!({"version":1,"workspace_paths":["."],"write":true,"commands":[],"skills":[],"mcp":[]})).unwrap();
        let output = fs::native(root.path(), &intent.call, &config).unwrap();
        let image = fs::worker_image(&directory, &capture_paths(&view.grant)).unwrap();
        image.validate(&view.grant).unwrap();
        validate_edit(&view.current, &image, &intent.call).unwrap();
        let observation = ToolObservation {
            attestation: Some(Attestation::Guard(super::super::GuardReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                enforcement_digest: intent.enforcement_digest,
                limits: intent.limits.clone(),
            })),
            intent,
            disposition: ToolDisposition::Succeeded,
            output,
            enforcement_cause: None,
            limit_cause: None,
            cache_update: super::super::CacheUpdate::NotApplicable,
        };
        view.pending = Some(observation.intent.id);
        Candidate {
            data: view.data_for(&image, &observation.intent.call).unwrap(),
            image,
            target: view.target(),
            observation,
        }
    }

    fn reference(candidate: &Candidate) -> WorkerDataRef {
        WorkerDataRef {
            assignment_id: candidate.observation.intent.agent_run_id,
            purpose: WorkerDataPurpose::Mutation,
            first_sequence: 1,
            seal_sequence: 1 + candidate.data.len().div_ceil(WORKER_CHUNK_BYTES) as u64,
            chunks: candidate.data.len().div_ceil(WORKER_CHUNK_BYTES) as u16,
            bytes: candidate.data.len() as u32,
            sha256: digest(&candidate.data),
        }
    }

    #[tokio::test]
    async fn private_command_requires_an_admitted_image() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = runtime(temp.path());
        let mut intent = runtime.intent(
            crate::session::RunId::new(),
            AgentRunId::new(),
            ToolCall::Command {
                program: "check".into(),
                args: Vec::new(),
                cwd: ".".into(),
            },
        );
        intent.target = EffectTarget::WorkerView {
            assignment_id: intent.agent_run_id,
            base_digest: [1; 32],
            revision: 0,
            cache_generation: None,
        };
        let observation = runtime.execute(intent, Cancellation::new()).await;
        assert_eq!(observation.disposition, ToolDisposition::Denied);
        assert!(observation.attestation.is_none());
        assert!(observation.output == crate::tools::PRIVATE_IMAGE_REQUIRED);
        assert!(observation.valid());
    }

    #[test]
    fn private_target_reads_own_acknowledged_edits_and_keeps_siblings_and_workspace_isolated() {
        let (dir, runtime, mut view) = fixture();
        let sibling = View::capture(
            &runtime,
            crate::session::RunId::new(),
            AgentRunId::new(),
            grant(),
            WORKER_DATA_BYTES as u32,
        )
        .unwrap();
        std::fs::write(dir.path().join("owned/a"), b"host drift").unwrap();
        let edit = intent(
            &runtime,
            &view,
            ToolCall::Edit {
                path: "owned/a".into(),
                expected_digest: hex_digest(b"original"),
                old: "original".into(),
                new: "private".into(),
            },
        );
        let candidate = captured(&mut view, edit);
        assert!(
            view.handover().is_err(),
            "unacknowledged edits cannot hand over"
        );
        let data = reference(&candidate);
        for chunk in candidate.chunks() {
            assert!(chunk.decoded().is_some());
        }
        let next_sequence = data.seal_sequence + 1;
        let observation = view.acknowledge(candidate, data).unwrap();
        assert!(observation.valid());
        assert!(!matches!(
            observation.attestation,
            Some(Attestation::Guard(_))
        ));
        for (actor, expected) in [(&view, "private"), (&sibling, "original")] {
            let read = intent(
                &runtime,
                actor,
                ToolCall::Read {
                    path: "owned/a".into(),
                    offset: None,
                    limit: None,
                },
            );
            let observed = actor.read(read);
            assert!(observed.valid());
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&observed.output).unwrap()["text"],
                format!("1\t{expected}\n")
            );
        }
        let edit = intent(
            &runtime,
            &view,
            ToolCall::Edit {
                path: "owned/a".into(),
                expected_digest: hex_digest(b"private"),
                old: "private".into(),
                new: "revised".into(),
            },
        );
        let candidate = captured(&mut view, edit);
        let incremental: WorkerChangeset = serde_json::from_slice(&candidate.data).unwrap();
        assert_eq!(
            incremental.files[0].before.as_ref().unwrap().sha256,
            hex_digest(b"private")
        );
        let mut data = reference(&candidate);
        data.first_sequence = next_sequence;
        data.seal_sequence = next_sequence + u64::from(data.chunks);
        view.acknowledge(candidate, data).unwrap();
        let listing = view.read(intent(
            &runtime,
            &view,
            ToolCall::List {
                path: "owned".into(),
            },
        ));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&listing.output).unwrap()["may_be_truncated"],
            false
        );
        let search = |query: &str, include: Option<&str>, case_sensitive: bool, offset: u32| {
            view.read(intent(
                &runtime,
                &view,
                ToolCall::Search {
                    path: "owned".into(),
                    query: query.into(),
                    regex: true,
                    include: include.map(str::to_owned),
                    case_sensitive,
                    offset,
                    limit: 100,
                },
            ))
        };
        let page = |observed: ToolObservation| {
            assert_eq!(observed.disposition, ToolDisposition::Succeeded);
            serde_json::from_str::<serde_json::Value>(&observed.output).unwrap()
        };
        assert_eq!(
            page(search(r"^rev\w+$", Some("a"), true, 0)),
            json!({"entries":[{"path":"owned/a","line":1,"text":"revised"}],"limit":100,"may_be_truncated":false,"next_offset":null})
        );
        assert_eq!(
            page(search("REVISED", None, false, 0))["entries"][0]["line"],
            1
        );
        for (query, include, case_sensitive, offset) in [
            ("REVISED", None, true, 0),
            ("revised", Some("b"), true, 0),
            ("revised", None, true, 1),
        ] {
            assert_eq!(
                page(search(query, include, case_sensitive, offset))["entries"],
                json!([])
            );
        }
        let mut refused = intent(
            &runtime,
            &view,
            ToolCall::Search {
                path: "owned".into(),
                query: "(".into(),
                regex: true,
                include: None,
                case_sensitive: true,
                offset: 0,
                limit: 100,
            },
        );
        refused.limits.result_bytes = 128;
        let refused = view.read(refused);
        assert_eq!(refused.disposition, ToolDisposition::Failed);
        assert!(refused.valid() && refused.output.contains("invalid or unsupported regex"));
        let handover = view.handover().unwrap();
        assert!(decode_data(&handover, WorkerDataPurpose::Handover).is_ok());
        let final_changes: WorkerChangeset = serde_json::from_slice(&handover).unwrap();
        assert_eq!(
            final_changes.files[0].before.as_ref().unwrap().sha256,
            hex_digest(b"original")
        );
        assert_eq!(
            final_changes.files[0].after.as_ref().unwrap().sha256,
            hex_digest(b"revised")
        );
        assert_eq!(
            std::fs::read(dir.path().join("owned/a")).unwrap(),
            b"host drift"
        );
        assert_eq!(view.dispose().unwrap().revision, 2);
    }

    #[test]
    fn command_leases_refuse_paths_the_capture_omits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("owned/target")).unwrap();
        std::fs::write(dir.path().join("owned/target/mod.rs"), b"x").unwrap();
        let runtime = runtime(dir.path());
        let capture = |grant: WorkerGrant| {
            View::capture(
                &runtime,
                crate::session::RunId::new(),
                AgentRunId::new(),
                grant,
                WORKER_DATA_BYTES as u32,
            )
        };
        assert!(capture(grant()).is_ok());
        let mut command = grant();
        command.commands = vec!["sh".into()];
        command.tools.push(WorkerTool::Command);
        assert!(matches!(capture(command), Err(ToolError::Path)));
    }

    #[tokio::test]
    async fn edit_refuses_commands_even_when_the_grant_allows_them() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = runtime(dir.path());
        let mut allowed = grant();
        allowed.commands = vec!["sh".into()];
        allowed.tools.push(WorkerTool::Command);
        let mut view = View::capture(
            &runtime,
            crate::session::RunId::new(),
            AgentRunId::new(),
            allowed,
            WORKER_DATA_BYTES as u32,
        )
        .unwrap();
        let call = ToolCall::Command {
            program: "sh".into(),
            args: vec![],
            cwd: ".".into(),
        };
        let refused = view
            .edit(&runtime, intent(&runtime, &view, call), Cancellation::new())
            .await
            .err()
            .unwrap();
        assert_eq!(refused.disposition, ToolDisposition::Denied);
        assert!(view.pending.is_none());
    }

    #[tokio::test]
    async fn private_target_refuses_scope_identity_revision_and_model_admission_before_dispatch() {
        let (dir, runtime, mut view) = fixture();
        for path in [
            "sibling/a",
            "Owned/a",
            "owned/.ENV",
            "owned/../a",
            "owned/é",
            "owned/a\\b",
        ] {
            let call = ToolCall::Write {
                path: path.into(),
                expected_digest: None,
                content: "bad".into(),
            };
            let result = view
                .edit(&runtime, intent(&runtime, &view, call), Cancellation::new())
                .await;
            assert_eq!(
                result.err().unwrap().disposition,
                ToolDisposition::Denied,
                "{path}"
            );
            assert!(view.pending.is_none());
        }
        let call = ToolCall::Write {
            path: "owned/a".into(),
            expected_digest: Some(hex_digest(b"original")),
            content: "private".into(),
        };
        let valid = intent(&runtime, &view, call);
        for field in 0..8 {
            let mut invalid = valid.clone();
            match field {
                0 => invalid.agent_run_id = AgentRunId::new(),
                1 => invalid.target = EffectTarget::Workspace,
                2 => {
                    invalid.target = EffectTarget::WorkerView {
                        cache_generation: None,
                        assignment_id: view.assignment,
                        base_digest: [9; 32],
                        revision: 0,
                    }
                }
                3 => {
                    invalid.target = EffectTarget::WorkerView {
                        cache_generation: None,
                        assignment_id: view.assignment,
                        base_digest: view.base_digest,
                        revision: 1,
                    };
                }
                4 => invalid.run_id = crate::session::RunId::new(),
                5 => invalid.policy_digest = [8; 32],
                6 => invalid.workspace_inode += 1,
                _ => invalid.expires_at_ms = 1,
            }
            assert!(
                view.edit(&runtime, invalid, Cancellation::new())
                    .await
                    .is_err()
            );
        }
        view.remaining_data = 1;
        let overflow = view
            .edit(&runtime, valid.clone(), Cancellation::new())
            .await
            .err()
            .unwrap();
        assert_eq!(overflow.disposition, ToolDisposition::Failed);
        assert!(overflow.output.contains("resource limit"));
        assert!(view.pending.is_none());
        let refused = runtime.execute(valid, Cancellation::new()).await;
        assert_eq!(refused.disposition, ToolDisposition::Denied);
        assert_eq!(
            std::fs::read(dir.path().join("owned/a")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn worker_command_capture_rejects_whole_unsafe_or_out_of_lease_deltas() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        for case in [
            "inside",
            "delete",
            "outside",
            "symlink",
            "hardlink",
            "fifo",
            "setid",
            "excluded",
            "conversion",
            "empty",
            "overflow",
        ] {
            let (_host, _runtime, view) = fixture();
            let root = tempfile::tempdir().unwrap();
            let directory = fs::open_directory(root.path()).unwrap();
            materialize(&view.current, &directory).unwrap();
            std::fs::write(root.path().join("owned/a"), b"accepted only as a whole").unwrap();
            let bad = root.path().join("owned/bad");
            match case {
                "inside" => {
                    std::fs::write(&bad, vec![0xff; 16 * 1024]).unwrap();
                    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o700)).unwrap();
                }
                "delete" => std::fs::remove_file(root.path().join("owned/a")).unwrap(),
                "outside" => std::fs::write(root.path().join("outside"), b"no lease").unwrap(),
                "symlink" => symlink("a", &bad).unwrap(),
                "hardlink" => std::fs::hard_link(root.path().join("owned/a"), &bad).unwrap(),
                "fifo" => nix::unistd::mkfifo(&bad, nix::sys::stat::Mode::S_IRUSR).unwrap(),
                "setid" => std::fs::set_permissions(
                    root.path().join("owned/a"),
                    std::fs::Permissions::from_mode(0o4600),
                )
                .unwrap(),
                "excluded" => std::fs::create_dir(root.path().join("owned/target")).unwrap(),
                "conversion" => {
                    std::fs::remove_file(root.path().join("owned/a")).unwrap();
                    std::fs::create_dir(root.path().join("owned/a")).unwrap();
                }
                "empty" => std::fs::create_dir(root.path().join("owned/empty")).unwrap(),
                "overflow" => std::fs::write(&bad, vec![b'x'; MAX_FILE_BYTES + 1]).unwrap(),
                _ => unreachable!(),
            }
            let (image, capture) = command_capture(&view.current, &directory, &view.grant);
            let expected = match case {
                "inside" | "empty" | "delete" => PrivateCapture::None {},
                "outside" => PrivateCapture::Rejected {
                    cause: PrivateCaptureRejection::OutOfLease,
                },
                "overflow" => PrivateCapture::Rejected {
                    cause: PrivateCaptureRejection::Overflow,
                },
                _ => PrivateCapture::Rejected {
                    cause: PrivateCaptureRejection::UnsupportedEntry,
                },
            };
            assert_eq!(capture, expected, "{case}");
            if matches!(capture, PrivateCapture::Rejected { .. }) {
                assert_eq!(image, view.current, "{case}");
            } else {
                let data = changes(&view.current, &image).unwrap();
                let WorkerDataFacts::Changes(files) =
                    decode_data(&data, WorkerDataPurpose::Mutation).unwrap()
                else {
                    panic!("changes")
                };
                assert_eq!(files.len(), if case == "inside" { 2 } else { 1 });
                assert!(!image.entries.contains_key("owned/empty"));
            }
        }
    }

    #[test]
    fn worker_command_overlay_requires_acknowledgment_and_preserves_rejected_revisions() {
        for case in [
            "sealed",
            "empty",
            "rejected",
            "allowance",
            "retained",
            "cumulative",
            "uncertain",
            "cancelled",
        ] {
            let (_dir, runtime, mut view) = fixture();
            let intent = intent(
                &runtime,
                &view,
                ToolCall::Command {
                    program: "sh".into(),
                    args: vec![],
                    cwd: ".".into(),
                },
            );
            let mut image = view.current.clone();
            if matches!(case, "sealed" | "allowance" | "retained" | "cumulative") {
                image
                    .entries
                    .insert("owned/a".into(), Entry::file(b"private", false));
            }
            if case == "allowance" {
                view.remaining_data = 1;
            }
            if case == "retained" {
                view.retained_limit = 1;
            }
            if case == "cumulative" {
                for i in 0..32 {
                    Arc::make_mut(&mut view.base).entries.insert(
                        format!("owned/base-{i}"),
                        Entry::file(&vec![0; MAX_FILE_BYTES], false),
                    );
                }
            }
            let observation = ToolObservation {
                intent: intent.clone(),
                disposition: if case == "uncertain" {
                    ToolDisposition::Uncertain
                } else if case == "cancelled" {
                    ToolDisposition::Cancelled
                } else {
                    ToolDisposition::Succeeded
                },
                output: serde_json::to_string(&WorkerCommandResult {
                    exit_code: Some(0),
                    stdout: "ok".into(),
                    stderr: String::new(),
                    stdout_total: crate::tools::types::OutputTotal { bytes: 2, lines: 1 },
                    stderr_total: Default::default(),
                    private_capture: if case == "rejected" {
                        PrivateCapture::Rejected {
                            cause: PrivateCaptureRejection::OutOfLease,
                        }
                    } else {
                        PrivateCapture::None {}
                    },
                })
                .unwrap(),
                attestation: Some(Attestation::Guard(super::super::GuardReceipt {
                    contract_version: 1,
                    intent_digest: intent.digest(),
                    enforcement_digest: intent.enforcement_digest,
                    limits: intent.limits.clone(),
                })),
                enforcement_cause: None,
                limit_cause: None,
                cache_update: super::super::CacheUpdate::Discarded,
            };
            let action = Action {
                transfer: Transfer {
                    target: view.target(),
                    grant: view.grant.clone(),
                    bytes: 1,
                    sha256: [1; 32],
                },
                input: vec![1].into(),
                output: Mutex::new(Some(image.encode().unwrap())),
                contained: std::sync::atomic::AtomicBool::new(true),
            };
            view.pending = Some(intent.id);
            match view.receive_command(observation, &action) {
                Ok(candidate) => {
                    assert_eq!(case, "sealed");
                    assert_eq!(view.revision, 0);
                    assert!(view.handover().is_err());
                    let reference = reference(&candidate);
                    let observation = candidate.observation(&reference).unwrap();
                    assert!(observation.valid());
                    let observation = view.acknowledge(candidate, reference).unwrap();
                    assert!(matches!(
                        observation.attestation,
                        Some(Attestation::WorkerCommand { .. })
                    ));
                    assert_eq!(view.revision, 1);
                }
                Err(observation) => {
                    assert_ne!(case, "sealed");
                    assert_eq!(view.revision, 0);
                    if matches!(case, "uncertain" | "cancelled") {
                        assert!(view.stopped);
                        assert!(observation.valid());
                    } else {
                        assert!(observation.valid(), "{case}");
                        assert!(!view.stopped);
                        if matches!(case, "allowance" | "retained" | "cumulative") {
                            let result: WorkerCommandResult =
                                serde_json::from_str(&observation.output).unwrap();
                            assert_eq!(
                                result.private_capture,
                                PrivateCapture::Rejected {
                                    cause: PrivateCaptureRejection::Overflow
                                }
                            );
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn forged_guard_images_end_uncertain_and_stop_the_view() {
        for case in ["outside_lease", "rejected_modified", "directory_to_file"] {
            let (_dir, runtime, mut view) = fixture();
            let intent = intent(
                &runtime,
                &view,
                ToolCall::Command {
                    program: "sh".into(),
                    args: vec![],
                    cwd: ".".into(),
                },
            );
            let mut image = view.current.clone();
            match case {
                "outside_lease" => {
                    image
                        .entries
                        .insert("sibling.txt".into(), Entry::file(b"forged", false));
                }
                "rejected_modified" => {
                    image
                        .entries
                        .insert("owned/a".into(), Entry::file(b"forged", false));
                }
                _ => {
                    image
                        .entries
                        .insert("owned".into(), Entry::file(b"x", false));
                    image.entries.remove("owned/a");
                }
            }
            let observation = ToolObservation {
                intent: intent.clone(),
                disposition: ToolDisposition::Succeeded,
                output: serde_json::to_string(&WorkerCommandResult {
                    exit_code: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                    stdout_total: Default::default(),
                    stderr_total: Default::default(),
                    private_capture: if case == "rejected_modified" {
                        PrivateCapture::Rejected {
                            cause: PrivateCaptureRejection::OutOfLease,
                        }
                    } else {
                        PrivateCapture::None {}
                    },
                })
                .unwrap(),
                attestation: Some(Attestation::Guard(super::super::GuardReceipt {
                    contract_version: 1,
                    intent_digest: intent.digest(),
                    enforcement_digest: intent.enforcement_digest,
                    limits: intent.limits.clone(),
                })),
                enforcement_cause: None,
                limit_cause: None,
                cache_update: super::super::CacheUpdate::Discarded,
            };
            let action = Action {
                transfer: Transfer {
                    target: view.target(),
                    grant: view.grant.clone(),
                    bytes: 1,
                    sha256: [1; 32],
                },
                input: vec![1].into(),
                output: Mutex::new(Some(image.encode().unwrap())),
                contained: std::sync::atomic::AtomicBool::new(true),
            };
            view.pending = Some(intent.id);
            let refused = view
                .receive_command(observation, &action)
                .err()
                .expect(case);
            assert_eq!(refused.disposition, ToolDisposition::Uncertain, "{case}");
            assert!(view.stopped, "{case}");
            assert!(view.pending.is_none(), "{case}");
            assert_eq!(view.revision, 0, "{case}");
            assert!(view.handover().is_err(), "{case}");
            let retried = view
                .command(&runtime, intent.clone(), Cancellation::new())
                .await
                .err()
                .expect(case);
            assert_eq!(retried.disposition, ToolDisposition::Denied, "{case}");
        }
    }

    #[test]
    fn cleanup_binds_the_enforcement_of_the_last_intent_class() {
        let (_dir, runtime, mut view) = fixture();
        view.process_enforcement_digest = [9; 32];
        let command = ToolCall::Command {
            program: "sh".into(),
            args: Vec::new(),
            cwd: ".".into(),
        };
        let mut intent = intent(&runtime, &view, command);
        intent.enforcement_digest = view.process_enforcement_digest;
        assert_ne!(view.enforcement_digest, view.process_enforcement_digest);
        view.note_intent(&intent);
        let cleanup = view.dispose().unwrap();
        assert_eq!(cleanup.enforcement_digest, intent.enforcement_digest);
        assert_eq!(cleanup.last_intent_digest, Some(intent.digest()));
    }

    #[test]
    fn dispose_refuses_a_view_with_a_dropped_pending_dispatch() {
        let (_dir, _runtime, mut view) = fixture();
        view.pending = Some(uuid::Uuid::nil());
        assert!(matches!(view.dispose(), Err(ToolError::Uncertain)));
    }

    #[test]
    fn private_creation_requires_an_existing_parent_directory_before_dispatch() {
        let (_dir, _runtime, view) = fixture();
        let write = |path: &str| ToolCall::Write {
            path: path.into(),
            content: "x".into(),
            expected_digest: None,
        };
        for call in [
            write("owned/missing/leaf"),
            ToolCall::Mkdir {
                path: "owned/missing/leaf".into(),
            },
            write("owned/a/leaf"),
        ] {
            assert!(edited_image(&view.current, &call).is_err());
        }
        assert!(edited_image(&view.current, &write("owned/leaf")).is_ok());
        assert!(
            edited_image(
                &view.current,
                &ToolCall::Mkdir {
                    path: "owned/dir".into()
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn worker_snapshot_refuses_links_special_modes_case_aliases_protected_paths_and_root_drift() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        for kind in ["symlink", "hardlink", "setid", "protected", "case", "fifo"] {
            let (dir, runtime, _) = fixture();
            let bad = dir.path().join("owned/bad");
            if kind == "case" && dir.path().join("owned/A").exists() {
                continue;
            }
            match kind {
                "symlink" => symlink("a", &bad).unwrap(),
                "hardlink" => std::fs::hard_link(dir.path().join("owned/a"), &bad).unwrap(),
                "setid" => std::fs::set_permissions(
                    dir.path().join("owned/a"),
                    std::fs::Permissions::from_mode(0o4600),
                )
                .unwrap(),
                "protected" => {
                    std::fs::create_dir(dir.path().join("owned/.git")).unwrap();
                }
                "case" => std::fs::write(dir.path().join("owned/A"), b"alias").unwrap(),
                _ => nix::unistd::mkfifo(&bad, nix::sys::stat::Mode::S_IRUSR).unwrap(),
            }
            assert!(
                View::capture(
                    &runtime,
                    crate::session::RunId::new(),
                    AgentRunId::new(),
                    grant(),
                    WORKER_DATA_BYTES as u32
                )
                .is_err(),
                "{kind}"
            );
        }
        let (dir, runtime, _) = fixture();
        let saved = dir.path().with_extension("saved");
        std::fs::rename(dir.path(), &saved).unwrap();
        std::fs::create_dir(dir.path()).unwrap();
        assert!(
            View::capture(
                &runtime,
                crate::session::RunId::new(),
                AgentRunId::new(),
                grant(),
                WORKER_DATA_BYTES as u32
            )
            .is_err()
        );
        std::fs::rename(&saved, dir.path().join("retired")).unwrap();
    }

    #[test]
    fn worker_candidates_accept_exact_file_and_byte_bounds_without_raising_command_caps() {
        let mut before = Image {
            entries: BTreeMap::new(),
        };
        let mut after = before.clone();
        for index in 0..WORKER_FILES {
            let path = format!("owned-{index:04}");
            if index < 16 {
                before.entries.insert(
                    path.clone(),
                    Entry::file(&vec![b'a'; MAX_FILE_BYTES], false),
                );
                after
                    .entries
                    .insert(path, Entry::file(&vec![b'b'; MAX_FILE_BYTES], false));
            } else {
                after.entries.insert(path, Entry::file(b"", false));
            }
        }
        before.validate(&grant()).unwrap();
        after.validate(&grant()).unwrap();
        let bytes = changes(&before, &after).unwrap();
        let WorkerDataFacts::Changes(facts) =
            decode_data(&bytes, WorkerDataPurpose::Mutation).unwrap()
        else {
            panic!("changes")
        };
        assert_eq!(facts.len(), WORKER_FILES);
        let command: super::super::CommandChangeset = serde_json::from_slice(&bytes).unwrap();
        assert!(
            !command.valid(),
            "command still owns its 64-file/8-KiB ceiling"
        );
        let mut too_many = after.clone();
        too_many.entries.insert("overflow".into(), Entry::Directory);
        assert!(matches!(too_many.validate(&grant()), Err(ToolError::Limit)));
        after
            .entries
            .insert("owned-0016".into(), Entry::file(b"x", false));
        assert!(matches!(changes(&before, &after), Err(ToolError::Limit)));
        let mut base32 = Image {
            entries: BTreeMap::new(),
        };
        let file = Entry::file(&vec![b'z'; MAX_FILE_BYTES], false);
        for i in 0..32 {
            base32.entries.insert(format!("file-{i:04}"), file.clone());
        }
        base32.validate(&grant()).unwrap();
        base32
            .entries
            .insert("overflow".into(), Entry::file(b"x", false));
        assert!(matches!(base32.validate(&grant()), Err(ToolError::Limit)));
        let directory = tempfile::tempdir().unwrap();
        for i in 0..WORKER_FILES {
            std::fs::write(directory.path().join(format!("f-{i:04}")), []).unwrap();
        }
        let root = fs::open_directory(directory.path()).unwrap();
        assert_eq!(
            fs::worker_image(&root, &[".".into()])
                .unwrap()
                .entries
                .len(),
            WORKER_FILES
        );
        std::fs::write(directory.path().join("overflow"), []).unwrap();
        assert!(matches!(
            fs::worker_image(&root, &[".".into()]),
            Err(ToolError::Limit)
        ));
        assert_eq!(WORKER_CHUNKS, 1093);
        assert_eq!(WORKER_MANIFEST_CHUNKS, 69);
    }

    #[tokio::test]
    async fn worker_bulk_frames_reject_corruption_short_duplicate_and_trailing_frames() {
        let bytes = vec![b'x'; WORKER_CHUNK_BYTES + 1];
        let mut wire = Vec::new();
        write_frame(&mut wire, &bytes).unwrap();
        assert!(
            read_frame(&mut &wire[..]).unwrap() == bytes,
            "synchronous frame bytes"
        );
        assert!(
            receive_frame(&mut &wire[..]).await.unwrap() == bytes,
            "asynchronous frame bytes"
        );
        let mut rows = vec![wire[..wire.len() - 1].to_vec(), wire[..40].to_vec()];
        let mut corrupt = wire.clone();
        corrupt[4] ^= 1;
        rows.push(corrupt);
        let mut short = wire.clone();
        short[36..40].copy_from_slice(&1u32.to_le_bytes());
        rows.push(short);
        let mut duplicate = wire.clone();
        duplicate[40 + WORKER_CHUNK_BYTES..44 + WORKER_CHUNK_BYTES]
            .copy_from_slice(&(WORKER_CHUNK_BYTES as u32).to_le_bytes());
        rows.push(duplicate);
        let mut oversized = wire.clone();
        oversized[..4].copy_from_slice(&(WORKER_DATA_BYTES as u32 + 1).to_le_bytes());
        rows.push(oversized);
        for row in rows {
            assert!(read_frame(&mut &row[..]).is_err());
            assert!(receive_frame(&mut &row[..]).await.is_err());
        }
        let (dir, runtime, mut view) = fixture();
        let call = ToolCall::Write {
            path: "owned/a".into(),
            expected_digest: Some(hex_digest(b"original")),
            content: "new".into(),
        };
        let intent = intent(&runtime, &view, call);
        let input = view.current.encode().unwrap();
        let transfer = Transfer {
            target: view.target(),
            grant: grant(),
            bytes: input.len() as u32,
            sha256: digest(&input),
        };
        let mut changed = input.clone();
        changed.push(b' ');
        assert!(transfer.decode(&changed).is_err());
        let mut action_runtime = runtime.clone();
        action_runtime.worker = Some(Arc::new(Action {
            transfer,
            input: input.into(),
            output: Mutex::new(None),
            contained: std::sync::atomic::AtomicBool::new(false),
        }));
        let observation = ToolObservation {
            intent: intent.clone(),
            disposition: ToolDisposition::Succeeded,
            output: "ok".into(),
            attestation: Some(Attestation::Guard(super::super::GuardReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                enforcement_digest: intent.enforcement_digest,
                limits: intent.limits.clone(),
            })),
            enforcement_cause: None,
            limit_cause: None,
            cache_update: super::super::CacheUpdate::NotApplicable,
        };
        assert!(wire_valid(&observation, &intent));
        let mut malformed = observation.clone();
        malformed.limit_cause = Some(super::super::LimitCause::Resource);
        assert!(!wire_valid(&malformed, &intent));
        malformed.disposition = ToolDisposition::Limit;
        assert!(wire_valid(&malformed, &intent));
        malformed.limit_cause = None;
        assert!(!wire_valid(&malformed, &intent));
        let mut response = serde_json::to_vec(&observation).unwrap();
        response.push(b'\n');
        write_frame(
            &mut response,
            &edited_image(&view.current, &intent.call)
                .unwrap()
                .encode()
                .unwrap(),
        )
        .unwrap();
        response.push(1);
        let error = super::super::guard::read_worker_observation(
            &mut &response[..],
            &action_runtime,
            &intent,
            std::time::Duration::from_millis(u64::from(intent.limits.runtime_ms)),
        )
        .await
        .unwrap_err();
        let pre_error = read_frame(&mut &wire[..40]).unwrap_err();
        let before_go =
            super::super::guard::dispatch_result(&intent, false, Err(pre_error)).unwrap();
        assert_eq!(before_go.disposition, ToolDisposition::Failed);
        assert!(before_go.attestation.is_none());
        let after_go = super::super::guard::dispatch_result(&intent, true, Err(error));
        view.pending = Some(intent.id);
        let unknown = view
            .receive(
                intent.clone(),
                action_runtime.worker.as_deref().unwrap(),
                after_go,
            )
            .err()
            .unwrap();
        assert_eq!(unknown.disposition, ToolDisposition::Uncertain);
        assert!(matches!(
            unknown.intent.target,
            EffectTarget::WorkerView { .. }
        ));
        assert!(view.stopped && view.handover().is_err());
        assert!(
            view.edit(&runtime, intent, Cancellation::new())
                .await
                .is_err()
        );
        assert!(
            view.dispose().is_err(),
            "unknown containment cannot claim disposal"
        );
        assert_eq!(
            std::fs::read(dir.path().join("owned/a")).unwrap(),
            b"original"
        );
    }
    #[test]
    fn worker_handover_transport_preserves_sources_and_rejects_whole_conflicting_or_aliased_batches()
     {
        let (dir, runtime, view) = fixture();
        let old = vec![b'a'; 16 * 1024];
        let new = vec![b'b'; 16 * 1024];
        std::fs::write(dir.path().join("owned/a"), &old).unwrap();
        std::fs::write(dir.path().join("owned/b"), b"current").unwrap();
        let change = FileChange {
            path: "owned/a".into(),
            before: Some(FileVersion::from_bytes(&old, false)),
            after: Some(FileVersion::from_bytes(&new, false)),
        };
        let encode = |files| serde_json::to_vec(&WorkerChangeset { files }).unwrap();
        let call = |bytes: &[u8]| {
            let data = WorkerDataRef {
                assignment_id: view.assignment,
                purpose: WorkerDataPurpose::Handover,
                first_sequence: 1,
                seal_sequence: bytes.len().div_ceil(WORKER_CHUNK_BYTES) as u64 + 1,
                chunks: bytes.len().div_ceil(WORKER_CHUNK_BYTES) as u16,
                bytes: u32::try_from(bytes.len()).expect("bounded worker transfer bytes"),
                sha256: digest(bytes),
            };
            runtime.intent(
                view.run_id,
                AgentRunId::new(),
                ToolCall::ApplyChangeset {
                    source: ChangesetSource::WorkerHandover {
                        assignment_id: view.assignment,
                        handover_sequence: data.seal_sequence + 1,
                        data,
                    },
                },
            )
        };
        let bytes = encode(vec![change.clone()]);
        let changes = decode_handover(&call(&bytes), &bytes, &runtime.config).unwrap();
        let rewind = runtime.intent(
            view.run_id,
            AgentRunId::new(),
            ToolCall::ApplyChangeset {
                source: ChangesetSource::Rewind {
                    plan_digest: [7; 32],
                    bytes: bytes.len() as u32,
                    sha256: digest(&bytes),
                },
            },
        );
        assert_eq!(
            decode_handover(&rewind, &bytes, &runtime.config).unwrap(),
            changes
        );
        let mut tampered = bytes.clone();
        tampered.push(b' ');
        assert!(decode_handover(&rewind, &tampered, &runtime.config).is_err());
        let mut readonly = runtime.config.clone();
        readonly.write = false;
        assert!(decode_handover(&rewind, &bytes, &readonly).is_err());

        assert!(
            !super::super::CommandChangeset {
                files: changes.files.clone()
            }
            .valid()
        );
        let mut wrong_source = call(&bytes);
        wrong_source.target = view.target();
        assert!(decode_handover(&wrong_source, &bytes, &runtime.config).is_err());
        let mut changed = bytes.clone();
        changed.push(b' ');
        assert!(decode_handover(&call(&bytes), &changed, &runtime.config).is_err());
        let conflict = encode(vec![
            change.clone(),
            FileChange {
                path: "owned/b".into(),
                before: Some(FileVersion::from_bytes(b"stale", false)),
                after: Some(FileVersion::from_bytes(b"new", false)),
            },
        ]);
        let conflict = decode_handover(&call(&conflict), &conflict, &runtime.config).unwrap();
        assert!(matches!(
            fs::apply_worker_changeset(dir.path(), &conflict),
            Err(ToolError::Conflict)
        ));
        assert_eq!(std::fs::read(dir.path().join("owned/a")).unwrap(), old);
        for path in ["owned/A", "Owned/a"] {
            let alias = encode(vec![FileChange {
                path: path.into(),
                ..change.clone()
            }]);
            let alias = decode_handover(&call(&alias), &alias, &runtime.config).unwrap();
            assert!(
                fs::apply_worker_changeset(dir.path(), &alias).is_err(),
                "{path}"
            );
        }
        for path in ["owned/.env", "owned/é", "owned/../a"] {
            let protected = encode(vec![FileChange {
                path: path.into(),
                ..change.clone()
            }]);
            assert!(
                decode_handover(&call(&protected), &protected, &runtime.config).is_err(),
                "{path}"
            );
        }
        fs::apply_worker_changeset(dir.path(), &changes).unwrap();
        assert_eq!(std::fs::read(dir.path().join("owned/a")).unwrap(), new);
        assert_eq!(
            std::fs::read(dir.path().join("owned/b")).unwrap(),
            b"current"
        );
    }
    #[test]
    fn private_creation_root_keeps_absence_and_prepares_only_its_parent_directories() {
        let (dir, runtime, _) = fixture();
        let mut grant = grant();
        grant.read_paths = vec!["owned/new".into()];
        grant.write_paths = grant.read_paths.clone();
        let mut view = View::capture(
            &runtime,
            crate::session::RunId::new(),
            AgentRunId::new(),
            grant,
            WORKER_DATA_BYTES as u32,
        )
        .unwrap();
        assert_eq!(view.base.entries.get("owned/new"), Some(&Entry::Absent));
        let mkdir = intent(
            &runtime,
            &view,
            ToolCall::Mkdir {
                path: "owned/new".into(),
            },
        );
        let candidate = captured(&mut view, mkdir);
        let empty: WorkerChangeset = serde_json::from_slice(&candidate.data).unwrap();
        assert!(empty.files.is_empty());
        let data = reference(&candidate);
        let next_sequence = data.seal_sequence + 1;
        view.acknowledge(candidate, data).unwrap();
        let write = intent(
            &runtime,
            &view,
            ToolCall::Write {
                path: "owned/new/file".into(),
                expected_digest: None,
                content: "created".into(),
            },
        );
        let candidate = captured(&mut view, write);
        let mut data = reference(&candidate);
        data.first_sequence = next_sequence;
        data.seal_sequence = next_sequence + u64::from(data.chunks);
        view.acknowledge(candidate, data).unwrap();
        let changes: WorkerChangeset = serde_json::from_slice(&view.handover().unwrap()).unwrap();
        assert_eq!(changes.files.len(), 1);
        assert!(changes.files[0].before.is_none());
        assert!(!dir.path().join("owned/new").exists());
    }
}

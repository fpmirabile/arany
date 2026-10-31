use super::{Engine, EngineError, RunCancellation, ToolGrant, ensure_session_workspace};
use crate::session::input::WorkspaceInputs;
use crate::session::rewind::{
    self, CheckpointTarget, FileState, Overwrite, RewindAttestation, RewindFinished, RewindOutcome,
    RewindPlan, RewindStarted,
};
use crate::session::{
    AgentRunId, Checkpoint, CheckpointReason, CheckpointStatus, Event, ForkLineage, RunId,
    SessionDefaults, SessionId, SessionView,
};
use crate::tools::{
    Attestation, FileChange, FileVersion, ToolCall, ToolDisposition, ToolObservation, ToolRuntime,
};
use crate::worker::{ChangesetSource, WorkerChangeset};
use std::path::PathBuf;
use std::time::Duration;

const REWIND_TIMEOUT: Duration = Duration::from_secs(120);
const REWIND_CHECKPOINT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub struct ConversationRewind {
    pub session_id: Option<SessionId>,
    pub defaults: SessionDefaults,
    pub prompt: Option<String>,
    pub images: Vec<crate::ImageAttachment>,
}

#[derive(Debug)]
pub enum ConversationRewindOutcome {
    Restored(Box<ConversationRewind>),
    Unchanged,
    Failed(EngineError),
}

#[derive(Debug)]
pub struct RewindBothResult {
    pub code: RewindFinished,
    pub conversation: ConversationRewindOutcome,
}

struct PreparedConversation {
    boundary: Option<ForkLineage>,
    conversation: ConversationRewind,
}

pub(super) fn fork_error(error: crate::store::StoreError) -> EngineError {
    match error {
        crate::store::StoreError::ReplayLimit => EngineError::ConversationRewindLimit,
        crate::store::StoreError::ForkNotAtRunBoundary => EngineError::ForkNotAtRunBoundary,
        other => other.into(),
    }
}

pub struct RewindRequest {
    pub session_id: SessionId,
    pub workspace: PathBuf,
    pub target: CheckpointTarget,
    pub tools: ToolGrant,
    pub kept: Vec<String>,
    pub overwrite: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RewindIssue {
    Drifted,
    HistoryDrift,
    Unavailable,
    Unsafe,
    OutsideGrant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewindFile {
    pub path: String,
    pub observed: Option<FileState>,
    pub issues: Vec<RewindIssue>,
    pub kept: bool,
}

pub struct RewindPreview {
    request: RewindRequest,
    files: Vec<RewindFile>,
    plan: RewindPlan,
    started: RewindStarted,
    permissions_digest: Option<[u8; 32]>,
    comparisons: Vec<FileChange>,
    checkpoint_storage_unavailable: bool,
}
impl RewindPreview {
    pub fn rewind_id(&self) -> uuid::Uuid {
        self.started.rewind_id
    }
    pub fn files(&self) -> &[RewindFile] {
        &self.files
    }
    pub fn plan(&self) -> &RewindPlan {
        &self.plan
    }
    pub fn comparisons(&self) -> &[FileChange] {
        &self.comparisons
    }
    pub fn checkpoint_storage_unavailable(&self) -> bool {
        self.checkpoint_storage_unavailable
    }
    pub fn observations_match(&self, previous: &Self) -> bool {
        self.request.session_id == previous.request.session_id
            && self.request.target == previous.request.target
            && self.started.head_sequence == previous.started.head_sequence
            && self.started.prefix_digest == previous.started.prefix_digest
            && self.started.policy_digest == previous.started.policy_digest
            && self.started.enforcement_digest == previous.started.enforcement_digest
            && self.permissions_digest == previous.permissions_digest
            && self.files.len() == previous.files.len()
            && self
                .files
                .iter()
                .zip(&previous.files)
                .all(|(a, b)| a.path == b.path && a.observed == b.observed)
    }
    pub fn can_restore(&self) -> bool {
        !self.plan.files.is_empty()
            && self.plan.blockers.is_empty()
            && self.files.iter().all(|f| f.kept || f.issues.is_empty())
    }
}

struct Prepared {
    preview: RewindPreview,
    runtime: ToolRuntime,
    changeset: WorkerChangeset,
}

impl Engine {
    pub async fn rewind_conversation(
        &self,
        session: SessionId,
        workspace: PathBuf,
        before_run: RunId,
    ) -> Result<ConversationRewind, EngineError> {
        self.rewind_conversation_checked(session, workspace, before_run, None)
            .await
    }

    pub async fn rewind_conversation_at_head(
        &self,
        session: SessionId,
        workspace: PathBuf,
        before_run: RunId,
        head: (u64, [u8; 32]),
    ) -> Result<ConversationRewind, EngineError> {
        self.rewind_conversation_checked(session, workspace, before_run, Some((head.0, head.1)))
            .await
    }

    async fn rewind_conversation_checked(
        &self,
        session: SessionId,
        workspace: PathBuf,
        before_run: RunId,
        head: Option<(u64, [u8; 32])>,
    ) -> Result<ConversationRewind, EngineError> {
        let _slot = self.claim()?;
        let mut lock = self.lock_session(session)?;
        let view = self
            .journal
            .store()
            .load_view(session)
            .await?
            .ok_or(EngineError::MissingSession)?;
        lock.keep();
        let _ancestors = self.lock_lineage(&view).await?;
        if head.is_some_and(|(sequence, digest)| {
            sequence != view.last_sequence || digest != view.rewinds.prefix_digest()
        }) {
            return Err(EngineError::RewindPreviewChanged);
        }
        let identity = WorkspaceInputs::admit_identity(&workspace, &self.state)?;
        ensure_session_workspace(&view, identity)?;
        let prepared = self.prepare_conversation(&view, before_run).await?;
        self.finish_conversation(prepared).await
    }

    async fn prepare_conversation(
        &self,
        view: &SessionView,
        before_run: RunId,
    ) -> Result<PreparedConversation, EngineError> {
        if view.provider_setup_blocks_continuation() {
            return Err(EngineError::ProviderSetupBlocked);
        }
        if view.runs.last().is_some_and(|run| {
            matches!(
                run.status,
                crate::RunStatus::Active | crate::RunStatus::Pending
            )
        }) {
            return Err(EngineError::SessionNotIdle);
        }
        let index = view
            .runs
            .iter()
            .position(|run| run.id == before_run)
            .filter(|_| {
                view.rewinds.checkpoints.iter().any(|target| {
                    matches!(target, CheckpointTarget::Run { run_id, .. } if *run_id == before_run)
                })
            })
            .ok_or(EngineError::ForkNotAtRunBoundary)?;
        let boundary = if index == 0 {
            None
        } else {
            let sequence = view.runs[index - 1]
                .finished_sequence
                .ok_or(EngineError::ForkNotAtRunBoundary)?;
            Some(
                self.journal
                    .store()
                    .prepare_fork(view.id, Some(sequence))
                    .await
                    .map_err(fork_error)?,
            )
        };
        let selected = &view.runs[index];
        Ok(PreparedConversation {
            boundary,
            conversation: ConversationRewind {
                session_id: None,
                defaults: view.defaults.clone(),
                prompt: Some(selected.objective.clone()),
                images: selected.images.clone(),
            },
        })
    }

    async fn finish_conversation(
        &self,
        mut prepared: PreparedConversation,
    ) -> Result<ConversationRewind, EngineError> {
        if let Some(boundary) = prepared.boundary {
            prepared.conversation.session_id = Some(
                self.journal
                    .store()
                    .fork_at(boundary, "Forked Session".into())
                    .await
                    .map_err(fork_error)?,
            );
        }
        Ok(prepared.conversation)
    }

    pub async fn rewind_preview(
        &self,
        request: RewindRequest,
    ) -> Result<RewindPreview, EngineError> {
        let _slot = self.claim()?;
        let mut lock = self.lock_session(request.session_id)?;
        let view = self
            .journal
            .store()
            .load_view(request.session_id)
            .await?
            .ok_or(EngineError::MissingSession)?;
        lock.keep();
        let _ancestors = self.lock_lineage(&view).await?;
        let identity = WorkspaceInputs::admit_identity(&request.workspace, &self.state)?;
        ensure_session_workspace(&view, identity)?;
        let _writer = self
            .state
            .lock_workspace(identity)
            .map_err(workspace_error)?;
        Ok(
            tokio::time::timeout(REWIND_TIMEOUT, self.prepare_rewind(request, &view))
                .await
                .map_err(|_| EngineError::Tool(crate::tools::ToolError::Limit))??
                .preview,
        )
    }

    pub async fn rewind(
        &self,
        preview: RewindPreview,
        cancellation: RunCancellation,
    ) -> Result<RewindFinished, EngineError> {
        Ok(self.rewind_inner(preview, cancellation, false).await?.code)
    }

    pub async fn rewind_both(
        &self,
        preview: RewindPreview,
        cancellation: RunCancellation,
    ) -> Result<RewindBothResult, EngineError> {
        self.rewind_inner(preview, cancellation, true).await
    }

    async fn rewind_inner(
        &self,
        preview: RewindPreview,
        cancellation: RunCancellation,
        both: bool,
    ) -> Result<RewindBothResult, EngineError> {
        let _slot = self.claim()?;
        if !preview.can_restore() {
            return Err(EngineError::RewindBlocked);
        }
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        let session = preview.request.session_id;
        let mut lock = self.lock_session(session)?;
        let view = self
            .journal
            .store()
            .load_view(session)
            .await?
            .ok_or(EngineError::MissingSession)?;
        lock.keep();
        let _ancestors = self.lock_lineage(&view).await?;
        let identity = WorkspaceInputs::admit_identity(&preview.request.workspace, &self.state)?;
        ensure_session_workspace(&view, identity)?;
        let _writer = self
            .state
            .lock_workspace(identity)
            .map_err(workspace_error)?;
        let conversation = if both {
            let CheckpointTarget::Run { run_id, .. } = preview.request.target else {
                return Err(EngineError::ForkNotAtRunBoundary);
            };
            let prepared = self.prepare_conversation(&view, run_id).await?;
            self.journal
                .admit(session, 2 + usize::from(prepared.boundary.is_some()), 0)
                .await?;
            Some(prepared)
        } else {
            None
        };
        let expected = preview.started;
        let permissions_digest = preview.permissions_digest;
        let prepared =
            tokio::time::timeout(REWIND_TIMEOUT, self.prepare_rewind(preview.request, &view))
                .await
                .map_err(|_| EngineError::Tool(crate::tools::ToolError::Limit))??;
        let restorable = prepared.preview.can_restore();
        let mut started = prepared.preview.started;
        if !restorable
            || !same_basis(&started, &expected)
            || prepared.preview.permissions_digest != permissions_digest
        {
            return Err(EngineError::RewindPreviewChanged);
        }
        started.rewind_id = expected.rewind_id;
        let code = self
            .publish_rewind(
                session,
                started,
                prepared.runtime,
                prepared.changeset,
                cancellation.clone(),
            )
            .await?;
        let conversation = match conversation {
            Some(prepared)
                if code.outcome == RewindOutcome::Restored && !cancellation.is_cancelled() =>
            {
                match self.finish_conversation(prepared).await {
                    Ok(conversation) => ConversationRewindOutcome::Restored(Box::new(conversation)),
                    Err(error) => ConversationRewindOutcome::Failed(error),
                }
            }
            _ => ConversationRewindOutcome::Unchanged,
        };
        Ok(RewindBothResult { code, conversation })
    }

    async fn publish_rewind(
        &self,
        session: SessionId,
        started: RewindStarted,
        runtime: ToolRuntime,
        changeset: WorkerChangeset,
        cancellation: RunCancellation,
    ) -> Result<RewindFinished, EngineError> {
        let bytes = serde_json::to_vec(&changeset).map_err(|_| EngineError::InvalidRequest)?;
        if crate::session::worker::decode_rewind_data(&bytes).is_err() {
            return Err(EngineError::RewindBlocked);
        }
        let files = checkpoint_files(&changeset)?;
        let intent = rewind_intent(&runtime, &started, &bytes)?;
        if !started.valid() {
            return Err(EngineError::RewindTooLarge);
        }
        self.journal.admit(session, 2, 0).await?;
        if cancellation.is_cancelled() {
            return Err(EngineError::CancelledBeforeStart);
        }
        self.journal
            .append(
                session,
                Event::WorkspaceRewindStarted {
                    record: Box::new(started.clone()),
                },
            )
            .await?;
        let checkpoint = self
            .rewind_checkpoint(session, started.rewind_id, files, &cancellation)
            .await;
        crate::fault::rewind_checkpoint(session, REWIND_TIMEOUT).await;
        let mut finished = RewindFinished {
            rewind_id: started.rewind_id,
            outcome: RewindOutcome::Failed,
            attestation: None,
            conflicts: Vec::new(),
            conflict_count: 0,
            checkpoint: None,
        };
        if cancellation.is_cancelled() {
            finished.outcome = RewindOutcome::Cancelled;
        } else if !started.overwrite.is_empty() && checkpoint.status == CheckpointStatus::Skipped {
            finished.outcome = RewindOutcome::Failed;
        } else {
            let observation = runtime
                .clone()
                .with_handover(bytes.into())
                .execute(intent.clone(), cancellation)
                .await;
            finished.outcome = restore_outcome(observation.disposition);
            if matches!(
                finished.outcome,
                RewindOutcome::Restored | RewindOutcome::Uncertain
            ) {
                finished.attestation = rewind_attestation(&started, &observation);
                if finished.attestation.is_some() {
                    finished.checkpoint = Some(checkpoint);
                } else {
                    finished.outcome = RewindOutcome::Uncertain;
                }
            }
            if finished.outcome == RewindOutcome::Conflict {
                let drifted = Self::drifted_paths(&runtime, &changeset).await;
                record_conflicts(&mut finished, drifted)?;
            }
        }
        self.journal
            .append(
                session,
                Event::WorkspaceRewindFinished {
                    record: Box::new(finished.clone()),
                },
            )
            .await?;
        Ok(finished)
    }

    async fn rewind_checkpoint(
        &self,
        session: SessionId,
        rewind_id: uuid::Uuid,
        files: Vec<(String, Vec<u8>)>,
        cancellation: &RunCancellation,
    ) -> Checkpoint {
        if files.is_empty() {
            Checkpoint::not_needed()
        } else if cancellation.is_cancelled() {
            Checkpoint::skipped(CheckpointReason::StoreUnavailable)
        } else {
            let capture = crate::store::CheckpointCapture {
                session,
                owner: crate::store::CheckpointOwner::Rewind { rewind_id },
                files,
            };
            tokio::time::timeout(
                REWIND_CHECKPOINT_TIMEOUT,
                self.journal.store().capture_checkpoint(capture),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_else(|| Checkpoint::skipped(CheckpointReason::StoreUnavailable))
        }
    }

    async fn drifted_paths(runtime: &ToolRuntime, changeset: &WorkerChangeset) -> Vec<String> {
        let mut drifted = Vec::new();
        for change in &changeset.files {
            let (runtime, path) = (runtime.clone(), change.path.clone());
            let live = tokio::task::spawn_blocking(move || runtime.rewind_file(&path)).await;
            if !matches!(&live, Ok(Ok(version)) if *version == change.before) {
                drifted.push(change.path.clone());
            }
        }
        drifted
    }

    fn rewind_runtime(&self, request: &RewindRequest) -> Result<ToolRuntime, EngineError> {
        let config = match &request.tools {
            ToolGrant::None => return Err(EngineError::RewindBlocked),
            ToolGrant::Configured => crate::tools::config::Config::load(&self.state)?,
            ToolGrant::Trusted(permissions) => {
                permissions.verify(&request.workspace)?;
                crate::tools::WorkspacePermissions::open(&request.workspace)
                    .map_err(|_| crate::tools::ToolError::Configuration)?
                    .config(&request.workspace)?
            }
        };
        Ok(ToolRuntime::admit(
            config,
            crate::tools::load_permissions(&self.state)?,
            ToolRuntime::load_exact_command_rules(&self.state)?,
            &request.workspace,
            &self.state,
            self.config
                .guard_executable
                .clone()
                .ok_or(EngineError::GuardUnavailable)?,
        )?)
    }

    async fn prepare_rewind(
        &self,
        request: RewindRequest,
        view: &SessionView,
    ) -> Result<Prepared, EngineError> {
        if view.provider_setup_blocks_continuation() {
            return Err(EngineError::ProviderSetupBlocked);
        }
        if !view.rewinds.checkpoints.contains(&request.target) {
            return Err(EngineError::InvalidRequest);
        }
        let runtime = self.rewind_runtime(&request)?;
        ensure_session_workspace(view, runtime.workspace_identity())?;
        let plan = rewind::plan(view, &request.target).map_err(|_| EngineError::RewindTooLarge)?;
        if request.kept.len() + request.overwrite.len() > rewind::MAX_REWIND_SELECTIONS {
            return Err(EngineError::RewindTooLarge);
        }
        if request.overwrite.windows(2).any(|p| p[0] >= p[1]) {
            return Err(EngineError::InvalidRequest);
        }
        let mut scan = PreviewScan::default();
        for file in &plan.files {
            scan.file(self, &runtime, &request, file).await?;
        }
        let PreviewScan {
            mut files,
            mut changes,
            comparisons,
            checkpoint_storage_unavailable,
            overwrite,
            ..
        } = scan;
        if overwrite.len() != request.overwrite.len() {
            return Err(EngineError::InvalidRequest);
        }
        check_batch(&runtime, &mut files, &mut changes).await?;
        let selected = plan
            .select(&request.kept, &overwrite)
            .map_err(|_| EngineError::RewindTooLarge)?;
        let pin = runtime.rewind_pin();
        let started = RewindStarted {
            rewind_id: uuid::Uuid::now_v7(),
            target: request.target.clone(),
            head_sequence: view.last_sequence,
            prefix_digest: view.rewinds.prefix.digest(),
            plan_digest: selected.digest(),
            file_count: u32::try_from(selected.files.len())
                .map_err(|_| EngineError::RewindTooLarge)?,
            restored_bytes: selected.restored_bytes(),
            kept: request.kept.clone(),
            overwrite,
            workspace_device: pin.workspace_device,
            workspace_inode: pin.workspace_inode,
            policy_digest: pin.policy_digest,
            enforcement_digest: pin.enforcement_digest,
        };
        let permissions_digest = runtime.receipt().permissions_digest;
        Ok(Prepared {
            preview: RewindPreview {
                request,
                files,
                plan: selected,
                started,
                permissions_digest,
                comparisons,
                checkpoint_storage_unavailable,
            },
            runtime,
            changeset: WorkerChangeset { files: changes },
        })
    }
}

async fn live_version(
    runtime: &ToolRuntime,
    path: &str,
    issues: &mut Vec<RewindIssue>,
) -> Result<Option<FileVersion>, EngineError> {
    use crate::tools::ToolError;
    if !runtime.rewind_path_allowed(path) {
        issues.push(RewindIssue::OutsideGrant);
        return Ok(None);
    }
    if !crate::session::worker::writable_path(path) {
        issues.push(RewindIssue::Unsafe);
        return Ok(None);
    }
    let (runtime, path) = (runtime.clone(), path.to_owned());
    match tokio::task::spawn_blocking(move || runtime.rewind_file(&path))
        .await
        .map_err(|_| ToolError::Operation)
        .and_then(|read| read)
    {
        Ok(version) => Ok(version),
        Err(ToolError::QueueFull) => Err(EngineError::Tool(ToolError::QueueFull)),
        Err(_) => {
            issues.push(RewindIssue::Unsafe);
            Ok(None)
        }
    }
}

async fn check_batch(
    runtime: &ToolRuntime,
    files: &mut [RewindFile],
    changes: &mut Vec<FileChange>,
) -> Result<(), EngineError> {
    use crate::tools::ToolError;
    let runtime = runtime.clone();
    let changeset = WorkerChangeset {
        files: changes.clone(),
    };
    match tokio::task::spawn_blocking(move || {
        let bytes = serde_json::to_vec(&changeset).map_err(|_| ToolError::Operation)?;
        if bytes.len() > crate::session::worker::WORKER_DATA_BYTES {
            return Err(ToolError::Limit);
        }
        crate::session::worker::decode_rewind_data(&bytes).map_err(|_| ToolError::Path)?;
        runtime.rewind_batch(&changeset.files)
    })
    .await
    .map_err(|_| ToolError::Operation)
    .and_then(|checked| checked)
    {
        Ok(()) => {}
        Err(ToolError::Limit) => return Err(EngineError::RewindTooLarge),
        Err(ToolError::QueueFull) => return Err(EngineError::Tool(ToolError::QueueFull)),
        Err(_) => {
            for file in files.iter_mut().filter(|f| !f.kept && f.issues.is_empty()) {
                file.issues.push(RewindIssue::Unsafe);
            }
            changes.clear();
        }
    }
    Ok(())
}

fn file_state(version: &FileVersion) -> Result<FileState, EngineError> {
    Ok(FileState {
        sha256: version.sha256.clone(),
        bytes: u32::try_from(
            version
                .bytes_with_limit(crate::tools::types::MAX_FILE_BYTES)
                .ok_or(EngineError::InvalidHistory)?
                .len(),
        )
        .map_err(|_| EngineError::InvalidHistory)?,
        executable: version.executable,
    })
}
fn parse_digest(value: &str) -> Result<[u8; 32], EngineError> {
    let mut bytes = [0; 32];
    if value.len() != 64 {
        return Err(EngineError::InvalidHistory);
    }
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| EngineError::InvalidHistory)?;
    }
    Ok(bytes)
}
fn workspace_error(error: crate::store::StoreError) -> EngineError {
    match error {
        crate::store::StoreError::WorkspaceBusy => EngineError::WorkspaceBusy,
        other => other.into(),
    }
}
fn rewind_attestation(
    started: &RewindStarted,
    observation: &ToolObservation,
) -> Option<RewindAttestation> {
    let intent = &observation.intent;
    if !observation.valid()
        || intent.id != started.rewind_id
        || intent.policy_digest != started.policy_digest
        || intent.enforcement_digest != started.enforcement_digest
        || (intent.workspace_device, intent.workspace_inode)
            != (started.workspace_device, started.workspace_inode)
        || !matches!(&intent.call, ToolCall::ApplyChangeset {
            source: ChangesetSource::Rewind { plan_digest, .. }
        } if *plan_digest == started.plan_digest)
    {
        return None;
    }
    let attestation = observation.attestation.as_ref()?;
    let receipt = match attestation {
        Attestation::Guard(receipt) => serde_json::to_vec(receipt),
        Attestation::InProcess { .. } => serde_json::to_vec(&(intent.digest(), attestation)),
        Attestation::Host(_) | Attestation::Unguarded(_) | Attestation::Git(_) => return None,
    }
    .ok()?;
    Some(RewindAttestation {
        plan_digest: started.plan_digest,
        workspace_device: started.workspace_device,
        workspace_inode: started.workspace_inode,
        enforcement_digest: started.enforcement_digest,
        receipt_digest: crate::validate::digest(&receipt),
    })
}

fn same_basis(started: &RewindStarted, expected: &RewindStarted) -> bool {
    started.head_sequence == expected.head_sequence
        && started.prefix_digest == expected.prefix_digest
        && started.plan_digest == expected.plan_digest
        && started.policy_digest == expected.policy_digest
        && started.enforcement_digest == expected.enforcement_digest
}

fn checkpoint_files(changeset: &WorkerChangeset) -> Result<Vec<(String, Vec<u8>)>, EngineError> {
    changeset
        .files
        .iter()
        .filter_map(|f| f.before.as_ref().map(|v| (f.path.clone(), v)))
        .map(|(p, v)| {
            v.bytes_with_limit(crate::tools::types::MAX_FILE_BYTES)
                .map(|bytes| (p, bytes))
                .ok_or(EngineError::InvalidHistory)
        })
        .collect()
}

fn rewind_intent(
    runtime: &ToolRuntime,
    started: &RewindStarted,
    bytes: &[u8],
) -> Result<crate::tools::EffectIntent, EngineError> {
    let mut intent = runtime.intent(
        RunId::new(),
        AgentRunId::new(),
        ToolCall::ApplyChangeset {
            source: ChangesetSource::Rewind {
                plan_digest: started.plan_digest,
                bytes: u32::try_from(bytes.len()).map_err(|_| EngineError::RewindTooLarge)?,
                sha256: crate::validate::digest(bytes),
            },
        },
    )?;
    intent.id = started.rewind_id;
    Ok(intent)
}

fn restore_outcome(disposition: ToolDisposition) -> RewindOutcome {
    match disposition {
        ToolDisposition::Succeeded => RewindOutcome::Restored,
        ToolDisposition::Conflict => RewindOutcome::Conflict,
        ToolDisposition::Denied => RewindOutcome::Denied,
        ToolDisposition::Cancelled => RewindOutcome::Cancelled,
        ToolDisposition::Uncertain => RewindOutcome::Uncertain,
        _ => RewindOutcome::Failed,
    }
}

#[derive(Default)]
struct PreviewScan {
    files: Vec<RewindFile>,
    changes: Vec<FileChange>,
    comparisons: Vec<FileChange>,
    comparison_bytes: u64,
    checkpoint_storage_unavailable: bool,
    overwrite: Vec<Overwrite>,
    current_bytes: u64,
}

impl PreviewScan {
    async fn file(
        &mut self,
        engine: &Engine,
        runtime: &ToolRuntime,
        request: &RewindRequest,
        file: &rewind::FilePlan,
    ) -> Result<(), EngineError> {
        let mut issues = Vec::new();
        let kept = request.kept.contains(&file.path);
        let before = live_version(runtime, &file.path, &mut issues).await?;
        let observed = before.as_ref().map(file_state).transpose()?;
        if !kept {
            self.current_bytes += observed.as_ref().map_or(0, |v| u64::from(v.bytes));
            if self.current_bytes > rewind::MAX_REWIND_BYTES {
                return Err(EngineError::RewindTooLarge);
            }
        }
        if issues.is_empty() && observed != file.expected_current {
            issues.push(RewindIssue::Drifted);
        }
        if file.history_drift {
            issues.push(RewindIssue::HistoryDrift);
        }
        let after = self.restore_version(engine, file, &mut issues).await?;
        let compared_bytes = observed.as_ref().map_or(0, |v| u64::from(v.bytes))
            + file.restore_to.as_ref().map_or(0, |v| u64::from(v.bytes));
        if self.comparison_bytes + compared_bytes <= 2 * rewind::MAX_REWIND_BYTES
            && !issues.iter().any(|issue| {
                matches!(
                    issue,
                    RewindIssue::OutsideGrant | RewindIssue::Unsafe | RewindIssue::Unavailable
                )
            })
        {
            self.comparison_bytes += compared_bytes;
            self.comparisons.push(FileChange {
                path: file.path.clone(),
                before: before.clone(),
                after: after.clone(),
            });
        }
        if request.overwrite.contains(&file.path) {
            if issues.is_empty()
                || issues
                    .iter()
                    .any(|i| !matches!(i, RewindIssue::Drifted | RewindIssue::HistoryDrift))
            {
                return Err(EngineError::InvalidRequest);
            }
            self.overwrite.push(Overwrite {
                path: file.path.clone(),
                observed: observed.clone(),
            });
            issues.clear();
        }
        if !kept && issues.is_empty() {
            self.changes.push(FileChange {
                path: file.path.clone(),
                before,
                after,
            });
        }
        self.files.push(RewindFile {
            path: file.path.clone(),
            observed,
            issues,
            kept,
        });
        Ok(())
    }

    async fn restore_version(
        &mut self,
        engine: &Engine,
        file: &rewind::FilePlan,
        issues: &mut Vec<RewindIssue>,
    ) -> Result<Option<FileVersion>, EngineError> {
        Ok(if let Some(state) = &file.restore_to {
            if file.unavailable {
                issues.push(RewindIssue::Unavailable);
                None
            } else {
                match engine
                    .journal
                    .store()
                    .checkpoint_blob(parse_digest(&state.sha256)?)
                    .await
                {
                    Ok(Some(bytes)) if bytes.len() == state.bytes as usize => {
                        Some(FileVersion::from_bytes(&bytes, state.executable))
                    }
                    result => {
                        self.checkpoint_storage_unavailable |= result.is_err();
                        issues.push(RewindIssue::Unavailable);
                        None
                    }
                }
            }
        } else {
            None
        })
    }
}

fn record_conflicts(
    finished: &mut RewindFinished,
    drifted: Vec<String>,
) -> Result<(), EngineError> {
    finished.conflict_count =
        u32::try_from(drifted.len()).map_err(|_| EngineError::RewindTooLarge)?;
    finished.conflicts = drifted
        .into_iter()
        .take(rewind::MAX_REWIND_CONFLICTS)
        .collect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn rewind_selection_confirmation_pins_observations_authority_and_head() {
        let session = SessionId::new();
        let target = CheckpointTarget::Run {
            run_id: RunId::new(),
            sequence: 2,
        };
        let preview = || RewindPreview {
            request: RewindRequest {
                session_id: session,
                workspace: "/fixture".into(),
                target: target.clone(),
                tools: ToolGrant::None,
                kept: Vec::new(),
                overwrite: Vec::new(),
            },
            files: vec![RewindFile {
                path: "src/a".into(),
                observed: None,
                issues: vec![RewindIssue::Drifted],
                kept: false,
            }],
            plan: RewindPlan {
                files: Vec::new(),
                blockers: Vec::new(),
                non_rewindable: Vec::new(),
            },
            started: RewindStarted {
                rewind_id: uuid::Uuid::now_v7(),
                target: target.clone(),
                head_sequence: 3,
                prefix_digest: [1; 32],
                plan_digest: [2; 32],
                file_count: 1,
                restored_bytes: 0,
                kept: Vec::new(),
                overwrite: Vec::new(),
                workspace_device: 1,
                workspace_inode: 2,
                policy_digest: [3; 32],
                enforcement_digest: [4; 32],
            },
            permissions_digest: None,
            comparisons: Vec::new(),
            checkpoint_storage_unavailable: false,
        };
        type Mutation = fn(&mut RewindPreview);
        for (change, expected) in [
            (
                (|p: &mut RewindPreview| {
                    p.files[0].kept = true;
                    p.started.plan_digest = [9; 32];
                }) as Mutation,
                true,
            ),
            (|p| p.request.session_id = SessionId::new(), false),
            (
                |p| {
                    p.request.target = CheckpointTarget::Run {
                        run_id: RunId::new(),
                        sequence: 2,
                    }
                },
                false,
            ),
            (|p| p.started.head_sequence += 1, false),
            (|p| p.started.prefix_digest = [9; 32], false),
            (|p| p.started.policy_digest = [9; 32], false),
            (|p| p.started.enforcement_digest = [9; 32], false),
            (|p| p.permissions_digest = Some([9; 32]), false),
            (
                |p| {
                    p.files[0].observed = Some(FileState {
                        sha256: "a".repeat(64),
                        bytes: 1,
                        executable: false,
                    })
                },
                false,
            ),
            (|p| p.files[0].path = "src/b".into(), false),
        ] {
            let original = preview();
            let mut selected = preview();
            change(&mut selected);
            assert_eq!(selected.observations_match(&original), expected);
        }
        assert!(preview().observations_match(&preview()));
    }

    fn write_grant(state: &std::path::Path, write: bool) {
        let path = state.join("tools.json");
        std::fs::write(&path, serde_json::to_vec(&serde_json::json!({"workspace_paths":["src"],"write":write,"commands":[],"skills":[],"mcp":[]})).unwrap()).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn publications(view: &mut SessionView, count: usize, bytes: u32) {
        let paths: Vec<_> = (0..count).map(|index| format!("src/f{index:04}")).collect();
        publications_at(view, &paths, bytes);
    }

    fn result_path(call: &ToolCall) -> String {
        let ToolCall::Write { path, .. } = call else {
            unreachable!()
        };
        path.clone()
    }
    fn publications_at(view: &mut SessionView, paths: &[String], bytes: u32) {
        let run = &mut view.runs[0];
        let mut template = run.agents[1].tools[0].clone();
        template.intent.target = crate::session::EffectTarget::Workspace;
        template.checkpoint = Some(crate::session::Checkpoint {
            status: crate::session::CheckpointStatus::Captured,
            reason: None,
            pruned_before_run: None,
        });
        run.agents[1].tools.clear();
        for (i, path) in paths.iter().enumerate() {
            let mut tool = template.clone();
            let before = FileState {
                sha256: crate::validate::hex_digest(b"old"),
                bytes,
                executable: false,
            };
            tool.intent.call = ToolCall::Write {
                path: path.clone(),
                content: "new".into(),
                expected_digest: Some(before.sha256.clone()),
            };
            tool.finished_sequence = Some(template.finished_sequence.unwrap() + i as u64);
            let o = tool.observation.as_mut().unwrap();
            o.intent = tool.intent.clone();
            o.output = serde_json::to_string(&crate::session::rewind::FilePublicationResult {
                sha256: crate::validate::hex_digest(b"new"),
                bytes: 3,
                executable: false,
                before: Some(before),
            })
            .unwrap();
            run.agents[1].tools.push(tool);
        }
    }

    #[tokio::test]
    async fn rewind_engine_checks_live_drift_grant_blob_availability_and_bounds() {
        crate::test_support::isolate_account_root();
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/a"), "new").unwrap();
        let state = temp.path().join("state");
        let engine = Engine::open(
            crate::StateRoot::admit(&state).unwrap(),
            super::super::EngineConfig {
                guard_executable: Some(std::env::current_exe().unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        write_grant(&state, true);
        let f = crate::session::worker_protocol_fixture();
        let mut view = SessionView::replay(f.session, &f.envelopes())
            .unwrap()
            .unwrap();
        view.workspace_identity =
            Some(WorkspaceInputs::admit_identity(&workspace, &engine.state).unwrap());
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap();
        view.id = session;
        let target = view.rewinds.checkpoints[0].clone();
        let request = |kept, overwrite| RewindRequest {
            session_id: session,
            workspace: workspace.clone(),
            target: target.clone(),
            tools: ToolGrant::Configured,
            kept,
            overwrite,
        };
        assert!(
            engine
                .prepare_rewind(request(Vec::new(), Vec::new()), &view)
                .await
                .unwrap()
                .preview
                .can_restore()
        );
        std::fs::write(workspace.join("src/a"), "external").unwrap();
        let refused = engine
            .prepare_rewind(request(Vec::new(), Vec::new()), &view)
            .await
            .unwrap()
            .preview;
        assert_eq!(refused.files()[0].issues, [RewindIssue::Drifted]);
        assert!(!refused.can_restore());
        let overwrite = engine
            .prepare_rewind(request(Vec::new(), vec!["src/a".into()]), &view)
            .await
            .unwrap()
            .preview;
        assert!(overwrite.can_restore());
        assert!(overwrite.observations_match(&refused));
        std::fs::write(workspace.join("src/a"), "external again").unwrap();
        let changed = engine
            .prepare_rewind(request(Vec::new(), vec!["src/a".into()]), &view)
            .await
            .unwrap()
            .preview;
        assert!(!changed.observations_match(&refused));
        std::fs::write(workspace.join("src/a"), "external").unwrap();
        assert_eq!(
            overwrite.plan.files[0]
                .expected_current
                .as_ref()
                .unwrap()
                .sha256,
            crate::validate::hex_digest(b"external")
        );
        let kept = engine
            .prepare_rewind(request(vec!["src/a".into()], Vec::new()), &view)
            .await
            .unwrap()
            .preview;
        assert!(kept.plan.files.is_empty());
        write_grant(&state, false);
        let denied = engine
            .prepare_rewind(request(Vec::new(), Vec::new()), &view)
            .await
            .unwrap()
            .preview;
        assert_eq!(denied.files()[0].issues, [RewindIssue::OutsideGrant]);
        write_grant(&state, true);
        assert!(matches!(
            engine
                .prepare_rewind(request(vec!["src/a".into(); 65], Vec::new()), &view)
                .await,
            Err(EngineError::RewindTooLarge)
        ));
        for (count, bytes) in [(2049, 3), (33, 1024 * 1024)] {
            let mut oversized = view.clone();
            publications(&mut oversized, count, bytes);
            assert!(matches!(
                engine
                    .prepare_rewind(request(Vec::new(), Vec::new()), &oversized)
                    .await,
                Err(EngineError::RewindTooLarge)
            ));
        }
        publications(&mut view, 1, 3);
        std::fs::write(workspace.join("src/f0000"), "new").unwrap();
        let missing = engine
            .prepare_rewind(request(Vec::new(), Vec::new()), &view)
            .await
            .unwrap()
            .preview;
        assert_eq!(missing.files()[0].issues, [RewindIssue::Unavailable]);
        for path in [
            "src/caf\u{e9}".to_owned(),
            format!("src/{}", ["d"; 16].join("/")),
        ] {
            publications_at(&mut view, std::slice::from_ref(&path), 3);
            let refused = engine
                .prepare_rewind(request(Vec::new(), Vec::new()), &view)
                .await
                .unwrap()
                .preview;
            assert!(refused.files()[0].issues.contains(&RewindIssue::Unsafe));
            assert!(!refused.can_restore(), "{path}");
        }
        publications(&mut view, 1, 3);
        std::fs::write(workspace.join("src/f0000"), vec![0; 1024 * 1024 + 1]).unwrap();
        let oversized = engine
            .prepare_rewind(request(Vec::new(), Vec::new()), &view)
            .await
            .unwrap()
            .preview;
        assert_eq!(oversized.files()[0].path, "src/f0000");
        assert!(oversized.files()[0].issues.contains(&RewindIssue::Unsafe));
        assert!(!oversized.can_restore());
        let kept = engine
            .prepare_rewind(request(vec!["src/f0000".into()], Vec::new()), &view)
            .await
            .unwrap()
            .preview;
        assert!(kept.files()[0].kept);
        assert!(kept.plan.files.is_empty());
        let before_events = engine.journal.store().load_session(session).await.unwrap();
        let checkpoints = rusqlite::Connection::open(state.join("checkpoints.sqlite")).unwrap();
        checkpoints
            .execute(
                "INSERT INTO blobs(digest,bytes,len) VALUES(?1,?2,3)",
                rusqlite::params![
                    parse_digest(&crate::validate::hex_digest(b"old"))
                        .unwrap()
                        .as_slice(),
                    b"old".as_slice()
                ],
            )
            .unwrap();
        drop(checkpoints);
        for (paths, restore) in [
            (["src/missing", "src/missing/child"], false),
            (["src/Missing", "src/missing"], false),
            (["src/Missing", "src/missing"], true),
        ] {
            let paths = paths.map(str::to_owned);
            publications_at(&mut view, &paths, 3);
            for tool in &mut view.runs[0].agents[1].tools {
                let observation = tool.observation.as_mut().unwrap();
                let mut result: crate::session::rewind::FilePublicationResult =
                    serde_json::from_str(&observation.output).unwrap();
                if restore {
                    tool.intent.call = ToolCall::Delete {
                        path: result_path(&observation.intent.call),
                        expected_digest: Some(result.before.as_ref().unwrap().sha256.clone()),
                    };
                    observation.output =
                        serde_json::to_string(&crate::session::rewind::RemovedFileResult {
                            removed: result.before.unwrap(),
                        })
                        .unwrap();
                } else {
                    if let ToolCall::Write {
                        expected_digest, ..
                    } = &mut tool.intent.call
                    {
                        *expected_digest = None;
                    }
                    result.before = None;
                    observation.output = serde_json::to_string(&result).unwrap();
                }
                observation.intent = tool.intent.clone();
            }
            let refused = engine
                .prepare_rewind(
                    request(
                        Vec::new(),
                        if restore { Vec::new() } else { paths.to_vec() },
                    ),
                    &view,
                )
                .await
                .unwrap()
                .preview;
            assert_eq!(refused.files().len(), 2);
            assert!(
                refused
                    .files()
                    .iter()
                    .all(|f| f.issues == [RewindIssue::Unsafe])
            );
            assert!(!refused.can_restore());
            assert!(matches!(
                engine.rewind(refused, RunCancellation::new()).await,
                Err(EngineError::RewindBlocked)
            ));
            assert_eq!(
                engine.journal.store().load_session(session).await.unwrap(),
                before_events
            );
        }
        engine.close().await.unwrap();
    }

    #[tokio::test]
    async fn rewind_engine_capacity_refusal_precedes_started_and_dispatch() {
        crate::test_support::isolate_account_root();
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        let state = temp.path().join("state");
        let engine = Engine::open(
            crate::StateRoot::admit(&state).unwrap(),
            super::super::EngineConfig {
                guard_executable: Some(std::env::current_exe().unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        write_grant(&state, true);
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap();
        for _ in 1..9999 {
            engine
                .journal
                .append(
                    session,
                    Event::SessionRenamed {
                        title: "Capacity".into(),
                    },
                )
                .await
                .unwrap();
        }
        let target = CheckpointTarget::Run {
            run_id: RunId::new(),
            sequence: 1,
        };
        let runtime = engine
            .rewind_runtime(&RewindRequest {
                session_id: session,
                workspace: workspace.clone(),
                target: target.clone(),
                tools: ToolGrant::Configured,
                kept: Vec::new(),
                overwrite: Vec::new(),
            })
            .unwrap();
        let pin = runtime.rewind_pin();
        let started = RewindStarted {
            rewind_id: uuid::Uuid::now_v7(),
            target,
            head_sequence: 9999,
            prefix_digest: [1; 32],
            plan_digest: [2; 32],
            file_count: 1,
            restored_bytes: 0,
            kept: Vec::new(),
            overwrite: Vec::new(),
            workspace_device: pin.workspace_device,
            workspace_inode: pin.workspace_inode,
            policy_digest: pin.policy_digest,
            enforcement_digest: pin.enforcement_digest,
        };
        let changeset = WorkerChangeset {
            files: vec![FileChange {
                path: "src/a".into(),
                before: None,
                after: Some(FileVersion::from_bytes(b"new", false)),
            }],
        };
        let mut overlapping = changeset.clone();
        overlapping.files.push(FileChange {
            path: "src/a/b".into(),
            before: None,
            after: Some(FileVersion::from_bytes(b"new", false)),
        });
        assert!(matches!(
            engine
                .publish_rewind(
                    session,
                    started.clone(),
                    runtime.clone(),
                    overlapping,
                    RunCancellation::new(),
                )
                .await,
            Err(EngineError::RewindBlocked)
        ));
        assert!(matches!(
            engine
                .publish_rewind(session, started, runtime, changeset, RunCancellation::new())
                .await,
            Err(EngineError::Store(crate::StoreError::ReplayLimit))
        ));
        let events = engine.journal.store().load_session(session).await.unwrap();
        assert_eq!(events.len(), 9999);
        assert!(!workspace.join("src/a").exists());
        engine.close().await.unwrap();
    }

    #[tokio::test]
    async fn rewind_engine_busy_and_writer_locks_refuse_without_events() {
        crate::test_support::isolate_account_root();
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let engine = Engine::open(
            crate::StateRoot::admit(&temp.path().join("state")).unwrap(),
            super::super::EngineConfig::default(),
        )
        .unwrap();
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap();
        let request = || RewindRequest {
            session_id: session,
            workspace: workspace.clone(),
            target: CheckpointTarget::Run {
                run_id: RunId::new(),
                sequence: 1,
            },
            tools: ToolGrant::None,
            kept: Vec::new(),
            overwrite: Vec::new(),
        };
        let slot = engine.claim().unwrap();
        assert!(matches!(
            engine.rewind_preview(request()).await,
            Err(EngineError::Busy)
        ));
        drop(slot);
        let identity = WorkspaceInputs::admit_identity(&workspace, &engine.state).unwrap();
        let writer = engine.state.lock_workspace(identity).unwrap();
        assert!(matches!(
            engine.rewind_preview(request()).await,
            Err(EngineError::WorkspaceBusy)
        ));
        drop(writer);
        assert_eq!(
            engine
                .journal
                .store()
                .load_session(session)
                .await
                .unwrap()
                .len(),
            1
        );
        engine.close().await.unwrap();
    }
}

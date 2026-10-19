use super::{Engine, EngineError, RunCancellation, ToolGrant, ensure_session_workspace};
use crate::session::input::WorkspaceInputs;
use crate::session::rewind::{
    self, CheckpointTarget, FileState, Overwrite, RewindAttestation, RewindFinished, RewindOutcome,
    RewindPlan, RewindStarted,
};
use crate::session::{
    AgentRunId, Checkpoint, CheckpointReason, CheckpointStatus, Event, RunId, SessionId,
    SessionView,
};
use crate::tools::{
    Attestation, EffectIntent, FileChange, FileVersion, ToolCall, ToolDisposition, ToolRuntime,
};
use crate::worker::{ChangesetSource, WorkerChangeset};
use std::path::PathBuf;
use std::time::Duration;

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
        Ok(tokio::time::timeout(
            Duration::from_secs(120),
            self.prepare_rewind(request, &view),
        )
        .await
        .map_err(|_| EngineError::Tool(crate::tools::ToolError::Limit))??
        .preview)
    }

    pub async fn rewind(
        &self,
        preview: RewindPreview,
        cancellation: RunCancellation,
    ) -> Result<RewindFinished, EngineError> {
        let _slot = self.claim()?;
        if !preview.can_restore() {
            return Err(EngineError::RewindBlocked);
        }
        if i64::try_from(crate::tools::now_ms()).unwrap_or(i64::MAX)
            >= preview.started.expires_at_ms
        {
            return Err(EngineError::RewindPreviewExpired);
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
        let expected = preview.started;
        let permissions_digest = preview.permissions_digest;
        let prepared = tokio::time::timeout(
            Duration::from_secs(120),
            self.prepare_rewind(preview.request, &view),
        )
        .await
        .map_err(|_| EngineError::Tool(crate::tools::ToolError::Limit))??;
        let restorable = prepared.preview.can_restore();
        let mut started = prepared.preview.started;
        if !restorable
            || started.head_sequence != expected.head_sequence
            || started.prefix_digest != expected.prefix_digest
            || started.plan_digest != expected.plan_digest
            || started.policy_digest != expected.policy_digest
            || started.enforcement_digest != expected.enforcement_digest
            || prepared.preview.permissions_digest != permissions_digest
        {
            return Err(EngineError::RewindPreviewExpired);
        }
        started.rewind_id = expected.rewind_id;
        started.expires_at_ms = expected.expires_at_ms;
        self.publish_rewind(
            session,
            started,
            prepared.runtime,
            prepared.changeset,
            cancellation,
        )
        .await
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
        let files = changeset
            .files
            .iter()
            .filter_map(|f| f.before.as_ref().map(|v| (f.path.clone(), v)))
            .map(|(p, v)| {
                v.bytes_with_limit(1024 * 1024)
                    .map(|bytes| (p, bytes))
                    .ok_or(EngineError::InvalidHistory)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut intent = runtime.intent(
            RunId::new(),
            AgentRunId::new(),
            ToolCall::ApplyChangeset {
                source: ChangesetSource::Rewind {
                    plan_digest: started.plan_digest,
                    bytes: u32::try_from(bytes.len()).map_err(|_| EngineError::RewindTooLarge)?,
                    sha256: crate::tools::types::digest(&bytes),
                },
            },
        );
        intent.id = started.rewind_id;
        intent.expires_at_ms =
            u64::try_from(started.expires_at_ms).map_err(|_| EngineError::InvalidRequest)?;
        if !started.valid() {
            return Err(EngineError::RewindTooLarge);
        }
        if crate::tools::now_ms() >= intent.expires_at_ms {
            return Err(EngineError::RewindPreviewExpired);
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
        let checkpoint = if files.is_empty() {
            Checkpoint::not_needed()
        } else {
            let capture = crate::store::CheckpointCapture {
                session,
                owner: crate::store::CheckpointOwner::Rewind {
                    rewind_id: started.rewind_id,
                },
                files,
            };
            tokio::time::timeout(
                Duration::from_secs(3),
                self.journal.store().capture_checkpoint(capture),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_else(|| Checkpoint::skipped(CheckpointReason::StoreUnavailable))
        };
        #[cfg(any(test, feature = "test-support"))]
        if let Some((entered, release)) = crate::fault::FaultPlan::checkpoint_gate(session) {
            let _ = entered.send(());
            let _ = tokio::time::timeout(Duration::from_secs(120), release).await;
        }
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
                .with_handover(bytes.into())
                .execute(intent.clone(), cancellation)
                .await;
            finished.outcome = match observation.disposition {
                ToolDisposition::Succeeded => RewindOutcome::Restored,
                ToolDisposition::Conflict => RewindOutcome::Conflict,
                ToolDisposition::Denied => RewindOutcome::Denied,
                ToolDisposition::Cancelled => RewindOutcome::Cancelled,
                ToolDisposition::Uncertain => RewindOutcome::Uncertain,
                _ => RewindOutcome::Failed,
            };
            if matches!(
                finished.outcome,
                RewindOutcome::Restored | RewindOutcome::Uncertain
            ) {
                finished.attestation =
                    rewind_attestation(&started, &intent, observation.attestation.as_ref());
                if finished.attestation.is_some() {
                    finished.checkpoint = Some(checkpoint);
                } else {
                    finished.outcome = RewindOutcome::Uncertain;
                }
            }
            if finished.outcome == RewindOutcome::Conflict {
                let drifted = Self::drifted_paths(&runtime, &changeset).await;
                finished.conflict_count =
                    u32::try_from(drifted.len()).map_err(|_| EngineError::RewindTooLarge)?;
                finished.conflicts = drifted
                    .into_iter()
                    .take(rewind::MAX_REWIND_CONFLICTS)
                    .collect();
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
            ToolRuntime::load_permissions(&self.state)?,
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
        let mut files: Vec<RewindFile> = Vec::new();
        let mut changes = Vec::new();
        let mut overwrite = Vec::new();
        let mut current_bytes = 0u64;
        for file in &plan.files {
            let mut issues = Vec::new();
            let kept = request.kept.contains(&file.path);
            let before = live_version(&runtime, &file.path, kept, &mut issues).await?;
            let observed = before.as_ref().map(file_state).transpose()?;
            if !kept {
                current_bytes += observed.as_ref().map_or(0, |v| u64::from(v.bytes));
                if current_bytes > rewind::MAX_REWIND_BYTES {
                    return Err(EngineError::RewindTooLarge);
                }
            }
            if issues.is_empty() && observed != file.expected_current {
                issues.push(RewindIssue::Drifted);
            }
            if file.history_drift {
                issues.push(RewindIssue::HistoryDrift);
            }
            let after = if let Some(state) = &file.restore_to {
                if file.unavailable {
                    issues.push(RewindIssue::Unavailable);
                    None
                } else {
                    match self
                        .journal
                        .store()
                        .checkpoint_blob(parse_digest(&state.sha256)?)
                        .await
                    {
                        Ok(Some(bytes)) if bytes.len() == state.bytes as usize => {
                            Some(FileVersion::from_bytes(&bytes, state.executable))
                        }
                        _ => {
                            issues.push(RewindIssue::Unavailable);
                            None
                        }
                    }
                }
            } else {
                None
            };
            if request.overwrite.contains(&file.path) {
                if issues.is_empty()
                    || issues
                        .iter()
                        .any(|i| !matches!(i, RewindIssue::Drifted | RewindIssue::HistoryDrift))
                {
                    return Err(EngineError::InvalidRequest);
                }
                overwrite.push(Overwrite {
                    path: file.path.clone(),
                    observed: observed.clone(),
                });
                issues.clear();
            }
            if !kept && issues.is_empty() {
                changes.push(FileChange {
                    path: file.path.clone(),
                    before,
                    after,
                });
            }
            files.push(RewindFile {
                path: file.path.clone(),
                observed,
                issues,
                kept,
            });
        }
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
            expires_at_ms: i64::try_from(pin.expires_at_ms)
                .map_err(|_| EngineError::InvalidRequest)?,
        };
        let permissions_digest = runtime.receipt().permissions_digest;
        Ok(Prepared {
            preview: RewindPreview {
                request,
                files,
                plan: selected,
                started,
                permissions_digest,
            },
            runtime,
            changeset: WorkerChangeset { files: changes },
        })
    }
}

async fn live_version(
    runtime: &ToolRuntime,
    path: &str,
    kept: bool,
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
        Err(ToolError::Limit) if !kept => Err(EngineError::RewindTooLarge),
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
    let (runtime, batch) = (runtime.clone(), changes.clone());
    match tokio::task::spawn_blocking(move || runtime.rewind_batch(&batch))
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
                .bytes_with_limit(1024 * 1024)
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
    intent: &EffectIntent,
    attestation: Option<&Attestation>,
) -> Option<RewindAttestation> {
    let Some(Attestation::Guard(receipt)) = attestation else {
        return None;
    };
    if !receipt.valid_for(intent) {
        return None;
    }
    Some(RewindAttestation {
        plan_digest: started.plan_digest,
        workspace_device: started.workspace_device,
        workspace_inode: started.workspace_inode,
        enforcement_digest: started.enforcement_digest,
        receipt_digest: crate::tools::types::digest(&serde_json::to_vec(receipt).ok()?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::worker::{WorkerBaseVersion, WorkerDataFacts};
    use std::os::unix::fs::PermissionsExt;

    fn write_grant(state: &std::path::Path, write: bool) {
        let path = state.join("tools.json");
        std::fs::write(&path, serde_json::to_vec(&serde_json::json!({"version":1,"workspace_paths":["src"],"write":write,"commands":[],"skills":[],"mcp":[]})).unwrap()).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn publications(view: &mut SessionView, count: usize, bytes: u32) {
        let paths: Vec<_> = (0..count).map(|index| format!("src/f{index:04}")).collect();
        publications_at(view, &paths, bytes);
    }

    fn publications_at(view: &mut SessionView, paths: &[String], bytes: u32) {
        let tool = view.runs[0]
            .tools
            .iter()
            .find(|t| matches!(t.intent.call, ToolCall::ApplyChangeset { .. }))
            .unwrap();
        let ToolCall::ApplyChangeset {
            source: ChangesetSource::WorkerHandover { data, .. },
        } = &tool.intent.call
        else {
            unreachable!()
        };
        let data = data.clone();
        let sealed = view.runs[0]
            .workers
            .data
            .iter_mut()
            .find(|s| s.reference == data)
            .unwrap();
        let WorkerDataFacts::Changes(files) = &mut sealed.facts else {
            unreachable!()
        };
        files.clear();
        for path in paths {
            files.push(crate::session::worker::WorkerFileFact {
                path: path.clone(),
                before: WorkerBaseVersion::File {
                    sha256: crate::tools::types::hex_digest(b"old"),
                    bytes,
                    executable: false,
                },
                after: WorkerBaseVersion::File {
                    sha256: crate::tools::types::hex_digest(b"new"),
                    bytes: 3,
                    executable: false,
                },
            });
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
        let session = view.id;
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
        assert_eq!(
            overwrite.plan.files[0]
                .expected_current
                .as_ref()
                .unwrap()
                .sha256,
            crate::tools::types::hex_digest(b"external")
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
            expires_at_ms: i64::try_from(pin.expires_at_ms).unwrap(),
        };
        let changeset = WorkerChangeset {
            files: vec![FileChange {
                path: "src/a".into(),
                before: None,
                after: Some(FileVersion::from_bytes(b"new", false)),
            }],
        };
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

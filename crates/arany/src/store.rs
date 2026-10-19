use crate::provider::evidence::{
    EvidenceOp, EvidenceRecord, NativeEvidenceRecord, ProviderEvidenceRecord,
};
use crate::session::{
    Event, EventEnvelope, ReplayError, RunId, SessionId, SessionListItem, SessionView,
    WorkspaceIdentity, payload_version,
};
use std::io;
use std::sync::mpsc::sync_channel;
use std::thread::JoinHandle;
use tokio::sync::{mpsc, oneshot};

mod evidence;
mod journal;
mod replay;
mod state;
use journal::{HotSession, append, fork_session, open_connection};
use replay::{
    latest_session_id, list_session_items, load_session, resolve_open_view, resolve_view,
    session_for_run,
};
pub use state::HistoryReset;
#[cfg(test)]
pub(crate) use state::capture_archive;
pub(crate) use state::checkpoints::Capture as CheckpointCapture;
pub use state::checkpoints::CheckpointReset;
pub(crate) use state::{
    BUILD_CACHE_ARCHIVE_BYTES, BuildCacheIdentity, BuildCacheLease, SessionRunLock,
    WorkspaceRunLock, archive_size_with_limit,
};
pub use state::{Record, RecordKind, StateRoot};
#[cfg(target_os = "macos")]
pub(crate) use state::{capture_archive_async_with_limit, restore_archive_async_with_limit};
#[cfg(target_os = "linux")]
pub(crate) use state::{capture_archive_with_limit, restore_archive_with_limit};

const DATABASE_FILE: &str = "events.sqlite3";
pub(crate) const MAX_EVENT_BYTES: usize = 64 * 1024;
// JSON escaping writes at most six bytes (`\u00XX`) per objective byte.
const MAX_ESCAPED_OBJECTIVE_BYTES: usize = 6 * crate::session::MAX_USER_MESSAGE_BYTES;
const MAX_GOAL_STARTED_BYTES: usize = MAX_ESCAPED_OBJECTIVE_BYTES + 16 * 1024;
const MAX_MESSAGE_ACCEPTED_BYTES: usize =
    MAX_ESCAPED_OBJECTIVE_BYTES + crate::provider::MAX_IMAGE_BYTES.div_ceil(3) * 4 + 16 * 1024;
const MAX_TOOL_FINISHED_BYTES: usize = crate::tools::types::MAX_TOOL_OBSERVATION_BYTES + 512;
// Run start needs extra headroom because GoalStarted and MessageAccepted exceed the ordinary Event bound.
const RUN_START_EXTRA_BYTES: usize =
    MAX_GOAL_STARTED_BYTES + MAX_MESSAGE_ACCEPTED_BYTES - 2 * MAX_EVENT_BYTES;
fn event_payload_limit(kind: &str, version: i64) -> usize {
    match kind {
        "SupervisorVerdictRecorded" if payload_version(kind) == Some(version) => {
            crate::supervisor::MAX_VERDICT_BYTES
        }
        "MessageAccepted" if payload_version(kind) == Some(version) => MAX_MESSAGE_ACCEPTED_BYTES,
        "GoalStarted" if payload_version(kind) == Some(version) => MAX_GOAL_STARTED_BYTES,
        "ToolFinished" if payload_version(kind) == Some(version) => MAX_TOOL_FINISHED_BYTES,
        "HookFinished" if payload_version(kind) == Some(version) => {
            crate::tools::types::MAX_TOOL_OBSERVATION_BYTES
        }
        _ => MAX_EVENT_BYTES,
    }
}
const MAX_DATABASE_BYTES: i64 = 256 * 1024 * 1024;
type ResolvedSession = (Vec<EventEnvelope>, Option<SessionView>);

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("invalid state directory")]
    InvalidStateDirectory,
    #[error("state directory is not private")]
    StateNotPrivate,
    #[error("state storage is full")]
    StorageFull,
    #[error("Session has an active operation")]
    SessionBusy,
    #[error("Workspace has an active writer")]
    WorkspaceBusy,
    #[error("account replacement is already active")]
    AccountBusy,
    #[error("Workspace build cache is in use by another Tool")]
    BuildCacheBusy,
    #[error("Workspace build cache is invalid or unsafe")]
    InvalidBuildCache,
    #[error("checkpoint storage is unavailable")]
    CheckpointUnavailable,
    #[error("checkpoint storage changed during recovery; retry before resetting")]
    CheckpointChanged,
    #[error("history is in use by another Arany process; close it and retry")]
    HistoryBusy,
    #[error("saved history changed during recovery; retry before resetting")]
    HistoryChanged,
    #[error("Session history exceeds the replay limit")]
    ReplayLimit,
    #[error("too many Sessions for discovery; resume an exact ID")]
    TooManySessions,
    #[error("corrupt or unsupported Event history")]
    InvalidHistory,
    #[error("invalid compaction snapshot")]
    InvalidSnapshot,
    #[error("store thread closed unexpectedly")]
    Closed,
    #[error("store is read-only")]
    ReadOnly,
    #[error("state filesystem operation failed")]
    Io(#[source] io::Error),
    #[error("SQLite operation failed")]
    Sqlite(#[source] rusqlite::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryRecovery {
    InvalidHistory,
    Damaged,
    InvalidStateDirectory,
    Inaccessible,
    Busy,
    Changed,
    StorageFull,
    Unavailable,
}

impl HistoryRecovery {
    pub fn is_resettable(self) -> bool {
        matches!(
            self,
            Self::InvalidHistory | Self::Damaged | Self::InvalidStateDirectory
        )
    }
}

impl StoreError {
    pub fn history_recovery(&self) -> HistoryRecovery {
        match self {
            Self::InvalidHistory => HistoryRecovery::InvalidHistory,
            Self::Sqlite(rusqlite::Error::SqliteFailure(code, _))
                if matches!(
                    code.code,
                    rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
                ) =>
            {
                HistoryRecovery::Damaged
            }
            Self::InvalidStateDirectory => HistoryRecovery::InvalidStateDirectory,
            Self::StateNotPrivate | Self::Io(_) => HistoryRecovery::Inaccessible,
            Self::HistoryBusy => HistoryRecovery::Busy,
            Self::HistoryChanged => HistoryRecovery::Changed,
            Self::StorageFull => HistoryRecovery::StorageFull,
            _ => HistoryRecovery::Unavailable,
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<crate::private_fs::Error> for StoreError {
    fn from(error: crate::private_fs::Error) -> Self {
        match error {
            crate::private_fs::Error::InvalidPath => Self::InvalidStateDirectory,
            crate::private_fs::Error::NotPrivate => Self::StateNotPrivate,
            crate::private_fs::Error::Io(error) => Self::Io(error),
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

impl From<ReplayError> for StoreError {
    fn from(error: ReplayError) -> Self {
        match error {
            ReplayError::InvalidSnapshot => Self::InvalidSnapshot,
            _ => Self::InvalidHistory,
        }
    }
}

enum Request {
    SupervisorCorpus {
        session_id: SessionId,
        reply: oneshot::Sender<Result<String, crate::supervisor::CorpusError>>,
    },
    AdmitOperation {
        session_id: SessionId,
        event_slots: usize,
        tool_observations: usize,
        run_start: bool,
        reply: oneshot::Sender<Result<(), StoreError>>,
    },
    Append {
        session_id: SessionId,
        event: Box<Event>,
        reply: oneshot::Sender<Result<EventEnvelope, StoreError>>,
    },
    Load {
        session_id: SessionId,
        reply: oneshot::Sender<Result<Vec<EventEnvelope>, StoreError>>,
    },
    RunSession {
        run_id: RunId,
        reply: oneshot::Sender<Result<Option<SessionId>, StoreError>>,
    },
    LoadView {
        session_id: SessionId,
        reply: oneshot::Sender<Result<Option<SessionView>, StoreError>>,
    },
    LoadOpenView {
        session_id: SessionId,
        reply: oneshot::Sender<Result<Option<SessionView>, StoreError>>,
    },
    LoadResolved {
        session_id: SessionId,
        reply: oneshot::Sender<Result<ResolvedSession, StoreError>>,
    },
    LatestSession {
        workspace_identity: WorkspaceIdentity,
        reply: oneshot::Sender<Result<Option<SessionId>, StoreError>>,
    },
    ListSessions {
        workspace_identity: WorkspaceIdentity,
        reply: oneshot::Sender<Result<Vec<SessionListItem>, StoreError>>,
    },
    Fork {
        source_session_id: SessionId,
        title: String,
        reply: oneshot::Sender<Result<SessionId, StoreError>>,
    },
    Evidence {
        op: EvidenceOp,
        reply: oneshot::Sender<Result<Option<EvidenceRecord>, StoreError>>,
    },
    CheckpointCapture {
        capture: CheckpointCapture,
        reply: oneshot::Sender<Result<crate::session::Checkpoint, StoreError>>,
    },
    CheckpointLoad {
        digest: [u8; 32],
        reply: oneshot::Sender<Result<Option<Vec<u8>>, StoreError>>,
    },
    CheckpointReset {
        reply: oneshot::Sender<Result<CheckpointReset, StoreError>>,
    },
    Close {
        reply: oneshot::Sender<()>,
    },
}

pub struct Store {
    requests: Option<mpsc::Sender<Request>>,
    thread: Option<JoinHandle<()>>,
}

impl Store {
    pub fn open(root: StateRoot) -> Result<Self, StoreError> {
        Self::start(root, false)
    }

    pub fn open_read_only(root: StateRoot) -> Result<Self, StoreError> {
        Self::start(root, true)
    }

    fn start(root: StateRoot, read_only: bool) -> Result<Self, StoreError> {
        let (requests, mut receiver) = mpsc::channel(64);
        let (ready_sender, ready_receiver) = sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("arany-sqlite".into())
            .spawn(move || {
                let _history = match root.lock_history(false) {
                    Ok(lock) => lock,
                    Err(error) => {
                        let _ = ready_sender.send(Err(error));
                        return;
                    }
                };
                let connection = open_connection(&root, read_only);
                let Ok(mut connection) = connection else {
                    let _ = ready_sender.send(connection.map(|_| ()));
                    return;
                };
                if ready_sender.send(Ok(())).is_err() {
                    return;
                }
                let mut checkpoint_checked = None;
                let mut pending_prunes: state::checkpoints::Pruned = if !read_only {
                    state::checkpoints::sweep(
                        &root,
                        i64::try_from(crate::tools::now_ms()).unwrap_or(i64::MAX),
                        &mut checkpoint_checked,
                    ).unwrap_or_default()
                } else { Default::default() };
                let mut pruning_notified_run = None;
                let mut hot_session = None::<HotSession>;
                while let Some(request) = receiver.blocking_recv() {
                    match request {
                        Request::AdmitOperation {
                            session_id,
                            event_slots,
                            tool_observations,
                            run_start,
                            reply,
                        } => {
                            let result = if read_only {
                                Err(StoreError::ReadOnly)
                            } else {
                                journal::admit_operation(
                                    &mut connection,
                                    session_id,
                                    event_slots,
                                    tool_observations,
                                    run_start,
                                )
                            };
                            let _ = reply.send(result);
                        }
                        Request::Append {
                            session_id,
                            event,
                            reply,
                        } => {
                            #[cfg(any(test, feature = "test-support"))]
                            if crate::fault::FaultPlan::append_fails(session_id, event.as_ref()) {
                                let _ = reply.send(Err(StoreError::Closed));
                                continue;
                            }
                            let result = if read_only {
                                Err(StoreError::ReadOnly)
                            } else if matches!(event.as_ref(), Event::SessionForked { .. }) {
                                Err(StoreError::InvalidHistory)
                            } else {
                                append(&mut connection, &mut hot_session, session_id, *event)
                            };
                            if let Ok(envelope) = &result
                                && let Event::ToolFinished {
                                    run_id,
                                    checkpoint: Some(checkpoint),
                                    ..
                                } = &envelope.event
                                && let Some(pruned) = checkpoint.pruned_before_run
                            {
                                if pending_prunes.get(&session_id).is_some_and(|pending| pending <= &pruned) {
                                    pending_prunes.remove(&session_id);
                                }
                                pruning_notified_run = Some(*run_id);
                            }
                            #[cfg(any(test, feature = "test-support"))]
                            let result = match result {
                                Ok(envelope) => {
                                    match crate::fault::FaultPlan::acknowledgment_gate(
                                        session_id,
                                        &envelope.event,
                                    ) {
                                        Some((entered, release)) => {
                                            let _ = entered.send(());
                                            // A dropped release means the commit was never acknowledged.
                                            release
                                                .recv()
                                                .map(|()| envelope)
                                                .map_err(|_| StoreError::Closed)
                                        }
                                        None => Ok(envelope),
                                    }
                                }
                                error => error,
                            };
                            let _ = reply.send(result);
                        }
                        Request::SupervisorCorpus { session_id, reply } => {
                            let _ = reply.send(replay::supervisor_corpus(&connection, session_id));
                        }
                        Request::Load { session_id, reply } => {
                            let _ = reply.send(load_session(&connection, session_id));
                        }
                        Request::RunSession { run_id, reply } => {
                            let _ = reply.send(session_for_run(&connection, run_id));
                        }
                        Request::LoadView { session_id, reply } => {
                            let _ = reply.send(
                                resolve_view(&connection, session_id, 0, &mut 0)
                                    .map(|(_, view)| view),
                            );
                        }
                        Request::LoadOpenView { session_id, reply } => {
                            let _ = reply.send(
                                resolve_open_view(&connection, session_id, &mut 0)
                                    .map(|(_, view)| view),
                            );
                        }
                        Request::LoadResolved { session_id, reply } => {
                            let _ = reply.send(resolve_view(&connection, session_id, 0, &mut 0));
                        }
                        Request::LatestSession {
                            workspace_identity,
                            reply,
                        } => {
                            let _ = reply.send(latest_session_id(&connection, workspace_identity));
                        }
                        Request::ListSessions {
                            workspace_identity,
                            reply,
                        } => {
                            let _ = reply.send(list_session_items(&connection, workspace_identity));
                        }
                        Request::Fork {
                            source_session_id,
                            title,
                            reply,
                        } => {
                            hot_session = None;
                            let result = if read_only {
                                Err(StoreError::ReadOnly)
                            } else {
                                fork_session(&mut connection, source_session_id, title)
                            };
                            let _ = reply.send(result);
                        }
                        Request::Evidence { op, reply } => {
                            let _ = reply.send(evidence::apply(&mut connection, read_only, op));
                        }
                        Request::CheckpointCapture { capture, reply } => {
                            let result = if read_only {
                                Err(StoreError::ReadOnly)
                            } else {
                                let started = connection.query_row(
                                    "SELECT MAX(sequence) FROM events WHERE session_id=?1 AND run_id=?2 AND kind='ToolStarted' AND json_extract(payload,'$.intent.id')=?3",
                                    rusqlite::params![capture.session.to_string(), capture.run.to_string(), capture.intent_id.to_string()],
                                    |row| row.get::<_, Option<i64>>(0),
                                );
                                let Ok(Some(started)) = started else {
                                    let _ = reply.send(Err(StoreError::InvalidHistory));
                                    continue;
                                };
                                let (run, session) = (capture.run, capture.session);
                                let mut checkpoint = state::checkpoints::capture(
                                    &root, capture, u64::try_from(started).unwrap_or_default(),
                                    i64::try_from(crate::tools::now_ms()).unwrap_or(i64::MAX),
                                    &mut checkpoint_checked,
                                );
                                if let Some(pruned) = checkpoint.pruned_before_run {
                                    let pending = pending_prunes.entry(session).or_insert(pruned);
                                    *pending = (*pending).max(pruned);
                                }
                                checkpoint.pruned_before_run = pending_prunes.get(&session)
                                    .copied()
                                    .filter(|_| pruning_notified_run != Some(run));
                                Ok(checkpoint)
                            };
                            let _ = reply.send(result);
                        }
                        Request::CheckpointLoad { digest, reply } => {
                            let _ = reply.send(state::checkpoints::load(&root, digest, &mut checkpoint_checked));
                        }
                        Request::CheckpointReset { reply } => {
                            let result = if read_only {
                                Err(StoreError::ReadOnly)
                            } else {
                                state::checkpoints::prepare_reset(&root)
                            };
                            let _ = reply.send(result);
                        }
                        Request::Close { reply } => {
                            let _ = reply.send(());
                            break;
                        }
                    }
                }
            })?;
        match ready_receiver.recv() {
            Ok(Ok(())) => Ok(Self {
                requests: Some(requests),
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err(StoreError::Closed)
            }
        }
    }

    pub(crate) async fn capture_checkpoint(
        &self,
        capture: CheckpointCapture,
    ) -> Result<crate::session::Checkpoint, StoreError> {
        self.call(|reply| Request::CheckpointCapture { capture, reply })
            .await
    }

    pub async fn checkpoint_blob(&self, digest: [u8; 32]) -> Result<Option<Vec<u8>>, StoreError> {
        self.call(|reply| Request::CheckpointLoad { digest, reply })
            .await
    }

    pub async fn prepare_checkpoint_reset(&self) -> Result<CheckpointReset, StoreError> {
        self.call(|reply| Request::CheckpointReset { reply }).await
    }

    pub async fn append(
        &self,
        session_id: SessionId,
        event: Event,
    ) -> Result<EventEnvelope, StoreError> {
        self.call(|reply| Request::Append {
            session_id,
            event: Box::new(event),
            reply,
        })
        .await
    }

    pub(crate) async fn clear_provider_evidence(&self, name: String) -> Result<(), StoreError> {
        self.evidence(EvidenceOp::ClearProvider(name)).await?;
        Ok(())
    }

    pub(crate) async fn record_provider_evidence(
        &self,
        record: ProviderEvidenceRecord,
    ) -> Result<(), StoreError> {
        self.evidence(EvidenceOp::RecordProvider(record)).await?;
        Ok(())
    }

    pub(crate) async fn load_provider_evidence(
        &self,
        name: String,
    ) -> Result<Option<ProviderEvidenceRecord>, StoreError> {
        match self.evidence(EvidenceOp::LoadProvider(name)).await? {
            None => Ok(None),
            Some(EvidenceRecord::Provider(record)) => Ok(Some(record)),
            Some(EvidenceRecord::Native(_)) => Err(StoreError::InvalidHistory),
        }
    }

    pub(crate) async fn clear_native_evidence(
        &self,
        fingerprint: [u8; 32],
    ) -> Result<(), StoreError> {
        self.evidence(EvidenceOp::ClearNative(fingerprint)).await?;
        Ok(())
    }

    pub(crate) async fn record_native_evidence(
        &self,
        record: NativeEvidenceRecord,
    ) -> Result<(), StoreError> {
        self.evidence(EvidenceOp::RecordNative(record)).await?;
        Ok(())
    }

    pub(crate) async fn load_native_evidence(
        &self,
        fingerprint: [u8; 32],
    ) -> Result<Option<NativeEvidenceRecord>, StoreError> {
        match self.evidence(EvidenceOp::LoadNative(fingerprint)).await? {
            None => Ok(None),
            Some(EvidenceRecord::Native(record)) => Ok(Some(record)),
            Some(EvidenceRecord::Provider(_)) => Err(StoreError::InvalidHistory),
        }
    }

    async fn evidence(&self, op: EvidenceOp) -> Result<Option<EvidenceRecord>, StoreError> {
        self.call(|reply| Request::Evidence { op, reply }).await
    }

    pub(crate) async fn supervisor_corpus(
        &self,
        session_id: SessionId,
    ) -> Result<String, crate::supervisor::CorpusError> {
        let (reply, receiver) = oneshot::channel();
        self.requests
            .as_ref()
            .ok_or(StoreError::Closed)?
            .send(Request::SupervisorCorpus { session_id, reply })
            .await
            .map_err(|_| StoreError::Closed)?;
        receiver.await.map_err(|_| StoreError::Closed)?
    }

    pub async fn load_session(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<EventEnvelope>, StoreError> {
        self.call(|reply| Request::Load { session_id, reply }).await
    }

    pub(crate) async fn admit_operation(
        &self,
        session_id: SessionId,
        event_slots: usize,
        tool_observations: usize,
    ) -> Result<(), StoreError> {
        self.admit(session_id, event_slots, tool_observations, false)
            .await
    }

    // Reserve objective and image envelopes before the first GoalStarted and MessageAccepted.
    pub(crate) async fn admit_run_start(
        &self,
        session_id: SessionId,
        event_slots: usize,
    ) -> Result<(), StoreError> {
        self.admit(session_id, event_slots, 0, true).await
    }

    async fn admit(
        &self,
        session_id: SessionId,
        event_slots: usize,
        tool_observations: usize,
        run_start: bool,
    ) -> Result<(), StoreError> {
        self.call(|reply| Request::AdmitOperation {
            session_id,
            event_slots,
            tool_observations,
            run_start,
            reply,
        })
        .await
    }

    pub async fn session_for_run(&self, run_id: RunId) -> Result<Option<SessionId>, StoreError> {
        self.call(|reply| Request::RunSession { run_id, reply })
            .await
    }

    pub async fn load_view(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionView>, StoreError> {
        self.call(|reply| Request::LoadView { session_id, reply })
            .await
    }

    pub async fn load_open_view(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionView>, StoreError> {
        self.call(|reply| Request::LoadOpenView { session_id, reply })
            .await
    }

    pub(crate) async fn load_resolved(
        &self,
        session_id: SessionId,
    ) -> Result<ResolvedSession, StoreError> {
        self.call(|reply| Request::LoadResolved { session_id, reply })
            .await
    }

    pub(crate) async fn latest_session_id(
        &self,
        workspace_identity: WorkspaceIdentity,
    ) -> Result<Option<SessionId>, StoreError> {
        self.call(|reply| Request::LatestSession {
            workspace_identity,
            reply,
        })
        .await
    }

    pub(crate) async fn list_sessions(
        &self,
        workspace_identity: WorkspaceIdentity,
    ) -> Result<Vec<SessionListItem>, StoreError> {
        self.call(|reply| Request::ListSessions {
            workspace_identity,
            reply,
        })
        .await
    }

    pub(crate) async fn fork_session(
        &self,
        source_session_id: SessionId,
        title: String,
    ) -> Result<SessionId, StoreError> {
        self.call(|reply| Request::Fork {
            source_session_id,
            title,
            reply,
        })
        .await
    }

    async fn call<T>(
        &self,
        request: impl FnOnce(oneshot::Sender<Result<T, StoreError>>) -> Request,
    ) -> Result<T, StoreError> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .as_ref()
            .ok_or(StoreError::Closed)?
            .send(request(reply))
            .await
            .map_err(|_| StoreError::Closed)?;
        answer.await.map_err(|_| StoreError::Closed)?
    }

    pub async fn close(mut self) -> Result<(), StoreError> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .take()
            .ok_or(StoreError::Closed)?
            .send(Request::Close { reply })
            .await
            .map_err(|_| StoreError::Closed)?;
        answer.await.map_err(|_| StoreError::Closed)?;
        self.join()
    }

    fn join(&mut self) -> Result<(), StoreError> {
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| StoreError::Closed)?;
        }
        Ok(())
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        self.requests.take();
        let _ = self.join();
    }
}

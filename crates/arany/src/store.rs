use crate::session::{
    Event, EventEnvelope, ForkLineage, ReplayError, RunId, SessionId, SessionListItem, SessionPage,
    SessionView, WorkspaceIdentity,
};
use std::io;
use std::sync::mpsc::sync_channel;
use std::thread::JoinHandle;
use tokio::sync::{mpsc, oneshot};

pub use replay::{InspectedEvent, InspectionCause, InspectionFailure, SessionInspection};
mod format;
mod journal;
mod replay;
mod state;
pub use format::{RECORD_FORMATS, RecordFormat};
pub(crate) use journal::MAX_HOT_SESSION_PAYLOAD_BYTES;
use journal::{HotSession, append, open_connection};
pub(crate) use replay::{MAX_DISCOVERED_SESSIONS, MAX_LIST_SCAN_EVENTS, MAX_SESSION_EVENTS};
use replay::{
    inspect_session, latest_session_id, list_session_page, load_session, resolve_open_view,
    resolve_view, session_for_run,
};
pub use state::checkpoints::CheckpointReset;
pub(crate) use state::checkpoints::{
    Capture as CheckpointCapture, CaptureOwner as CheckpointOwner,
};
pub(crate) use state::spill::{SWEEP_TIME_MS, SpillAccess};
pub use state::{ArchivedHistory, ArchivedRecords, HistoryReset};
pub(crate) use state::{
    BUILD_CACHE_DEFAULT_VIEW_BYTES, BuildCacheIdentity, BuildCacheLease, SessionRunLock,
    WorkspaceRunLock,
};
pub(crate) use state::{CACHE_ROOTS, CacheChange, CacheGuardRecovery, validate_cache_path};
pub(crate) use state::{
    MAX_SESSION_LOCK_FILES, MAX_STATE_OVERHEAD_FILES, MAX_WORKSPACE_LOCK_FILES,
};
pub use state::{ProviderRecordEdit, ProviderRecordTarget, Record, RecordKind, StateRoot};

const DATABASE_FILE: &str = "events.sqlite3";
const PROVIDER_SETUP_EDIT_EVENT_SLOTS: usize = 2;
pub(crate) const MAX_EVENT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_COMPACTION_EVENT_BYTES: usize = 400 * 1024;
const COMPACTION_EVENT_WRAPPER_BYTES: usize = 1024;
pub(crate) const MAX_COMPACTION_RECORD_BYTES: usize =
    MAX_COMPACTION_EVENT_BYTES - COMPACTION_EVENT_WRAPPER_BYTES;
// JSON escaping writes at most six bytes (`\u00XX`) per objective byte.
const MAX_ESCAPED_OBJECTIVE_BYTES: usize = 6 * crate::session::MAX_USER_MESSAGE_BYTES;
pub(crate) const MAX_GOAL_STARTED_BYTES: usize = MAX_ESCAPED_OBJECTIVE_BYTES + 16 * 1024;
pub(crate) const MAX_MESSAGE_ACCEPTED_BYTES: usize =
    MAX_ESCAPED_OBJECTIVE_BYTES + crate::provider::MAX_IMAGE_BYTES.div_ceil(3) * 4 + 16 * 1024;
pub(crate) const MAX_TOOL_FINISHED_BYTES: usize =
    crate::tools::types::MAX_TOOL_OBSERVATION_BYTES + 512;
pub(crate) const MAX_GOAL_FINISHED_BYTES: usize =
    crate::session::MAX_GOAL_REPORT_BYTES + crate::session::MAX_BUDGET_REFUSAL_BYTES + 512;
const _: () = assert!(MAX_GOAL_FINISHED_BYTES <= MAX_EVENT_BYTES);
// The complete intent: Tool arguments plus bounded metadata; Tool admission reserves it as an observation slot.
pub(crate) const MAX_TOOL_STARTED_BYTES: usize = 512 * 1024;
const _: () = assert!(MAX_TOOL_STARTED_BYTES <= MAX_TOOL_FINISHED_BYTES);
// Run start needs extra headroom because GoalStarted and MessageAccepted exceed the ordinary Event bound.
const RUN_START_EXTRA_BYTES: usize =
    MAX_GOAL_STARTED_BYTES + MAX_MESSAGE_ACCEPTED_BYTES - 2 * MAX_EVENT_BYTES;
fn event_payload_limit(kind: &str) -> usize {
    match kind {
        "SupervisorVerdictRecorded" => crate::supervisor::MAX_VERDICT_BYTES,
        "MessageAccepted" => MAX_MESSAGE_ACCEPTED_BYTES,
        "ContextCompacted" | "GoalContextCompacted" => MAX_COMPACTION_EVENT_BYTES,
        "GoalStarted" => MAX_GOAL_STARTED_BYTES,
        "GoalFinished" => MAX_GOAL_FINISHED_BYTES,
        "ToolProposed" | "ToolStarted" | "CommandToolProbeStarted" => MAX_TOOL_STARTED_BYTES,
        "ToolFinished" => MAX_TOOL_FINISHED_BYTES,
        "HookFinished" | "CommandToolProbeFinished" => {
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
    #[error("target is not a committed Run boundary")]
    ForkNotAtRunBoundary,
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
    #[error("history is busy with a committing write; retry the listing")]
    ListingBusy,
    #[error("history is in use by another Arany process; close it and retry")]
    HistoryBusy,
    #[error("saved history changed during recovery; retry before resetting")]
    HistoryChanged,
    #[error("Session history exceeds the replay limit")]
    ReplayLimit,
    #[error("too many Sessions for discovery; resume an exact ID")]
    TooManySessions,
    #[error("Session listing cursor no longer matches the journal")]
    StaleCursor,
    #[error("corrupt or unsupported Event history")]
    InvalidHistory,
    #[error("an interrupted history archive no longer matches its files")]
    ArchiveConflict,
    #[error("invalid compaction snapshot")]
    InvalidSnapshot,
    #[error("store thread closed unexpectedly")]
    Closed,
    #[error("store is read-only")]
    ReadOnly,
    #[error("Provider record changed since it was inspected")]
    RecordChanged,
    #[error("record replacement may have taken effect; its directory sync failed")]
    ReplaceUnconfirmed,
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
    ArchiveConflict,
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
            Self::ArchiveConflict => HistoryRecovery::ArchiveConflict,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionKind {
    Operation,
    Compaction,
    // Reserve the objective and image envelopes.
    RunStart,
}

enum Request {
    SupervisorCorpus {
        session_id: SessionId,
        reply: oneshot::Sender<Result<String, crate::supervisor::CorpusError>>,
    },
    Admit {
        session_id: SessionId,
        event_slots: usize,
        tool_observations: usize,
        kind: AdmissionKind,
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
    Inspect {
        session_id: SessionId,
        reply: oneshot::Sender<Result<replay::SessionInspection, StoreError>>,
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
        after: Option<(SessionId, u64)>,
        limit: usize,
        watermark: Option<u64>,
        reply: oneshot::Sender<Result<(SessionPage, u64), StoreError>>,
    },
    ListAllSessions {
        workspace_identity: WorkspaceIdentity,
        reply: oneshot::Sender<Result<Vec<SessionListItem>, StoreError>>,
    },
    PrepareFork {
        source_session_id: SessionId,
        source_sequence: Option<u64>,
        reply: oneshot::Sender<Result<ForkLineage, StoreError>>,
    },
    ForkAt {
        boundary: ForkLineage,
        title: String,
        reply: oneshot::Sender<Result<SessionId, StoreError>>,
    },
    Fork {
        source_session_id: SessionId,
        title: String,
        reply: oneshot::Sender<Result<SessionId, StoreError>>,
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

// A requester that stopped waiting needs no reply.
fn send<T>(reply: oneshot::Sender<T>, value: T) {
    let _ = reply.send(value);
}

struct StoreThread {
    root: StateRoot,
    read_only: bool,
    connection: rusqlite::Connection,
    hot_session: Option<HotSession>,
    checkpoint_checked: state::checkpoints::Checked,
    pending_prunes: state::checkpoints::Pruned,
    pruning_notified_run: Option<RunId>,
}

impl StoreThread {
    fn new(root: StateRoot, read_only: bool, connection: rusqlite::Connection) -> Self {
        let mut checkpoint_checked = None;
        let pending_prunes: state::checkpoints::Pruned = if !read_only {
            state::checkpoints::sweep(
                &root,
                i64::try_from(crate::tools::now_ms()).unwrap_or(i64::MAX),
                &mut checkpoint_checked,
            )
            .unwrap_or_default()
        } else {
            Default::default()
        };
        Self {
            root,
            read_only,
            connection,
            hot_session: None,
            checkpoint_checked,
            pending_prunes,
            pruning_notified_run: None,
        }
    }

    fn handle(&mut self, request: Request) -> std::ops::ControlFlow<()> {
        let connection = &self.connection;
        match request {
            Request::Admit {
                session_id,
                event_slots,
                tool_observations,
                kind,
                reply,
            } => {
                let admission = journal::Admission {
                    session_id,
                    event_slots,
                    tool_observations,
                    kind,
                };
                self.admit(admission, reply);
            }
            Request::Append {
                session_id,
                event,
                reply,
            } => self.append(session_id, event, reply),
            Request::SupervisorCorpus { session_id, reply } => {
                send(reply, replay::supervisor_corpus(connection, session_id));
            }
            Request::Load { session_id, reply } => {
                send(reply, load_session(connection, session_id))
            }
            Request::Inspect { session_id, reply } => {
                send(reply, inspect_session(connection, session_id));
            }
            Request::RunSession { run_id, reply } => {
                send(reply, session_for_run(connection, run_id))
            }
            Request::LoadView { session_id, reply } => send(
                reply,
                resolve_view(connection, session_id, 0, &mut 0).map(|(_, view)| view),
            ),
            Request::LoadOpenView { session_id, reply } => send(
                reply,
                resolve_open_view(connection, session_id, &mut 0).map(|(_, view)| view),
            ),
            Request::LoadResolved { session_id, reply } => {
                send(reply, resolve_view(connection, session_id, 0, &mut 0));
            }
            Request::LatestSession {
                workspace_identity,
                reply,
            } => send(reply, latest_session_id(connection, workspace_identity)),
            Request::ListSessions {
                workspace_identity,
                after,
                limit,
                watermark,
                reply,
            } => send(
                reply,
                list_session_page(
                    connection,
                    workspace_identity,
                    after,
                    limit,
                    replay::MAX_LIST_SCAN_EVENTS,
                    watermark,
                ),
            ),
            Request::ListAllSessions {
                workspace_identity,
                reply,
            } => send(
                reply,
                replay::list_all_sessions(connection, workspace_identity),
            ),
            Request::PrepareFork {
                source_session_id,
                source_sequence,
                reply,
            } => self.prepare_fork(source_session_id, source_sequence, reply),
            Request::ForkAt {
                boundary,
                title,
                reply,
            } => self.fork_at(boundary, title, reply),
            Request::Fork {
                source_session_id,
                title,
                reply,
            } => self.fork(source_session_id, title, reply),
            Request::CheckpointCapture { capture, reply } => self.capture(capture, reply),
            Request::CheckpointLoad { digest, reply } => self.load_checkpoint(digest, reply),
            Request::CheckpointReset { reply } => self.reset_checkpoints(reply),
            Request::Close { reply } => {
                send(reply, ());
                return std::ops::ControlFlow::Break(());
            }
        }
        std::ops::ControlFlow::Continue(())
    }

    fn admit(
        &mut self,
        admission: journal::Admission,
        reply: oneshot::Sender<Result<(), StoreError>>,
    ) {
        let result = if self.read_only {
            Err(StoreError::ReadOnly)
        } else {
            journal::admit(&mut self.connection, self.hot_session.as_ref(), admission)
        };
        send(reply, result);
    }

    fn prepare_fork(
        &mut self,
        source_session_id: SessionId,
        source_sequence: Option<u64>,
        reply: oneshot::Sender<Result<ForkLineage, StoreError>>,
    ) {
        send(
            reply,
            journal::prepare_fork(
                &mut self.connection,
                self.hot_session.as_ref(),
                source_session_id,
                source_sequence,
            ),
        );
    }

    fn fork_at(
        &mut self,
        boundary: ForkLineage,
        title: String,
        reply: oneshot::Sender<Result<SessionId, StoreError>>,
    ) {
        let cached = self.hot_session.take();
        let result = if self.read_only {
            Err(StoreError::ReadOnly)
        } else {
            journal::fork_at(&mut self.connection, cached.as_ref(), boundary, title)
        };
        send(reply, result);
    }

    fn fork(
        &mut self,
        source_session_id: SessionId,
        title: String,
        reply: oneshot::Sender<Result<SessionId, StoreError>>,
    ) {
        let cached = self.hot_session.take();
        let connection = &mut self.connection;
        let result = if self.read_only {
            Err(StoreError::ReadOnly)
        } else {
            journal::prepare_fork(connection, cached.as_ref(), source_session_id, None)
                .and_then(|boundary| journal::fork_at(connection, cached.as_ref(), boundary, title))
        };
        send(reply, result);
    }

    fn load_checkpoint(
        &mut self,
        digest: [u8; 32],
        reply: oneshot::Sender<Result<Option<Vec<u8>>, StoreError>>,
    ) {
        send(
            reply,
            state::checkpoints::load(&self.root, digest, &mut self.checkpoint_checked),
        );
    }

    fn reset_checkpoints(&mut self, reply: oneshot::Sender<Result<CheckpointReset, StoreError>>) {
        let result = if self.read_only {
            Err(StoreError::ReadOnly)
        } else {
            state::checkpoints::prepare_reset(&self.root)
        };
        send(reply, result);
    }

    fn append(
        &mut self,
        session_id: SessionId,
        event: Box<Event>,
        reply: oneshot::Sender<Result<EventEnvelope, StoreError>>,
    ) {
        if let Err(error) = crate::fault::append(session_id, event.as_ref()) {
            let _ = reply.send(Err(error));
            return;
        }
        let result = if self.read_only {
            Err(StoreError::ReadOnly)
        } else if matches!(event.as_ref(), Event::SessionForked { .. }) {
            Err(StoreError::InvalidHistory)
        } else {
            append(
                &mut self.connection,
                &mut self.hot_session,
                session_id,
                *event,
            )
        };
        if let Ok(envelope) = &result {
            let notice = match &envelope.event {
                Event::ToolFinished {
                    run_id,
                    checkpoint: Some(checkpoint),
                    ..
                } => Some((Some(*run_id), checkpoint)),
                Event::WorkspaceRewindFinished { record } => record
                    .checkpoint
                    .as_ref()
                    .map(|checkpoint| (None, checkpoint)),
                _ => None,
            };
            if let Some((run, checkpoint)) = notice
                && let Some(pruned) = checkpoint.pruned_before_run
            {
                if self
                    .pending_prunes
                    .get(&session_id)
                    .is_some_and(|pending| pending <= &pruned)
                {
                    self.pending_prunes.remove(&session_id);
                }
                if run.is_some() {
                    self.pruning_notified_run = run;
                }
            }
        }
        let result = crate::fault::acknowledge(session_id, result);
        let _ = reply.send(result);
    }

    fn capture(
        &mut self,
        capture: CheckpointCapture,
        reply: oneshot::Sender<Result<crate::session::Checkpoint, StoreError>>,
    ) {
        let result = if self.read_only {
            Err(StoreError::ReadOnly)
        } else {
            let (started, run) = match capture.owner {
                CheckpointOwner::Tool { run, intent_id, .. } => (
                    self.connection.query_row(
                        "SELECT MAX(sequence) FROM events WHERE session_id=?1 AND run_id=?2 AND kind='ToolStarted' AND json_extract(payload,'$.intent.id')=?3",
                        rusqlite::params![capture.session.to_string(), run.to_string(), intent_id.to_string()],
                        |row| row.get::<_, Option<i64>>(0),
                    ),
                    Some(run),
                ),
                CheckpointOwner::Rewind { rewind_id } => (
                    self.connection.query_row(
                        "SELECT MAX(sequence) FROM events WHERE session_id=?1 AND kind='WorkspaceRewindStarted' AND json_extract(payload,'$.rewind_id')=?2",
                        rusqlite::params![capture.session.to_string(), rewind_id.to_string()],
                        |row| row.get::<_, Option<i64>>(0),
                    ),
                    None,
                ),
            };
            let Ok(Some(started)) = started else {
                let _ = reply.send(Err(StoreError::InvalidHistory));
                return;
            };
            let session = capture.session;
            let mut checkpoint = state::checkpoints::capture(
                &self.root,
                capture,
                u64::try_from(started).unwrap_or_default(),
                i64::try_from(crate::tools::now_ms()).unwrap_or(i64::MAX),
                &mut self.checkpoint_checked,
            );
            if let Some(pruned) = checkpoint.pruned_before_run {
                let pending = self.pending_prunes.entry(session).or_insert(pruned);
                *pending = (*pending).max(pruned);
            }
            checkpoint.pruned_before_run = self
                .pending_prunes
                .get(&session)
                .copied()
                .filter(|_| run.is_none() || self.pruning_notified_run != run);
            Ok(checkpoint)
        };
        let _ = reply.send(result);
    }
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
                let Ok(connection) = connection else {
                    let _ = ready_sender.send(connection.map(|_| ()));
                    return;
                };
                if !read_only {
                    let _ = state::spill::sweep(&root, &connection);
                }
                if ready_sender.send(Ok(())).is_err() {
                    return;
                }
                let mut store = StoreThread::new(root, read_only, connection);
                while let Some(request) = receiver.blocking_recv() {
                    if store.handle(request).is_break() {
                        break;
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
        if matches!(&event, Event::ProviderSetupEditStarted { .. }) {
            self.admit(
                session_id,
                PROVIDER_SETUP_EDIT_EVENT_SLOTS,
                0,
                AdmissionKind::Operation,
            )
            .await?;
        }
        self.call(|reply| Request::Append {
            session_id,
            event: Box::new(event),
            reply,
        })
        .await
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

    pub async fn inspect_session(
        &self,
        session_id: SessionId,
    ) -> Result<SessionInspection, StoreError> {
        self.call(|reply| Request::Inspect { session_id, reply })
            .await
    }

    pub(crate) async fn admit(
        &self,
        session_id: SessionId,
        event_slots: usize,
        tool_observations: usize,
        kind: AdmissionKind,
    ) -> Result<(), StoreError> {
        self.call(|reply| Request::Admit {
            session_id,
            event_slots,
            tool_observations,
            kind,
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

    pub(crate) async fn list_all_sessions(
        &self,
        workspace_identity: WorkspaceIdentity,
    ) -> Result<Vec<SessionListItem>, StoreError> {
        self.call(|reply| Request::ListAllSessions {
            workspace_identity,
            reply,
        })
        .await
    }

    pub(crate) async fn list_sessions(
        &self,
        workspace_identity: WorkspaceIdentity,
        after: Option<(SessionId, u64)>,
        limit: usize,
        watermark: Option<u64>,
    ) -> Result<(SessionPage, u64), StoreError> {
        self.call(|reply| Request::ListSessions {
            workspace_identity,
            after,
            limit,
            watermark,
            reply,
        })
        .await
    }

    pub(crate) async fn prepare_fork(
        &self,
        source_session_id: SessionId,
        source_sequence: Option<u64>,
    ) -> Result<ForkLineage, StoreError> {
        self.call(|reply| Request::PrepareFork {
            source_session_id,
            source_sequence,
            reply,
        })
        .await
    }

    pub(crate) async fn fork_at(
        &self,
        boundary: ForkLineage,
        title: String,
    ) -> Result<SessionId, StoreError> {
        self.call(|reply| Request::ForkAt {
            boundary,
            title,
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

pub use state::{
    StorageCategory, StorageError, StorageReason, StorageReport, StorageStatus, StorageUsage,
    collect_storage,
};

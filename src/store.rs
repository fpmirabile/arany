use crate::session::{
    EVENT_PAYLOAD_VERSION, Event, EventEnvelope, ReplayError, RunId, SessionId, SessionListItem,
    SessionView,
};
use std::io;
use std::net::SocketAddr;
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
pub use state::StateRoot;
#[cfg(test)]
pub(crate) use state::capture_archive;
pub(crate) use state::{
    BUILD_CACHE_ARCHIVE_BYTES, BuildCacheIdentity, BuildCacheLease, SessionRunLock,
    WorkspaceRunLock, archive_size_with_limit,
};
#[cfg(test)]
pub(crate) use state::{begin_released, fail_next_commit_marker};
#[cfg(target_os = "macos")]
pub(crate) use state::{capture_archive_async_with_limit, restore_archive_async_with_limit};
#[cfg(target_os = "linux")]
pub(crate) use state::{capture_archive_with_limit, restore_archive_with_limit};

const DATABASE_FILE: &str = "events.sqlite3";
pub(crate) const MAX_EVENT_BYTES: usize = 64 * 1024;
fn event_payload_limit(kind: &str, version: i64) -> usize {
    if kind == "MessageAccepted" && version == EVENT_PAYLOAD_VERSION {
        320 * 1024
    } else {
        MAX_EVENT_BYTES
    }
}
const MAX_DATABASE_BYTES: i64 = 256 * 1024 * 1024;
type ResolvedSession = (Vec<EventEnvelope>, Option<SessionView>);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProviderEvidenceRecord {
    pub name: String,
    pub digest: [u8; 32],
    pub addresses: Vec<SocketAddr>,
    pub checked_at_ms: i64,
    pub expires_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeEvidenceRecord {
    pub fingerprint: [u8; 32],
    pub checked_at_ms: i64,
    pub expires_at_ms: i64,
}

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
    AdmitOperation {
        session_id: SessionId,
        event_slots: usize,
        message_extra: bool,
        reply: oneshot::Sender<Result<(), StoreError>>,
    },
    Append {
        session_id: SessionId,
        event: Box<Event>,
        reply: oneshot::Sender<Result<EventEnvelope, StoreError>>,
    },
    #[cfg(test)]
    GateGoalAppendAcknowledgment {
        session_id: SessionId,
        admits: fn(&Event) -> bool,
        entered: oneshot::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
        reply: oneshot::Sender<Result<(), StoreError>>,
    },
    #[cfg(test)]
    FailGoalAppend {
        session_id: SessionId,
        admits: fn(&Event) -> bool,
        reply: oneshot::Sender<Result<(), StoreError>>,
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
        workspace_identity: (u64, u64),
        reply: oneshot::Sender<Result<Option<SessionId>, StoreError>>,
    },
    ListSessions {
        workspace_identity: (u64, u64),
        reply: oneshot::Sender<Result<Vec<SessionListItem>, StoreError>>,
    },
    Fork {
        source_session_id: SessionId,
        title: String,
        reply: oneshot::Sender<Result<SessionId, StoreError>>,
    },
    ClearProviderEvidence {
        name: String,
        reply: oneshot::Sender<Result<(), StoreError>>,
    },
    RecordProviderEvidence {
        record: ProviderEvidenceRecord,
        reply: oneshot::Sender<Result<(), StoreError>>,
    },
    LoadProviderEvidence {
        name: String,
        reply: oneshot::Sender<Result<Option<ProviderEvidenceRecord>, StoreError>>,
    },
    ClearNativeEvidence {
        fingerprint: [u8; 32],
        reply: oneshot::Sender<Result<(), StoreError>>,
    },
    RecordNativeEvidence {
        record: NativeEvidenceRecord,
        reply: oneshot::Sender<Result<(), StoreError>>,
    },
    LoadNativeEvidence {
        fingerprint: [u8; 32],
        reply: oneshot::Sender<Result<Option<NativeEvidenceRecord>, StoreError>>,
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
                let mut hot_session = None::<HotSession>;
                #[cfg(test)]
                let mut goal_append_ack_gate = None::<(
                    SessionId,
                    fn(&Event) -> bool,
                    oneshot::Sender<()>,
                    std::sync::mpsc::Receiver<()>,
                )>;
                #[cfg(test)]
                let mut goal_append_failure = None::<(SessionId, fn(&Event) -> bool)>;
                while let Some(request) = receiver.blocking_recv() {
                    match request {
                        Request::AdmitOperation {
                            session_id,
                            event_slots,
                            message_extra,
                            reply,
                        } => {
                            let result = if read_only {
                                Err(StoreError::ReadOnly)
                            } else {
                                journal::admit_operation(
                                    &mut connection,
                                    session_id,
                                    event_slots,
                                    message_extra,
                                )
                            };
                            let _ = reply.send(result);
                        }
                        Request::Append {
                            session_id,
                            event,
                            reply,
                        } => {
                            #[cfg(test)]
                            if goal_append_failure
                                .as_ref()
                                .is_some_and(|(failing, admits)| {
                                    *failing == session_id && admits(event.as_ref())
                                })
                            {
                                goal_append_failure = None;
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
                            #[cfg(test)]
                            let result = {
                                let held = goal_append_ack_gate.as_ref().is_some_and(
                                    |(gated_session, admits, _, _)| {
                                        *gated_session == session_id
                                            && result
                                                .as_ref()
                                                .is_ok_and(|envelope| admits(&envelope.event))
                                    },
                                );
                                if held {
                                    let (_, _, entered, release) =
                                        goal_append_ack_gate.take().expect("matching append gate");
                                    let _ = entered.send(());
                                    // A dropped release means the commit was never acknowledged.
                                    match release.recv() {
                                        Ok(()) => result,
                                        Err(_) => Err(StoreError::Closed),
                                    }
                                } else {
                                    result
                                }
                            };
                            let _ = reply.send(result);
                        }
                        #[cfg(test)]
                        Request::GateGoalAppendAcknowledgment {
                            session_id,
                            admits,
                            entered,
                            release,
                            reply,
                        } => {
                            goal_append_ack_gate = Some((session_id, admits, entered, release));
                            let _ = reply.send(Ok(()));
                        }
                        #[cfg(test)]
                        Request::FailGoalAppend {
                            session_id,
                            admits,
                            reply,
                        } => {
                            goal_append_failure = Some((session_id, admits));
                            let _ = reply.send(Ok(()));
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
                        Request::ClearProviderEvidence { name, reply } => {
                            let result = if read_only {
                                Err(StoreError::ReadOnly)
                            } else {
                                evidence::clear(&mut connection, &name)
                            };
                            let _ = reply.send(result);
                        }
                        Request::RecordProviderEvidence { record, reply } => {
                            let result = if read_only {
                                Err(StoreError::ReadOnly)
                            } else {
                                evidence::record(&mut connection, &record)
                            };
                            let _ = reply.send(result);
                        }
                        Request::LoadProviderEvidence { name, reply } => {
                            let _ = reply.send(evidence::load(&connection, &name));
                        }
                        Request::ClearNativeEvidence { fingerprint, reply } => {
                            let result = if read_only {
                                Err(StoreError::ReadOnly)
                            } else {
                                evidence::clear_native(&mut connection, &fingerprint)
                            };
                            let _ = reply.send(result);
                        }
                        Request::RecordNativeEvidence { record, reply } => {
                            let result = if read_only {
                                Err(StoreError::ReadOnly)
                            } else {
                                evidence::record_native(&mut connection, &record)
                            };
                            let _ = reply.send(result);
                        }
                        Request::LoadNativeEvidence { fingerprint, reply } => {
                            let _ = reply.send(evidence::load_native(&connection, &fingerprint));
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

    /// Fails the next Event that `admits` accepts before it commits.
    #[cfg(test)]
    pub(crate) async fn fail_next_goal_append(
        &self,
        session_id: SessionId,
        admits: fn(&Event) -> bool,
    ) -> Result<(), StoreError> {
        self.call(|reply| Request::FailGoalAppend {
            session_id,
            admits,
            reply,
        })
        .await
    }

    /// Holds the acknowledgment of the next committed Event that `admits` accepts. Dropping the
    /// release sender fails that acknowledgment after the commit.
    #[cfg(test)]
    pub(crate) async fn gate_goal_append_acknowledgment(
        &self,
        session_id: SessionId,
        admits: fn(&Event) -> bool,
    ) -> Result<(oneshot::Receiver<()>, std::sync::mpsc::Sender<()>), StoreError> {
        let (entered, wait_entered) = oneshot::channel();
        let (release, wait_release) = std::sync::mpsc::channel();
        self.call(|reply| Request::GateGoalAppendAcknowledgment {
            session_id,
            admits,
            entered,
            release: wait_release,
            reply,
        })
        .await?;
        Ok((wait_entered, release))
    }

    pub(crate) async fn clear_provider_evidence(&self, name: String) -> Result<(), StoreError> {
        self.call(|reply| Request::ClearProviderEvidence { name, reply })
            .await
    }

    pub(crate) async fn record_provider_evidence(
        &self,
        record: ProviderEvidenceRecord,
    ) -> Result<(), StoreError> {
        self.call(|reply| Request::RecordProviderEvidence { record, reply })
            .await
    }

    pub(crate) async fn load_provider_evidence(
        &self,
        name: String,
    ) -> Result<Option<ProviderEvidenceRecord>, StoreError> {
        self.call(|reply| Request::LoadProviderEvidence { name, reply })
            .await
    }

    pub(crate) async fn clear_native_evidence(
        &self,
        fingerprint: [u8; 32],
    ) -> Result<(), StoreError> {
        self.call(|reply| Request::ClearNativeEvidence { fingerprint, reply })
            .await
    }

    pub(crate) async fn record_native_evidence(
        &self,
        record: NativeEvidenceRecord,
    ) -> Result<(), StoreError> {
        self.call(|reply| Request::RecordNativeEvidence { record, reply })
            .await
    }

    pub(crate) async fn load_native_evidence(
        &self,
        fingerprint: [u8; 32],
    ) -> Result<Option<NativeEvidenceRecord>, StoreError> {
        self.call(|reply| Request::LoadNativeEvidence { fingerprint, reply })
            .await
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
    ) -> Result<(), StoreError> {
        self.admit(session_id, event_slots, false).await
    }

    /// Admits the Run start, the only operation that precedes `MessageAccepted` and so reserves its image envelope.
    pub(crate) async fn admit_run_start(
        &self,
        session_id: SessionId,
        event_slots: usize,
    ) -> Result<(), StoreError> {
        self.admit(session_id, event_slots, true).await
    }

    async fn admit(
        &self,
        session_id: SessionId,
        event_slots: usize,
        message_extra: bool,
    ) -> Result<(), StoreError> {
        self.call(|reply| Request::AdmitOperation {
            session_id,
            event_slots,
            message_extra,
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

    /// The Session as the append path validates it: an open Run or Goal is not projected as interrupted.
    pub(crate) async fn load_open_view(
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
        workspace_identity: (u64, u64),
    ) -> Result<Option<SessionId>, StoreError> {
        self.call(|reply| Request::LatestSession {
            workspace_identity,
            reply,
        })
        .await
    }

    pub(crate) async fn list_sessions(
        &self,
        workspace_identity: (u64, u64),
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

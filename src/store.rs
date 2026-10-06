use crate::session::{
    Event, EventEnvelope, ReplayError, RunId, SessionId, SessionListItem, SessionView,
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
use replay::{latest_session_id, list_session_items, load_session, resolve_view, session_for_run};
pub(crate) use state::SessionRunLock;
pub use state::StateRoot;

const DATABASE_FILE: &str = "events.sqlite3";
pub(crate) const MAX_EVENT_BYTES: usize = 64 * 1024;
fn event_payload_limit(kind: &str, version: i64) -> usize {
    if kind == "MessageAccepted" && version == 2 {
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
    #[error("account replacement is already active")]
    AccountBusy,
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
                let connection = open_connection(&root, read_only);
                let Ok(mut connection) = connection else {
                    let _ = ready_sender.send(connection.map(|_| ()));
                    return;
                };
                if ready_sender.send(Ok(())).is_err() {
                    return;
                }
                let mut hot_session = None::<HotSession>;
                while let Some(request) = receiver.blocking_recv() {
                    match request {
                        Request::AdmitOperation {
                            session_id,
                            event_slots,
                            reply,
                        } => {
                            let result = if read_only {
                                Err(StoreError::ReadOnly)
                            } else {
                                journal::admit_operation(&mut connection, session_id, event_slots)
                            };
                            let _ = reply.send(result);
                        }
                        Request::Append {
                            session_id,
                            event,
                            reply,
                        } => {
                            let result = if read_only {
                                Err(StoreError::ReadOnly)
                            } else if matches!(event.as_ref(), Event::SessionForked { .. }) {
                                Err(StoreError::InvalidHistory)
                            } else {
                                append(&mut connection, &mut hot_session, session_id, *event)
                            };
                            let _ = reply.send(result);
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
        self.call(|reply| Request::AdmitOperation {
            session_id,
            event_slots,
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

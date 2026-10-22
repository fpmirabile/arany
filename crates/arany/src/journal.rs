use crate::session::{Event, EventEnvelope, SessionId};
use crate::store::{Store, StoreError};
pub mod check;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::broadcast::{
    self,
    error::{RecvError, TryRecvError},
};

pub(crate) const TAP_CAPACITY: usize = 256;

/// One item of the ordered committed Event stream.
#[derive(Clone, Debug)]
pub enum Tapped {
    Committed(Arc<EventEnvelope>),
    /// The subscriber fell `missed` Events behind and resynchronizes from a view.
    Resync {
        missed: u64,
    },
}

/// Events appended through the Journal after the subscription, in commit order. A fork commits
/// inside the Store and is not tapped, and an append whose acknowledgment fails or is dropped is
/// never sent and leaves a gap without `Resync`; a subscriber that needs completeness reloads the view.
#[derive(Debug)]
pub struct Subscription(broadcast::Receiver<Arc<EventEnvelope>>);

impl Subscription {
    /// `None` once the Journal is closed and every committed Event was received.
    pub async fn next(&mut self) -> Option<Tapped> {
        tapped(self.0.recv().await)
    }

    fn blocking_next(&mut self) -> Option<Tapped> {
        tapped(self.0.blocking_recv())
    }

    pub(crate) async fn follow(
        mut self,
        end: impl Future<Output = ()>,
        mut observe: impl FnMut(Tapped),
    ) {
        tokio::pin!(end);
        loop {
            tokio::select! {
                tapped = self.next() => match tapped {
                    Some(tapped) => observe(tapped),
                    None => return,
                },
                () = &mut end => break,
            }
        }
        // Appends tap synchronously, so every Event committed before the end is already queued.
        loop {
            match self.0.try_recv() {
                Ok(envelope) => observe(Tapped::Committed(envelope)),
                Err(TryRecvError::Lagged(missed)) => observe(Tapped::Resync { missed }),
                Err(TryRecvError::Empty | TryRecvError::Closed) => return,
            }
        }
    }
}

fn tapped(received: Result<Arc<EventEnvelope>, RecvError>) -> Option<Tapped> {
    match received {
        Ok(envelope) => Some(Tapped::Committed(envelope)),
        Err(RecvError::Lagged(missed)) => Some(Tapped::Resync { missed }),
        Err(RecvError::Closed) => None,
    }
}

pub(crate) struct Journal {
    store: Store,
    tap: broadcast::Sender<Arc<EventEnvelope>>,
    diagnostics: Option<std::thread::JoinHandle<()>>,
}

impl Journal {
    pub(crate) fn new(store: Store) -> Self {
        let (tap, _) = broadcast::channel(TAP_CAPACITY);
        let diagnostics = crate::diagnostics::enabled()
            .then(|| {
                let mut subscription = Subscription(tap.subscribe());
                std::thread::Builder::new()
                    .name("arany-diagnostics".into())
                    .spawn(move || {
                        while let Some(tapped) = subscription.blocking_next() {
                            match tapped {
                                Tapped::Committed(envelope) => {
                                    crate::diagnostics::event_failure(&envelope.event);
                                }
                                Tapped::Resync { missed } => crate::diagnostics::tap_resync(missed),
                            }
                        }
                    })
                    .ok()
            })
            .flatten();
        Self {
            store,
            tap,
            diagnostics,
        }
    }

    pub(crate) fn store(&self) -> &Store {
        &self.store
    }

    pub(crate) fn subscribe(&self) -> Subscription {
        Subscription(self.tap.subscribe())
    }

    pub(crate) async fn append(
        &self,
        session_id: SessionId,
        event: Event,
    ) -> Result<Arc<EventEnvelope>, StoreError> {
        Ok(self.append_then(session_id, event, |_| ()).await?.0)
    }

    // Feed the committed Event to the ledger before any subscriber can observe it.
    pub(crate) async fn append_then<T>(
        &self,
        session_id: SessionId,
        event: Event,
        first: impl FnOnce(&EventEnvelope) -> T,
    ) -> Result<(Arc<EventEnvelope>, T), StoreError> {
        let envelope = Arc::new(self.store.append(session_id, event).await?);
        let read = first(&envelope);
        // Without subscribers the send fails; a full ring overwrites its oldest Event.
        let _ = self.tap.send(Arc::clone(&envelope));
        Ok((envelope, read))
    }

    pub(crate) async fn admit(
        &self,
        session_id: SessionId,
        event_slots: usize,
        tool_observations: usize,
    ) -> Result<(), StoreError> {
        self.store
            .admit_operation(session_id, event_slots, tool_observations)
            .await
    }

    pub(crate) async fn admit_sealed_data(
        &self,
        session_id: SessionId,
        event_slots: usize,
        tool_observations: usize,
    ) -> Result<(), StoreError> {
        self.store
            .admit_sealed_data(session_id, event_slots, tool_observations)
            .await
    }

    pub(crate) async fn admit_run_start(
        &self,
        session_id: SessionId,
        event_slots: usize,
    ) -> Result<(), StoreError> {
        self.store.admit_run_start(session_id, event_slots).await
    }

    pub(crate) async fn close(self) -> Result<(), StoreError> {
        let Self {
            store,
            tap,
            diagnostics,
        } = self;
        drop(tap);
        let closed = store.close().await;
        if let Some(diagnostics) = diagnostics {
            let _ = diagnostics.join();
        }
        closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::WorkspaceIdentity;
    use crate::store::StateRoot;

    #[tokio::test]
    async fn lagging_subscriber_resyncs_from_the_view_without_blocking_appends() {
        let temp = tempfile::tempdir().expect("temporary root");
        let state = StateRoot::admit(&temp.path().join("state")).expect("private State");
        let journal = Journal::new(Store::open(state).expect("Store"));
        let session_id = SessionId::new();
        let mut follower = journal.subscribe();
        journal
            .append(
                session_id,
                Event::SessionStarted {
                    title: "Tap".into(),
                    workspace_identity: WorkspaceIdentity {
                        device: 1,
                        inode: 2,
                    },
                },
            )
            .await
            .expect("Session start");
        let renames = TAP_CAPACITY + 44;
        for index in 0..renames {
            // A subscriber that never reads cannot hold an append back.
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                journal.append(
                    session_id,
                    Event::SessionRenamed {
                        title: format!("Tap {index}"),
                    },
                ),
            )
            .await
            .expect("append is not blocked by the subscriber")
            .expect("rename");
        }
        let committed = u64::try_from(renames + 1).expect("bounded Event count");
        let missed = committed - TAP_CAPACITY as u64;
        assert!(matches!(
            follower.next().await,
            Some(Tapped::Resync { missed: lagged }) if lagged == missed
        ));
        let view = journal
            .store()
            .load_view(session_id)
            .await
            .expect("view")
            .expect("Session");
        assert_eq!(view.title, format!("Tap {}", renames - 1));
        let mut retained = Vec::new();
        journal.close().await.expect("close");
        while let Some(tapped) = follower.next().await {
            match tapped {
                Tapped::Committed(envelope) => retained.push(envelope.sequence),
                Tapped::Resync { .. } => panic!("one lag yields one Resync"),
            }
        }
        assert_eq!(retained.len(), TAP_CAPACITY);
        assert_eq!(retained.last(), Some(&view.last_sequence));
        assert!(retained.windows(2).all(|pair| pair[1] == pair[0] + 1));
        assert!(
            retained
                .iter()
                .all(|sequence| *sequence <= view.last_sequence),
            "the view already covers the retained Events, so the resync loses none"
        );
    }
}

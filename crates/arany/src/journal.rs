use crate::session::{Event, EventEnvelope, SessionId};
use crate::store::{AdmissionKind, Store, StoreError};
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
    writer: tokio::sync::Mutex<()>,
    tap: broadcast::Sender<Arc<EventEnvelope>>,
    diagnostics: Option<std::thread::JoinHandle<()>>,
    pub(crate) tool_latency: crate::fault::LatencyRecorder,
}

pub(crate) struct JournalWriter<'a> {
    journal: &'a Journal,
    _guard: tokio::sync::MutexGuard<'a, ()>,
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
            writer: tokio::sync::Mutex::new(()),
            tap,
            diagnostics,
            tool_latency: Default::default(),
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
        self.writer()
            .await
            .append_then(session_id, event, first)
            .await
    }

    pub(crate) async fn writer(&self) -> JournalWriter<'_> {
        JournalWriter {
            journal: self,
            _guard: self.writer.lock().await,
        }
    }

    pub(crate) async fn admit(
        &self,
        session_id: SessionId,
        event_slots: usize,
        tool_observations: usize,
    ) -> Result<(), StoreError> {
        self.admit_as(
            session_id,
            event_slots,
            tool_observations,
            AdmissionKind::Operation,
        )
        .await
    }

    pub(crate) async fn admit_as(
        &self,
        session_id: SessionId,
        event_slots: usize,
        tool_observations: usize,
        kind: AdmissionKind,
    ) -> Result<(), StoreError> {
        self.store
            .admit(session_id, event_slots, tool_observations, kind)
            .await
    }

    pub(crate) async fn close(self) -> Result<(), StoreError> {
        let Self {
            store,
            writer: _,
            tap,
            diagnostics,
            tool_latency: _,
        } = self;
        drop(tap);
        let closed = store.close().await;
        if let Some(diagnostics) = diagnostics {
            let _ = diagnostics.join();
        }
        closed
    }
}

impl JournalWriter<'_> {
    pub(crate) async fn append_then<T>(
        &self,
        session_id: SessionId,
        event: Event,
        first: impl FnOnce(&EventEnvelope) -> T,
    ) -> Result<(Arc<EventEnvelope>, T), StoreError> {
        let envelope = self.journal.store.append(session_id, event).await?;
        crate::fault::commit_latency(&self.journal.tool_latency, &envelope);
        let envelope = Arc::new(envelope);
        let read = first(&envelope);
        // Without subscribers the send fails; a full ring overwrites its oldest Event.
        let _ = self.journal.tap.send(Arc::clone(&envelope));
        Ok((envelope, read))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::WorkspaceIdentity;
    use crate::store::StateRoot;
    use std::{sync::Mutex, task::Poll};

    const APPEND_TEST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

    async fn session_journal(root: &std::path::Path) -> (Journal, SessionId, Subscription) {
        let state = StateRoot::admit(&root.join("state")).expect("private State");
        let journal = Journal::new(Store::open(state).expect("Store"));
        let session_id = SessionId::new();
        let follower = journal.subscribe();
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
        (journal, session_id, follower)
    }

    #[tokio::test]
    async fn suspended_append_callers_keep_reducer_feeds_and_taps_in_commit_order() {
        let _deadline = crate::test_support::engine::deadline(
            "ordered append acknowledgments",
            APPEND_TEST_DEADLINE,
        );
        let temp = tempfile::tempdir().unwrap();
        let (journal, session, mut follower) = session_journal(temp.path()).await;
        assert!(matches!(follower.next().await, Some(Tapped::Committed(_))));
        let view = Arc::new(Mutex::new(
            journal.store().load_view(session).await.unwrap().unwrap(),
        ));
        let (first, second) = {
            let first_view = Arc::clone(&view);
            let (entered, release) = crate::FaultPlan::gate_goal_append_acknowledgment(
                session,
                |event| matches!(event, Event::SessionRenamed { title } if title == "First"),
            );
            let first = journal.append_then(
                session,
                Event::SessionRenamed {
                    title: "First".into(),
                },
                move |event| first_view.lock().unwrap().apply_committed(event),
            );
            tokio::pin!(first);
            std::future::poll_fn(|cx| {
                assert!(first.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            entered.await.unwrap();
            let second_view = Arc::clone(&view);
            let second = journal.append_then(
                session,
                Event::SessionRenamed {
                    title: "Second".into(),
                },
                move |event| second_view.lock().unwrap().apply_committed(event),
            );
            tokio::pin!(second);
            std::future::poll_fn(|cx| {
                assert!(second.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            release.send(()).unwrap();
            journal.store().load_view(session).await.unwrap();
            let (second, first) = tokio::join!(biased; second, first);
            let (first, first_feed) = first.unwrap();
            let (second, second_feed) = second.unwrap();
            first_feed.unwrap();
            second_feed.unwrap();
            (first, second)
        };
        for expected in [first.sequence, second.sequence] {
            let Some(Tapped::Committed(event)) = follower.next().await else {
                panic!("ordered committed Event");
            };
            assert_eq!(event.sequence, expected);
            assert!(view.lock().unwrap().last_sequence >= event.sequence);
        }
        assert_eq!(view.lock().unwrap().title, "Second");
        journal.close().await.unwrap();
    }

    #[tokio::test]
    async fn writer_keeps_follow_up_facts_adjacent_to_their_source() {
        let _deadline =
            crate::test_support::engine::deadline("journal fact group", APPEND_TEST_DEADLINE);
        let temp = tempfile::tempdir().unwrap();
        let (journal, session, _) = session_journal(temp.path()).await;
        {
            let writer = journal.writer().await;
            let (source, ()) = writer
                .append_then(
                    session,
                    Event::SessionRenamed {
                        title: "Source".into(),
                    },
                    |_| (),
                )
                .await
                .unwrap();
            let competing = journal.append(
                session,
                Event::SessionRenamed {
                    title: "Competing".into(),
                },
            );
            tokio::pin!(competing);
            std::future::poll_fn(|cx| {
                assert!(competing.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            let (follow_up, ()) = writer
                .append_then(
                    session,
                    Event::SessionRenamed {
                        title: "Follow-up".into(),
                    },
                    |_| (),
                )
                .await
                .unwrap();
            drop(writer);
            let competing = competing.await.unwrap();
            assert_eq!(follow_up.sequence, source.sequence + 1);
            assert_eq!(competing.sequence, follow_up.sequence + 1);
        }
        journal.close().await.unwrap();
    }

    #[tokio::test]
    async fn lagging_subscriber_resyncs_from_the_view_without_blocking_appends() {
        let temp = tempfile::tempdir().expect("temporary root");
        let (journal, session_id, mut follower) = session_journal(temp.path()).await;
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

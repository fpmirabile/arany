use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChildSteeringTarget {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub agent_run_id: AgentRunId,
}

#[derive(Clone, Default)]
pub struct RunSteering {
    state: Arc<Mutex<SteeringState>>,
    pause: Arc<AtomicBool>,
    pause_signal: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct SteeringState {
    waiting: Option<String>,
    run: Option<(SessionId, RunId)>,
    children: Vec<ChildSteeringTarget>,
    child_waiting: Vec<(ChildSteeringTarget, String)>,
    run_bytes: usize,
    recorded: u64,
}

const NOTE_SEPARATOR: &str = "\n\n";
const MAX_STEERING_CHILDREN: usize = 8;

impl SteeringState {
    fn pending_bytes(&self) -> usize {
        self.waiting
            .as_ref()
            .map_or(0, |text| text.len() + MAX_STEERING_LABEL_BYTES)
            + self
                .child_waiting
                .iter()
                .filter(|(target, _)| self.run == Some((target.session_id, target.run_id)))
                .map(|(_, text)| text.len() + MAX_CHILD_STEERING_LABEL_BYTES)
                .sum::<usize>()
    }

    fn child_available(&self, target: ChildSteeringTarget) -> bool {
        self.run == Some((target.session_id, target.run_id)) && self.children.contains(&target)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SteeringRefused {
    RunFull,
    Empty,
    TooLarge,
    Unavailable,
    ChildLimit,
}

impl RunSteering {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Option<MutexGuard<'_, SteeringState>> {
        self.state.lock().ok()
    }

    /// Holds a note until the Engine records it or the caller withdraws it. A note offered while
    /// another waits is appended to it, so the Engine delivers one merged note. Recorded text plus
    /// one history label per note, and the waiting note, stay within this Run's byte bound.
    pub fn offer_note(&self, text: String) -> Result<(), SteeringRefused> {
        let mut state = self.lock().ok_or(SteeringRefused::Unavailable)?;
        if text.trim().is_empty() {
            return Err(SteeringRefused::Empty);
        }
        if text.len() + MAX_STEERING_LABEL_BYTES > MAX_STEERING_NOTE_BYTES {
            return Err(SteeringRefused::TooLarge);
        }
        let merged_len = state
            .waiting
            .as_ref()
            .map_or(0, |waiting| waiting.len() + NOTE_SEPARATOR.len())
            + text.len();
        if state.run_bytes + state.pending_bytes()
            - state
                .waiting
                .as_ref()
                .map_or(0, |text| text.len() + MAX_STEERING_LABEL_BYTES)
            + merged_len
            + MAX_STEERING_LABEL_BYTES
            > MAX_STEERING_NOTE_BYTES
        {
            return Err(SteeringRefused::RunFull);
        }
        match state.waiting.as_mut() {
            Some(waiting) => {
                waiting.push_str(NOTE_SEPARATOR);
                waiting.push_str(&text);
            }
            None => state.waiting = Some(text),
        }
        Ok(())
    }

    pub fn offer_child_note(
        &self,
        target: ChildSteeringTarget,
        text: String,
    ) -> Result<(), SteeringRefused> {
        let mut state = self.lock().ok_or(SteeringRefused::Unavailable)?;
        if self.pause_requested() {
            return Err(SteeringRefused::Unavailable);
        }
        if !state.child_available(target) {
            return Err(
                if state.run == Some((target.session_id, target.run_id))
                    && state.children.len() >= MAX_STEERING_CHILDREN
                {
                    SteeringRefused::ChildLimit
                } else {
                    SteeringRefused::Unavailable
                },
            );
        }
        if text.trim().is_empty() {
            return Err(SteeringRefused::Empty);
        }
        if text.len() + MAX_CHILD_STEERING_LABEL_BYTES > MAX_STEERING_NOTE_BYTES {
            return Err(SteeringRefused::TooLarge);
        }
        let index = state.child_waiting.iter().position(|(id, _)| *id == target);
        let active = state
            .child_waiting
            .iter()
            .filter(|(id, _)| state.run == Some((id.session_id, id.run_id)))
            .count();
        if index.is_none() && active >= MAX_STEERING_CHILDREN {
            return Err(SteeringRefused::Unavailable);
        }
        let cost = text.len()
            + if index.is_some() {
                NOTE_SEPARATOR.len()
            } else {
                MAX_CHILD_STEERING_LABEL_BYTES
            };
        if state.run_bytes + state.pending_bytes() + cost > MAX_STEERING_NOTE_BYTES {
            return Err(SteeringRefused::RunFull);
        }
        if let Some(index) = index {
            state.child_waiting[index].1.push_str(NOTE_SEPARATOR);
            state.child_waiting[index].1.push_str(&text);
        } else {
            state.child_waiting.push((target, text));
        }
        Ok(())
    }

    pub fn withdraw_child_note(&self, target: ChildSteeringTarget) -> Option<String> {
        let mut state = self.lock()?;
        let index = state
            .child_waiting
            .iter()
            .position(|(id, _)| *id == target)?;
        Some(state.child_waiting.remove(index).1)
    }

    pub(crate) fn take_child_note(&self, target: ChildSteeringTarget) -> Option<String> {
        let mut state = self.lock()?;
        if self.pause_requested() || !state.child_available(target) {
            return None;
        }
        let index = state
            .child_waiting
            .iter()
            .position(|(id, _)| *id == target)?;
        let text = state.child_waiting.remove(index).1;
        state.run_bytes += text.len() + MAX_CHILD_STEERING_LABEL_BYTES;
        Some(text)
    }

    pub(super) fn register_child(&self, target: ChildSteeringTarget) {
        if let Some(mut state) = self.lock()
            && state.run == Some((target.session_id, target.run_id))
            && state.children.len() < MAX_STEERING_CHILDREN
            && !state.children.contains(&target)
        {
            state.children.push(target);
        }
    }

    pub(super) fn close_child(&self, target: ChildSteeringTarget) {
        if let Some(mut state) = self.lock() {
            state.children.retain(|id| *id != target);
        }
    }

    pub(super) fn end_run(&self, session: SessionId, run: RunId) {
        if let Some(mut state) = self.lock()
            && state.run == Some((session, run))
        {
            state.run = None;
            state.children.clear();
        }
    }

    /// Removes the waiting note before the Engine took it and returns its full merged text.
    pub fn withdraw_note(&self) -> Option<String> {
        self.lock().and_then(|mut state| state.waiting.take())
    }

    pub fn delivered_notes(&self) -> u64 {
        self.lock().map_or(0, |state| state.recorded)
    }

    /// Returns true only for the first pause request.
    pub fn pause(&self) -> bool {
        let first = !self.pause.swap(true, Ordering::SeqCst);
        if first {
            self.pause_signal.notify_waiters();
        }
        first
    }

    pub(crate) fn pause_requested(&self) -> bool {
        self.pause.load(Ordering::SeqCst)
    }

    // Resolves once a pause is requested; the waiter registers before it reads the flag.
    pub(crate) async fn paused(&self) {
        let notified = self.pause_signal.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.pause_requested() {
            notified.await;
        }
    }

    pub(crate) fn begin_run(&self, session: SessionId, run: RunId) {
        if let Some(mut state) = self.lock() {
            state.run_bytes = 0;
            state.run = Some((session, run));
            state.children.clear();
        }
    }

    pub(crate) fn take_note(&self) -> Option<String> {
        let mut state = self.lock()?;
        let fits = state.waiting.as_ref().is_some_and(|text| {
            state.run_bytes + text.len() + MAX_STEERING_LABEL_BYTES <= MAX_STEERING_NOTE_BYTES
        });
        if !fits {
            return None;
        }
        let text = state.waiting.take()?;
        state.run_bytes += text.len() + MAX_STEERING_LABEL_BYTES;
        Some(text)
    }

    pub(crate) fn note_recorded(&self) {
        if let Some(mut state) = self.lock() {
            state.recorded += 1;
        }
    }
}

#[cfg(test)]
mod run_steering_tests {
    use super::*;

    #[test]
    fn run_steering_merges_waiting_notes_and_bounds_the_run() {
        let steering = RunSteering::new();
        assert_eq!(
            steering.offer_note(" \n".into()),
            Err(SteeringRefused::Empty)
        );
        assert_eq!(
            steering.offer_note("x".repeat(MAX_STEERING_NOTE_BYTES + 1)),
            Err(SteeringRefused::TooLarge)
        );
        // A note that can never fit with its label is TooLarge, not a full Run.
        assert_eq!(
            steering.offer_note("x".repeat(MAX_STEERING_NOTE_BYTES - MAX_STEERING_LABEL_BYTES + 1)),
            Err(SteeringRefused::TooLarge)
        );
        steering
            .offer_note("x".repeat(MAX_STEERING_NOTE_BYTES - MAX_STEERING_LABEL_BYTES))
            .expect("the largest single note fits an empty Run");
        assert!(steering.withdraw_note().is_some());
        steering.offer_note("first".into()).expect("first note");
        steering.offer_note("second".into()).expect("merged note");
        assert_eq!(
            steering.withdraw_note().as_deref(),
            Some("first\n\nsecond"),
            "withdrawal returns the whole merged text"
        );
        assert!(
            steering.withdraw_note().is_none(),
            "nothing is waiting twice"
        );

        steering.offer_note("one".into()).expect("note offered");
        steering.offer_note("two".into()).expect("note merged");
        assert_eq!(steering.take_note().as_deref(), Some("one\n\ntwo"));
        assert!(
            steering.withdraw_note().is_none(),
            "a note the Engine took cannot be withdrawn"
        );
        assert_eq!(steering.delivered_notes(), 0);
        steering.note_recorded();
        assert_eq!(steering.delivered_notes(), 1);

        let taken =
            "x".repeat(MAX_STEERING_NOTE_BYTES - "one\n\ntwo".len() - 2 * MAX_STEERING_LABEL_BYTES);
        steering
            .offer_note(taken.clone())
            .expect("fills the Run bound");
        assert_eq!(steering.take_note(), Some(taken));
        steering.note_recorded();
        assert_eq!(
            steering.offer_note("y".into()),
            Err(SteeringRefused::RunFull)
        );
        steering.begin_run(SessionId::new(), RunId::new());
        steering
            .offer_note("x".repeat(MAX_STEERING_NOTE_BYTES - MAX_STEERING_LABEL_BYTES))
            .expect("a new Run has its own byte quota");
        assert!(steering.take_note().is_some());
        steering.note_recorded();
        assert_eq!(
            steering.offer_note("y".into()),
            Err(SteeringRefused::RunFull)
        );

        assert!(steering.pause());
        assert!(!steering.pause(), "only the first pause request is new");
        assert!(steering.pause_requested());
    }

    #[test]
    fn run_steering_charges_a_label_per_note_and_refuses_the_next_tiny_note() {
        let steering = RunSteering::new();
        let mut taken = 0;
        while steering.offer_note("n".into()).is_ok() {
            assert!(steering.take_note().is_some());
            taken += 1;
        }
        assert_eq!(
            taken,
            MAX_STEERING_NOTE_BYTES / (1 + MAX_STEERING_LABEL_BYTES)
        );
        assert_eq!(
            steering.offer_note("n".into()),
            Err(SteeringRefused::RunFull),
            "the next one-byte note exceeds the bound with its label"
        );
    }

    #[test]
    fn child_steering_is_targeted_and_shares_the_run_quota() {
        let steering = RunSteering::new();
        let (session_id, run_id) = (SessionId::new(), RunId::new());
        let a = ChildSteeringTarget {
            session_id,
            run_id,
            agent_run_id: AgentRunId::new(),
        };
        let b = ChildSteeringTarget {
            agent_run_id: AgentRunId::new(),
            ..a
        };
        assert_eq!(
            steering.offer_child_note(a, "x".into()),
            Err(SteeringRefused::Unavailable)
        );
        steering.begin_run(session_id, run_id);
        for target in [a, b] {
            steering.register_child(target);
        }
        steering.offer_child_note(a, "one".into()).unwrap();
        steering.offer_child_note(a, "two".into()).unwrap();
        steering.offer_child_note(b, "sibling".into()).unwrap();
        steering.offer_note("primary".into()).unwrap();
        assert_eq!(steering.take_child_note(a).as_deref(), Some("one\n\ntwo"));
        assert_eq!(steering.delivered_notes(), 0);
        let spent = "one\n\ntwo".len() + MAX_CHILD_STEERING_LABEL_BYTES;
        let pending = "sibling".len()
            + MAX_CHILD_STEERING_LABEL_BYTES
            + "primary".len()
            + MAX_STEERING_LABEL_BYTES;
        let remaining = MAX_STEERING_NOTE_BYTES - spent - pending;
        steering
            .offer_child_note(a, "x".repeat(remaining - MAX_CHILD_STEERING_LABEL_BYTES))
            .unwrap();
        assert_eq!(
            steering.offer_note("y".into()),
            Err(SteeringRefused::RunFull)
        );
        assert!(steering.take_child_note(a).is_some());
        assert_eq!(steering.take_note().as_deref(), Some("primary"));
        steering.close_child(b);
        assert!(steering.take_child_note(b).is_none());
        assert_eq!(steering.withdraw_child_note(b).as_deref(), Some("sibling"));
        steering.begin_run(session_id, RunId::new());
        assert_eq!(
            steering.offer_child_note(a, "stale".into()),
            Err(SteeringRefused::Unavailable)
        );
        assert!(steering.take_child_note(a).is_none());
        let targets: Vec<_> = (0..8)
            .map(|_| ChildSteeringTarget {
                run_id: steering.lock().unwrap().run.unwrap().1,
                agent_run_id: AgentRunId::new(),
                ..a
            })
            .collect();
        for target in &targets {
            steering.register_child(*target);
            steering
                .offer_child_note(*target, "pending".into())
                .unwrap();
        }
        for target in &targets {
            steering.close_child(*target);
        }
        let ninth = ChildSteeringTarget {
            agent_run_id: AgentRunId::new(),
            ..targets[0]
        };
        steering.register_child(ninth);
        assert_eq!(
            steering.offer_child_note(ninth, "new".into()),
            Err(SteeringRefused::Unavailable)
        );
        assert_eq!(
            steering.withdraw_child_note(targets[0]).as_deref(),
            Some("pending")
        );
        steering.offer_child_note(ninth, "new".into()).unwrap();
        let poisoned = steering.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.state.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert_eq!(
            steering.offer_child_note(a, "x".into()),
            Err(SteeringRefused::Unavailable)
        );
        assert!(steering.withdraw_child_note(a).is_none());
    }

    #[test]
    fn run_steering_take_reserves_quota_and_poisoned_lock_fails_closed() {
        let steering = RunSteering::new();
        steering
            .offer_note("x".repeat(MAX_STEERING_NOTE_BYTES - MAX_STEERING_LABEL_BYTES))
            .expect("within quota");
        assert!(steering.take_note().is_some());
        assert_eq!(
            steering.offer_note("unrecorded takes still count".into()),
            Err(SteeringRefused::RunFull)
        );

        let poisoned = RunSteering::new();
        let handle = poisoned.clone();
        let _ = std::thread::spawn(move || {
            let _guard = handle.state.lock().expect("unpoisoned");
            panic!("poison the steering lock");
        })
        .join();
        assert_eq!(
            poisoned.offer_note("x".into()),
            Err(SteeringRefused::Unavailable)
        );
        assert!(poisoned.take_note().is_none());
        assert_eq!(poisoned.delivered_notes(), 0);
    }

    #[test]
    fn a_ninth_active_child_gets_its_own_refusal() {
        let steering = RunSteering::new();
        let (session_id, run_id) = (SessionId::new(), RunId::new());
        steering.begin_run(session_id, run_id);
        let target = |_| ChildSteeringTarget {
            session_id,
            run_id,
            agent_run_id: crate::session::AgentRunId::new(),
        };
        let children: Vec<_> = (0..9).map(target).collect();
        for child in &children {
            steering.register_child(*child);
        }
        assert_eq!(steering.offer_child_note(children[7], "ok".into()), Ok(()));
        assert_eq!(
            steering.offer_child_note(children[8], "no".into()),
            Err(SteeringRefused::ChildLimit)
        );
    }

    #[test]
    fn stale_run_notes_do_not_count_against_the_active_run() {
        let steering = RunSteering::new();
        let session_id = SessionId::new();
        let first = RunId::new();
        steering.begin_run(session_id, first);
        let target = |run_id| ChildSteeringTarget {
            session_id,
            run_id,
            agent_run_id: crate::session::AgentRunId::new(),
        };
        let stale: Vec<_> = (0..8).map(|_| target(first)).collect();
        for child in &stale {
            steering.register_child(*child);
            steering.offer_child_note(*child, "x".repeat(900)).unwrap();
        }
        let second = RunId::new();
        steering.begin_run(session_id, second);
        let fresh = target(second);
        steering.register_child(fresh);
        assert_eq!(steering.offer_child_note(fresh, "x".repeat(4096)), Ok(()));
        assert!(steering.withdraw_child_note(stale[0]).is_some());
    }
}

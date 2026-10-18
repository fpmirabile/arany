use crate::session::{Event, SessionId};
#[cfg(test)]
use crate::store::BuildCacheIdentity;
use std::io;
use std::sync::Mutex;
use tokio::sync::oneshot;

type Admits = fn(&Event) -> bool;

enum Fault {
    Checkpoint(SessionId, oneshot::Sender<()>, oneshot::Receiver<()>),
    CheckpointStall(SessionId),
    SupervisorOff(SessionId),
    WorkerReceipt(uuid::Uuid),
    GoalAppend(SessionId, Admits),
    GoalAcknowledgment(
        SessionId,
        Admits,
        oneshot::Sender<()>,
        std::sync::mpsc::Receiver<()>,
    ),
    #[cfg_attr(not(test), expect(dead_code, reason = "read only by test fixtures"))]
    CandidateSync(String),
    #[cfg(test)]
    CandidateSyncGate(
        String,
        oneshot::Sender<()>,
        std::sync::mpsc::Receiver<()>,
        oneshot::Sender<()>,
    ),
    #[cfg_attr(not(test), expect(dead_code, reason = "read only by test fixtures"))]
    CommitMarker(String),
}

// Faults use Session, Tool intent or cache identity to keep fixtures isolated.
static PLAN: Mutex<Vec<Fault>> = Mutex::new(Vec::new());

fn plan() -> std::sync::MutexGuard<'static, Vec<Fault>> {
    PLAN.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn take(mut matches: impl FnMut(&Fault) -> bool) -> Option<Fault> {
    let mut plan = plan();
    let index = plan.iter().position(&mut matches)?;
    Some(plan.remove(index))
}

pub struct FaultPlan;

impl FaultPlan {
    pub fn gate_checkpoint(session: SessionId) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, wait) = oneshot::channel();
        let (release, released) = oneshot::channel();
        plan().push(Fault::Checkpoint(session, entered, released));
        (wait, release)
    }

    pub fn stall_checkpoint(session: SessionId) {
        plan().push(Fault::CheckpointStall(session));
    }

    pub(crate) fn checkpoint_stalled(session: SessionId) -> bool {
        take(|fault| matches!(fault, Fault::CheckpointStall(id) if *id == session)).is_some()
    }

    pub(crate) fn checkpoint_gate(
        session: SessionId,
    ) -> Option<(oneshot::Sender<()>, oneshot::Receiver<()>)> {
        match take(|fault| matches!(fault, Fault::Checkpoint(id, ..) if *id == session)) {
            Some(Fault::Checkpoint(_, entered, release)) => Some((entered, release)),
            _ => None,
        }
    }

    pub fn supervisor_off(session_id: SessionId) {
        plan().push(Fault::SupervisorOff(session_id));
    }

    pub(crate) fn take_supervisor_off(session_id: SessionId) -> bool {
        take(|fault| matches!(fault, Fault::SupervisorOff(id) if *id == session_id)).is_some()
    }

    /// Fails the next Event that `admits` accepts for the Session before it commits.
    pub fn fail_goal_append(session_id: SessionId, admits: Admits) {
        plan().push(Fault::GoalAppend(session_id, admits));
    }

    /// Holds the acknowledgment of the next committed Event that `admits` accepts. Dropping the
    /// release sender fails that acknowledgment after the commit.
    pub fn gate_goal_append_acknowledgment(
        session_id: SessionId,
        admits: Admits,
    ) -> (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered, wait_entered) = oneshot::channel();
        let (release, wait_release) = std::sync::mpsc::channel();
        plan().push(Fault::GoalAcknowledgment(
            session_id,
            admits,
            entered,
            wait_release,
        ));
        (wait_entered, release)
    }

    #[cfg(test)]
    pub(crate) fn fail_candidate_sync(identity: &BuildCacheIdentity) {
        plan().push(Fault::CandidateSync(identity.token()));
    }

    #[cfg(test)]
    pub(crate) fn gate_candidate_sync(
        identity: &BuildCacheIdentity,
    ) -> (
        oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
        oneshot::Receiver<()>,
    ) {
        let (entered, wait_entered) = oneshot::channel();
        let (release, wait_release) = std::sync::mpsc::channel();
        let (finished, wait_finished) = oneshot::channel();
        plan().push(Fault::CandidateSyncGate(
            identity.token(),
            entered,
            wait_release,
            finished,
        ));
        (wait_entered, release, wait_finished)
    }

    #[cfg(test)]
    pub(crate) fn fail_commit_marker(identity: &BuildCacheIdentity) {
        plan().push(Fault::CommitMarker(identity.token()));
    }

    pub fn lose_guard_receipt(intent: uuid::Uuid) {
        plan().push(Fault::WorkerReceipt(intent));
    }

    pub(crate) fn worker_receipt_lost(intent: uuid::Uuid) -> bool {
        take(|fault| matches!(fault, Fault::WorkerReceipt(id) if *id == intent)).is_some()
    }

    pub(crate) fn append_fails(session_id: SessionId, event: &Event) -> bool {
        take(|fault| {
            matches!(fault, Fault::GoalAppend(session, admits)
                if *session == session_id && admits(event))
        })
        .is_some()
    }

    pub(crate) fn acknowledgment_gate(
        session_id: SessionId,
        event: &Event,
    ) -> Option<(oneshot::Sender<()>, std::sync::mpsc::Receiver<()>)> {
        match take(|fault| {
            matches!(fault, Fault::GoalAcknowledgment(session, admits, ..)
                if *session == session_id && admits(event))
        }) {
            Some(Fault::GoalAcknowledgment(_, _, entered, release)) => Some((entered, release)),
            _ => None,
        }
    }

    pub(crate) fn candidate_sync(token: &str) -> io::Result<Option<oneshot::Sender<()>>> {
        let finished = None;
        #[cfg(test)]
        let finished = if let Some(Fault::CandidateSyncGate(_, entered, release, finished)) =
            take(|fault| matches!(fault, Fault::CandidateSyncGate(key, ..) if key == token))
        {
            let _ = entered.send(());
            release
                .recv()
                .map_err(|_| io::Error::other("candidate sync gate closed"))?;
            Some(finished)
        } else {
            finished
        };
        match take(|fault| matches!(fault, Fault::CandidateSync(key) if key == token)) {
            Some(_) => Err(io::Error::other("injected candidate sync failure")),
            None => Ok(finished),
        }
    }

    pub(crate) fn commit_marker(token: &str) -> io::Result<()> {
        match take(|fault| matches!(fault, Fault::CommitMarker(key) if key == token)) {
            Some(_) => Err(io::Error::other("injected commit marker failure")),
            None => Ok(()),
        }
    }
}

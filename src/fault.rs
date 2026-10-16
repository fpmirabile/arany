use crate::session::{Event, SessionId};
#[cfg(test)]
use crate::store::BuildCacheIdentity;
use std::io;
use std::sync::Mutex;
use tokio::sync::oneshot;

type Admits = fn(&Event) -> bool;

enum Fault {
    GoalAppend(SessionId, Admits),
    GoalAcknowledgment(
        SessionId,
        Admits,
        oneshot::Sender<()>,
        std::sync::mpsc::Receiver<()>,
    ),
    #[cfg_attr(not(test), allow(dead_code))]
    CandidateSync(String),
    #[cfg_attr(not(test), allow(dead_code))]
    CommitMarker(String),
}

// Faults are keyed by Session ID or cache identity so parallel tests never see each other's.
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

    /// Makes the next candidate sync for this cache token fail once.
    #[cfg(test)]
    pub(crate) fn fail_candidate_sync(identity: &BuildCacheIdentity) {
        plan().push(Fault::CandidateSync(identity.token()));
    }

    /// Makes the next committed-marker write for this cache token fail once.
    #[cfg(test)]
    pub(crate) fn fail_commit_marker(identity: &BuildCacheIdentity) {
        plan().push(Fault::CommitMarker(identity.token()));
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

    pub(crate) fn candidate_sync(token: &str) -> io::Result<()> {
        match take(|fault| matches!(fault, Fault::CandidateSync(key) if key == token)) {
            Some(_) => Err(io::Error::other("injected candidate sync failure")),
            None => Ok(()),
        }
    }

    pub(crate) fn commit_marker(token: &str) -> io::Result<()> {
        match take(|fault| matches!(fault, Fault::CommitMarker(key) if key == token)) {
            Some(_) => Err(io::Error::other("injected commit marker failure")),
            None => Ok(()),
        }
    }
}

use crate::provider::{ProviderError, ProviderRequest, ProviderResponse};
use crate::session::{Event, EventEnvelope, RunId, SessionId};
use crate::store::{AdmissionKind, BuildCacheLease, StoreError};
use crate::tools::{ToolError, ToolObservation};
use std::io;
use std::time::Duration;
use tokio::time::Instant;

#[cfg(any(test, feature = "test-support"))]
pub use injection::FaultPlan;

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) fn tool_preflight(session: SessionId, event: &mut Event) {
    #[cfg(any(test, feature = "test-support"))]
    FaultPlan::tool_preflight(session, event);
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) fn append(session: SessionId, event: &Event) -> Result<(), StoreError> {
    #[cfg(any(test, feature = "test-support"))]
    if FaultPlan::append_fails(session, event) {
        return Err(StoreError::Closed);
    }
    Ok(())
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) fn acknowledge(
    session: SessionId,
    result: Result<EventEnvelope, StoreError>,
) -> Result<EventEnvelope, StoreError> {
    #[cfg(any(test, feature = "test-support"))]
    if let Ok(envelope) = &result
        && let Some((entered, release)) = FaultPlan::acknowledgment_gate(session, &envelope.event)
    {
        let _ = entered.send(());
        // A dropped release means the commit was never acknowledged.
        release.recv().map_err(|_| StoreError::Closed)?;
    }
    result
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) async fn acknowledge_check(view: &crate::CheckView) -> Result<(), StoreError> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some((entered, release)) = FaultPlan::check_acknowledgment_gate(view) {
        let _ = entered.send(());
        release.await.map_err(|_| StoreError::Closed)?;
    }
    Ok(())
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) fn record_admission(
    session: SessionId,
    count: usize,
    slots: usize,
    kind: AdmissionKind,
    observations: usize,
) {
    #[cfg(any(test, feature = "test-support"))]
    crate::test_support::admissions::record(session, count, slots, kind, observations);
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) fn record_sync(state: &std::path::Path, record: &str) -> Result<(), StoreError> {
    #[cfg(any(test, feature = "test-support"))]
    if FaultPlan::record_sync_fails(state, record) {
        return Err(StoreError::ReplaceUnconfirmed);
    }
    Ok(())
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) fn record_publication(state: &std::path::Path, record: &str) -> Result<(), StoreError> {
    #[cfg(any(test, feature = "test-support"))]
    if FaultPlan::record_publication_fails(state, record) {
        return Err(StoreError::Io(io::Error::other("record publication fault")));
    }
    Ok(())
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) fn commit_marker(token: &str) -> io::Result<()> {
    #[cfg(any(test, feature = "test-support"))]
    FaultPlan::commit_marker(token)?;
    Ok(())
}

pub(crate) enum CheckpointStall {
    Ready,
    #[cfg(any(test, feature = "test-support"))]
    Stalled,
}

impl CheckpointStall {
    pub(crate) async fn wait(self) {
        match self {
            Self::Ready => {}
            #[cfg(any(test, feature = "test-support"))]
            Self::Stalled => std::future::pending::<()>().await,
        }
    }
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) fn checkpoint_stall(session: SessionId) -> CheckpointStall {
    #[cfg(any(test, feature = "test-support"))]
    if FaultPlan::checkpoint_stalled(session) {
        return CheckpointStall::Stalled;
    }
    CheckpointStall::Ready
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) async fn checkpoint_captured(
    session: SessionId,
    checkpoint: &crate::Checkpoint,
    deadline: Instant,
) {
    #[cfg(any(test, feature = "test-support"))]
    if checkpoint.status == crate::CheckpointStatus::Captured
        && let Some((entered, release)) = FaultPlan::checkpoint_gate(session)
    {
        let _ = entered.send(());
        let _ = tokio::time::timeout_at(deadline, release).await;
    }
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) async fn rewind_checkpoint(session: SessionId, timeout: Duration) {
    #[cfg(any(test, feature = "test-support"))]
    if let Some((entered, release)) = FaultPlan::checkpoint_gate(session) {
        let _ = entered.send(());
        let _ = tokio::time::timeout(timeout, release).await;
    }
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) async fn retry_backoff(session: SessionId, delay: Duration) {
    #[cfg(any(test, feature = "test-support"))]
    match FaultPlan::retry_backoff(session) {
        Some(injection::RetryBackoff::Immediate) => return,
        Some(injection::RetryBackoff::Gated(entered, release)) => {
            let _ = entered.send(());
            let _ = release.await;
            return;
        }
        None => {}
    }
    tokio::time::sleep(delay).await;
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) async fn autonomous_continuation(session: SessionId) {
    #[cfg(any(test, feature = "test-support"))]
    if let Some((entered, release)) = FaultPlan::autonomous_continuation_gate(session) {
        let _ = entered.send(());
        let _ = release.await;
    }
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) fn file_parent_sync(intent: Option<uuid::Uuid>) -> Result<(), ToolError> {
    #[cfg(any(test, feature = "test-support"))]
    if intent.is_some_and(FaultPlan::file_parent_sync_fails) {
        return Err(ToolError::Uncertain);
    }
    Ok(())
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) fn guard_result(
    intent: uuid::Uuid,
    result: Result<ToolObservation, ToolError>,
) -> Result<ToolObservation, ToolError> {
    #[cfg(any(test, feature = "test-support"))]
    if result.is_ok() && FaultPlan::guard_receipt_lost(intent) {
        return Err(ToolError::Uncertain);
    }
    result
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) async fn guard_cache_report(observation: &ToolObservation) {
    #[cfg(any(test, feature = "test-support"))]
    FaultPlan::guard_cache_report_gate(observation).await;
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) fn guard_dispatched(intent: &crate::tools::EffectIntent) {
    #[cfg(any(test, feature = "test-support"))]
    FaultPlan::guard_dispatched(&intent.call);
}

pub(crate) struct CandidateSync {
    #[cfg(any(test, feature = "test-support"))]
    finished: Option<tokio::sync::oneshot::Sender<()>>,
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) fn candidate_sync(lease: &BuildCacheLease) -> io::Result<CandidateSync> {
    Ok(CandidateSync {
        #[cfg(any(test, feature = "test-support"))]
        finished: FaultPlan::candidate_sync(lease.token())?,
    })
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) fn candidate_sync_finished(completion: io::Result<CandidateSync>) {
    #[cfg(any(test, feature = "test-support"))]
    if let Ok(CandidateSync {
        finished: Some(finished),
    }) = completion
    {
        let _ = finished.send(());
    }
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) fn record_tool_failure(
    intent: uuid::Uuid,
    site: &'static str,
    stage: &'static str,
    check: Option<&'static str>,
    detail: Option<&str>,
) {
    #[cfg(any(test, feature = "test-support"))]
    crate::test_support::record_tool_failure(intent, site, stage, check, detail.map(str::to_owned));
}

#[cfg_attr(not(any(test, feature = "test-support")), expect(unused_variables))]
pub(crate) fn supervisor_policy(session: SessionId) -> crate::supervisor::SupervisorPolicy {
    #[cfg(any(test, feature = "test-support"))]
    if FaultPlan::take_supervisor_off(session) {
        return crate::supervisor::SupervisorPolicy::all_off();
    }
    crate::supervisor::SupervisorPolicy::default()
}

#[cfg(feature = "test-support")]
pub(crate) type LatencyRecorder =
    std::sync::OnceLock<std::sync::Arc<crate::test_support::latency::Recorder>>;
#[cfg(not(feature = "test-support"))]
#[derive(Default)]
pub(crate) struct LatencyRecorder;

#[derive(Default)]
pub(crate) struct LatencyReceipt<'a> {
    #[cfg(feature = "test-support")]
    receipt: Option<crate::test_support::latency::Receipt<'a>>,
    #[cfg(not(feature = "test-support"))]
    lifetime: std::marker::PhantomData<&'a ()>,
}

impl<'a> LatencyReceipt<'a> {
    #[cfg_attr(not(feature = "test-support"), expect(unused_variables))]
    pub(crate) fn new(
        recorder: &'a LatencyRecorder,
        request: &ProviderRequest,
        call_index: impl FnOnce() -> usize,
    ) -> Self {
        Self {
            #[cfg(feature = "test-support")]
            receipt: crate::test_support::latency::Receipt::primary(recorder, request, call_index),
            #[cfg(not(feature = "test-support"))]
            lifetime: std::marker::PhantomData,
        }
    }

    #[cfg_attr(not(feature = "test-support"), expect(unused_variables))]
    pub(crate) fn received(
        self,
        result: &Result<Result<ProviderResponse, ProviderError>, tokio::time::error::Elapsed>,
    ) {
        #[cfg(feature = "test-support")]
        if let (Some(receipt), Ok(Ok(response))) = (self.receipt, result) {
            receipt.received(response, std::time::Instant::now());
        }
    }
}

#[cfg_attr(not(feature = "test-support"), expect(unused_variables))]
pub(crate) fn dispose_latency(recorder: &LatencyRecorder, run: RunId) {
    #[cfg(feature = "test-support")]
    if let Some(recorder) = recorder.get() {
        recorder.dispose(run);
    }
}

#[cfg_attr(not(feature = "test-support"), expect(unused_variables))]
pub(crate) fn commit_latency(recorder: &LatencyRecorder, envelope: &EventEnvelope) {
    #[cfg(feature = "test-support")]
    if let Some(recorder) = recorder.get() {
        recorder.committed(envelope, std::time::Instant::now());
    }
}

#[cfg(any(test, feature = "test-support"))]
mod injection {
    use crate::session::{Event, SessionId};
    #[cfg(test)]
    use crate::store::BuildCacheIdentity;
    use std::io;
    use std::sync::Mutex;
    use tokio::sync::oneshot;

    type Admits = fn(&Event) -> bool;

    enum Fault {
        ToolPreflight(SessionId, &'static str, oneshot::Sender<()>),
        CheckAcknowledgment(
            crate::CheckId,
            fn(&crate::CheckView) -> bool,
            oneshot::Sender<()>,
            oneshot::Receiver<()>,
        ),
        Checkpoint(SessionId, oneshot::Sender<()>, oneshot::Receiver<()>),
        AutonomousContinuation(SessionId, oneshot::Sender<()>, oneshot::Receiver<()>),
        CheckpointStall(SessionId),
        SupervisorOff(SessionId),
        GuardReceipt(uuid::Uuid),
        FileParentSync(uuid::Uuid),
        GuardDispatch(crate::tools::ToolCall, oneshot::Sender<()>),
        GuardCacheReport(
            uuid::Uuid,
            oneshot::Sender<crate::tools::ToolObservation>,
            oneshot::Receiver<()>,
        ),
        RecordSync(std::path::PathBuf, &'static str),
        RecordPublication(std::path::PathBuf, &'static str),
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
        // Without a gate the Session's retry backoffs end at once; a gate holds one backoff.
        RetryBackoff(
            SessionId,
            Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
        ),
    }

    pub(crate) enum RetryBackoff {
        Immediate,
        Gated(oneshot::Sender<()>, oneshot::Receiver<()>),
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
        pub fn corrupt_tool_preflight(
            session: SessionId,
            kind: &'static str,
        ) -> oneshot::Receiver<()> {
            let (entered, wait) = oneshot::channel();
            plan().push(Fault::ToolPreflight(session, kind, entered));
            wait
        }

        pub(crate) fn tool_preflight(session: SessionId, event: &mut Event) {
            if let Some(Fault::ToolPreflight(_, _, entered)) = take(
                |fault| matches!(fault, Fault::ToolPreflight(id, kind, _) if *id == session && *kind == event.kind()),
            ) {
                if let Event::ToolProposed { intent, .. } | Event::ToolStarted { intent, .. } =
                    event
                {
                    intent.workspace_inode = intent.workspace_inode.saturating_add(1);
                }
                let _ = entered.send(());
            }
        }

        pub fn gate_check_acknowledgment(
            check: crate::CheckId,
            admits: fn(&crate::CheckView) -> bool,
        ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
            let (entered, wait) = oneshot::channel();
            let (release, released) = oneshot::channel();
            plan().push(Fault::CheckAcknowledgment(check, admits, entered, released));
            (wait, release)
        }

        pub(crate) fn check_acknowledgment_gate(
            view: &crate::CheckView,
        ) -> Option<(oneshot::Sender<()>, oneshot::Receiver<()>)> {
            match take(
                |fault| matches!(fault, Fault::CheckAcknowledgment(id, admits, ..) if *id == view.id && admits(view)),
            ) {
                Some(Fault::CheckAcknowledgment(_, _, entered, release)) => {
                    Some((entered, release))
                }
                _ => None,
            }
        }

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

        pub fn gate_autonomous_continuation(
            session_id: SessionId,
        ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
            let (entered, wait_entered) = oneshot::channel();
            let (release, wait_release) = oneshot::channel();
            plan().push(Fault::AutonomousContinuation(
                session_id,
                entered,
                wait_release,
            ));
            (wait_entered, release)
        }

        pub(crate) fn autonomous_continuation_gate(
            session_id: SessionId,
        ) -> Option<(oneshot::Sender<()>, oneshot::Receiver<()>)> {
            match take(
                |fault| matches!(fault, Fault::AutonomousContinuation(id, ..) if *id == session_id),
            ) {
                Some(Fault::AutonomousContinuation(_, entered, release)) => {
                    Some((entered, release))
                }
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

        /// Ends every same-route retry backoff of the Session at once; the journaled delays are unchanged.
        pub fn skip_retry_backoff(session: SessionId) {
            plan().push(Fault::RetryBackoff(session, None));
        }

        /// Holds the Session's next retry backoff until released; the first receiver reports entry.
        pub fn gate_retry_backoff(
            session: SessionId,
        ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
            let (entered, wait) = oneshot::channel();
            let (release, released) = oneshot::channel();
            plan().push(Fault::RetryBackoff(session, Some((entered, released))));
            (wait, release)
        }

        pub(crate) fn retry_backoff(session: SessionId) -> Option<RetryBackoff> {
            if let Some(Fault::RetryBackoff(_, Some((entered, release)))) =
                take(|fault| matches!(fault, Fault::RetryBackoff(id, Some(_)) if *id == session))
            {
                return Some(RetryBackoff::Gated(entered, release));
            }
            plan()
                .iter()
                .any(|fault| matches!(fault, Fault::RetryBackoff(id, None) if *id == session))
                .then_some(RetryBackoff::Immediate)
        }

        // The named record's next replacement in that state directory fails its directory sync after the rename.
        pub fn fail_record_sync(state: std::path::PathBuf, record: &'static str) {
            plan().push(Fault::RecordSync(state, record));
        }

        pub(crate) fn record_sync_fails(state: &std::path::Path, record: &str) -> bool {
            take(|fault| matches!(fault, Fault::RecordSync(path, name) if path == state && *name == record))
            .is_some()
        }

        pub fn fail_record_publication(state: std::path::PathBuf, record: &'static str) {
            plan().push(Fault::RecordPublication(state, record));
        }

        pub(crate) fn record_publication_fails(state: &std::path::Path, record: &str) -> bool {
            take(|fault| matches!(fault, Fault::RecordPublication(path, name) if path == state && *name == record))
                .is_some()
        }

        pub fn fail_file_parent_sync(intent: uuid::Uuid) {
            plan().push(Fault::FileParentSync(intent));
        }

        pub(crate) fn file_parent_sync_fails(intent: uuid::Uuid) -> bool {
            take(|fault| matches!(fault, Fault::FileParentSync(id) if *id == intent)).is_some()
        }

        pub fn lose_guard_receipt(intent: uuid::Uuid) {
            plan().push(Fault::GuardReceipt(intent));
        }

        pub fn observe_guard_dispatch(call: crate::tools::ToolCall) -> oneshot::Receiver<()> {
            let (entered, wait) = oneshot::channel();
            plan().push(Fault::GuardDispatch(call, entered));
            wait
        }

        pub(crate) fn guard_dispatched(call: &crate::tools::ToolCall) {
            if let Some(Fault::GuardDispatch(_, entered)) = take(
                |fault| matches!(fault, Fault::GuardDispatch(candidate, _) if candidate == call),
            ) {
                let _ = entered.send(());
            }
        }

        pub fn gate_guard_cache_report(
            intent: uuid::Uuid,
        ) -> (
            oneshot::Receiver<crate::tools::ToolObservation>,
            oneshot::Sender<()>,
        ) {
            let (entered, wait) = oneshot::channel();
            let (release, released) = oneshot::channel();
            plan().push(Fault::GuardCacheReport(intent, entered, released));
            (wait, release)
        }

        pub(crate) async fn guard_cache_report_gate(observation: &crate::tools::ToolObservation) {
            if let Some(Fault::GuardCacheReport(_, entered, release)) = take(
                |fault| matches!(fault, Fault::GuardCacheReport(id, ..) if *id == observation.intent.id),
            ) {
                let _ = entered.send(observation.clone());
                let _ = release.await;
            }
        }

        pub(crate) fn guard_receipt_lost(intent: uuid::Uuid) -> bool {
            take(|fault| matches!(fault, Fault::GuardReceipt(id) if *id == intent)).is_some()
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
}

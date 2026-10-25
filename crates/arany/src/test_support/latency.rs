use crate::{
    AgentRunId, Event, EventEnvelope, ProviderCallDisposition, ProviderOutcome, ProviderResponse,
    RunId, SessionId, ToolCall, ToolDisposition,
};
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;
use uuid::Uuid;

const MAX_SAMPLES: usize = 330;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operation {
    Read,
    Edit,
    Command,
    Other,
    InvalidProposal,
}

fn operation(call: &ToolCall) -> Operation {
    match call {
        ToolCall::Read { .. } => Operation::Read,
        ToolCall::Edit { .. } => Operation::Edit,
        ToolCall::Command { .. } => Operation::Command,
        ToolCall::ParseAgentDefinition { .. }
        | ToolCall::List { .. }
        | ToolCall::Search { .. }
        | ToolCall::Mkdir { .. }
        | ToolCall::Write { .. }
        | ToolCall::MultiEdit { .. }
        | ToolCall::Delete { .. }
        | ToolCall::Move { .. }
        | ToolCall::CommandUnguarded { .. }
        | ToolCall::ApplyChangeset { .. }
        | ToolCall::PrepareWorker { .. }
        | ToolCall::Skill { .. }
        | ToolCall::McpList { .. }
        | ToolCall::McpCall { .. }
        | ToolCall::GitStatus { .. }
        | ToolCall::GitDiff { .. }
        | ToolCall::GitLog { .. }
        | ToolCall::GitCommit { .. }
        | ToolCall::GitBranch { .. }
        | ToolCall::GitSwitch { .. }
        | ToolCall::GitPush { .. }
        | ToolCall::GhPrCreate { .. }
        | ToolCall::GhPrList { .. }
        | ToolCall::Glob { .. } => Operation::Other,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum LatencyError {
    #[error("overlapping receipt")]
    Overlap,
    #[error("capture overflow")]
    Overflow,
    #[error("finish without Provider receipt")]
    FinishWithoutReceipt,
    #[error("missing finish before next Provider record")]
    MissingFinishBeforeProviderRecord,
    #[error("mismatched intent")]
    MismatchedIntent,
    #[error("unmatched finish")]
    UnmatchedFinish,
    #[error("missing Provider record")]
    MissingProviderRecord,
    #[error("mismatched Provider receipt")]
    MismatchedProviderReceipt,
    #[error("wrong start Event")]
    WrongStartEvent,
    #[error("mismatched start")]
    MismatchedStart,
    #[error("missing start")]
    MissingStart,
    #[error("wrong finish Event")]
    WrongFinishEvent,
    #[error("finish without start")]
    FinishWithoutStart,
    #[error("mismatched finish or missing permission")]
    MismatchedFinishOrPermission,
    #[error("missing finish")]
    MissingFinish,
    #[error("invalid sample count")]
    InvalidSampleCount,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub agent_run_id: AgentRunId,
    pub call_index: usize,
    pub operation: Operation,
    pub provider_sequence: Option<u64>,
    pub provider_disposition: Option<ProviderCallDisposition>,
    pub intent_id: Option<Uuid>,
    pub started_sequence: Option<u64>,
    pub finished_sequence: Option<u64>,
    pub disposition: Option<ToolDisposition>,
    pub tool_receipt_to_commit_us: Option<u64>,
}

struct Pending {
    sample: Sample,
    received: Instant,
}

struct State {
    pending: Option<Pending>,
    samples: Vec<Sample>,
    invalid: Option<LatencyError>,
}

pub struct Recorder(Mutex<State>);

impl Default for Recorder {
    fn default() -> Self {
        Self(Mutex::new(State {
            pending: None,
            samples: Vec::with_capacity(MAX_SAMPLES),
            invalid: None,
        }))
    }
}

pub(crate) struct Receipt<'a> {
    pub(crate) recorder: &'a Recorder,
    pub(crate) session_id: SessionId,
    pub(crate) run_id: RunId,
    pub(crate) agent_run_id: AgentRunId,
    pub(crate) call_index: usize,
}

impl<'a> Receipt<'a> {
    #[cfg(feature = "test-support")]
    pub(crate) fn primary(
        recorder: &'a crate::fault::LatencyRecorder,
        request: &crate::provider::ProviderRequest,
        call_index: impl FnOnce() -> usize,
    ) -> Option<Self> {
        if crate::provider::ProviderRole::for_phase(request.phase)
            != crate::provider::ProviderRole::Primary
        {
            return None;
        }
        recorder.get().map(|recorder| Self {
            recorder,
            session_id: request.session_id,
            run_id: request.run_id,
            agent_run_id: request.agent_run_id,
            call_index: call_index(),
        })
    }

    pub(crate) fn received(self, response: &ProviderResponse, received: Instant) {
        let ProviderOutcome::Tools(requests) = &response.outcome else {
            return;
        };
        let mut state = self.recorder.state();
        if state.pending.is_some() {
            state.invalid = Some(LatencyError::Overlap);
            return;
        }
        if state.samples.len() == MAX_SAMPLES {
            state.invalid = Some(LatencyError::Overflow);
            return;
        }
        let call = (requests.len() == 1).then(|| &requests[0].call);
        state.pending = Some(Pending {
            sample: Sample {
                session_id: self.session_id,
                run_id: self.run_id,
                agent_run_id: self.agent_run_id,
                call_index: self.call_index,
                operation: call.map_or(Operation::InvalidProposal, operation),
                provider_sequence: None,
                provider_disposition: None,
                intent_id: None,
                started_sequence: None,
                finished_sequence: None,
                disposition: None,
                tool_receipt_to_commit_us: None,
            },
            received,
        });
    }
}

impl Recorder {
    fn state(&self) -> MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn samples(&self) -> Result<Vec<Sample>, LatencyError> {
        let state = self.state();
        if let Some(reason) = state.invalid {
            return Err(reason);
        }
        Ok(state.samples.clone())
    }

    pub fn has_pending(&self) -> bool {
        self.state().pending.is_some()
    }

    pub(crate) fn dispose(&self, run: RunId) {
        let mut state = self.state();
        if state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.sample.run_id == run)
        {
            let pending = state.pending.take().expect("matching pending measurement");
            state.samples.push(pending.sample);
        }
    }

    pub(crate) fn committed(&self, envelope: &EventEnvelope, acknowledged: Instant) {
        let mut state = self.state();
        let Some(pending) = &mut state.pending else {
            if matches!(envelope.event, Event::ToolFinished { .. }) {
                state.invalid = Some(LatencyError::FinishWithoutReceipt);
            }
            return;
        };
        let sample = &mut pending.sample;
        if envelope.session_id != sample.session_id
            || envelope.run_id != Some(sample.run_id)
            || envelope.agent_run_id != Some(sample.agent_run_id)
        {
            return;
        }
        match &envelope.event {
            Event::ProviderCallRecorded { record, .. } => {
                if sample.provider_sequence.is_some() {
                    state.invalid = Some(LatencyError::MissingFinishBeforeProviderRecord);
                    return;
                }
                sample.provider_sequence = Some(envelope.sequence);
                sample.provider_disposition = Some(record.disposition);
                if record.disposition != ProviderCallDisposition::ToolRequested {
                    let pending = state.pending.take().expect("closed Provider refusal");
                    state.samples.push(pending.sample);
                }
            }
            Event::ToolStarted { intent, .. } => {
                if sample.started_sequence.is_some() || sample.operation != operation(&intent.call)
                {
                    state.invalid = Some(LatencyError::MismatchedIntent);
                    return;
                }
                sample.intent_id = Some(intent.id);
                sample.started_sequence = Some(envelope.sequence);
            }
            Event::ToolFinished { observation, .. } => {
                if sample.intent_id != Some(observation.intent.id) {
                    state.invalid = Some(LatencyError::UnmatchedFinish);
                    return;
                }
                sample.finished_sequence = Some(envelope.sequence);
                sample.disposition = Some(observation.disposition);
                sample.tool_receipt_to_commit_us = acknowledged
                    .checked_duration_since(pending.received)
                    .and_then(|duration| u64::try_from(duration.as_micros()).ok());
                let pending = state.pending.take().expect("acknowledged measurement");
                state.samples.push(pending.sample);
            }
            _ => {}
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JournalIntervals {
    pub provider_to_finish_ms: Option<i64>,
    pub started_to_finish_ms: Option<i64>,
    pub clock_invalid: bool,
}

impl Sample {
    pub fn successful(&self) -> bool {
        matches!(
            self.operation,
            Operation::Read | Operation::Edit | Operation::Command
        ) && self.provider_sequence.is_some()
            && self.intent_id.is_some()
            && self.started_sequence.is_some()
            && self.finished_sequence.is_some()
            && self.provider_disposition == Some(ProviderCallDisposition::ToolRequested)
            && self.disposition == Some(ToolDisposition::Succeeded)
            && self.tool_receipt_to_commit_us.is_some()
    }

    pub fn audit(&self, events: &[EventEnvelope]) -> Result<JournalIntervals, LatencyError> {
        let scoped = |event: &&EventEnvelope| {
            event.session_id == self.session_id
                && event.run_id == Some(self.run_id)
                && event.agent_run_id == Some(self.agent_run_id)
        };
        let calls: Vec<_> = events
            .iter()
            .filter(scoped)
            .filter(|event| matches!(event.event, Event::ProviderCallRecorded { .. }))
            .collect();
        let provider = calls
            .get(self.call_index)
            .ok_or(LatencyError::MissingProviderRecord)?;
        let Event::ProviderCallRecorded { record, .. } = &provider.event else {
            unreachable!("filtered Provider records");
        };
        if Some(provider.sequence) != self.provider_sequence
            || Some(record.disposition) != self.provider_disposition
        {
            return Err(LatencyError::MismatchedProviderReceipt);
        }
        let find = |sequence| {
            events
                .iter()
                .find(|event| Some(event.sequence) == sequence && scoped(event))
        };
        let started = find(self.started_sequence);
        let finished = find(self.finished_sequence);
        if let Some(started) = started {
            let Event::ToolStarted { intent, .. } = &started.event else {
                return Err(LatencyError::WrongStartEvent);
            };
            if Some(intent.id) != self.intent_id
                || operation(&intent.call) != self.operation
                || record.requested_tools != 1
                || record.disposition != ProviderCallDisposition::ToolRequested
                || provider.sequence >= started.sequence
            {
                return Err(LatencyError::MismatchedStart);
            }
        } else if self.started_sequence.is_some() || self.intent_id.is_some() {
            return Err(LatencyError::MissingStart);
        }
        if let Some(finished) = finished {
            let Event::ToolFinished { observation, .. } = &finished.event else {
                return Err(LatencyError::WrongFinishEvent);
            };
            let started = started.ok_or(LatencyError::FinishWithoutStart)?;
            if Some(observation.intent.id) != self.intent_id
                || Some(observation.disposition) != self.disposition
                || started.sequence >= finished.sequence
                || calls
                    .get(self.call_index + 1)
                    .is_some_and(|next| next.sequence <= finished.sequence)
                || !events.iter().filter(scoped).any(|event| {
                    matches!(&event.event,
                    Event::ToolStarted { verdict: Some(verdict), .. }
                    if verdict.intent_digest == observation.intent.digest()
                        && event.sequence == started.sequence && event.sequence < finished.sequence)
                })
            {
                return Err(LatencyError::MismatchedFinishOrPermission);
            }
        } else if self.finished_sequence.is_some()
            || self.disposition.is_some()
            || self.tool_receipt_to_commit_us.is_some()
        {
            return Err(LatencyError::MissingFinish);
        }
        let interval = |start: &EventEnvelope| {
            finished.and_then(|end| end.created_at_ms.checked_sub(start.created_at_ms))
        };
        let provider_delta = interval(provider);
        let started_delta = started.and_then(interval);
        let clock_invalid = finished.is_some()
            && [provider_delta, started_delta]
                .into_iter()
                .any(|delta| delta.is_none_or(|value| value < 0));
        Ok(JournalIntervals {
            provider_to_finish_ms: provider_delta.filter(|delta| *delta >= 0),
            started_to_finish_ms: started_delta.filter(|delta| *delta >= 0),
            clock_invalid,
        })
    }
}

pub fn percentiles(samples: &[u64]) -> Result<[u64; 3], LatencyError> {
    if samples.is_empty() || samples.len() > MAX_SAMPLES {
        return Err(LatencyError::InvalidSampleCount);
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    Ok([
        sorted[(50 * sorted.len()).div_ceil(100) - 1],
        sorted[(95 * sorted.len()).div_ceil(100) - 1],
        sorted[sorted.len() - 1],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_never_replaces_an_attempt_on_overlap_or_overflow() {
        let run = RunId::new();
        let session = SessionId::new();
        let agent = AgentRunId::new();
        let response = ProviderResponse {
            outcome: crate::test_support::scenario::tool_outcome(ToolCall::Read {
                path: "src/main.rs".into(),
                offset: None,
                limit: None,
            }),
            turn: crate::ProviderTurn::scripted(),
            response_id: None,
            usage: Default::default(),
            wire_provenance: None,
        };
        for overlap in [false, true] {
            let recorder = Recorder::default();
            let count = if overlap { 2 } else { MAX_SAMPLES + 1 };
            for index in 0..count {
                Receipt {
                    recorder: &recorder,
                    session_id: session,
                    run_id: run,
                    agent_run_id: agent,
                    call_index: index,
                }
                .received(&response, Instant::now());
                if !overlap {
                    recorder.dispose(run);
                }
            }
            let expected = if overlap {
                LatencyError::Overlap
            } else {
                LatencyError::Overflow
            };
            assert_eq!(recorder.samples(), Err(expected));
            assert_eq!(
                serde_json::to_value(recorder.samples()).unwrap(),
                serde_json::json!({"Err":if overlap { "overlap" } else { "overflow" }})
            );
            let state = recorder.state();
            assert_eq!(state.samples.len(), if overlap { 0 } else { MAX_SAMPLES });
            assert_eq!(state.pending.is_some(), overlap);
        }
    }
}

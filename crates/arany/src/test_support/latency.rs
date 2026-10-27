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
        | ToolCall::GhPrView { .. }
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
    pub requested_tools: usize,
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
    started: Vec<(Uuid, u64)>,
    finished: Vec<Uuid>,
    failed: bool,
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
        let call = requests
            .first()
            .filter(|first| {
                requests.len() <= usize::from(crate::session::MAX_STEP_TOOL_CALLS)
                    && requests.iter().enumerate().all(|(index, request)| {
                        operation(&request.call) == operation(&first.call)
                            && requests[..index].iter().all(|prior| prior.id != request.id)
                    })
            })
            .map(|request| &request.call);
        state.pending = Some(Pending {
            sample: Sample {
                session_id: self.session_id,
                run_id: self.run_id,
                agent_run_id: self.agent_run_id,
                call_index: self.call_index,
                operation: call.map_or(Operation::InvalidProposal, operation),
                requested_tools: requests.len(),
                provider_sequence: None,
                provider_disposition: None,
                intent_id: None,
                started_sequence: None,
                finished_sequence: None,
                disposition: None,
                tool_receipt_to_commit_us: None,
            },
            received,
            started: Vec::new(),
            finished: Vec::new(),
            failed: false,
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
                if pending.started.len() >= sample.requested_tools
                    || pending.started.iter().any(|(id, _)| *id == intent.id)
                    || sample.operation != operation(&intent.call)
                {
                    state.invalid = Some(LatencyError::MismatchedIntent);
                    return;
                }
                pending.started.push((intent.id, envelope.sequence));
                sample.intent_id = Some(intent.id);
                sample.started_sequence = Some(envelope.sequence);
            }
            Event::ToolFinished { observation, .. } => {
                let start = pending
                    .started
                    .iter()
                    .find(|(id, _)| *id == observation.intent.id);
                if start.is_none() || pending.finished.contains(&observation.intent.id) {
                    state.invalid = Some(LatencyError::UnmatchedFinish);
                    return;
                }
                pending.finished.push(observation.intent.id);
                pending.failed |= observation.disposition != ToolDisposition::Succeeded;
                if pending.finished.len() < sample.requested_tools {
                    return;
                }
                sample.intent_id = Some(observation.intent.id);
                sample.started_sequence = start.map(|(_, sequence)| *sequence);
                sample.finished_sequence = Some(envelope.sequence);
                sample.disposition = Some(observation.disposition);
                sample.tool_receipt_to_commit_us = (!pending.failed)
                    .then(|| {
                        acknowledged
                            .checked_duration_since(pending.received)
                            .and_then(|duration| u64::try_from(duration.as_micros()).ok())
                    })
                    .flatten();
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
                || record.requested_tools as usize != self.requested_tools
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
        if self.successful() {
            self.audit_step(
                events,
                provider.sequence,
                calls.get(self.call_index + 1).map(|event| event.sequence),
            )?;
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

    fn audit_step(
        &self,
        events: &[EventEnvelope],
        provider: u64,
        next: Option<u64>,
    ) -> Result<(), LatencyError> {
        if !(1..=usize::from(crate::session::MAX_STEP_TOOL_CALLS)).contains(&self.requested_tools) {
            return Err(LatencyError::MismatchedFinishOrPermission);
        }
        let mut starts = Vec::with_capacity(self.requested_tools);
        let mut finishes = Vec::with_capacity(self.requested_tools);
        for event in events.iter().filter(|event| {
            event.session_id == self.session_id
                && event.run_id == Some(self.run_id)
                && event.agent_run_id == Some(self.agent_run_id)
                && event.sequence > provider
                && next.is_none_or(|next| event.sequence < next)
        }) {
            match &event.event {
                Event::ToolStarted { .. } if starts.len() < self.requested_tools => {
                    starts.push(event)
                }
                Event::ToolFinished { .. } if finishes.len() < self.requested_tools => {
                    finishes.push(event)
                }
                Event::ToolStarted { .. } | Event::ToolFinished { .. } => {
                    return Err(LatencyError::MismatchedFinishOrPermission);
                }
                _ => {}
            }
        }
        if starts.len() != self.requested_tools
            || finishes.len() != self.requested_tools
            || finishes.last().map(|event| event.sequence) != self.finished_sequence
        {
            return Err(LatencyError::MismatchedFinishOrPermission);
        }
        for (index, finish) in finishes.iter().enumerate() {
            let Event::ToolFinished { observation, .. } = &finish.event else {
                unreachable!("filtered Tool finishes");
            };
            let digest = observation.intent.digest();
            let matching = starts
                .iter()
                .filter(|start| {
                    matches!(
                        &start.event,
                        Event::ToolStarted { intent, verdict: Some(verdict), .. }
                        if start.sequence < finish.sequence
                            && intent.id == observation.intent.id
                            && crate::EffectIntent::from(intent.clone()) == observation.intent
                            && verdict.approved
                            && verdict.intent_digest == digest
                    )
                })
                .count();
            let repeated = finishes[..index].iter().any(|prior| {
                matches!(
                    &prior.event, Event::ToolFinished { observation: prior, .. }
                        if prior.intent.id == observation.intent.id
                )
            });
            if observation.disposition != ToolDisposition::Succeeded
                || operation(&observation.intent.call) != self.operation
                || matching != 1
                || repeated
            {
                return Err(LatencyError::MismatchedFinishOrPermission);
            }
        }
        Ok(())
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

    fn read_step_history() -> [EventEnvelope; 5] {
        let (session, run, agent) = (SessionId::new(), RunId::new(), AgentRunId::new());
        let reads = ["src/main.rs", "src/lib.rs"].map(|path| crate::EffectIntent {
            host: None,
            host_command: None,
            target: crate::session::worker::EffectTarget::Workspace,
            id: Uuid::now_v7(),
            run_id: run,
            agent_run_id: agent,
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: ToolCall::Read {
                path: path.into(),
                offset: None,
                limit: None,
            },
            limits: crate::ToolLimits::default(),
            use_count: 1,
        });
        let events = [
            Event::ProviderCallRecorded {
                run_id: run,
                agent_run_id: agent,
                record: crate::ProviderCallRecord {
                    phase: crate::AgentPhase::RootPlan,
                    disposition: ProviderCallDisposition::ToolRequested,
                    requested_tools: 2,
                    rate_limits: None,
                    delegation: None,
                    response_id: None,
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_input_tokens: None,
                    cache_creation_input_tokens: None,
                    wire_provenance: None,
                    failure_reason: None,
                    refusal: None,
                },
            },
            read_start(&reads[0]),
            read_start(&reads[1]),
            read_finish(&reads[1]),
            read_finish(&reads[0]),
        ];
        let mut sequence = 0;
        events.map(|event| {
            sequence += 1;
            EventEnvelope {
                sequence,
                session_id: session,
                run_id: Some(run),
                agent_run_id: Some(agent),
                event,
                created_at_ms: 0,
            }
        })
    }

    fn read_start(intent: &crate::EffectIntent) -> Event {
        use crate::tools::permissions::{Decision, PermissionSource, PermissionVerdict};
        Event::ToolStarted {
            run_id: intent.run_id,
            agent_run_id: intent.agent_run_id,
            intent: intent.clone().into(),
            active_ceiling_ms: 0,
            verdict: Some(PermissionVerdict {
                host: None,
                intent_digest: intent.digest(),
                decision: Decision::Allow,
                mode: None,
                basis: PermissionSource::Mode,
                source: PermissionSource::Mode,
                approved: true,
                hook_restriction: None,
                hooks: 0,
                narrowed: false,
            }),
        }
    }

    fn read_finish(intent: &crate::EffectIntent) -> Event {
        Event::ToolFinished {
            checkpoint: None,
            run_id: intent.run_id,
            agent_run_id: intent.agent_run_id,
            observation: crate::ToolObservation {
                spill: None,
                intent: intent.clone(),
                disposition: ToolDisposition::Succeeded,
                output: String::new(),
                attestation: Some(crate::Attestation::for_site(intent)),
                approval_cause: None,
                cache_update: crate::CacheUpdate::NotApplicable,
                enforcement_cause: None,
                limit_cause: None,
            },
        }
    }

    fn read_step_recorder(events: &[EventEnvelope; 5]) -> (Recorder, Instant) {
        let recorder = Recorder::default();
        let received = Instant::now();
        let requests = events[1..3]
            .iter()
            .map(|event| {
                let Event::ToolStarted { intent, .. } = &event.event else {
                    unreachable!()
                };
                crate::ToolRequest {
                    id: crate::ToolUseId::new(intent.id.to_string()).unwrap(),
                    call: intent.call.clone(),
                }
            })
            .collect();
        Receipt {
            recorder: &recorder,
            session_id: events[0].session_id,
            run_id: events[0].run_id.unwrap(),
            agent_run_id: events[0].agent_run_id.unwrap(),
            call_index: 0,
        }
        .received(
            &ProviderResponse {
                outcome: ProviderOutcome::Tools(requests),
                turn: crate::ProviderTurn::scripted(),
                response_id: None,
                usage: Default::default(),
                wire_provenance: None,
            },
            received,
        );
        (recorder, received)
    }

    #[test]
    fn multi_read_capture_rejects_duplicate_excess_and_unmatched_facts() {
        let events = read_step_history();
        let mut extra = events[1].clone();
        let Event::ToolStarted {
            intent, verdict, ..
        } = &mut extra.event
        else {
            unreachable!()
        };
        intent.id = Uuid::now_v7();
        verdict.as_mut().unwrap().intent_digest =
            crate::EffectIntent::from(intent.clone()).digest();
        for (name, indices, expected) in [
            (
                "repeated start",
                vec![0, 1, 1],
                LatencyError::MismatchedIntent,
            ),
            (
                "excess start",
                vec![0, 1, 2, 5],
                LatencyError::MismatchedIntent,
            ),
            (
                "finish before start",
                vec![0, 3],
                LatencyError::UnmatchedFinish,
            ),
            (
                "repeated finish",
                vec![0, 1, 2, 3, 3],
                LatencyError::UnmatchedFinish,
            ),
        ] {
            let (recorder, acknowledged) = read_step_recorder(&events);
            let facts = [
                &events[0], &events[1], &events[2], &events[3], &events[4], &extra,
            ];
            for index in indices {
                recorder.committed(facts[index], acknowledged);
            }
            assert_eq!(recorder.samples(), Err(expected), "{name}");
        }
    }

    #[test]
    fn multi_read_capture_waits_for_every_success_and_audits_the_final_finish() {
        let events = read_step_history();
        for failed in [false, true] {
            let (recorder, received) = read_step_recorder(&events);
            for event in &events[..3] {
                recorder.committed(event, received);
            }
            let mut first_finish = events[3].clone();
            if failed {
                let Event::ToolFinished { observation, .. } = &mut first_finish.event else {
                    unreachable!()
                };
                observation.disposition = ToolDisposition::Failed;
            }
            recorder.committed(&first_finish, received);
            assert!(recorder.has_pending());
            assert!(recorder.samples().unwrap().is_empty());
            recorder.committed(&events[4], received);
            assert!(!recorder.has_pending());
            let samples = recorder.samples().unwrap();
            assert_eq!(samples.len(), 1);
            let sample = &samples[0];
            assert_eq!(sample.successful(), !failed);
            assert_eq!(sample.tool_receipt_to_commit_us, (!failed).then_some(0));
            assert_eq!(sample.finished_sequence, Some(events[4].sequence));
            if failed {
                continue;
            }
            sample.audit(&events).unwrap();
            let missing = events
                .iter()
                .filter(|event| event.sequence != first_finish.sequence)
                .cloned()
                .collect::<Vec<_>>();
            let mut premature = sample.clone();
            let Event::ToolStarted { intent, .. } = &events[2].event else {
                unreachable!()
            };
            premature.intent_id = Some(intent.id);
            premature.started_sequence = Some(events[2].sequence);
            premature.finished_sequence = Some(first_finish.sequence);
            for (name, sample, history) in [
                ("missing finish", sample, missing.as_slice()),
                ("premature endpoint", &premature, events.as_slice()),
            ] {
                assert_eq!(
                    sample.audit(history),
                    Err(LatencyError::MismatchedFinishOrPermission),
                    "{name}"
                );
            }
        }
    }

    #[test]
    fn multi_read_audit_rejects_unpaired_or_unpermitted_siblings() {
        #[derive(Debug)]
        enum Damage {
            MissingStart,
            MissingVerdict,
            RefusedVerdict,
            WrongVerdict,
            RepeatedStart,
            RepeatedFinish,
            UnmatchedFinish,
            LateStart,
            ChangedIntent,
        }
        let events = read_step_history();
        let (recorder, acknowledged) = read_step_recorder(&events);
        for event in &events {
            recorder.committed(event, acknowledged);
        }
        let sample = recorder.samples().unwrap().pop().unwrap();
        sample.audit(&events).unwrap();
        for damage in [
            Damage::MissingStart,
            Damage::MissingVerdict,
            Damage::RefusedVerdict,
            Damage::WrongVerdict,
            Damage::RepeatedStart,
            Damage::RepeatedFinish,
            Damage::UnmatchedFinish,
            Damage::LateStart,
            Damage::ChangedIntent,
        ] {
            let mut history = events.to_vec();
            let Event::ToolStarted {
                intent, verdict, ..
            } = &mut history[2].event
            else {
                unreachable!()
            };
            match damage {
                Damage::MissingStart => {
                    history.remove(2);
                }
                Damage::MissingVerdict => *verdict = None,
                Damage::RefusedVerdict => verdict.as_mut().unwrap().approved = false,
                Damage::WrongVerdict => verdict.as_mut().unwrap().intent_digest = [9; 32],
                Damage::RepeatedStart => {
                    history.insert(3, events[2].clone());
                }
                Damage::RepeatedFinish => {
                    let Event::ToolFinished {
                        observation: original,
                        ..
                    } = &events[4].event
                    else {
                        unreachable!()
                    };
                    let Event::ToolFinished { observation, .. } = &mut history[3].event else {
                        unreachable!()
                    };
                    observation.intent = original.intent.clone();
                }
                Damage::UnmatchedFinish => {
                    let Event::ToolFinished { observation, .. } = &mut history[3].event else {
                        unreachable!()
                    };
                    observation.intent.id = Uuid::now_v7();
                }
                Damage::LateStart => history[2].sequence = events[3].sequence,
                Damage::ChangedIntent => intent.workspace_inode += 1,
            }
            assert_eq!(
                sample.audit(&history),
                Err(LatencyError::MismatchedFinishOrPermission),
                "{damage:?}"
            );
        }
    }

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

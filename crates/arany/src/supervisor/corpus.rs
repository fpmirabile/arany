use super::*;
use crate::{SessionId, StateRoot, Store, StoreError};
use aws_lc_rs::{
    hmac,
    rand::{SecureRandom, SystemRandom},
};
use serde_json::{Value, json};

const MAX_RECORDS: usize = 10_000;
const MAX_BYTES: usize = 8 * 1024 * 1024;
// Labels from later Events reach only the Run's most recent records.
const RECENT_RECORDS: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum CorpusError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("Session not found")]
    NotFound,
    #[error("corpus randomness unavailable")]
    Random,
    #[error(
        "supervisor corpus overflow: {records} records, {bytes} bytes (limits: 10000 records, 8388608 bytes)"
    )]
    Overflow { records: usize, bytes: usize },
}

pub async fn export_corpus(root: StateRoot, session_id: SessionId) -> Result<String, CorpusError> {
    let _lock = root.lock_session_read_only(session_id)?;
    let store = Store::open_read_only(root)?;
    let result = store.supervisor_corpus(session_id).await;
    store.close().await?;
    result
}

#[derive(Default, Serialize)]
struct Labels {
    pause_or_cancel: bool,
    steering: bool,
    goal_failed: bool,
}

#[derive(Serialize)]
struct Record {
    run_ordinal: usize,
    actor_ordinal: u8,
    role: crate::AgentRole,
    site: Site,
    operation: Option<&'static str>,
    disposition_class: Option<ToolDisposition>,
    cause_class: Option<crate::tools::EnforcementCause>,
    features: BoundaryFeatures,
    journaled_level: Option<Level>,
    salted_call_key: String,
    salted_outcome_key: String,
    relative_step: usize,
    labels: Labels,
}

#[derive(Serialize)]
struct BoundaryFeatures {
    r#loop: LoopFeatures,
    probing: ProbingFeatures,
    disproportion: Option<DisproportionFeatures>,
}

pub(crate) struct Corpus {
    evaluator: SupervisorState,
    key: hmac::Key,
    records: Vec<Record>,
    count: usize,
    recent: VecDeque<(RunId, u64, usize)>,
    last_run: Option<RunId>,
    run_step: usize,
}

impl Corpus {
    pub(crate) fn new() -> Result<Self, CorpusError> {
        let mut salt = [0; 32];
        SystemRandom::new()
            .fill(&mut salt)
            .map_err(|_| CorpusError::Random)?;
        Ok(Self {
            evaluator: Default::default(),
            key: hmac::Key::new(hmac::HMAC_SHA256, &salt),
            records: Vec::new(),
            count: 0,
            recent: VecDeque::with_capacity(RECENT_RECORDS),
            last_run: None,
            run_step: 0,
        })
    }

    fn salted(&self, domain: &[u8], digest: &[u8; 32]) -> String {
        let mut context = hmac::Context::with_key(&self.key);
        context.update(domain);
        context.update(digest);
        context
            .sign()
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    pub(crate) fn observe(&mut self, view: &SessionView, envelope: &EventEnvelope) {
        self.evaluator.decide_verdict(view, envelope);
        self.label_recent(view, envelope);
        self.record_boundary(view, envelope);
    }

    fn label_recent(&mut self, view: &SessionView, envelope: &EventEnvelope) {
        match &envelope.event {
            Event::SupervisorVerdictRecorded { verdict, .. } => {
                if let Some((_, sequence, index)) = self.recent.back()
                    && *sequence == verdict.subject_sequence
                    && let Some(record) = self.records.get_mut(*index)
                {
                    record.journaled_level = Some(
                        if verdict
                            .signals
                            .iter()
                            .any(|s| s.level == Level::WouldNarrow)
                        {
                            Level::WouldNarrow
                        } else {
                            Level::Notice
                        },
                    );
                }
            }
            Event::GoalSteeringNoteRecorded { run_id, .. }
            | Event::AgentSteeringNoteRecorded { run_id, .. } => {
                self.label(*run_id, |labels| labels.steering = true);
            }
            Event::RunFinished {
                run_id,
                disposition: crate::RunDisposition::Cancelled,
                ..
            } => {
                self.label(*run_id, |labels| labels.pause_or_cancel = true);
            }
            Event::GoalFinished {
                goal_id,
                status: crate::GoalStatus::Failed,
                ..
            } => {
                if let Some(run) = view
                    .goals
                    .iter()
                    .find(|g| g.id == *goal_id)
                    .and_then(|g| g.run_ids.last())
                {
                    self.label(*run, |labels| labels.goal_failed = true);
                }
            }
            _ => {}
        }
    }

    fn record_boundary(&mut self, view: &SessionView, envelope: &EventEnvelope) {
        let Some(run) = view.runs.last() else { return };
        let Some(actor) = envelope.agent_run_id else {
            return;
        };
        if envelope.run_id != Some(run.id) {
            return;
        }
        let Some(window) = self
            .evaluator
            .windows
            .iter()
            .find(|(id, _)| *id == actor)
            .map(|(_, w)| w)
        else {
            return;
        };
        let Some(step) = window.back().filter(|s| s.sequence == envelope.sequence) else {
            return;
        };
        if self.last_run != Some(run.id) {
            self.last_run = Some(run.id);
            self.run_step = 0;
            self.recent.clear();
        }
        self.run_step += 1;
        self.count += 1;
        if self.count > MAX_RECORDS {
            return;
        }
        let Some(agent) = run.agents.iter().find(|a| a.id == actor) else {
            return;
        };
        let calls: Vec<_> = window.iter().filter(|s| s.observation || s.stall).collect();
        let (looping, _) = loop_features(&calls, window);
        let (site, _, footprint) = subject(run, actor, envelope, self.evaluator.declared_roots)
            .expect("evaluated boundary");
        let (operation, disposition, cause) = match &envelope.event {
            Event::ToolFinished { observation, .. } => (
                Some(observation.intent.call.facts().name),
                Some(observation.disposition),
                observation.enforcement_cause,
            ),
            _ => (None, None, None),
        };
        self.records.push(Record {
            run_ordinal: view.runs.len(),
            actor_ordinal: agent.ordinal,
            role: agent.role,
            site,
            operation,
            disposition_class: disposition,
            cause_class: cause,
            features: BoundaryFeatures {
                r#loop: looping,
                probing: probing_features(&calls),
                disproportion: footprint,
            },
            journaled_level: None,
            salted_call_key: self.salted(b"arany.supervisor.corpus.call.v1", &step.call),
            salted_outcome_key: self.salted(b"arany.supervisor.corpus.outcome.v1", &step.outcome),
            relative_step: self.run_step,
            labels: Labels::default(),
        });
        if self.recent.len() == RECENT_RECORDS {
            self.recent.pop_front();
        }
        self.recent
            .push_back((run.id, envelope.sequence, self.records.len() - 1));
    }

    fn label(&mut self, run: RunId, update: impl Fn(&mut Labels)) {
        for (id, _, index) in &self.recent {
            if *id == run {
                update(&mut self.records[*index].labels);
            }
        }
    }

    pub(crate) fn finish(self, view: &SessionView) -> Result<String, CorpusError> {
        let runs: Vec<_> = view.runs.iter().enumerate().map(|(i, r)| {
            let outcome = match r.status {
                crate::RunStatus::Finished => "completed",
                crate::RunStatus::Failed => "failed",
                crate::RunStatus::Cancelled => "cancelled",
                crate::RunStatus::StoppedByHook => "stopped_by_hook",
                crate::RunStatus::Interrupted | crate::RunStatus::Pending | crate::RunStatus::Active => "interrupted",
            };
            json!({"run_ordinal":i + 1, "outcome":outcome, "policy":r.config.as_ref().and_then(|c| c.supervisor.as_ref()).map(|p| json!({"evaluator":p.evaluator, "window":p.window, "floors":p.floors, "candidates":p.candidates}))})
        }).collect();
        let goals: Vec<_> = view.goals.iter().enumerate().map(|(i, g)| {
            json!({"goal_ordinal":i + 1, "outcome":if g.budget_closed { "budget_exhausted" } else { g.status.as_str() }, "run_ordinals":g.run_ids.iter().filter_map(|id| view.runs.iter().position(|r| r.id == *id).map(|n| n + 1)).collect::<Vec<_>>()})
        }).collect();
        let header = json!({"format":"arany.supervisor.corpus.v1", "evaluator":1, "session_ordinal":1, "runs":runs, "goals":goals});
        encode(&header, &self.records, self.count)
    }
}

fn encode(header: &Value, records: &[Record], count: usize) -> Result<String, CorpusError> {
    let mut output = String::new();
    let mut bytes = 0usize;
    let header = serde_json::to_string(header).expect("closed corpus header");
    bytes += header.len() + 1;
    if bytes <= MAX_BYTES {
        output.push_str(&header);
        output.push('\n');
    }
    for record in records {
        let line = serde_json::to_string(record).expect("closed corpus record");
        bytes = bytes.saturating_add(line.len() + 1);
        if bytes <= MAX_BYTES && count <= MAX_RECORDS {
            output.push_str(&line);
            output.push('\n');
        }
    }
    if count > MAX_RECORDS || bytes > MAX_BYTES {
        return Err(CorpusError::Overflow {
            records: count,
            bytes,
        });
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Record {
        Record {
            run_ordinal: 1,
            actor_ordinal: 0,
            role: crate::AgentRole::Primary,
            site: Site::ToolOutcome,
            operation: Some("read"),
            disposition_class: Some(ToolDisposition::Succeeded),
            cause_class: None,
            features: BoundaryFeatures {
                r#loop: LoopFeatures::default(),
                probing: ProbingFeatures::default(),
                disproportion: None,
            },
            journaled_level: None,
            salted_call_key: "a".repeat(64),
            salted_outcome_key: "b".repeat(64),
            relative_step: 1,
            labels: Labels::default(),
        }
    }

    #[test]
    fn corpus_limits_accept_exact_caps_and_refuse_whole_exports_with_counts() {
        let records: Vec<_> = (0..MAX_RECORDS).map(|_| record()).collect();
        let output = encode(&json!({}), &records, MAX_RECORDS).expect("exact record cap");
        assert_eq!(output.lines().count(), MAX_RECORDS + 1);
        assert!(matches!(
            encode(&json!({}), &records, MAX_RECORDS + 1),
            Err(CorpusError::Overflow {
                records: 10_001,
                ..
            })
        ));
        let header = json!("x".repeat(MAX_BYTES - 3));
        assert_eq!(encode(&header, &[], 0).unwrap().len(), MAX_BYTES);
        let header = json!("x".repeat(MAX_BYTES - 2));
        assert!(
            matches!(encode(&header, &[], 0), Err(CorpusError::Overflow { records: 0, bytes }) if bytes == MAX_BYTES + 1)
        );
    }
}

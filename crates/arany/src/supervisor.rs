pub(crate) mod corpus;
pub use corpus::{CorpusError, export_corpus};

use crate::session::worker::{WorkerBaseVersion, WorkerDataFacts, WorkerDataPurpose};
use crate::session::{AgentRunId, Event, EventEnvelope, ReplayError, RunId, RunView, SessionView};
use crate::tools::types::CommandResult;
use crate::tools::{Attestation, ToolCall, ToolDisposition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};

pub(crate) const WINDOW: usize = 32;
pub(crate) const MAX_VERDICTS: u16 = 256;
pub(crate) const MAX_VERDICT_BYTES: usize = 2048;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorPolicy {
    pub evaluator: u16,
    pub window: u16,
    pub floors: Thresholds,
    pub candidates: Thresholds,
    pub digest: [u8; 32],
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Thresholds {
    pub identical: u16,
    pub failing: u16,
    pub cycle_two: u16,
    pub cycle_other: u16,
    pub oscillation: u16,
    pub stalled: u16,
    pub denial_streak: u16,
    pub denials_window: u16,
    pub distinct_denied: u16,
    pub alias: u16,
    pub files: u16,
    pub files_per_root: u16,
    pub delete_permille: u16,
    pub deleted_files: u16,
    pub growth_permille: u16,
    pub changed_kib: u16,
}

impl Default for SupervisorPolicy {
    fn default() -> Self {
        let mut policy = Self {
            evaluator: 1,
            window: WINDOW as u16,
            floors: Thresholds {
                identical: 3,
                failing: 2,
                cycle_two: 3,
                cycle_other: 3,
                oscillation: 1,
                stalled: 2,
                denial_streak: 2,
                denials_window: u16::MAX,
                distinct_denied: u16::MAX,
                alias: 1,
                files: 16,
                files_per_root: 4,
                delete_permille: 250,
                deleted_files: 1,
                growth_permille: u16::MAX,
                changed_kib: u16::MAX,
            },
            candidates: Thresholds {
                identical: 4,
                failing: 3,
                cycle_two: 3,
                cycle_other: 5,
                oscillation: 2,
                stalled: 3,
                denial_streak: 3,
                denials_window: 4,
                distinct_denied: 3,
                alias: 2,
                files: 32,
                files_per_root: 8,
                delete_permille: 500,
                deleted_files: 8,
                growth_permille: 4000,
                changed_kib: 64,
            },
            digest: [0; 32],
        };
        policy.digest = policy.computed_digest();
        policy
    }
}
impl SupervisorPolicy {
    #[cfg(any(test, feature = "test-support"))]
    pub fn all_off() -> Self {
        let mut policy = Self::default();
        let mut value = serde_json::to_value(&policy.floors).expect("thresholds");
        for v in value
            .as_object_mut()
            .expect("threshold fields")
            .values_mut()
        {
            *v = serde_json::json!(u16::MAX);
        }
        policy.floors = serde_json::from_value(value).expect("thresholds");
        policy.candidates = policy.floors.clone();
        policy.digest = policy.computed_digest();
        policy
    }

    fn computed_digest(&self) -> [u8; 32] {
        hash(
            &serde_json::to_vec(&(
                "arany.supervisor.policy.v1",
                self.evaluator,
                self.window,
                &self.floors,
                &self.candidates,
            ))
            .expect("typed policy"),
        )
    }
    pub(crate) fn valid(&self) -> bool {
        self.evaluator == 1
            && self.window == WINDOW as u16
            && self.digest == self.computed_digest()
            && [&self.floors, &self.candidates].iter().all(|t| {
                [
                    t.identical,
                    t.failing,
                    t.cycle_two,
                    t.cycle_other,
                    t.oscillation,
                    t.stalled,
                    t.denial_streak,
                    t.denials_window,
                    t.distinct_denied,
                    t.alias,
                    t.files,
                    t.files_per_root,
                    t.delete_permille,
                    t.deleted_files,
                    t.growth_permille,
                    t.changed_kib,
                ]
                .iter()
                .all(|v| *v > 0)
            })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Site {
    ToolOutcome,
    CommandCapture,
    WorkerCapture,
    Handover,
    Stall,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Shadow,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Notice,
    WouldNarrow,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Response {
    Pause,
    Revoke,
    RequireApproval,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopFeatures {
    pub repeat_identical: u16,
    pub repeat_failing: u16,
    pub cycle_reps: u16,
    pub cycle_len: u16,
    pub edit_oscillation: u16,
    pub stalled_calls: u16,
}
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbingFeatures {
    pub denial_streak: u16,
    pub denials_window: u16,
    pub distinct_denied: u16,
    pub alias_rejections: u16,
}
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisproportionFeatures {
    pub files: u16,
    pub deleted_files: u16,
    pub changed_kib: u16,
    pub declared_roots: u16,
    #[serde(deserialize_with = "Option::deserialize")]
    pub base_kib: Option<u16>,
    pub files_per_root: u16,
    pub delete_permille: u16,
    #[serde(deserialize_with = "Option::deserialize")]
    pub growth_permille: Option<u16>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Features {
    Loop(LoopFeatures),
    Probing(ProbingFeatures),
    Disproportion(DisproportionFeatures),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    Loop,
    Probing,
    Disproportion,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signal {
    pub signal: SignalKind,
    pub level: Level,
    #[serde(deserialize_with = "Option::deserialize")]
    pub would_response: Option<Response>,
    pub features: Features,
    pub evidence: Vec<u64>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorVerdict {
    pub subject_sequence: u64,
    pub site: Site,
    pub evaluator: u16,
    pub mode: Mode,
    pub signals: Vec<Signal>,
    pub capped: bool,
}
impl SupervisorVerdict {
    pub(crate) fn valid(&self) -> bool {
        self.subject_sequence > 0
            && self.evaluator == 1
            && (1..=3).contains(&self.signals.len())
            && self.signals.iter().all(|s| {
                matches!(
                    (&s.signal, &s.features),
                    (SignalKind::Loop, Features::Loop(_))
                        | (SignalKind::Probing, Features::Probing(_))
                        | (SignalKind::Disproportion, Features::Disproportion(_))
                ) && !s.evidence.is_empty()
                    && s.evidence.len() <= 8
                    && s.evidence
                        .iter()
                        .all(|n| *n > 0 && *n <= self.subject_sequence)
                    && s.evidence.windows(2).all(|w| w[0] < w[1])
                    && (s.level == Level::Notice) == s.would_response.is_none()
            })
            && serde_json::to_vec(self).is_ok_and(|v| v.len() <= MAX_VERDICT_BYTES)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Step {
    sequence: u64,
    call: [u8; 32],
    outcome: [u8; 32],
    succeeded: bool,
    denied: bool,
    alias: bool,
    stall: bool,
    // Capture sites carry footprint facts; only Tool outcomes advance call patterns.
    observation: bool,
    edits: Vec<([u8; 32], [u8; 32])>,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SupervisorState {
    declared_roots: u16,
    windows: Vec<(AgentRunId, VecDeque<Step>)>,
    verdicts: u16,
    pending: Option<(RunId, AgentRunId, SupervisorVerdict)>,
    last_verdict: Option<(u64, u64)>,
    recorded: Vec<(AgentRunId, SupervisorVerdict)>,
}
impl SupervisorState {
    pub fn verdicts(&self) -> &[(AgentRunId, SupervisorVerdict)] {
        &self.recorded
    }

    #[cfg(feature = "test-support")]
    pub fn with_verdicts_for_test(recorded: Vec<(AgentRunId, SupervisorVerdict)>) -> Self {
        assert!(recorded.len() <= usize::from(MAX_VERDICTS));
        assert!(recorded.iter().all(|(_, verdict)| verdict.valid()));
        Self {
            verdicts: u16::try_from(recorded.len()).expect("bounded verdict count"),
            recorded,
            ..Self::default()
        }
    }

    pub(crate) fn pending_event(&self) -> Option<Event> {
        self.pending
            .as_ref()
            .map(
                |(run_id, agent_run_id, verdict)| Event::SupervisorVerdictRecorded {
                    run_id: *run_id,
                    agent_run_id: *agent_run_id,
                    verdict: verdict.clone(),
                },
            )
    }
    pub(crate) fn previous_subject(&self, sequence: u64) -> u64 {
        self.last_verdict
            .filter(|(n, _)| *n == sequence)
            .map_or(sequence, |(_, subject)| subject)
    }
    pub(crate) fn before(
        view: &mut SessionView,
        envelope: &EventEnvelope,
    ) -> Result<bool, ReplayError> {
        let Some(run) = view.runs.last_mut() else {
            return Ok(false);
        };
        if let Event::SupervisorVerdictRecorded {
            run_id,
            agent_run_id,
            verdict,
        } = &envelope.event
        {
            if run.supervisor.pending.as_ref() != Some(&(*run_id, *agent_run_id, verdict.clone()))
                || verdict.subject_sequence.checked_add(1) != Some(envelope.sequence)
            {
                return Err(ReplayError::InvalidTransition);
            }
            run.supervisor.pending = None;
            run.supervisor
                .recorded
                .push((*agent_run_id, verdict.clone()));
            run.supervisor.verdicts += 1;
            run.supervisor.last_verdict = Some((envelope.sequence, verdict.subject_sequence));
            return Ok(true);
        }
        if run.supervisor.pending.is_some() {
            if envelope.run_id == Some(run.id) {
                return Err(ReplayError::InvalidTransition);
            }
            run.supervisor.pending = None;
        }
        Ok(false)
    }
    pub(crate) fn after(view: &mut SessionView, envelope: &EventEnvelope) {
        let Some(run) = view.runs.last() else { return };
        if run.config.as_ref().is_none_or(|c| c.supervisor.is_none()) {
            return;
        }
        if matches!(
            envelope.event,
            Event::RunStarted { .. } | Event::GoalPlanContractRecorded { .. }
        ) && let Some(run_id) = envelope.run_id
        {
            let roots = view
                .goals
                .iter()
                .find(|goal| goal.run_ids.contains(&run_id))
                .and_then(|goal| goal.plan_contract.as_ref())
                .map_or(1, |plan| plan.paths.len().max(1));
            if let Some(run) = view.runs.last_mut() {
                run.supervisor.declared_roots = count(roots);
            }
            return;
        }
        if envelope.run_id != Some(run.id) {
            return;
        }
        let Some(actor) = envelope.agent_run_id else {
            return;
        };
        let Some((site, step, footprint)) = subject(run, actor, envelope) else {
            return;
        };
        let run = view.runs.last_mut().expect("subject Run");
        let Some(policy) = run.config.as_ref().and_then(|c| c.supervisor.clone()) else {
            return;
        };
        run.supervisor.observe(
            run.id,
            actor,
            envelope.sequence,
            (site, step, footprint),
            &policy,
        );
    }

    pub(crate) fn tool_verdict_follows(
        view: &SessionView,
        sequence: u64,
        observation: &crate::tools::ToolObservation,
    ) -> bool {
        let run = view.runs.last().expect("active Run");
        let Some(policy) = run.config.as_ref().and_then(|c| c.supervisor.as_ref()) else {
            return false;
        };
        let actor = observation.intent.agent_run_id;
        let envelope = EventEnvelope {
            session_id: view.id,
            run_id: Some(run.id),
            agent_run_id: Some(actor),
            sequence,
            created_at_ms: 0,
            event: Event::ToolFinished {
                run_id: run.id,
                agent_run_id: actor,
                observation: observation.clone(),
                checkpoint: None,
            },
        };
        let Some((site, step, footprint)) = subject(run, actor, &envelope) else {
            return false;
        };
        let mut state = run.supervisor.clone();
        state.observe(run.id, actor, sequence, (site, step, footprint), policy);
        state.pending.is_some()
    }

    fn observe(
        &mut self,
        run_id: crate::session::RunId,
        actor: AgentRunId,
        sequence: u64,
        subject: (Site, Step, Option<DisproportionFeatures>),
        policy: &SupervisorPolicy,
    ) {
        let (site, step, footprint) = subject;
        let index = match self.windows.iter().position(|(id, _)| *id == actor) {
            Some(index) => index,
            None => {
                assert!(self.windows.len() < 9, "bounded Run actors");
                self.windows.push((actor, VecDeque::with_capacity(WINDOW)));
                self.windows.len() - 1
            }
        };
        let window = &mut self.windows[index].1;
        if window.len() == WINDOW {
            window.pop_front();
        }
        window.push_back(step);
        let signals = evaluate(window, footprint, policy);
        let signals = new_facts(signals, site, sequence);
        if !signals.is_empty() && self.verdicts < MAX_VERDICTS {
            self.pending = Some((
                run_id,
                actor,
                SupervisorVerdict {
                    subject_sequence: sequence,
                    site,
                    evaluator: 1,
                    mode: Mode::Shadow,
                    signals,
                    capped: self.verdicts == MAX_VERDICTS - 1,
                },
            ));
        }
    }
}

// Call-pattern signals report new facts only: the subject must be part of the evidence.
fn new_facts(mut signals: Vec<Signal>, site: Site, subject: u64) -> Vec<Signal> {
    signals.retain(|s| {
        s.signal == SignalKind::Disproportion
            || !matches!(site, Site::WorkerCapture | Site::Handover)
                && s.evidence.contains(&subject)
    });
    signals
}

fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
fn count(n: usize) -> u16 {
    n.try_into().unwrap_or(u16::MAX)
}
fn ratio(n: u64, d: u64) -> u16 {
    (n / d.max(1)).min(u64::from(u16::MAX)) as u16
}

fn evaluate(
    window: &VecDeque<Step>,
    footprint: Option<DisproportionFeatures>,
    policy: &SupervisorPolicy,
) -> Vec<Signal> {
    let calls: Vec<_> = window.iter().filter(|s| s.observation || s.stall).collect();
    let (looping, oscillating) = loop_features(&calls, window);
    let probing = probing_features(&calls);
    let loop_hit = |t: &Thresholds| {
        looping.repeat_identical >= t.identical
            || looping.repeat_failing >= t.failing
            || looping.cycle_reps
                >= if looping.cycle_len == 2 {
                    t.cycle_two
                } else {
                    t.cycle_other
                }
            || looping.edit_oscillation >= t.oscillation
            || looping.stalled_calls >= t.stalled
    };
    let probe_hit = |t: &Thresholds| {
        probing.denial_streak >= t.denial_streak
            || probing.alias_rejections >= t.alias
            || probing.distinct_denied >= t.distinct_denied
                && probing.denials_window >= t.denials_window
    };
    let mut signals = Vec::with_capacity(3);
    let mut push = |features, notice, narrow, response, evidence: Vec<u64>| {
        if notice || narrow {
            signals.push(Signal {
                signal: match &features {
                    Features::Loop(_) => SignalKind::Loop,
                    Features::Probing(_) => SignalKind::Probing,
                    Features::Disproportion(_) => SignalKind::Disproportion,
                },
                features,
                level: if narrow {
                    Level::WouldNarrow
                } else {
                    Level::Notice
                },
                would_response: narrow.then_some(response),
                evidence,
            });
        }
    };
    push(
        Features::Loop(looping.clone()),
        loop_hit(&policy.floors),
        loop_hit(&policy.candidates),
        Response::Pause,
        loop_evidence(window, &calls, &looping, policy, &oscillating),
    );
    push(
        Features::Probing(probing.clone()),
        probe_hit(&policy.floors),
        probe_hit(&policy.candidates),
        Response::Revoke,
        evidence(window, |s| s.denied),
    );
    if let Some(f) = footprint {
        let notice = f.files >= policy.floors.files
            || f.files_per_root >= policy.floors.files_per_root
                && f.delete_permille >= policy.floors.delete_permille;
        let t = &policy.candidates;
        let narrow = f.files >= t.files && f.files_per_root >= t.files_per_root
            || f.deleted_files >= t.deleted_files && f.delete_permille >= t.delete_permille
            || f.changed_kib >= t.changed_kib
                && f.growth_permille.is_some_and(|n| n >= t.growth_permille);
        push(
            Features::Disproportion(f),
            notice,
            narrow,
            Response::RequireApproval,
            vec![window.back().expect("subject").sequence],
        );
    }
    signals
}

fn loop_features(calls: &[&Step], window: &VecDeque<Step>) -> (LoopFeatures, Vec<u64>) {
    let mut looping = LoopFeatures::default();
    if let Some(last) = calls.last() {
        if last.observation {
            looping.repeat_identical = count(
                calls
                    .iter()
                    .rev()
                    .take_while(|s| {
                        s.observation && (s.call, s.outcome) == (last.call, last.outcome)
                    })
                    .count(),
            );
            looping.repeat_failing = count(
                calls
                    .iter()
                    .rev()
                    .take_while(|s| s.observation && !s.succeeded && s.call == last.call)
                    .count(),
            );
        }
        looping.stalled_calls = count(
            calls
                .iter()
                .rev()
                .take_while(|s| !(s.observation && s.succeeded))
                .filter(|s| s.stall)
                .count(),
        );
    }
    for k in 2..=4 {
        if calls.len() < k || calls[calls.len() - k..].iter().any(|s| !s.observation) {
            continue;
        }
        let repetitions = (0..calls.len() / k)
            .take_while(|rep| {
                (0..k).all(|j| {
                    let a = calls[calls.len() - 1 - j];
                    let b = calls[calls.len() - 1 - j - rep * k];
                    b.observation && (a.call, a.outcome) == (b.call, b.outcome)
                })
            })
            .count();
        if count(repetitions) > looping.cycle_reps {
            looping.cycle_reps = count(repetitions);
            looping.cycle_len = k as u16;
        }
    }
    let mut edits: BTreeMap<[u8; 32], (Vec<[u8; 32]>, u16)> = BTreeMap::new();
    for step in window {
        for (path, digest) in &step.edits {
            let (seen, returns) = edits.entry(*path).or_default();
            if seen.last().is_some_and(|last| last != digest) && seen.contains(digest) {
                *returns = returns.saturating_add(1);
            }
            seen.push(*digest);
            looping.edit_oscillation = looping.edit_oscillation.max(*returns);
        }
    }
    let sequences = window
        .iter()
        .filter(|s| {
            s.edits
                .iter()
                .any(|(path, _)| edits.get(path).is_some_and(|(_, n)| *n > 0))
        })
        .map(|s| s.sequence)
        .collect();
    (looping, sequences)
}
fn probing_features(calls: &[&Step]) -> ProbingFeatures {
    let denied: Vec<_> = calls.iter().filter(|s| s.denied).collect();
    let mut distinct = Vec::with_capacity(WINDOW);
    for s in &denied {
        if !distinct.contains(&s.call) {
            distinct.push(s.call);
        }
    }
    ProbingFeatures {
        denial_streak: count(calls.iter().rev().take_while(|s| s.denied).count()),
        denials_window: count(denied.len()),
        distinct_denied: count(distinct.len()),
        alias_rejections: count(denied.iter().filter(|s| s.alias).count()),
    }
}
fn evidence(window: &VecDeque<Step>, predicate: impl Fn(&Step) -> bool) -> Vec<u64> {
    let mut sequences: Vec<_> = window
        .iter()
        .rev()
        .filter(|s| predicate(s))
        .take(8)
        .map(|s| s.sequence)
        .collect();
    sequences.reverse();
    sequences
}
fn loop_evidence(
    window: &VecDeque<Step>,
    calls: &[&Step],
    looping: &LoopFeatures,
    policy: &SupervisorPolicy,
    oscillating: &[u64],
) -> Vec<u64> {
    evidence(window, |s| {
        let reached = |value: u16, floor: u16, candidate: u16| value >= floor.min(candidate);
        let identical = if reached(
            looping.repeat_identical,
            policy.floors.identical,
            policy.candidates.identical,
        ) {
            looping.repeat_identical
        } else {
            0
        };
        let failing = if reached(
            looping.repeat_failing,
            policy.floors.failing,
            policy.candidates.failing,
        ) {
            looping.repeat_failing
        } else {
            0
        };
        let (floor, candidate) = if looping.cycle_len == 2 {
            (policy.floors.cycle_two, policy.candidates.cycle_two)
        } else {
            (policy.floors.cycle_other, policy.candidates.cycle_other)
        };
        let cycle = if reached(looping.cycle_reps, floor, candidate) {
            looping.cycle_reps * looping.cycle_len
        } else {
            0
        };
        let suffix = usize::from(identical.max(failing).max(cycle));
        let in_pattern = calls
            .iter()
            .rev()
            .take(suffix)
            .any(|step| step.sequence == s.sequence);
        let stalled = looping.stalled_calls > 0
            && s.stall
            && calls
                .iter()
                .rev()
                .take_while(|step| !step.succeeded)
                .any(|step| step.sequence == s.sequence);
        in_pattern || stalled || oscillating.contains(&s.sequence)
    })
}

fn call_key(call: &ToolCall) -> [u8; 32] {
    let mut value = crate::tools::types::model_call(call);
    if let ToolCall::McpCall { arguments, .. } = call {
        value["arguments"] =
            crate::tools::types::parse_mcp_arguments(arguments).expect("admitted typed arguments");
    }
    if let Some(object) = value.as_object_mut() {
        for field in ["content", "old", "new"] {
            if let Some(content) = object.get_mut(field) {
                *content =
                    serde_json::json!(hash(content.as_str().expect("typed content").as_bytes()));
            }
        }
    }
    hash(&serde_json::to_vec(&("arany.supervisor.call.v1", value)).expect("typed call"))
}
fn subject(
    run: &RunView,
    actor: AgentRunId,
    envelope: &EventEnvelope,
) -> Option<(Site, Step, Option<DisproportionFeatures>)> {
    let mut step = Step {
        sequence: envelope.sequence,
        call: [0; 32],
        outcome: [0; 32],
        succeeded: false,
        denied: false,
        alias: false,
        stall: false,
        observation: false,
        edits: Vec::new(),
    };
    let mut footprint = None;
    let site = match &envelope.event {
        Event::ToolFinished { observation: o, .. }
            if !o.intent.call.is_internal()
                && !matches!(o.intent.call, ToolCall::ApplyChangeset { .. }) =>
        {
            step.call = call_key(&o.intent.call);
            let mut projection = crate::tools::types::model_observation(o);
            let object = projection.as_object_mut().expect("typed result");
            object.remove("call");
            object.remove("target");
            let command = matches!(
                o.intent.call,
                ToolCall::Command { .. } | ToolCall::CommandUnguarded { .. }
            )
            .then(|| {
                serde_json::from_str::<CommandResult>(&o.output)
                    .ok()
                    .filter(CommandResult::valid)
            })
            .flatten();
            let exit_class = command.as_ref().map(|c| match c.exit_code {
                Some(0) => 0,
                Some(_) => 1,
                None => 2,
            });
            step.outcome = hash(
                &serde_json::to_vec(&(
                    o.disposition,
                    o.enforcement_cause,
                    exit_class,
                    hash(&serde_json::to_vec(&projection).expect("typed projection")),
                ))
                .expect("typed outcome"),
            );
            step.observation = true;
            step.succeeded = o.disposition == ToolDisposition::Succeeded;
            step.denied = o.disposition == ToolDisposition::Denied || o.enforcement_cause.is_some();
            step.alias =
                o.enforcement_cause == Some(crate::tools::EnforcementCause::FilesystemDenied);
            if step.succeeded {
                if let Some(Attestation::WorkerMutation { .. }) = o.attestation {
                    // The preceding seal already supplies the private edit facts.
                } else if let ToolCall::Write { path, content, .. } = &o.intent.call {
                    step.edits.push((
                        hash(path.as_bytes()),
                        hash(crate::tools::types::hex_digest(content.as_bytes()).as_bytes()),
                    ));
                } else if let ToolCall::Edit { path, .. } = &o.intent.call
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(&o.output)
                    && let Some(digest) = value.get("sha256").and_then(|v| v.as_str())
                {
                    step.edits
                        .push((hash(path.as_bytes()), hash(digest.as_bytes())));
                }
            }
            if let Some(command) = command.filter(|c| !c.changeset.files.is_empty()) {
                let facts: Vec<_> = command
                    .changeset
                    .files
                    .iter()
                    .map(|f| {
                        let version = |v: &crate::tools::FileVersion| WorkerBaseVersion::File {
                            sha256: v.sha256.clone(),
                            bytes: v.bytes().expect("validated command version").len() as u32,
                            executable: v.executable,
                        };
                        crate::session::worker::WorkerFileFact {
                            path: f.path.clone(),
                            before: f.before.as_ref().map_or(WorkerBaseVersion::Absent, version),
                            after: f.after.as_ref().map_or(WorkerBaseVersion::Absent, version),
                        }
                    })
                    .collect();
                footprint = Some(capture_features(run, actor, &facts));
                capture_edits(&mut step, &facts);
                Site::CommandCapture
            } else {
                Site::ToolOutcome
            }
        }
        Event::WorkerDataSealed { record, .. }
            if record.data.purpose == WorkerDataPurpose::Mutation =>
        {
            let data = run
                .workers
                .data
                .iter()
                .find(|d| d.reference == record.data)?;
            let WorkerDataFacts::Changes(facts) = &data.facts else {
                return None;
            };
            footprint = Some(capture_features(run, actor, facts));
            capture_edits(&mut step, facts);
            Site::WorkerCapture
        }
        Event::WorkerHandoverRecorded { record, .. } => {
            let data = run
                .workers
                .data
                .iter()
                .find(|d| d.reference == record.data)?;
            let WorkerDataFacts::Changes(facts) = &data.facts else {
                return None;
            };
            footprint = Some(capture_features(run, record.assignment_id, facts));
            Site::Handover
        }
        Event::ProviderCallRecorded { record, .. }
            if matches!(
                record.disposition,
                crate::session::ProviderCallDisposition::InvalidResponse
            ) =>
        {
            step.stall = true;
            Site::Stall
        }
        Event::HookFinished { outcome, .. }
            if matches!(
                outcome.result,
                crate::tools::permissions::HookResult::Blocked { .. }
            ) && run.lifecycle_hooks.last().is_some_and(|h| {
                matches!(
                    h.invocation.event,
                    crate::tools::permissions::HookEvent::Stop
                        | crate::tools::permissions::HookEvent::SubagentStop
                )
            }) =>
        {
            step.stall = true;
            Site::Stall
        }
        _ => return None,
    };
    Some((site, step, footprint))
}
fn capture_edits(step: &mut Step, facts: &[crate::session::worker::WorkerFileFact]) {
    for f in facts {
        if let WorkerBaseVersion::File { sha256, .. } = &f.after {
            step.edits
                .push((hash(f.path.as_bytes()), hash(sha256.as_bytes())));
        }
    }
}
fn capture_features(
    run: &RunView,
    actor: AgentRunId,
    facts: &[crate::session::worker::WorkerFileFact],
) -> DisproportionFeatures {
    let bytes = |version: &WorkerBaseVersion| match version {
        WorkerBaseVersion::File { bytes, .. } => u64::from(*bytes),
        _ => 0,
    };
    let changed = facts.iter().fold(0u64, |n, f| {
        n.saturating_add(bytes(&f.before))
            .saturating_add(bytes(&f.after))
    });
    let deleted = facts
        .iter()
        .filter(|f| matches!(f.after, WorkerBaseVersion::Absent))
        .count();
    let worker = run
        .agents
        .iter()
        .find(|a| a.id == actor)
        .and_then(|a| a.assignment.as_ref());
    let roots = worker
        .map_or(usize::from(run.supervisor.declared_roots), |w| {
            w.pins.request.lease.len()
        })
        .max(1);
    let base = worker
        .and_then(|w| run.workers.data.iter().find(|d| d.reference == w.pins.base))
        .and_then(|d| match &d.facts {
            WorkerDataFacts::Manifest(entries) => Some(
                entries
                    .iter()
                    .filter(|e| {
                        worker.is_some_and(|w| {
                            crate::session::worker::within(&e.path, &w.pins.request.lease)
                        })
                    })
                    .fold(0u64, |n, e| n.saturating_add(bytes(&e.version))),
            ),
            _ => None,
        });
    DisproportionFeatures {
        files: count(facts.len()),
        deleted_files: count(deleted),
        changed_kib: ratio(changed.saturating_add(1023), 1024),
        declared_roots: count(roots),
        base_kib: base.map(|n| ratio(n.saturating_add(1023), 1024)),
        files_per_root: count(facts.len() / roots),
        delete_permille: ratio((deleted as u64).saturating_mul(1000), facts.len() as u64),
        growth_permille: base.map(|n| ratio(changed.saturating_mul(1000), n)),
    }
}

#[cfg(test)]
mod tests;
